//! `/$/datasets/{name}/shapes`: the shapes a reader reads (#137), the dataset's installed
//! pack (#148), and the dataset's own shape
//! set. A dataset is read with shapes generated from its profile, under its own set where it
//! has one: a folder of Turtle files, one per node shape, kept in versions (`v1`, `v2`…, the
//! last [`KEEP`]) in the store directory's `shapes` folder, so a store copy carries them.
//! See [`tripletrail::shapes`].

pub(crate) mod edit;

use crate::View;
use crate::admin::Entry;
use crate::{HttpError, Params, Reply, State};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tripletrail::oxigraph::io::{RdfFormat, RdfSerializer};
use tripletrail::oxigraph::model::vocab::{rdf, xsd};
use tripletrail::oxigraph::model::{Graph, Literal, NamedNode, Triple};
use tripletrail::shapes::{self, Reader, SetFile};

/// Versions of a dataset's shape set kept.
pub(crate) const KEEP: u64 = 10;
/// The dataset's own set, in the answers.
const LOCAL_SET: &str = "urn:tripletrail:shapes:local";
const CURRENT: &str = "CURRENT";
/// The installed pack (#148), in the answers, and its folder and description in the shapes
/// folder.
const PACK_SET: &str = "urn:tripletrail:shapes:pack";
const PACK: &str = "pack";
const PACK_INFO: &str = "pack.json";
/// Where a write keeps the pack it replaces until the new one is in place.
const PACK_OLD: &str = ".pack.old";
/// One write at a time, server-wide.
pub(crate) static WRITES: Mutex<()> = Mutex::new(());

/// Where the dataset's shapes come from when it is bound to a git repository (`git.json`).
#[derive(Clone, Debug)]
pub(crate) struct Binding {
    pub(crate) url: String,
    /// A branch, `HEAD`, or a full ref (`refs/tags/v1`).
    pub(crate) reference: String,
    /// The folder of the repository whose `.ttl` files (subfolders included) are the set.
    pub(crate) path: String,
    pub(crate) user: Option<String>,
    /// A file on the server with a token for https, sent as the password.
    pub(crate) token_file: Option<String>,
    /// A file on the server with the secret push webhooks are signed with.
    pub(crate) webhook_secret_file: Option<String>,
    /// Seconds between pulls; none when only the webhook (or an admin) pulls.
    pub(crate) poll: Option<u64>,
    /// For reviewed changes (#145): `github` or `gitlab`, when the URL does not say.
    pub(crate) forge: Option<String>,
    /// The forge's REST API, when it is not the usual one for the URL's host.
    pub(crate) api: Option<String>,
    /// The repository on the forge (`owner/repo`, a GitLab project's path), when the URL's
    /// path is not it.
    pub(crate) project: Option<String>,
    /// A file on the server with the token proposals use on the forge's API (one that may
    /// push branches and open pull requests), when it is not `token_file`.
    pub(crate) review_token_file: Option<String>,
    /// Whether a proposal's comment on the forge lists the new validation results (focus
    /// nodes and values: data). Without it, the comment gives counts by shape only, since
    /// those who may read the repository need not be those who may read the data.
    pub(crate) review_results: bool,
}

/// Whether a token may be sent to `url`: over https, or plain http to this machine only.
#[cfg_attr(not(feature = "git"), allow(dead_code))]
pub(crate) fn safe_for_tokens(url: &str) -> bool {
    url.starts_with("https://")
        || ["http://127.0.0.1", "http://localhost", "http://[::1]"]
            .iter()
            .any(|local| {
                url.strip_prefix(local)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/']))
            })
}

const BINDING: &str = "git.json";

impl Binding {
    pub(crate) fn read(folder: &Path) -> Option<Self> {
        let value: Value =
            serde_json::from_str(&fs::read_to_string(folder.join(BINDING)).ok()?).ok()?;
        Self::from_json(&value).ok()
    }

    pub(crate) fn from_json(value: &Value) -> Result<Self, HttpError> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        Ok(Self {
            url: text("url").ok_or_else(|| HttpError::bad_request("The binding needs a url"))?,
            reference: text("ref").unwrap_or_else(|| "HEAD".to_owned()),
            path: text("path").unwrap_or_default(),
            user: text("user"),
            token_file: text("tokenFile"),
            webhook_secret_file: text("webhookSecretFile"),
            poll: value.get("poll").and_then(Value::as_u64),
            forge: text("forge"),
            api: text("api"),
            project: text("project"),
            review_token_file: text("reviewTokenFile"),
            review_results: match value.get("reviewResults") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(HttpError::bad_request("reviewResults is true or false"));
                }
            },
        })
    }

    /// As saved, and as `GET …/shapes/git` shows it (`public`: without the server's paths).
    pub(crate) fn to_json(&self, public: bool) -> Value {
        let path = |p: &Option<String>| {
            if public {
                Value::Bool(p.is_some())
            } else {
                json!(p)
            }
        };
        json!({
            "url": self.url,
            "ref": self.reference,
            "path": self.path,
            "user": self.user,
            if public { "token" } else { "tokenFile" }: path(&self.token_file),
            if public { "webhook" } else { "webhookSecretFile" }: path(&self.webhook_secret_file),
            "poll": self.poll,
            "forge": self.forge,
            "api": self.api,
            "project": self.project,
            if public { "reviewToken" } else { "reviewTokenFile" }: path(&self.review_token_file),
            "reviewResults": self.review_results,
        })
    }

    #[cfg_attr(not(feature = "git"), allow(dead_code))]
    pub(crate) fn save(&self, folder: &Path) -> Result<(), HttpError> {
        fs::create_dir_all(folder)?;
        let tmp = folder.join(".git.json.writing");
        fs::write(
            &tmp,
            serde_json::to_string_pretty(&self.to_json(false)).unwrap_or_default(),
        )?;
        fs::rename(&tmp, folder.join(BINDING))?;
        Ok(())
    }
}

/// The `reader` parameter.
fn reader(params: &Params) -> Result<Option<Reader>, HttpError> {
    params
        .get("reader")
        .map(|name| {
            Reader::from_name(name).ok_or_else(|| {
                let names: Vec<&str> = Reader::ALL.iter().map(|r| r.name()).collect();
                HttpError::bad_request(format!(
                    "Unknown reader {name}: one of {}",
                    names.join(", ")
                ))
            })
        })
        .transpose()
}

/// The `profile` parameters (IRIs, repeated or comma-separated).
fn profiles(params: &Params) -> Result<Vec<NamedNode>, HttpError> {
    params
        .0
        .iter()
        .filter(|(k, _)| k == "profile")
        .flat_map(|(_, v)| v.split(','))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| {
            NamedNode::new(v).map_err(|_| HttpError::bad_request(format!("Invalid profile {v}")))
        })
        .collect()
}

/// Where the dataset's shape set is kept: in its store directory, or for an in-memory
/// dataset under the server's `--location`.
fn folder(state: &State, entry: &Entry) -> Result<PathBuf, HttpError> {
    if let Some(location) = &entry.location {
        return Ok(location.join("shapes"));
    }
    memory_folder(state, &entry.name).ok_or_else(|| {
        HttpError(
            409,
            format!(
                "Dataset {} is in memory and the server has no --location to keep its shapes in",
                entry.name
            ),
        )
    })
}

/// The dataset's shapes folder, when it has a set of its own.
pub(crate) fn own_folder(state: &State, entry: &Entry) -> Option<PathBuf> {
    folder(state, entry)
        .ok()
        .filter(|f| current(f).is_some() || pack_dir(f).is_some())
}

/// Copies a dataset's shapes folder (every kept version) to `to`, which must not exist yet:
/// into a store copy (`<copy>/shapes`), or to another dataset.
pub(crate) fn copy_folder(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let target = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_folder(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// The dataset's shapes folder, if it can have one.
pub(crate) fn folder_of(state: &State, entry: &Entry) -> Option<PathBuf> {
    folder(state, entry).ok()
}

/// Where an in-memory dataset's shapes are kept, removed when it is detached.
pub(crate) fn memory_folder(state: &State, name: &str) -> Option<PathBuf> {
    state
        .options
        .location
        .as_ref()
        .map(|base| base.join(".shapes").join(name))
}

/// Warnings about the names from before the rename to TripleTrail (#955), for the server's
/// start: shape files in the dataset's shapes folder (its own set and its pack) that use the
/// old namespaces, and the dataset's graphs with an old name.
pub(crate) fn old_names(state: &State, entry: &Entry) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(folder) = folder_of(state, entry) {
        if let Ok(Some((version, files))) = own_files(&folder) {
            let place = format!("{}/", folder.join(format!("v{version}")).display());
            warnings.extend(shapes::old_names(&files, &place));
        }
        if let (Some(dir), Ok(Some((_, files)))) = (pack_dir(&folder), pack_files(&folder)) {
            let place = format!("{}/", dir.display());
            warnings.extend(shapes::old_names(&files, &place));
        }
    }
    let filter = shapes::OLD_NAMES
        .iter()
        .map(|(old, _)| format!("STRSTARTS(STR(?g), \"{old}\")"))
        .collect::<Vec<_>>()
        .join(" || ");
    let query =
        format!("SELECT DISTINCT ?g WHERE {{ GRAPH ?g {{}} FILTER({filter}) }} ORDER BY ?g");
    if let Ok(tripletrail::oxigraph::sparql::QueryResults::Solutions(rows)) =
        entry.engine.query(&query)
    {
        for row in rows.flatten() {
            let Some(tripletrail::oxigraph::model::Term::NamedNode(graph)) = row.get("g") else {
                continue;
            };
            let old = graph.as_str();
            let Some(new) = shapes::new_name(old) else {
                continue;
            };
            warnings.push(format!(
                "graph <{old}> has a name from before the rename to TripleTrail, which the \
                 server no longer reads or writes: rename it with MOVE <{old}> TO <{new}>"
            ));
        }
    }
    warnings
}

/// The kept versions, oldest first.
fn versions(folder: &Path) -> Vec<u64> {
    let mut versions: Vec<u64> = fs::read_dir(folder)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.strip_prefix('v')?.parse().ok())
        .collect();
    versions.sort_unstable();
    versions
}

#[cfg_attr(not(feature = "git"), allow(dead_code))]
pub(crate) fn current_version(folder: &Path) -> Option<u64> {
    current(folder)
}

/// What the readers' shapes are made from besides the data, for caches: the own set's version
/// and when the installed pack was written (a pack installed or removed changes it; #92).
pub(crate) fn shapes_stamp(folder: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let own = current(folder);
    let pack = pack_dir(folder).and_then(|dir| {
        let meta = fs::metadata(dir.join(PACK_INFO)).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    });
    if own.is_none() && pack.is_none() {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (own, pack).hash(&mut hasher);
    Some(hasher.finish())
}

/// Where a version came from, when it was pulled from a repository.
pub(crate) fn version_source(folder: &Path, version: u64) -> Option<Value> {
    let text = fs::read_to_string(folder.join(format!("v{version}")).join(SOURCE)).ok()?;
    serde_json::from_str(&text).ok()
}

const SOURCE: &str = "source.json";

/// Rewrites where a version came from (a pull adding what an older one did not record).
#[cfg(feature = "git")]
pub(crate) fn set_version_source(
    folder: &Path,
    version: u64,
    source: &Value,
) -> Result<(), HttpError> {
    let dir = folder.join(format!("v{version}"));
    let tmp = dir.join(".source.json.writing");
    fs::write(&tmp, source.to_string())?;
    fs::rename(&tmp, dir.join(SOURCE))?;
    Ok(())
}

fn current(folder: &Path) -> Option<u64> {
    fs::read_to_string(folder.join(CURRENT))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn read_version(folder: &Path, version: u64) -> Result<Vec<SetFile>, HttpError> {
    let dir = folder.join(format!("v{version}"));
    let mut files = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".ttl") && !name.starts_with('.') {
            files.push(SetFile {
                text: fs::read_to_string(entry.path())?,
                name,
            });
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(files)
}

/// The dataset's own set: its version and files, if it has one.
fn own_files(folder: &Path) -> Result<Option<(u64, Vec<SetFile>)>, HttpError> {
    match current(folder) {
        Some(version) => Ok(Some((version, read_version(folder, version)?))),
        None => Ok(None),
    }
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Writes `files` as the next version and makes it current, keeping the last [`KEEP`].
pub(crate) fn write_version(
    folder: &Path,
    files: &[SetFile],
    source: Option<&Value>,
) -> Result<u64, HttpError> {
    fs::create_dir_all(folder)?;
    let version = versions(folder).last().map_or(1, |v| v + 1);
    let tmp = folder.join(format!(".v{version}.writing"));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    for file in files {
        let path = tmp.join(&file.name);
        fs::write(&path, &file.text)?;
        fs::File::open(&path)?.sync_all()?;
    }
    if let Some(source) = source {
        fs::write(tmp.join(SOURCE), source.to_string())?;
    }
    sync_dir(&tmp);
    fs::rename(&tmp, folder.join(format!("v{version}")))?;
    let pointer = folder.join(".CURRENT.writing");
    fs::write(&pointer, format!("{version}\n"))?;
    fs::File::open(&pointer)?.sync_all()?;
    fs::rename(&pointer, folder.join(CURRENT))?;
    sync_dir(folder);
    for old in versions(folder) {
        if old + KEEP <= version {
            let _ = fs::remove_dir_all(folder.join(format!("v{old}")));
        }
    }
    Ok(version)
}

/// A file name of a set: letters, digits, `_`, `-` and `.`, ending in `.ttl`.
pub(crate) fn file_name(name: &str) -> Result<&str, HttpError> {
    let ok = name.ends_with(".ttl")
        && name.len() > 4
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(name)
    } else {
        Err(HttpError::bad_request(format!(
            "Invalid shape file name {name}: letters, digits, _, - and ., ending in .ttl"
        )))
    }
}

fn invalid(errors: &[String]) -> HttpError {
    HttpError(
        422,
        format!("The shapes are not valid:\n{}", errors.join("\n")),
    )
}

/// The dataset's own set as a graph, with what it is.
fn own_set(version: u64, files: &[SetFile], source: Option<&Value>) -> Result<Graph, HttpError> {
    let mut graph = shapes::load(files).map_err(|e| invalid(&e))?;
    let set = NamedNode::new_unchecked(LOCAL_SET);
    let tts = |local: &str| NamedNode::new_unchecked(format!("{}{local}", shapes::TTS));
    graph.insert(&Triple::new(set.clone(), rdf::TYPE, tts("ShapeSet")));
    let repository = source.and_then(|s| s.get("repository")?.as_str());
    graph.insert(&Triple::new(
        set.clone(),
        tts("source"),
        Literal::new_simple_literal(repository.unwrap_or("local")),
    ));
    if let Some(commit) = source.and_then(|s| s.get("commit")?.as_str()) {
        graph.insert(&Triple::new(
            set.clone(),
            tts("commit"),
            Literal::new_simple_literal(commit),
        ));
    }
    graph.insert(&Triple::new(
        set,
        tts("version"),
        Literal::new_typed_literal(version.to_string(), xsd::INTEGER),
    ));
    Ok(graph)
}

/// The generated set (with the pack's shapes on top), without what makes it the generated
/// one or the pack, as the start of a set.
fn customisable(generated: &Graph) -> Graph {
    let in_set = NamedNode::new_unchecked(format!("{}inSet", shapes::TTS));
    let set = NamedNode::new_unchecked(shapes::GENERATED_SET);
    let pack = NamedNode::new_unchecked(PACK_SET);
    let mut graph = Graph::new();
    for t in generated.iter() {
        if t.predicate != in_set.as_ref()
            && t.subject != set.as_ref().into()
            && t.subject != pack.as_ref().into()
        {
            graph.insert(t);
        }
    }
    graph
}

/// The installed pack (#148): what `pack.json` says of it (`name`, `version`, `source`) and its
/// files, if the dataset has one.
fn pack_files(folder: &Path) -> Result<Option<(Value, Vec<SetFile>)>, HttpError> {
    let Some(dir) = pack_dir(folder) else {
        return Ok(None);
    };
    let Ok(text) = fs::read_to_string(dir.join(PACK_INFO)) else {
        return Ok(None);
    };
    let info: Value = serde_json::from_str(&text)
        .map_err(|e| HttpError(500, format!("The installed pack's {PACK_INFO}: {e}")))?;
    let mut files = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".ttl") && !name.starts_with('.') {
            files.push(SetFile {
                text: fs::read_to_string(entry.path())?,
                name,
            });
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Some((info, files)))
}

/// The folder of the installed pack. A write that stopped between its two renames left the
/// pack it replaced in `.pack.old`: that pack is still the installed one, and is put back as
/// soon as no write is running.
fn pack_dir(folder: &Path) -> Option<PathBuf> {
    let dir = folder.join(PACK);
    if dir.join(PACK_INFO).is_file() {
        return Some(dir);
    }
    let old = folder.join(PACK_OLD);
    if !old.join(PACK_INFO).is_file() {
        return None;
    }
    // Not while a write runs (it may be between its renames); a write puts it back itself.
    if let Ok(_write) = WRITES.try_lock() {
        restore_pack(folder);
        if dir.join(PACK_INFO).is_file() {
            return Some(dir);
        }
    }
    Some(old)
}

/// Puts back a pack a stopped write left in `.pack.old`. Under [`WRITES`].
fn restore_pack(folder: &Path) {
    let (dir, old) = (folder.join(PACK), folder.join(PACK_OLD));
    if !dir.join(PACK_INFO).is_file() && old.join(PACK_INFO).is_file() {
        let _ = fs::remove_dir_all(&dir);
        if fs::rename(&old, &dir).is_ok() {
            sync_dir(folder);
        }
    }
}

/// `name@version` of an installed pack.
fn pack_label(info: &Value) -> String {
    let text = |key: &str| info.get(key).and_then(Value::as_str).unwrap_or("?");
    format!("{}@{}", text("name"), text("version"))
}

/// The installed pack as a graph, with what it is: its node `urn:tripletrail:shapes:pack` carries
/// `tts:pack` (the name), `tts:packVersion` and `tts:source`.
fn pack_set(info: &Value, files: &[SetFile]) -> Result<Graph, HttpError> {
    let mut graph = shapes::load(files).map_err(|e| invalid(&e))?;
    let set = NamedNode::new_unchecked(PACK_SET);
    let tts = |local: &str| NamedNode::new_unchecked(format!("{}{local}", shapes::TTS));
    let text = |key: &str| info.get(key).and_then(Value::as_str).unwrap_or_default();
    graph.insert(&Triple::new(set.clone(), rdf::TYPE, tts("ShapeSet")));
    graph.insert(&Triple::new(
        set.clone(),
        tts("pack"),
        Literal::new_simple_literal(text("name")),
    ));
    graph.insert(&Triple::new(
        set.clone(),
        tts("packVersion"),
        Literal::new_simple_literal(text("version")),
    ));
    let source = info
        .get("source")
        .and_then(|s| s.get("repository")?.as_str())
        .unwrap_or("local");
    graph.insert(&Triple::new(
        set.clone(),
        tts("source"),
        Literal::new_simple_literal(source),
    ));
    if let Some(commit) = info.get("source").and_then(|s| s.get("commit")?.as_str()) {
        graph.insert(&Triple::new(
            set,
            tts("commit"),
            Literal::new_simple_literal(commit),
        ));
    }
    Ok(graph)
}

/// `dry=`: a check that keeps nothing (empty, `true` or `1`) or not (`false` or `0`). Any
/// other value is refused, so that a check that was meant never makes the change.
pub(crate) fn dry_run(params: &Params) -> Result<bool, HttpError> {
    match params.get("dry") {
        None | Some("false" | "0") => Ok(false),
        Some("" | "true" | "1") => Ok(true),
        Some(other) => Err(HttpError::bad_request(format!(
            "dry is true, 1, false or 0 (or empty for true), not {other:?}"
        ))),
    }
}

/// What installing the pack release `pack` would do (#148), as `PUT …/shapes/pack?dry=true`
/// and `tripletrail shapes check` answer it: the errors of the release and of the own set, what
/// changes against the `installed` release, the own set's overrides (`overrides`, and
/// `orphaned`: those that would stand alone), and with `has_instances`, the release's shapes
/// for classes the data has no instances of (`noData`; null when the data is not checked).
/// `valid` is false when a set has errors.
pub fn pack_check(
    installed: Option<&[SetFile]>,
    pack: &[SetFile],
    own: Option<&[SetFile]>,
    has_instances: Option<&dyn Fn(&NamedNode) -> bool>,
) -> Value {
    let mut errors = Vec::new();
    let mut load = |files: &[SetFile], layer: &str| match shapes::load(files) {
        Ok(graph) => Some(graph),
        Err(e) => {
            errors.extend(e.into_iter().map(|e| format!("{layer}: {e}")));
            None
        }
    };
    let new = load(pack, "pack");
    let own = match own {
        Some(files) => load(files, "own set"),
        None => Some(Graph::new()),
    };
    // An installed release that no longer reads is compared as if there was none.
    let installed = installed.and_then(|files| shapes::load(files).ok());
    let (Some(new), Some(own)) = (new, own) else {
        return json!({ "valid": false, "errors": errors });
    };
    let all = |_: &NamedNode| true;
    let up = shapes::upgrade(
        installed.as_ref(),
        &new,
        &own,
        has_instances.unwrap_or(&all),
    );
    let iris = |v: &[NamedNode]| v.iter().map(|n| json!(n.as_str())).collect::<Vec<_>>();
    let overrides = |v: &[shapes::Override]| {
        v.iter()
            .map(|o| json!({ "shape": o.shape, "packShape": o.pack_shape.as_str(), "how": o.how }))
            .collect::<Vec<_>>()
    };
    json!({
        "valid": true,
        "errors": errors,
        "added": iris(&up.added),
        "removed": iris(&up.removed),
        "changed": iris(&up.changed),
        "overrides": overrides(&up.overrides),
        "orphaned": overrides(&up.orphaned),
        "noData": has_instances.map(|_| up
            .no_data
            .iter()
            .map(|(shape, class)| json!({ "shape": shape.as_str(), "class": class.as_str() }))
            .collect::<Vec<_>>()),
    })
}

/// Whether the dataset has an instance of `class`, in any graph, under the dataset's query
/// timeout. A query that fails (the timeout) counts as yes: only a sure "none" is reported.
fn has_instances(state: &State, entry: &Entry) -> impl Fn(&NamedNode) -> bool {
    let timeout = crate::limits::Limits::of(state, &entry.name).timeout(None);
    let engine = entry.engine.clone();
    move |class: &NamedNode| {
        let (engine, _deadline) = crate::with_deadline(&engine, timeout);
        let ask = format!("ASK {{ {{ ?s a {class} }} UNION {{ GRAPH ?g {{ ?s a {class} }} }} }}");
        !matches!(
            engine.query(&ask),
            Ok(tripletrail::oxigraph::sparql::QueryResults::Boolean(false))
        )
    }
}

/// A pack's name or version: letters, digits, `_`, `-`, `.` and `+`, at most 100.
fn pack_name<'a>(what: &str, value: Option<&'a Value>) -> Result<&'a str, HttpError> {
    let text = value.and_then(Value::as_str).unwrap_or_default();
    let ok = !text.is_empty()
        && text.len() <= 100
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'));
    if ok {
        Ok(text)
    } else {
        Err(HttpError::bad_request(format!(
            "The pack needs a {what}: letters, digits, _, -, . and +"
        )))
    }
}

/// The `.ttl` files of a folder (not of its subfolders), as a shape set, sorted by name.
pub fn folder_files(dir: &Path) -> Result<Vec<SetFile>, String> {
    let what = |e: std::io::Error| format!("{}: {e}", dir.display());
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).map_err(what)? {
        let entry = entry.map_err(what)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".ttl") && !name.starts_with('.') && entry.path().is_file() {
            files.push(SetFile {
                text: fs::read_to_string(entry.path()).map_err(what)?,
                name,
            });
        }
    }
    if files.is_empty() {
        return Err(format!("{} has no .ttl files", dir.display()));
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(files)
}

/// `serve --pack` (#148): installs the pack in the folder `dir` on the dataset, unless that
/// release is installed already. The folder's `pack.json` names it (`name`, `version`);
/// without one, it is named after the folder, version `local`. Answers what was done.
pub(crate) fn install_folder(state: &State, entry: &Entry, dir: &Path) -> Result<String, String> {
    if entry.mode == crate::admin::Mode::ReadOnly {
        return Err(format!("dataset {} is attached read-only", entry.name));
    }
    let folder = folder(state, entry).map_err(|e| e.1)?;
    let files = folder_files(dir)?;
    // Only a folder without pack.json is named after the folder: one that cannot be read
    // is an error, not a pack of another name.
    let info_path = dir.join(PACK_INFO);
    let given: Value = match fs::read_to_string(&info_path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", info_path.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(format!("{}: {e}", info_path.display())),
    };
    let folder_name = dir
        .canonicalize()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let name = pack_name(
        "name (in pack.json, or the folder's)",
        Some(given.get("name").unwrap_or(&json!(folder_name))),
    )
    .map_err(|e| e.1)?
    .to_owned();
    let version = pack_name(
        "version",
        Some(given.get("version").unwrap_or(&json!("local"))),
    )
    .map_err(|e| e.1)?
    .to_owned();
    shapes::load(&files).map_err(|e| e.join("; "))?;
    let mut info = json!({ "name": name, "version": version });
    // As in a `PUT`: where the pack comes from (`repository`, a CI build's `commit`…).
    if let Some(source) = given.get("source").filter(|v| v.is_object()) {
        info["source"] = source.clone();
    }
    let label = pack_label(&info);
    let _write = WRITES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let installed = pack_files(&folder).map_err(|e| e.1)?;
    if let Some((i, f)) = &installed
        && pack_label(i) == label
        && i.get("source") == info.get("source")
        && *f == files
    {
        return Ok(format!("pack {label} is installed"));
    }
    write_pack(&folder, &info, &files).map_err(|e| e.1)?;
    Ok(match installed {
        Some((i, _)) => format!("installed pack {label}, in place of {}", pack_label(&i)),
        None => format!("installed pack {label}"),
    })
}

/// Installs a pack in place of the one there, in one rename: the new folder is written aside
/// and swapped in.
fn write_pack(folder: &Path, info: &Value, files: &[SetFile]) -> Result<(), HttpError> {
    fs::create_dir_all(folder)?;
    restore_pack(folder);
    let tmp = folder.join(".pack.writing");
    let old = folder.join(PACK_OLD);
    let _ = fs::remove_dir_all(&tmp);
    let _ = fs::remove_dir_all(&old);
    fs::create_dir_all(&tmp)?;
    for file in files {
        let path = tmp.join(&file.name);
        fs::write(&path, &file.text)?;
        fs::File::open(&path)?.sync_all()?;
    }
    fs::write(
        tmp.join(PACK_INFO),
        serde_json::to_string_pretty(info).unwrap_or_default(),
    )?;
    sync_dir(&tmp);
    let dir = folder.join(PACK);
    if dir.exists() {
        fs::rename(&dir, &old)?;
    }
    fs::rename(&tmp, &dir)?;
    sync_dir(folder);
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

/// The shapes a reader reads, and where they come from.
struct Active {
    /// The generated layer.
    generated: Graph,
    /// The installed pack (#148), when the dataset has one.
    pack: Option<Graph>,
    /// The dataset's own set, with its version, when it has one.
    own: Option<Graph>,
    /// The layers together, projected for the `reader` and `profile` parameters.
    graph: Graph,
    /// `generated`, or `local; version=N`, after `pack NAME@VERSION; ` with a pack.
    source: String,
    generation: Option<u64>,
}

/// The shapes `reader` reads of `entry`, with every profile: for the readers in other modules.
/// With a `view`, the generated layer comes from those graphs only (#203), so a caller who may
/// read only them learns of no class or property that is only in other graphs.
pub(crate) fn projected(
    state: &State,
    entry: &Entry,
    reader: Reader,
    view: &View,
) -> Result<Graph, HttpError> {
    let params = Params(vec![("reader".to_owned(), reader.name().to_owned())]);
    Ok(active(state, entry, &params, view, None)?.graph)
}

/// As `projected`, with `files` in place of the dataset's own shape set: what `reader` would
/// read once they are promoted, for a preview (#146). Their errors are a 422.
pub(crate) fn projected_with(
    state: &State,
    entry: &Entry,
    reader: Reader,
    view: &View,
    files: &[SetFile],
) -> Result<Graph, HttpError> {
    let params = Params(vec![("reader".to_owned(), reader.name().to_owned())]);
    Ok(active(state, entry, &params, view, Some(files))?.graph)
}

/// The shapes as `params` ask for them; `preview` stands in for the dataset's own set.
fn active(
    state: &State,
    entry: &Entry,
    params: &Params,
    view: &View,
    preview: Option<&[SetFile]>,
) -> Result<Active, HttpError> {
    let reader = reader(params)?;
    let profiles = profiles(params)?;
    let profile = crate::profile::compute(state, entry, params, view)?;
    let generated = shapes::generate(&profile);
    let folder = folder(state, entry).ok();
    let own = match (preview, &folder) {
        (Some(files), _) => Some((0, files.to_vec())),
        (None, Some(folder)) => own_files(folder)?,
        (None, None) => None,
    };
    let (own, source) = match own {
        Some((_, files)) if preview.is_some() => {
            (Some(own_set(0, &files, None)?), "preview".to_owned())
        }
        Some((version, files)) => {
            let from = folder.as_deref().and_then(|f| version_source(f, version));
            let commit = from
                .as_ref()
                .and_then(|s| s.get("commit")?.as_str())
                .map(str::to_owned);
            (
                Some(own_set(version, &files, from.as_ref())?),
                match commit {
                    Some(commit) => format!("git; version={version}; commit={commit}"),
                    None => format!("local; version={version}"),
                },
            )
        }
        None => (None, "generated".to_owned()),
    };
    // generated < pack < own (#148): the dataset's own set overrides the pack's shapes.
    let pack = match folder.as_deref().map(pack_files).transpose()?.flatten() {
        Some((info, files)) => Some((pack_label(&info), pack_set(&info, &files)?)),
        None => None,
    };
    let mut all = generated.clone();
    if let Some((_, pack)) = &pack {
        all = shapes::overlay(&all, pack);
    }
    if let Some(own) = &own {
        all = shapes::overlay(&all, own);
    }
    // A higher set may keep the generated shapes it does not take over from some readers.
    let uppers: Vec<&Graph> = pack.iter().map(|(_, g)| g).chain(own.as_ref()).collect();
    let all = shapes::hide_generated(&all, &generated, &uppers);
    // A stored validation report as every class's data-quality facet (#92).
    let all = shapes::add_quality(&all, &uppers);
    let source = match (&pack, own.is_some()) {
        (Some((label, _)), true) => format!("pack {label}; {source}"),
        (Some((label, _)), false) => format!("pack {label}"),
        (None, _) => source,
    };
    Ok(Active {
        graph: shapes::project(&all, reader, &profiles),
        generated,
        pack: pack.map(|(_, graph)| graph),
        own,
        source,
        generation: profile.generation,
    })
}

/// `…/shapes/sparql`: a SPARQL query over the shapes. The default graph holds what the reader
/// reads (as `GET …/shapes` answers it); the named graphs `urn:tripletrail:shapes:generated`,
/// `urn:tripletrail:shapes:pack` and `urn:tripletrail:shapes:local` hold each layer as it is, and `http://w3id.org/tripletrail/shapes`
/// the vocabulary, for labels.
fn sparql(
    state: &State,
    entry: &Entry,
    request: &mut tiny_http::Request,
    params: &Params,
) -> Result<Reply, HttpError> {
    let query = match params.get("query") {
        Some(q) => q.to_owned(),
        None if request.method() == &tiny_http::Method::Post
            && crate::content_type(request).as_deref() == Some("application/sparql-query") =>
        {
            crate::read_body(request, state.options.max_body)?
        }
        None => {
            return Err(HttpError::bad_request(
                "No query: query= or a SPARQL query body",
            ));
        }
    };
    let active = active(state, entry, params, &View::default(), None)?;
    let mut quads = Vec::new();
    {
        use tripletrail::oxigraph::io::RdfSerializer;
        let mut writer = RdfSerializer::from_format(RdfFormat::NQuads).for_writer(&mut quads);
        let graph = |name: &str| {
            tripletrail::oxigraph::model::GraphName::NamedNode(NamedNode::new_unchecked(name))
        };
        let layers = [
            (
                tripletrail::oxigraph::model::GraphName::DefaultGraph,
                Some(&active.graph),
            ),
            (graph(shapes::GENERATED_SET), Some(&active.generated)),
            (graph(PACK_SET), active.pack.as_ref()),
            (graph(LOCAL_SET), active.own.as_ref()),
        ];
        for (name, layer) in layers {
            for t in layer.into_iter().flat_map(Graph::iter) {
                writer.serialize_quad(t.in_graph(name.as_ref()))?;
            }
        }
        writer.finish()?;
    }
    let engine = tripletrail::Engine::new_tripletrail();
    engine.load(RdfFormat::NQuads, &quads[..])?;
    let vocabulary = tripletrail::oxigraph::io::RdfParser::from_format(RdfFormat::Turtle)
        .with_default_graph(NamedNode::new_unchecked(
            "http://w3id.org/tripletrail/shapes",
        ));
    engine.load(vocabulary, shapes::VOCABULARY.as_bytes())?;
    let limits = crate::limits::Limits::of(state, &entry.name);
    let timeout = limits.timeout(
        params
            .get("timeout")
            .map(crate::parse_timeout)
            .transpose()?,
    );
    let budget = crate::RowBudget::of(params, limits.max_rows)?;
    let mut reply = crate::run_query(
        &engine,
        &query,
        request,
        params,
        timeout,
        budget,
        tripletrail::QueryOptions::default(),
    )?;
    reply.headers.push(("TripleTrail-Shapes", active.source));
    Ok(reply)
}

/// `GET /$/datasets/{name}/shapes`.
pub(crate) fn reply(
    state: &State,
    entry: &Entry,
    request: &tiny_http::Request,
    params: &Params,
) -> Result<Reply, HttpError> {
    let format = crate::rdf_format(request, params, RdfFormat::Turtle, false)?;
    let Active {
        graph,
        source,
        generation,
        ..
    } = active(state, entry, params, &View::default(), None)?;
    let body = if format == RdfFormat::Turtle {
        shapes::to_turtle(&graph).into_bytes()
    } else {
        let mut writer = RdfSerializer::from_format(format).for_writer(Vec::new());
        for triple in graph.iter() {
            writer.serialize_triple(triple)?;
        }
        writer.finish()?
    };
    let mut reply = Reply::bytes(200, format.media_type(), body);
    reply.headers.push(("TripleTrail-Shapes", source));
    if let Some(generation) = generation {
        reply
            .headers
            .push(("TripleTrail-Data-Generation", generation.to_string()));
    }
    Ok(reply)
}

/// The routes under `/$/datasets/{name}/shapes/`: the dataset's own set.
pub(crate) fn route(
    state: &State,
    entry: &Entry,
    request: &mut tiny_http::Request,
    rest: &[&str],
    params: &Params,
    server_admin: bool,
    by: Option<&str>,
) -> Result<Reply, HttpError> {
    use tiny_http::Method;
    let method = request.method().clone();
    // The editor's forms: the same for every dataset.
    if matches!(method, Method::Get | Method::Head) && rest == ["form"] {
        return Ok(crate::json_reply(200, &edit::form()?));
    }
    // The editor's outline reads the layers, so it works without a folder too.
    if matches!(method, Method::Get | Method::Head) && rest == ["outline"] {
        let folder = folder(state, entry).ok();
        return Ok(crate::json_reply(
            200,
            &edit::outline(state, entry, folder.as_deref(), params)?,
        ));
    }
    let folder = match folder(state, entry) {
        Ok(folder) => folder,
        // A check keeps nothing, so it works on an in-memory dataset without `--location`.
        Err(_) if method == Method::Post && rest == ["validate"] => {
            return validate(state, None, request, params);
        }
        Err(e) => return Err(e),
    };
    let dry = rest == ["edit"] && dry_run(params)?;
    if entry.mode == crate::admin::Mode::ReadOnly
        && !matches!(method, Method::Get | Method::Head)
        && rest != ["validate"]
        && rest != ["sparql"]
        // A dry edit writes nothing.
        && !dry
    {
        return Err(HttpError(
            409,
            format!(
                "Dataset {} is attached read-only: its shapes are read, not changed",
                entry.name
            ),
        ));
    }
    let ok = |value: Value| Ok(crate::json_reply(200, &value));
    if let Some(binding) = Binding::read(&folder)
        && matches!(
            (&method, rest),
            (Method::Put | Method::Delete, ["files", _])
                | (Method::Put, ["files"])
                | (Method::Post, ["customise" | "rollback" | "edit"])
        )
        // A dry edit writes nothing: it exports the changed files of a bound set.
        && !dry
    {
        return Err(HttpError(
            409,
            format!(
                "The shapes of {} come from {}: change them there, or unbind with DELETE …/shapes/git",
                entry.name, binding.url
            ),
        ));
    }
    match (method, rest) {
        // Reviewed changes (#145, TripleTrail Enterprise): pull requests on the bound repository.
        (Method::Get | Method::Head, ["proposals"]) => {
            // With refresh=true, the open proposals' pull requests are asked of the forge.
            let refresh = match params.get("refresh") {
                Some("" | "true" | "1") => true,
                None | Some("false" | "0") => false,
                Some(other) => {
                    return Err(HttpError::bad_request(format!(
                        "refresh is true or false, not {other:?}"
                    )));
                }
            };
            proposals(state, &folder, refresh)
        }
        (Method::Post, ["proposals"]) => {
            let body = crate::read_body(request, state.options.max_body)?;
            let value: Value = serde_json::from_str(&body)
                .map_err(|e| HttpError::bad_request(format!("Invalid JSON: {e}")))?;
            // The full data is validated with the proposed shapes: in the background for a
            // proposal (unless `validate=false`), at once for a dry run with `validate=true`.
            let dry = dry_run(params)?;
            let validate = match params.get("validate") {
                Some("" | "true" | "1") => true,
                Some("false" | "0") => false,
                None => !dry,
                Some(other) => {
                    return Err(HttpError::bad_request(format!(
                        "validate is true or false, not {other:?}"
                    )));
                }
            };
            // A dry run validates while the request waits: within the request's time limit.
            let asked = params
                .get("timeout")
                .map(crate::parse_timeout)
                .transpose()?;
            let timeout = crate::limits::Limits::of(state, &entry.name).timeout(asked);
            propose(state, entry, &folder, &value, by, dry, validate, timeout)
        }
        (Method::Get | Method::Head, ["git"]) => {
            let binding = Binding::read(&folder);
            ok(json!({
                "dataset": entry.name,
                "bound": binding.is_some(),
                "binding": binding.map(|b| b.to_json(true)),
                "last": git_status(&folder),
            }))
        }
        (Method::Put, ["git"]) => {
            let body = crate::read_body(request, state.options.max_body)?;
            let value: Value = serde_json::from_str(&body)
                .map_err(|e| HttpError::bad_request(format!("Invalid JSON: {e}")))?;
            let binding = Binding::from_json(&value)?;
            bind(state, &folder, binding).and_then(ok)
        }
        (Method::Delete, ["git"]) => {
            let _write = WRITES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if Binding::read(&folder).is_none() {
                return Err(HttpError(
                    404,
                    format!("Dataset {} is not bound to a repository", entry.name),
                ));
            }
            fs::remove_file(folder.join(BINDING))?;
            ok(json!({ "bound": false, "version": current(&folder) }))
        }
        (Method::Post, ["pull"]) => {
            let binding = Binding::read(&folder).ok_or_else(|| {
                HttpError(
                    409,
                    format!("Dataset {} is not bound to a repository", entry.name),
                )
            })?;
            pull(state, request, &folder, &binding).and_then(ok)
        }
        (Method::Get | Method::Head, ["pack"]) => {
            let pack = pack_files(&folder)?;
            ok(json!({
                "dataset": entry.name,
                "installed": pack.is_some(),
                "name": pack.as_ref().and_then(|(i, _)| i.get("name").cloned()),
                "version": pack.as_ref().and_then(|(i, _)| i.get("version").cloned()),
                "source": pack.as_ref().and_then(|(i, _)| i.get("source").cloned()),
                "files": pack.map(|(_, files)| files.iter().map(|f| json!({
                    "name": f.name,
                    "bytes": f.text.len(),
                })).collect::<Vec<_>>()).unwrap_or_default(),
            }))
        }
        (Method::Put, ["pack"]) => {
            // `{"name", "version", "files": {name: text}, "source": {...}}`: the whole pack,
            // in place of the one installed. The dataset's own set stays on top of it.
            // Or `{"git": {"url", "tag", …}, "name"?, "version"?}`: the pack is read from a
            // tag (or ref) of a repository.
            let body = crate::read_body(request, state.options.max_body)?;
            let value: Value = serde_json::from_str(&body)
                .map_err(|e| HttpError::bad_request(format!("Invalid JSON: {e}")))?;
            let (files, source, name, version) = match value.get("git") {
                Some(git) => {
                    if !server_admin {
                        return Err(HttpError(
                            403,
                            "A pack from git names files and URLs of the server: it needs the \
                             group tripletrail:admin (a dataset's admins install a pack's files)"
                                .to_owned(),
                        ));
                    }
                    if value.get("files").is_some() {
                        return Err(HttpError::bad_request(
                            "Give the pack's files or its git repository, not both",
                        ));
                    }
                    let (files, source, name, version) = pack_from_git(state, &folder, git)?;
                    let name = match value.get("name") {
                        Some(given) => pack_name("name", Some(given))?.to_owned(),
                        None => pack_name(
                            "name (the repository's name is not one)",
                            Some(&json!(name)),
                        )?
                        .to_owned(),
                    };
                    let version = match value.get("version") {
                        Some(given) => pack_name("version", Some(given))?.to_owned(),
                        None => pack_name("version (the tag is not one)", Some(&json!(version)))?
                            .to_owned(),
                    };
                    (files, Some(source), name, version)
                }
                None => {
                    let name = pack_name("name", value.get("name"))?.to_owned();
                    let version = pack_name("version", value.get("version"))?.to_owned();
                    let given = value
                        .get("files")
                        .and_then(Value::as_object)
                        .ok_or_else(|| {
                            HttpError::bad_request("files is an object of name: text, or give git")
                        })?;
                    let mut files = Vec::new();
                    for (file, text) in given {
                        let file = file_name(file)?.to_owned();
                        let text = text
                            .as_str()
                            .ok_or_else(|| {
                                HttpError::bad_request(format!("{file}: the text is a string"))
                            })?
                            .to_owned();
                        files.push(SetFile { name: file, text });
                    }
                    if files.is_empty() {
                        return Err(HttpError::bad_request("files is empty: a pack has a file"));
                    }
                    files.sort_by(|a, b| a.name.cmp(&b.name));
                    let source = value.get("source").filter(|v| v.is_object()).cloned();
                    (files, source, name, version)
                }
            };
            let mut info = json!({ "name": name, "version": version });
            if let Some(source) = source {
                info["source"] = source;
            }
            // A check before the upgrade (#148): what it would do, keeping nothing.
            if dry_run(params)? {
                let installed = pack_files(&folder)?;
                let own = own_files(&folder)?;
                let has = has_instances(state, entry);
                let mut report = pack_check(
                    installed.as_ref().map(|(_, f)| f.as_slice()),
                    &files,
                    own.as_ref().map(|(_, f)| f.as_slice()),
                    Some(&has),
                );
                report["dry"] = json!(true);
                report["name"] = json!(name);
                report["version"] = json!(version);
                report["replaces"] = json!(installed.map(|(i, _)| pack_label(&i)));
                return ok(report);
            }
            let _write = WRITES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            shapes::load(&files).map_err(|e| invalid(&e))?;
            let replaced = pack_files(&folder)?.map(|(i, _)| pack_label(&i));
            write_pack(&folder, &info, &files)?;
            ok(json!({
                "name": name,
                "version": version,
                "files": files.len(),
                "replaced": replaced,
            }))
        }
        (Method::Delete, ["pack"]) => {
            let _write = WRITES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            restore_pack(&folder);
            let Some((info, _)) = pack_files(&folder)? else {
                return Err(HttpError(
                    404,
                    format!("Dataset {} has no pack installed", entry.name),
                ));
            };
            fs::remove_dir_all(folder.join(PACK))?;
            sync_dir(&folder);
            // The objects fetched for it, if it came from git.
            let _ = fs::remove_dir_all(folder.join(".pack-git"));
            ok(json!({ "removed": pack_label(&info) }))
        }
        (Method::Get | Method::Head | Method::Post, ["sparql"]) => {
            sparql(state, entry, request, params)
        }
        (Method::Get | Method::Head, ["files"]) => {
            let own = own_files(&folder)?;
            ok(json!({
                "dataset": entry.name,
                "version": own.as_ref().map(|(v, _)| v),
                "files": own.map(|(_, files)| files.iter().map(|f| json!({
                    "name": f.name,
                    "bytes": f.text.len(),
                })).collect::<Vec<_>>()).unwrap_or_default(),
                "versions": versions(&folder),
            }))
        }
        (Method::Get | Method::Head, ["files", name]) => {
            let name = file_name(name)?;
            let (_, files) = own_files(&folder)?.ok_or_else(|| {
                HttpError(
                    404,
                    format!("Dataset {} has no shapes of its own", entry.name),
                )
            })?;
            let file = files
                .into_iter()
                .find(|f| f.name == name)
                .ok_or_else(|| HttpError(404, format!("No shape file {name}")))?;
            Ok(Reply::bytes(200, "text/turtle", file.text.into_bytes()))
        }
        (Method::Put, ["files"]) => {
            // The whole set at once, as the next version: `{"files": {name: text}, "source":
            // {...}}` (a workbench's shapes promoted, #400). The source is kept with it.
            let body = crate::read_body(request, state.options.max_body)?;
            let value: Value = serde_json::from_str(&body)
                .map_err(|e| HttpError::bad_request(format!("Invalid JSON: {e}")))?;
            let given = value
                .get("files")
                .and_then(Value::as_object)
                .ok_or_else(|| HttpError::bad_request("files is an object of name: text"))?;
            let mut files = Vec::new();
            for (name, text) in given {
                let name = file_name(name)?.to_owned();
                let text = text
                    .as_str()
                    .ok_or_else(|| HttpError::bad_request(format!("{name}: the text is a string")))?
                    .to_owned();
                files.push(SetFile { name, text });
            }
            if files.is_empty() {
                return Err(HttpError::bad_request("files is empty: a set has a file"));
            }
            files.sort_by(|a, b| a.name.cmp(&b.name));
            let source = value.get("source").filter(|v| v.is_object());
            let _write = WRITES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            shapes::load(&files).map_err(|e| invalid(&e))?;
            let version = write_version(&folder, &files, source)?;
            ok(json!({ "version": version, "files": files.len() }))
        }
        (Method::Put | Method::Delete, ["files", name]) => {
            let name = file_name(name)?.to_owned();
            let text = if request.method() == &Method::Put {
                Some(crate::read_body(request, state.options.max_body)?)
            } else {
                None
            };
            let _write = WRITES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut files = own_files(&folder)?.map(|(_, f)| f).unwrap_or_default();
            let had = files.iter().any(|f| f.name == name);
            files.retain(|f| f.name != name);
            match text {
                Some(text) => files.push(SetFile { name, text }),
                None if !had => return Err(HttpError(404, format!("No shape file {name}"))),
                None => {}
            }
            files.sort_by(|a, b| a.name.cmp(&b.name));
            shapes::load(&files).map_err(|e| invalid(&e))?;
            let version = write_version(&folder, &files, None)?;
            ok(json!({ "version": version }))
        }
        (Method::Post, ["customise"]) => {
            let force = matches!(params.get("force"), Some("" | "true" | "1"));
            let _write = WRITES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if current(&folder).is_some() && !force {
                return Err(HttpError(
                    409,
                    format!(
                        "Dataset {} has shapes of its own already: force=true replaces them (the old version is kept)",
                        entry.name
                    ),
                ));
            }
            let profile = crate::profile::compute(state, entry, params, &View::default())?;
            // The start is what is read without a set of its own: the generated shapes with
            // the pack's on top (#148).
            let mut start = shapes::generate(&profile);
            if let Some((info, files)) = pack_files(&folder)? {
                start = shapes::overlay(&start, &pack_set(&info, &files)?);
            }
            let files = shapes::files_of(&customisable(&start));
            shapes::load(&files).map_err(|e| invalid(&e))?;
            let version = write_version(&folder, &files, None)?;
            ok(json!({ "version": version, "files": files.len() }))
        }
        (Method::Post, ["validate"]) => validate(state, Some(&folder), request, params),
        (Method::Post, ["edit"]) => {
            let body = crate::read_body(request, state.options.max_body)?;
            let value: Value = serde_json::from_str(&body)
                .map_err(|e| HttpError::bad_request(format!("Invalid JSON: {e}")))?;
            ok(edit::edit(state, entry, &folder, params, &value)?)
        }
        (Method::Post, ["rollback"]) => {
            let to: u64 = params
                .get("version")
                .and_then(|v| v.trim().parse().ok())
                .ok_or_else(|| HttpError::bad_request("rollback needs version=N"))?;
            let _write = WRITES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !versions(&folder).contains(&to) {
                return Err(HttpError(
                    404,
                    format!("No kept version {to} of the shapes"),
                ));
            }
            let files = read_version(&folder, to)?;
            let version = write_version(&folder, &files, version_source(&folder, to).as_ref())?;
            ok(json!({ "version": version, "from": to }))
        }
        _ => Err(HttpError(404, "Unknown shapes request".to_owned())),
    }
}

/// Checks the set as it is, or with the file in the body put in (`file=`), keeping nothing.
fn validate(
    state: &State,
    folder: Option<&Path>,
    request: &mut tiny_http::Request,
    params: &Params,
) -> Result<Reply, HttpError> {
    let mut files = match folder {
        Some(folder) => own_files(folder)?.map(|(_, f)| f).unwrap_or_default(),
        None => Vec::new(),
    };
    if let Some(name) = params.get("file") {
        let name = file_name(name)?.to_owned();
        let text = crate::read_body(request, state.options.max_body)?;
        files.retain(|f| f.name != name);
        files.push(SetFile { name, text });
    }
    let errors = shapes::load(&files).err().unwrap_or_default();
    Ok(crate::json_reply(
        200,
        &json!({ "valid": errors.is_empty(), "errors": errors }),
    ))
}

#[cfg(feature = "git")]
fn bind(state: &State, folder: &Path, binding: Binding) -> Result<Value, HttpError> {
    crate::shapes_git::bind(folder, binding, state.options.git_fetch)
}

#[cfg(feature = "git")]
fn pull(
    state: &State,
    request: &mut tiny_http::Request,
    folder: &Path,
    binding: &Binding,
) -> Result<Value, HttpError> {
    if crate::shapes_git::is_webhook(request) {
        crate::shapes_git::verify_webhook(state, request, binding)?;
    }
    crate::shapes_git::pull(folder, binding, state.options.git_fetch)
}

#[cfg(feature = "git")]
fn pack_from_git(
    state: &State,
    folder: &Path,
    git: &Value,
) -> Result<(Vec<SetFile>, Value, String, String), HttpError> {
    crate::shapes_git::fetch_pack(folder, git, state.options.git_fetch)
}

#[cfg(not(feature = "git"))]
fn pack_from_git(
    _state: &State,
    _folder: &Path,
    _git: &Value,
) -> Result<(Vec<SetFile>, Value, String, String), HttpError> {
    Err(without_git())
}

#[cfg(all(feature = "enterprise", feature = "git"))]
fn proposals(state: &State, folder: &Path, refresh: bool) -> Result<Reply, HttpError> {
    crate::shapes_review::list(state, folder, refresh).map(|v| crate::json_reply(200, &v))
}

#[cfg(all(feature = "enterprise", feature = "git"))]
#[allow(clippy::too_many_arguments)]
fn propose(
    state: &State,
    entry: &Entry,
    folder: &Path,
    body: &Value,
    by: Option<&str>,
    dry: bool,
    validate: bool,
    timeout: Option<std::time::Duration>,
) -> Result<Reply, HttpError> {
    crate::shapes_review::propose(state, entry, folder, body, by, dry, validate, timeout)
        .map(|v| crate::json_reply(if dry { 200 } else { 201 }, &v))
}

#[cfg(not(all(feature = "enterprise", feature = "git")))]
fn proposals(_state: &State, _folder: &Path, _refresh: bool) -> Result<Reply, HttpError> {
    Err(without_review())
}

#[cfg(not(all(feature = "enterprise", feature = "git")))]
#[allow(clippy::too_many_arguments)]
fn propose(
    _state: &State,
    _entry: &Entry,
    _folder: &Path,
    _body: &Value,
    _by: Option<&str>,
    _dry: bool,
    _validate: bool,
    _timeout: Option<std::time::Duration>,
) -> Result<Reply, HttpError> {
    Err(without_review())
}

#[cfg(not(all(feature = "enterprise", feature = "git")))]
fn without_review() -> HttpError {
    HttpError(
        501,
        "Reviewed shape changes are part of TripleTrail Enterprise (with git)".to_owned(),
    )
}

#[cfg(feature = "git")]
fn git_status(folder: &Path) -> Option<Value> {
    crate::shapes_git::status(folder)
}

#[cfg(not(feature = "git"))]
fn without_git() -> HttpError {
    HttpError(
        501,
        "This build has no git: build with the feature git".to_owned(),
    )
}

#[cfg(not(feature = "git"))]
fn bind(_state: &State, _folder: &Path, _binding: Binding) -> Result<Value, HttpError> {
    Err(without_git())
}

#[cfg(not(feature = "git"))]
fn pull(
    _state: &State,
    _request: &mut tiny_http::Request,
    _folder: &Path,
    _binding: &Binding,
) -> Result<Value, HttpError> {
    Err(without_git())
}

#[cfg(not(feature = "git"))]
fn git_status(_folder: &Path) -> Option<Value> {
    None
}
