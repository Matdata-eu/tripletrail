//! A dataset's shapes pulled from a git repository (#137): the repository is the master, the
//! server fetches a ref (shallow, with gix), reads the `.ttl` files under a path, checks them
//! and saves them as the next version of the dataset's own set, naming the commit. A commit
//! that fails the check leaves the shapes as they were. Pulls happen when the dataset is bound,
//! when the server first sees it, every `poll` seconds, and on `POST …/shapes/pull` (a
//! GitHub or GitLab push webhook, signed with the binding's secret, or an admin).

use crate::admin::Entry;
use crate::shapes::{self as local, Binding};
use crate::{HttpError, State};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tripletrail::geo::sync::RwLockUnpoisoned;
use tripletrail::shapes::{self, SetFile};

/// One pull at a time, server-wide.
static PULLS: Mutex<()> = Mutex::new(());
/// Where a pull leaves what it found (not copied with the shapes).
const STATUS: &str = ".git-status.json";
/// Where the fetched objects are kept (not copied with the shapes).
const REPOSITORY: &str = ".git";
const PULLED: &str = "refs/tripletrail/pulled";

/// The remote ref a binding's `ref` names: `HEAD`, a full ref, or a branch.
fn remote_ref(name: &str) -> String {
    if name == "HEAD" || name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/heads/{name}")
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What the last pull found.
pub(crate) fn status(folder: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(folder.join(STATUS)).ok()?).ok()
}

fn set_status(folder: &Path, status: &Value) {
    let _ = std::fs::write(folder.join(STATUS), status.to_string());
}

fn read_secret(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .map_err(|e| format!("reading {path}: {e}"))
}

/// A fetch: the commit, the shape files, and each file's path in the repository by name.
type Fetched = (String, Vec<SetFile>, serde_json::Map<String, Value>);

/// Fetches the binding's ref and reads its shape files: the commit, the files, and each
/// file's path in the repository (by its name), for reviewed changes (#145).
fn fetch(folder: &Path, binding: &Binding, max_bytes: Option<u64>) -> Result<Fetched, String> {
    let source = Source {
        url: &binding.url,
        reference: &binding.reference,
        path: &binding.path,
        user: binding.user.as_deref(),
        token_file: binding.token_file.as_deref(),
        max_fetch: max_bytes,
        max_read: max_bytes,
        max_files: Some(MAX_SHAPE_FILES),
    };
    let shape_file = |path: &str| {
        let name = path.rsplit('/').next().unwrap_or(path);
        name.ends_with(".ttl") && !name.starts_with('.')
    };
    let (id, found) = fetch_files(&folder.join(REPOSITORY), &source, &shape_file)?;
    let mut files = Vec::new();
    let mut paths = serde_json::Map::new();
    let folder_path = binding.path.trim_matches('/');
    for (path, data) in found {
        let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
        let text = String::from_utf8(data).map_err(|_| format!("{path} is not UTF-8"))?;
        local::file_name(&name).map_err(|HttpError(_, m)| format!("{path}: {m}"))?;
        let in_repository = if folder_path.is_empty() {
            path.clone()
        } else {
            format!("{folder_path}/{path}")
        };
        paths.insert(name.clone(), json!(in_repository));
        files.push(SetFile { name, text });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(twice) = files.windows(2).find(|w| w[0].name == w[1].name) {
        return Err(format!(
            "two files are named {} (files of subfolders are read as one set)",
            twice[0].name
        ));
    }
    Ok((id, files, paths))
}

/// A file of a pulled folder: its path below the folder, and its bytes.
pub(crate) type PulledFile = (String, Vec<u8>);

/// A folder of a ref of a git repository.
pub(crate) struct Source<'a> {
    pub(crate) url: &'a str,
    /// A branch, `HEAD`, or a full ref.
    pub(crate) reference: &'a str,
    /// The folder in the repository (empty: its root).
    pub(crate) path: &'a str,
    pub(crate) user: Option<&'a str>,
    /// A file on the server with a token for https, sent as the password.
    pub(crate) token_file: Option<&'a str>,
    /// Most bytes one fetch may add to the repository on disk (#419).
    pub(crate) max_fetch: Option<u64>,
    /// Most bytes of files read from the commit, all together.
    pub(crate) max_read: Option<u64>,
    /// Most files read from the commit.
    pub(crate) max_files: Option<usize>,
}

/// Most shape files a pull reads.
const MAX_SHAPE_FILES: usize = 10_000;

/// The bytes of the files under `dir`, subfolders included.
fn size_on_disk(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => size_on_disk(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

fn mib(bytes: u64) -> String {
    let m = bytes as f64 / f64::from(1u32 << 20);
    if m >= 10.0 {
        format!("{m:.0} MiB")
    } else {
        format!("{m:.1} MiB")
    }
}

/// Fetches `source`'s ref (shallow) into the bare repository `dir`, made if missing: the
/// commit, and the files under its path that `keep` takes (by their path below it, subfolders
/// included), sorted by path.
pub(crate) fn fetch_files(
    dir: &Path,
    source: &Source<'_>,
    keep: &dyn Fn(&str) -> bool,
) -> Result<(String, Vec<PulledFile>), String> {
    let error = |e: gix::Error| {
        let text = format!("{e:#}");
        if text.contains("No credentials were returned") {
            "the repository asks for credentials (HTTP 401): give a token in tokenFile".to_owned()
        } else {
            text
        }
    };
    if !dir.join("HEAD").exists() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        gix::init_bare(dir).map_err(error)?;
    }
    let repo = gix::open_opts(dir, gix::open::Options::isolated()).map_err(error)?;
    let url = gix::url::parse(source.url).map_err(|e| e.to_string())?;
    let spec = format!("+{}:{PULLED}", remote_ref(source.reference));
    let remote = repo
        .remote_at(url.clone())
        .map_err(error)?
        .with_refspecs([spec.as_str()], gix::remote::Direction::Fetch)
        .map_err(error)?;
    let shallow = gix::remote::fetch::Shallow::DepthAtRemote(1.try_into().expect("one"));
    let interrupt = AtomicBool::new(false);
    let over = AtomicBool::new(false);
    let done = AtomicBool::new(false);
    let fetched = std::thread::scope(|scope| {
        // A watcher interrupts the fetch once it has added more than `max_fetch` bytes to the
        // repository: gix writes the pack to disk as it arrives, whatever the transport.
        if let Some(limit) = source.max_fetch {
            let (done, over, interrupt) = (&done, &over, &interrupt);
            let before = size_on_disk(dir);
            scope.spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    if size_on_disk(dir).saturating_sub(before) > limit {
                        over.store(true, Ordering::Relaxed);
                        interrupt.store(true, Ordering::Relaxed);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
        }
        let fetched = receive(remote, url, source, shallow, &interrupt, &error);
        done.store(true, Ordering::Relaxed);
        fetched
    });
    if over.load(Ordering::Relaxed) {
        return Err(format!(
            "the fetch passed the limit of {} (--git-fetch-mb)",
            mib(source.max_fetch.unwrap_or_default())
        ));
    }
    fetched?;
    let mut reference = repo.find_reference(PULLED).map_err(error)?;
    let commit = reference.peel_to_commit().map_err(error)?;
    let id = commit.id.to_string();
    let mut tree = commit.tree().map_err(error)?;
    let path = source.path.trim_matches('/');
    if !path.is_empty() {
        let entry = tree
            .lookup_entry_by_path(path)
            .map_err(error)?
            .ok_or_else(|| format!("commit {id} has no {path}"))?;
        tree = entry
            .object()
            .map_err(error)?
            .try_into_tree()
            .map_err(|_| format!("{path} is not a folder in commit {id}"))?;
    }
    let mut files = Vec::new();
    let mut read = Reading {
        keep,
        files: &mut files,
        bytes: 0,
        max_bytes: source.max_read,
        max_files: source.max_files,
    };
    read_tree(&repo, &tree, "", &mut read)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((id, files))
}

/// Fetches into `remote`'s repository.
fn receive(
    remote: gix::Remote<'_>,
    url: gix::Url,
    source: &Source<'_>,
    shallow: gix::remote::fetch::Shallow,
    interrupt: &AtomicBool,
    error: &dyn Fn(gix::Error) -> String,
) -> Result<(), String> {
    let none = |_| Ok(None);
    match url.scheme {
        gix::url::Scheme::Http | gix::url::Scheme::Https => {
            let token = match source.token_file {
                Some(file) => Some(read_secret(file)?),
                None => None,
            };
            let http = Ureq::new(source.user, token.as_deref());
            let transport =
                gix::protocol::transport::client::blocking_io::http::Transport::new_http(
                    http,
                    url,
                    gix::protocol::transport::Protocol::V2,
                    false,
                );
            remote
                .to_connection_with_transport(transport)
                .with_credentials(none)
                .prepare_fetch(gix::progress::Discard, Default::default())
                .map_err(error)?
                .with_shallow(shallow)
                .receive(gix::progress::Discard, interrupt)
                .map_err(error)?;
        }
        _ => {
            remote
                .connect(gix::remote::Direction::Fetch)
                .map_err(error)?
                .with_credentials(none)
                .prepare_fetch(gix::progress::Discard, Default::default())
                .map_err(error)?
                .with_shallow(shallow)
                .receive(gix::progress::Discard, interrupt)
                .map_err(error)?;
        }
    }
    Ok(())
}

/// What `read_tree` takes and has read so far.
struct Reading<'k, 'f> {
    keep: &'k dyn Fn(&str) -> bool,
    files: &'f mut Vec<PulledFile>,
    bytes: u64,
    max_bytes: Option<u64>,
    max_files: Option<usize>,
}

/// The files of `tree` and its subfolders that `keep` takes, within the caps: a blob's size is
/// checked before it is loaded.
fn read_tree(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    at: &str,
    read: &mut Reading<'_, '_>,
) -> Result<(), String> {
    for entry in tree.iter() {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.filename().to_string();
        let mode = entry.mode();
        if mode.is_tree() {
            let sub = repo.find_tree(entry.oid()).map_err(|e| format!("{e:#}"))?;
            read_tree(repo, &sub, &format!("{at}{name}/"), read)?;
        } else if mode.is_blob() && (read.keep)(&format!("{at}{name}")) {
            if read.max_files.is_some_and(|m| read.files.len() >= m) {
                return Err(format!(
                    "more than {} files to read",
                    read.max_files.unwrap_or_default()
                ));
            }
            let size = repo
                .find_header(entry.oid())
                .map_err(|e| format!("{e:#}"))?
                .size();
            read.bytes += size;
            if let Some(max) = read.max_bytes
                && read.bytes > max
            {
                return Err(format!(
                    "the files to read take more than {} (--git-fetch-mb)",
                    mib(max)
                ));
            }
            let blob = repo.find_blob(entry.oid()).map_err(|e| format!("{e:#}"))?;
            read.files.push((format!("{at}{name}"), blob.data.clone()));
        }
    }
    Ok(())
}

/// Pulls the dataset's repository: the new version when the commit changed and passes the
/// check, 422 when it fails it (the shapes stay as they were), 502 when the fetch fails.
pub(crate) fn pull(
    folder: &Path,
    binding: &Binding,
    max_bytes: Option<u64>,
) -> Result<Value, HttpError> {
    let (answer, status) = try_pull(folder, binding, max_bytes);
    set_status(folder, &status);
    answer
}

/// What a pull answers, and what it leaves for `GET …/shapes/git`.
fn try_pull(
    folder: &Path,
    binding: &Binding,
    max_bytes: Option<u64>,
) -> (Result<Value, HttpError>, Value) {
    let _pull = PULLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (commit, files, paths) = match fetch(folder, binding, max_bytes) {
        Ok(found) => found,
        Err(e) => {
            let message = format!("Fetching {} {}: {e}", binding.url, binding.reference);
            let status = json!({ "pulled": now(), "error": message });
            return (Err(HttpError(502, message)), status);
        }
    };
    let _write = local::WRITES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let current = local::current_version(folder);
    let mut source = json!({
        "repository": binding.url,
        "ref": binding.reference,
        "path": binding.path,
        "commit": commit,
    });
    // Unchanged only when the same commit was read from the same place.
    if let Some(version) = current
        && let Some(stored) = local::version_source(folder, version)
        && ["repository", "ref", "path", "commit"]
            .iter()
            .all(|k| stored.get(k) == source.get(k))
    {
        // A version pulled before the paths were recorded gets them now (#145).
        if stored.get("files").is_none() {
            let mut stored = stored;
            stored["files"] = Value::Object(paths);
            let _ = local::set_version_source(folder, version, &stored);
        }
        let answer = json!({ "commit": commit, "version": version, "changed": false });
        let status = json!({ "pulled": now(), "commit": commit, "version": version });
        return (Ok(answer), status);
    }
    // Where each file is in the repository: a reviewed change commits it there.
    source["files"] = Value::Object(paths);
    if let Err(errors) = shapes::load(&files) {
        let message = format!(
            "Commit {commit} of {} is not a valid shape set; the shapes stay as they were:\n{}",
            binding.url,
            errors.join("\n")
        );
        let status =
            json!({ "pulled": now(), "commit": commit, "version": current, "error": message });
        return (Err(HttpError(422, message)), status);
    }
    match local::write_version(folder, &files, Some(&source)) {
        Ok(version) => (
            Ok(
                json!({ "commit": commit, "version": version, "changed": true, "files": files.len() }),
            ),
            json!({ "pulled": now(), "commit": commit, "version": version }),
        ),
        Err(HttpError(code, message)) => (
            Err(HttpError(code, message.clone())),
            json!({ "pulled": now(), "commit": commit, "version": current, "error": message }),
        ),
    }
}

/// Whether a request is a push webhook: it carries GitHub's or GitLab's secret header.
pub(crate) fn is_webhook(request: &tiny_http::Request) -> bool {
    request
        .headers()
        .iter()
        .any(|h| h.field.equiv("X-Hub-Signature-256") || h.field.equiv("X-Gitlab-Token"))
}

fn header<'r>(request: &'r tiny_http::Request, name: &str) -> Option<&'r str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

/// HMAC-SHA256 (RFC 2104).
fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.iter().map(|b| b ^ byte).collect::<Vec<u8>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks a webhook against the binding's secret: GitLab's `X-Gitlab-Token`, or GitHub's
/// `X-Hub-Signature-256` over the body.
pub(crate) fn verify_webhook(
    state: &State,
    request: &mut tiny_http::Request,
    binding: &Binding,
) -> Result<(), HttpError> {
    let refused = |why: &str| HttpError(401, format!("Webhook refused: {why}"));
    let Some(file) = &binding.webhook_secret_file else {
        return Err(refused("the binding has no webhookSecretFile"));
    };
    let secret = read_secret(file).map_err(|e| HttpError(500, e))?;
    if let Some(token) = header(request, "X-Gitlab-Token") {
        return if constant_time_eq(token.trim().as_bytes(), secret.as_bytes()) {
            Ok(())
        } else {
            Err(refused("wrong X-Gitlab-Token"))
        };
    }
    let signature = header(request, "X-Hub-Signature-256")
        .unwrap_or_default()
        .trim()
        .to_owned();
    let body = crate::read_bytes(request, state.options.max_body)?;
    let expected = format!("sha256={}", hex(&hmac_sha256(secret.as_bytes(), &body)));
    if constant_time_eq(signature.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(refused("wrong X-Hub-Signature-256"))
    }
}

/// Binds a dataset to a repository: pulls first, and keeps the binding only when that pull
/// gives a valid set.
pub(crate) fn bind(
    folder: &Path,
    binding: Binding,
    max_bytes: Option<u64>,
) -> Result<Value, HttpError> {
    check(&binding)?;
    // A binding that is refused leaves the status of the one in place, if any, as it was.
    let (pulled, status) = try_pull(folder, &binding, max_bytes);
    let pulled = pulled?;
    binding.save(folder)?;
    set_status(folder, &status);
    Ok(json!({ "bound": binding.url, "pull": pulled }))
}

/// Where a pack's objects are fetched to, in the shapes folder (not copied with the shapes).
const PACK_REPOSITORY: &str = ".pack-git";

/// A shape pack fetched from git (#148): `git` is `{"url", "tag" or "ref", "path", "user",
/// "tokenFile"}`. Answers the files, where they came from (`repository`, `ref`, `path`,
/// `commit`), and a name and version for the pack when none is given: the repository's
/// name, and the tag (else the commit's first 12 digits).
pub(crate) fn fetch_pack(
    folder: &Path,
    git: &Value,
    max_bytes: Option<u64>,
) -> Result<(Vec<SetFile>, Value, String, String), HttpError> {
    let mut git = git.clone();
    if let Some(tag) = git.get("tag").and_then(Value::as_str) {
        if git.get("ref").is_some() {
            return Err(HttpError::bad_request(
                "Give the pack's tag or its ref, not both",
            ));
        }
        git["ref"] = json!(format!("refs/tags/{tag}"));
    }
    let binding = Binding::from_json(&git)?;
    check(&binding)?;
    let (commit, files, _) = {
        let _pull = PULLS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fetch(&folder.join(PACK_REPOSITORY), &binding, max_bytes).map_err(|e| {
            HttpError(
                502,
                format!("Fetching {} {}: {e}", binding.url, binding.reference),
            )
        })?
    };
    if files.is_empty() {
        return Err(HttpError(
            422,
            format!(
                "Commit {commit} of {} has no .ttl files under {:?}",
                binding.url, binding.path
            ),
        ));
    }
    let name = binding
        .url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default()
        .trim_end_matches(".git")
        .to_owned();
    let version = match binding.reference.strip_prefix("refs/tags/") {
        Some(tag) => tag.to_owned(),
        None => commit.chars().take(12).collect(),
    };
    let source = json!({
        "repository": binding.url,
        "ref": binding.reference,
        "path": binding.path,
        "commit": commit,
    });
    Ok((files, source, name, version))
}

/// Refuses a repository that cannot be fetched safely: credentials in the URL, a token sent
/// over plain http, a ref that is not one, a secret file that cannot be read.
fn check(binding: &Binding) -> Result<(), HttpError> {
    let url = gix::url::parse(binding.url.as_str())
        .map_err(|e| HttpError::bad_request(format!("Invalid repository URL: {e}")))?;
    let web = matches!(url.scheme, gix::url::Scheme::Http | gix::url::Scheme::Https);
    if url.password().is_some() || (web && url.user().is_some()) {
        return Err(HttpError::bad_request(
            "The repository URL holds credentials, which would be shown and stored with it: \
             give the user as user and the token in a tokenFile",
        ));
    }
    if binding.token_file.is_some() && url.scheme != gix::url::Scheme::Https {
        return Err(HttpError::bad_request(
            "A tokenFile is only sent over https",
        ));
    }
    if let Some(api) = &binding.api
        && !local::safe_for_tokens(api)
    {
        return Err(HttpError::bad_request(
            "The forge's api is only called over https (or plain http to this machine)",
        ));
    }
    if binding.reference.is_empty() || binding.reference.contains([' ', ':', '*', '^', '~']) {
        return Err(HttpError::bad_request(format!(
            "Invalid ref {:?}",
            binding.reference
        )));
    }
    for file in [
        &binding.token_file,
        &binding.webhook_secret_file,
        &binding.review_token_file,
    ]
    .into_iter()
    .flatten()
    {
        read_secret(file).map_err(HttpError::bad_request)?;
    }
    Ok(())
}

/// Pulls bound datasets in the background: once when first seen, then every `poll` seconds.
/// Ends when the server is dropped or stops.
pub(crate) fn watch(state: Weak<State>) {
    std::thread::spawn(move || {
        let mut last: HashMap<String, Instant> = HashMap::new();
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let Some(state) = state.upgrade() else { return };
            if state.stopping.load(Ordering::Relaxed) {
                return;
            }
            let entries: Vec<Arc<Entry>> = state
                .registry
                .read_unpoisoned()
                .datasets
                .values()
                .cloned()
                .collect();
            for entry in entries {
                let Some(folder) = local::folder_of(&state, &entry) else {
                    continue;
                };
                let Some(binding) = Binding::read(&folder) else {
                    last.remove(&entry.name);
                    continue;
                };
                let due = last.get(&entry.name).is_none_or(|at| {
                    binding
                        .poll
                        .is_some_and(|p| at.elapsed() >= Duration::from_secs(p.max(10)))
                });
                if due {
                    last.insert(entry.name.clone(), Instant::now());
                    if let Err(HttpError(_, e)) = pull(&folder, &binding, state.options.git_fetch) {
                        eprintln!("pulling the shapes of {}: {e}", entry.name);
                    }
                }
            }
        }
    });
}

/// gix's HTTP transport on ureq: each request on its own thread, the body to post, the
/// headers and the answer through channels.
struct Ureq {
    agent: ureq::Agent,
    authorization: Option<String>,
}

impl Ureq {
    fn new(user: Option<&str>, token: Option<&str>) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(20))
                .timeout_read(Duration::from_secs(120))
                .build(),
            authorization: token.map(|t| {
                let user = user.unwrap_or("x-access-token");
                format!("Basic {}", base64(format!("{user}:{t}").as_bytes()))
            }),
        }
    }

    fn request(
        &self,
        url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        post: Option<gix::protocol::transport::client::blocking_io::http::PostBodyDataKind>,
    ) -> Response {
        let mut request = self
            .agent
            .request(if post.is_some() { "POST" } else { "GET" }, url);
        for line in headers {
            if let Some((name, value)) = line.as_ref().split_once(':') {
                request = request.set(name.trim(), value.trim());
            }
        }
        if let Some(auth) = &self.authorization {
            request = request.set("Authorization", auth);
        }
        let (body_tx, body_rx) = sync_channel::<io::Result<Vec<u8>>>(8);
        let (head_tx, head_rx) = sync_channel(1);
        let (answer_tx, answer_rx) = sync_channel(8);
        std::thread::spawn(move || {
            let sent = match post {
                None => {
                    drop(body_rx);
                    request.call()
                }
                Some(_) => {
                    let mut body = Vec::new();
                    let read = Channel::new(body_rx).read_to_end(&mut body);
                    match read {
                        Ok(_) => request.send_bytes(&body),
                        Err(e) => {
                            let _ = head_tx.send(Err(e));
                            return;
                        }
                    }
                }
            };
            let response = match sent {
                Ok(r) => r,
                Err(ureq::Error::Status(401, _)) => {
                    let _ = head_tx.send(Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "the repository needs credentials (HTTP 401): check tokenFile",
                    )));
                    return;
                }
                Err(ureq::Error::Status(code, _)) => {
                    let _ = head_tx.send(Err(io::Error::other(format!("HTTP status {code}"))));
                    return;
                }
                Err(e) => {
                    let _ = head_tx.send(Err(io::Error::other(e.to_string())));
                    return;
                }
            };
            let mut head = Vec::new();
            for name in response.headers_names() {
                if let Some(value) = response.header(&name) {
                    head.extend_from_slice(format!("{name}: {value}\n").as_bytes());
                }
            }
            if head_tx.send(Ok(head)).is_err() {
                return;
            }
            drop(head_tx);
            let mut reader = response.into_reader();
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(n) => {
                        if answer_tx.send(Ok(chunk[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = answer_tx.send(Err(e));
                        return;
                    }
                }
            }
        });
        Response {
            post_body: Sender(body_tx),
            headers: Channel::new(head_rx),
            body: Channel::new(answer_rx),
        }
    }
}

struct Response {
    post_body: Sender,
    headers: Channel,
    body: Channel,
}

/// The body to post, chunk by chunk.
struct Sender(SyncSender<io::Result<Vec<u8>>>);

impl Write for Sender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .send(Ok(buf.to_vec()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the request ended"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Bytes arriving in chunks, or an error; the end when the sender is gone.
struct Channel {
    from: Receiver<io::Result<Vec<u8>>>,
    buffer: Vec<u8>,
    at: usize,
}

impl Channel {
    fn new(from: Receiver<io::Result<Vec<u8>>>) -> Self {
        Self {
            from,
            buffer: Vec::new(),
            at: 0,
        }
    }
}

impl Read for Channel {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Channel {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        while self.at >= self.buffer.len() {
            match self.from.recv() {
                Ok(Ok(chunk)) => {
                    self.buffer = chunk;
                    self.at = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(&[]),
            }
        }
        Ok(&self.buffer[self.at..])
    }

    fn consume(&mut self, amount: usize) {
        self.at += amount;
    }
}

mod http_impl {
    use super::{Channel, Sender, Ureq};
    use gix::protocol::transport::client::blocking_io::http::{
        GetResponse, Http, PostBodyDataKind, PostResponse,
    };

    impl Http for Ureq {
        type Headers = Channel;
        type ResponseBody = Channel;
        type PostBody = Sender;

        fn get(
            &mut self,
            url: &str,
            _base_url: &str,
            headers: impl IntoIterator<Item = impl AsRef<str>>,
        ) -> gix::Result<GetResponse<Channel, Channel>> {
            let r = self.request(url, headers, None);
            Ok(GetResponse {
                headers: r.headers,
                body: r.body,
            })
        }

        fn post(
            &mut self,
            url: &str,
            _base_url: &str,
            headers: impl IntoIterator<Item = impl AsRef<str>>,
            body: PostBodyDataKind,
        ) -> gix::Result<PostResponse<Channel, Channel, Sender>> {
            let r = self.request(url, headers, Some(body));
            Ok(PostResponse {
                post_body: r.post_body,
                headers: r.headers,
                body: r.body,
            })
        }

        fn configure(&mut self, _config: &dyn std::any::Any) -> gix::Result {
            Ok(())
        }
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_and_hmac() {
        assert_eq!(base64(b"user:token"), "dXNlcjp0b2tlbg==");
        assert_eq!(base64(b"ab"), "YWI=");
        // RFC 4231, test case 2.
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn refs() {
        assert_eq!(remote_ref("main"), "refs/heads/main");
        assert_eq!(remote_ref("HEAD"), "HEAD");
        assert_eq!(remote_ref("refs/tags/v1"), "refs/tags/v1");
    }
}
