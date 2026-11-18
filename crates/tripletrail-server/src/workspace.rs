//! `/$/workspaces` (#400): the workbench's sandboxes. A workspace is a folder of files (data,
//! shapes, rules, queries, updates, lift mappings, `pipeline.yaml`, tests) and a scratch store
//! built from them, in memory, that `/$/workspaces/{id}/sparql` queries:
//!
//! - the files of `data/` go to the default graph (quad formats keep their graphs),
//! - those of `shapes/` to [`SHAPES_GRAPH`], those of `rules/` to [`RULES_GRAPH`];
//! - `POST …/sample` copies a sample of a dataset (instances of some classes and what they
//!   lead to, a few hops out) to a file of `data/`.
//!
//! A workspace never writes to a dataset: it only reads one when sampling it. A scratch
//! workspace is deleted once unused for [`WorkspaceOptions::ttl`]; a saved one is kept.
//! Workspaces are kept on disk (`<dir>/<id>/workspace.json` and `<dir>/<id>/files/`), so they
//! outlive a restart; the scratch store is rebuilt from the files when next used.

use crate::admin::rfc3339;
use crate::{Context, HttpError, Params, Reply, State, json_reply};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tiny_http::{Method, Request};
use tripletrail::Engine;
use tripletrail::geo::sync::{LockUnpoisoned, RwLockUnpoisoned};
use tripletrail::oxigraph::io::{RdfFormat, RdfParser, RdfSerializer};
use tripletrail::oxigraph::model::vocab::rdf;
use tripletrail::oxigraph::model::{GraphName, NamedNode, NamedOrBlankNode, Quad, Term, Triple};

/// The graph of the scratch store that holds the files of `shapes/`.
pub const SHAPES_GRAPH: &str = "urn:tripletrail:workspace:shapes";
/// The graph of the scratch store that holds the files of `rules/`.
pub const RULES_GRAPH: &str = "urn:tripletrail:workspace:rules";

/// The folders a workspace's files go in, and the files allowed at its top.
const FOLDERS: [&str; 7] = [
    "data", "shapes", "rules", "queries", "updates", "lift", "tests",
];
const TOP_FILES: [&str; 2] = ["pipeline.yaml", "README.md"];
/// The folders whose files are RDF and make up the scratch store.
const RDF_FOLDERS: [&str; 3] = ["data", "shapes", "rules"];
const META: &str = "workspace.json";
/// A built scratch store unused this long is dropped (and rebuilt from the files when next
/// used), so idle saved workspaces take no memory.
const IDLE_ENGINE: Duration = Duration::from_secs(15 * 60);

/// The workbench's workspaces: where they are kept, how long they live, and their caps.
#[derive(Clone, Debug)]
pub struct WorkspaceOptions {
    /// Serve `/$/workspaces`.
    pub enabled: bool,
    /// Where workspaces are kept. `None`: `<location>/.workspaces` with
    /// [`crate::Options::location`], else a folder in the system's temporary directory.
    pub dir: Option<PathBuf>,
    /// A scratch workspace unused this long is deleted.
    pub ttl: Duration,
    /// Most workspaces on the server.
    pub max_workspaces: usize,
    /// Most workspaces one caller (one agent; anonymous callers count as one) may have.
    pub max_per_owner: usize,
    /// Most triples in one workspace's files (a sample included).
    pub max_triples: usize,
    /// Largest file, in bytes, also once unpacked (`.gz`).
    pub max_file: usize,
    /// Most bytes of files in one workspace.
    pub max_bytes: u64,
    /// Most files in one workspace.
    pub max_files: usize,
    /// Most quads an overlay may change (added and removed) on top of its dataset (#406).
    pub max_overlay_changes: usize,
}

impl Default for WorkspaceOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: None,
            ttl: Duration::from_secs(24 * 3600),
            max_workspaces: 100,
            max_per_owner: 10,
            max_triples: 1_000_000,
            max_file: 16 << 20,
            max_bytes: 256 << 20,
            max_files: 1000,
            max_overlay_changes: 10_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lifetime {
    /// Deleted once unused for the server's TTL.
    Scratch,
    /// Kept until deleted.
    Saved,
}

impl Lifetime {
    fn parse(s: &str) -> Result<Self, HttpError> {
        match s {
            "scratch" => Ok(Self::Scratch),
            "saved" => Ok(Self::Saved),
            _ => Err(HttpError::bad_request(format!(
                "Unknown lifetime {s}: scratch or saved"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Scratch => "scratch",
            Self::Saved => "saved",
        }
    }
}

struct Meta {
    name: String,
    /// The agent that made it; `None` for anonymous callers, or without authentication.
    owner: Option<String>,
    lifetime: Lifetime,
    created: SystemTime,
    used: SystemTime,
    /// The files a sample of a dataset wrote, until replaced: never put in a share link.
    sampled: std::collections::BTreeSet<String>,
    /// The repository it was last pulled from: `url`, `ref`, `path`, `commit`, `at`.
    git: Option<Value>,
    /// The dataset it is an overlay of (#406), and when that was taken: `dataset`, `at`.
    overlay: Option<Value>,
}

impl Meta {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "owner": self.owner,
            "lifetime": self.lifetime.as_str(),
            "created": secs(self.created),
            "used": secs(self.used),
            "sampled": self.sampled,
            "git": self.git,
            "overlay": self.overlay,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            name: value.get("name")?.as_str()?.to_owned(),
            owner: value.get("owner")?.as_str().map(str::to_owned),
            lifetime: Lifetime::parse(value.get("lifetime")?.as_str()?).ok()?,
            created: UNIX_EPOCH + Duration::from_secs(value.get("created")?.as_u64()?),
            used: UNIX_EPOCH + Duration::from_secs(value.get("used")?.as_u64()?),
            sampled: value
                .get("sampled")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            git: value.get("git").filter(|g| g.is_object()).cloned(),
            overlay: value.get("overlay").filter(|o| o.is_object()).cloned(),
        })
    }
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

struct Inner {
    meta: Meta,
    /// The triples of each RDF file, once counted (kept up to date by writes).
    counts: Option<BTreeMap<String, usize>>,
    /// The scratch store, once built (dropped when one of its files changes).
    engine: Option<Engine>,
    /// `meta.used` as last saved: a read saves it again only now and then.
    saved_used: SystemTime,
    /// An overlay's base (#406): the dataset's quads as they were when it was taken, never
    /// written. Kept in memory only: lost on a restart.
    base: Option<tripletrail::store::Store>,
}

impl Inner {
    fn new(meta: Meta) -> Self {
        Self {
            saved_used: meta.used,
            meta,
            counts: None,
            engine: None,
            base: None,
        }
    }

    /// The overlay's base: `Ok(None)` for a workspace that is not an overlay, 409 when the
    /// base was lost (the server restarted since it was taken).
    fn base(&self) -> Result<Option<&tripletrail::store::Store>, HttpError> {
        match (&self.meta.overlay, &self.base) {
            (None, _) => Ok(None),
            (Some(_), Some(base)) => Ok(Some(base)),
            (Some(o), None) => Err(HttpError(
                409,
                format!(
                    "This workspace is an overlay of the dataset {}, taken before the server \
                     restarted: its changes are gone. Take the dataset again (POST …/overlay) or \
                     make it a plain workspace (DELETE …/overlay)",
                    o["dataset"].as_str().unwrap_or("?")
                ),
            )),
        }
    }

    /// The quads the overlay changed, at most (see [`tripletrail::store::Store::unmerged_len`]).
    fn overlay_changes(&self) -> Option<usize> {
        let base = self.base.as_ref()?;
        let changed = match self.engine.as_ref()?.store() {
            tripletrail::store::Dataset::TripleTrail(store) => store.unmerged_len(),
            _ => return None,
        };
        Some(changed.saturating_sub(base.unmerged_len()))
    }
}

/// How often a workspace that is only read saves when it was last used, so that a restart
/// does not take it for unused.
const SAVE_USED: Duration = Duration::from_secs(600);

struct Workspace {
    id: String,
    dir: PathBuf,
    /// The agent that made it (as in its [`Meta`]).
    owner: Option<String>,
    /// Largest file, unpacked (`.gz`) too.
    max_file: usize,
    inner: Mutex<Inner>,
    /// What the Pipeline tab's runs left: snapshots, reports and diffs (in memory only).
    pipeline: Mutex<crate::workspace_pipeline::State>,
    /// The pipeline as last described, answered while a run holds `pipeline`.
    pipeline_view: Mutex<Option<Value>>,
    /// A run holds `pipeline`: reads answer at once instead of waiting for it.
    pipeline_running: std::sync::atomic::AtomicBool,
    /// What other clients ran against it (in memory only).
    history: Mutex<crate::workspace_history::History>,
    /// Held while the workspace is pulled from git.
    #[cfg_attr(not(feature = "git"), allow(dead_code))]
    pulling: Mutex<()>,
}

impl Workspace {
    fn files_dir(&self) -> PathBuf {
        self.dir.join("files")
    }

    fn save_meta(&self, inner: &mut Inner) -> Result<(), HttpError> {
        let tmp = self.dir.join(".workspace.json.writing");
        fs::write(&tmp, inner.meta.to_json().to_string())?;
        fs::rename(&tmp, self.dir.join(META))?;
        inner.saved_used = inner.meta.used;
        Ok(())
    }

    /// Marks the workspace used now, saving that now and then.
    fn touch(&self, inner: &mut Inner) {
        inner.meta.used = SystemTime::now();
        if inner
            .meta
            .used
            .duration_since(inner.saved_used)
            .is_ok_and(|d| d > SAVE_USED)
        {
            let _ = self.save_meta(inner);
        }
    }

    /// The workspace's files (paths relative to its folder, `/`-separated) and their sizes.
    fn files(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        walk(&self.files_dir(), "", &mut out);
        out.sort();
        out
    }

    /// The scratch store, built from the files if it isn't.
    /// An overlay's is its base with the files on top (#406).
    fn engine(&self, inner: &mut Inner) -> Result<Engine, HttpError> {
        if inner.engine.is_none() {
            let engine = match inner.base()? {
                Some(base) => Engine::from_store(base.overlay()),
                None => Engine::new_tripletrail(),
            };
            let mut counts = BTreeMap::new();
            for (path, _) in self.files() {
                let Some(graph) = graph_of(&path) else {
                    continue;
                };
                let bytes = fs::read(self.files_dir().join(&path))?;
                let quads = parse(&path, &bytes, self.max_file)?;
                counts.insert(path, quads.len());
                engine.load_quads(quads.into_iter().map(|q| into_graph(q, &graph)))?;
            }
            inner.counts = Some(counts);
            inner.engine = Some(engine);
        }
        Ok(inner.engine.clone().expect("built above"))
    }

    /// How a pipeline run of the workspace is bounded: the store may hold four times the
    /// workspace's triples, and snapshots are kept of stores of up to its triples. An
    /// overlay's steps start from `start`, its base, and may hold the base's quads with as
    /// many changes as the overlay may make.
    fn pipeline_limits<'a>(
        &self,
        options: &WorkspaceOptions,
        timeout: Option<Duration>,
        room: &'a dyn Fn() -> u64,
        start: Option<Engine>,
    ) -> Result<crate::workspace_pipeline::Limits<'a>, HttpError> {
        let base = {
            let inner = self.inner.lock_unpoisoned();
            inner.base()?.map_or(0, |b| b.len())
        };
        let (max_quads, max_snapshot) = if base > 0 {
            (
                base.saturating_add(options.max_overlay_changes),
                base.saturating_add(options.max_triples),
            )
        } else {
            (options.max_triples.saturating_mul(4), options.max_triples)
        };
        Ok(crate::workspace_pipeline::Limits {
            timeout,
            max_quads,
            max_snapshot,
            room,
            start,
        })
    }

    /// The triples of each RDF file, counted if they are not.
    fn counts<'i>(
        &self,
        inner: &'i mut Inner,
    ) -> Result<&'i mut BTreeMap<String, usize>, HttpError> {
        if inner.counts.is_none() {
            let mut counts = BTreeMap::new();
            for (path, _) in self.files() {
                if graph_of(&path).is_some() {
                    let bytes = fs::read(self.files_dir().join(&path))?;
                    counts.insert(path.clone(), parse(&path, &bytes, self.max_file)?.len());
                }
            }
            inner.counts = Some(counts);
        }
        Ok(inner.counts.as_mut().expect("counted above"))
    }

    /// The triples of the workspace's RDF files, without `except`.
    fn triples(&self, inner: &mut Inner, except: &str) -> Result<usize, HttpError> {
        Ok(self
            .counts(inner)?
            .iter()
            .filter(|(p, _)| p.as_str() != except)
            .map(|(_, n)| n)
            .sum())
    }

    /// The bytes of the workspace's files, without `except`.
    fn bytes(&self, except: &str) -> u64 {
        self.files()
            .iter()
            .filter(|(p, _)| p != except)
            .map(|(_, n)| n)
            .sum()
    }

    fn to_json(&self, inner: &Inner, ttl: Duration) -> Value {
        let files: Vec<Value> = self
            .files()
            .into_iter()
            .map(|(path, bytes)| {
                let triples = inner.counts.as_ref().and_then(|c| c.get(&path).copied());
                let sampled = inner.meta.sampled.contains(&path);
                json!({ "path": path, "bytes": bytes, "triples": triples, "sampled": sampled })
            })
            .collect();
        let expires =
            (inner.meta.lifetime == Lifetime::Scratch).then(|| rfc3339(inner.meta.used + ttl));
        json!({
            "id": self.id,
            "name": inner.meta.name,
            "owner": inner.meta.owner,
            "lifetime": inner.meta.lifetime.as_str(),
            "created": rfc3339(inner.meta.created),
            "used": rfc3339(inner.meta.used),
            "expires": expires,
            "git": inner.meta.git,
            "overlay": inner.meta.overlay.as_ref().map(|o| {
                let mut o = o.clone();
                o["lost"] = (inner.base.is_none()).into();
                o["changes"] = inner.overlay_changes().into();
                o
            }),
            "files": files,
            "triples": inner.counts.as_ref().map(|c| c.values().sum::<usize>()),
            "graphs": {
                "data": "default graph",
                "shapes": SHAPES_GRAPH,
                "rules": RULES_GRAPH,
            },
        })
    }
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, u64)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = format!("{prefix}{name}");
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&entry.path(), &format!("{path}/"), out),
            Ok(t) if t.is_file() => {
                out.push((path, entry.metadata().map_or(0, |m| m.len())));
            }
            _ => {}
        }
    }
}

/// The graph a file's triples go to in the scratch store, if it is one of the store's files.
fn graph_of(path: &str) -> Option<GraphName> {
    match path.split('/').next()? {
        "data" => Some(GraphName::DefaultGraph),
        "shapes" => Some(NamedNode::new_unchecked(SHAPES_GRAPH).into()),
        "rules" => Some(NamedNode::new_unchecked(RULES_GRAPH).into()),
        _ => None,
    }
}

/// A file's quads; the default graph's go to `graph`.
fn into_graph(quad: Quad, graph: &GraphName) -> Quad {
    if quad.graph_name.is_default_graph() {
        Quad::new(quad.subject, quad.predicate, quad.object, graph.clone())
    } else {
        quad
    }
}

/// The RDF format of a file, by its extension (after a `.gz`).
fn format_of(path: &str) -> Option<(RdfFormat, bool)> {
    let (inner, gz) = match path.strip_suffix(".gz") {
        Some(p) => (p, true),
        None => (path, false),
    };
    let ext = Path::new(inner).extension()?.to_str()?;
    RdfFormat::from_extension(ext).map(|f| (f, gz))
}

/// Parses an RDF file, or says where it is wrong.
/// `limit`: the most bytes a `.gz` file may unpack to (`413` beyond).
fn parse(path: &str, bytes: &[u8], limit: usize) -> Result<Vec<Quad>, HttpError> {
    let (format, gz) = format_of(path).ok_or_else(|| {
        HttpError::bad_request(format!(
            "{path}: files of {} are RDF; name them .ttl, .nt, .nq, .trig, .rdf, .jsonld or .n3 \
             (optionally .gz)",
            RDF_FOLDERS.join(", ")
        ))
    })?;
    let mut unzipped = Vec::new();
    let bytes = if gz {
        flate2::read::MultiGzDecoder::new(bytes)
            .take(limit as u64 + 1)
            .read_to_end(&mut unzipped)
            .map_err(|e| HttpError::bad_request(format!("{path}: {e}")))?;
        if unzipped.len() > limit {
            return Err(HttpError(
                413,
                format!("{path}: unpacked, a file holds at most {limit} bytes"),
            ));
        }
        &unzipped[..]
    } else {
        bytes
    };
    RdfParser::from_format(format)
        .for_slice(bytes)
        .map(|q| q.map_err(|e| HttpError::bad_request(format!("{path}: {e}"))))
        .collect()
}

/// Checks a file path: `/`-separated, under one of [`FOLDERS`] or one of [`TOP_FILES`], with
/// plain names (letters, digits, `.`, `-`, `_`, not starting with `.`).
fn check_path(segments: &[&str]) -> Result<String, HttpError> {
    let path = segments.join("/");
    let plain = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.len() <= 128
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    let ok = match segments {
        [file] => TOP_FILES.contains(file),
        [folder, rest @ ..] => {
            FOLDERS.contains(folder) && rest.len() <= 8 && rest.iter().all(|s| plain(s))
        }
        [] => false,
    };
    if ok {
        Ok(path)
    } else {
        Err(HttpError::bad_request(format!(
            "No file {path}: files go in {}/ (plain names: letters, digits, '.', '-', '_'), or \
             are {}",
            FOLDERS.join("/, "),
            TOP_FILES.join(" or ")
        )))
    }
}

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".gz") {
        return "application/gzip";
    }
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("rq") => "application/sparql-query",
        Some("ru") => "application/sparql-update",
        Some("yaml" | "yml") => "application/yaml",
        Some("json") => "application/json",
        Some("md") => "text/markdown; charset=utf-8",
        Some("csv") => "text/csv; charset=utf-8",
        Some(ext) => match RdfFormat::from_extension(ext) {
            Some(f) => f.media_type(),
            None => "text/plain; charset=utf-8",
        },
        None => "text/plain; charset=utf-8",
    }
}

/// The server's workspaces.
pub(crate) struct Workspaces {
    dir: PathBuf,
    /// `dir` is a temporary folder of this server's, removed with it.
    temporary: bool,
    options: WorkspaceOptions,
    open: Mutex<HashMap<String, Arc<Workspace>>>,
    /// When the last sweep ran.
    swept: Mutex<Option<Instant>>,
    /// Whether the temporary folder could be made.
    made: std::sync::OnceLock<bool>,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A new workspace id: 32 hex digits no one can guess.
fn new_id() -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    hasher.update(now.as_nanos().to_le_bytes());
    hasher.update(NEXT.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(format!("{:?}", std::thread::current().id()).as_bytes());
    // Randomness from the OS, through the hasher's random keys.
    use std::hash::{BuildHasher, Hasher};
    let mut random = std::collections::hash_map::RandomState::new().build_hasher();
    random.write_u64(0);
    hasher.update(random.finish().to_le_bytes());
    hasher.finalize()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Workspaces {
    pub(crate) fn open(
        options: &crate::Options,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let ws = &options.workspaces;
        let (dir, temporary) = match (&ws.dir, &options.location) {
            (Some(d), _) => (d.clone(), false),
            (None, Some(location)) => (location.join(".workspaces"), false),
            // Made when the first workspace is (see `ready`).
            (None, None) if ws.enabled => (
                std::env::temp_dir().join(format!("tripletrail-workspaces-{}", new_id())),
                true,
            ),
            (None, None) => (PathBuf::new(), false),
        };
        let mut open = HashMap::new();
        if ws.enabled
            && let Ok(entries) = fs::read_dir(&dir)
        {
            for entry in entries.flatten() {
                let id = entry.file_name().to_string_lossy().into_owned();
                let meta = fs::read_to_string(entry.path().join(META))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    .and_then(|v| Meta::from_json(&v));
                if let Some(meta) = meta {
                    open.insert(
                        id.clone(),
                        Arc::new(Workspace {
                            id,
                            dir: entry.path(),
                            owner: meta.owner.clone(),
                            max_file: ws.max_file,
                            inner: Mutex::new(Inner::new(meta)),
                            pipeline: Mutex::default(),
                            pipeline_view: Mutex::default(),
                            pipeline_running: Default::default(),
                            history: Mutex::default(),
                            pulling: Mutex::default(),
                        }),
                    );
                }
            }
        }
        let workspaces = Self {
            dir,
            temporary,
            options: ws.clone(),
            open: Mutex::new(open),
            swept: Mutex::new(None),
            made: std::sync::OnceLock::new(),
        };
        workspaces.sweep();
        Ok(workspaces)
    }

    /// Deletes the scratch workspaces unused for the TTL, and drops idle scratch stores; at
    /// most once a second. A workspace busy (being built, or written) is left for next time,
    /// and the list of workspaces is not held meanwhile.
    fn sweep(&self) {
        {
            let mut swept = self.swept.lock_unpoisoned();
            if swept.is_some_and(|t: Instant| t.elapsed() < Duration::from_secs(1)) {
                return;
            }
            *swept = Some(Instant::now());
        }
        let now = SystemTime::now();
        let all: Vec<Arc<Workspace>> = self.open.lock_unpoisoned().values().cloned().collect();
        let mut expired = Vec::new();
        for ws in all {
            let Ok(mut inner) = ws.inner.try_lock() else {
                continue;
            };
            let idle = now.duration_since(inner.meta.used).unwrap_or_default();
            if inner.meta.lifetime == Lifetime::Scratch && idle > self.options.ttl {
                drop(inner);
                expired.push(ws);
            } else if idle > IDLE_ENGINE && inner.meta.overlay.is_none() {
                // An overlay's store holds its changes: kept while the workspace is.
                inner.engine = None;
            }
        }
        if expired.is_empty() {
            return;
        }
        let mut open = self.open.lock_unpoisoned();
        for ws in expired {
            open.remove(&ws.id);
            let _ = fs::remove_dir_all(&ws.dir);
        }
    }

    fn get(
        &self,
        id: &str,
        caller: Option<&crate::auth::Caller>,
    ) -> Result<Arc<Workspace>, HttpError> {
        let open = self.open.lock_unpoisoned();
        open.get(id)
            .filter(|ws| visible(&ws.owner, caller))
            .cloned()
            .ok_or_else(|| HttpError(404, format!("No workspace {id}")))
    }
}

impl Workspaces {
    /// Makes the folder of workspaces when it is a temporary one, private to this user, the
    /// first time a workspace is made.
    fn ready(&self) -> Result<(), HttpError> {
        if !self.temporary {
            return Ok(());
        }
        let made = self.made.get_or_init(|| {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            // Never a folder someone else made first.
            builder.create(&self.dir).is_ok()
        });
        if *made {
            Ok(())
        } else {
            Err(HttpError(
                500,
                format!("Cannot make the workspace folder {}", self.dir.display()),
            ))
        }
    }
}

impl Drop for Workspaces {
    /// A temporary folder (no `--workspace-dir` nor `--location`) goes with the server.
    fn drop(&mut self) {
        if self.temporary {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// Whether `caller` sees a workspace: its owner and admins do; without authentication,
/// everyone.
fn visible(owner: &Option<String>, caller: Option<&crate::auth::Caller>) -> bool {
    match caller {
        None => true,
        Some(c) => c.is_admin() || &c.agent == owner,
    }
}

/// Refuses a change unless the caller may make workspaces: with authentication, a caller with
/// an agent (or an anonymous one in a writer group); without, anyone when the server has no
/// update or admin token, else a request with one of them.
fn may_change(
    state: &State,
    request: &Request,
    caller: Option<&crate::auth::Caller>,
) -> Result<(), HttpError> {
    if let Some(caller) = caller {
        return if caller.agent.is_some() || caller.may_write() {
            Ok(())
        } else {
            Err(crate::auth::refuse(
                caller,
                "Anonymous callers cannot make or change workspaces",
            ))
        };
    }
    let tokens = [&state.options.update_token, &state.options.admin_token];
    if tokens.iter().all(|t| t.is_none())
        || tokens
            .iter()
            .filter_map(|t| t.as_deref())
            .any(|t| crate::admin::bearer_is(request, t))
    {
        Ok(())
    } else {
        Err(HttpError(
            401,
            "Changing workspaces needs the update or admin token: Authorization: Bearer <token>"
                .to_owned(),
        ))
    }
}

fn json_body(state: &State, request: &mut Request) -> Result<Value, HttpError> {
    let body = crate::read_body(request, state.options.max_body)?;
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&body).map_err(|e| HttpError::bad_request(format!("Invalid JSON: {e}")))
}

/// What is wrong with `text` as the workspace file `path`, without writing it (#409): RDF that
/// does not parse, shapes or rules (of `shapes/`, `rules/`, a test's `shapes.ttl` or
/// `rules.ttl`) that do not compile, a query or update that does not parse, YAML or JSON
/// results that do not. Other files are text and always pass.
fn check_file(path: &str, text: &str, limit: usize) -> Vec<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    let mut errors = Vec::new();
    match ext {
        "rq" => {
            if let Err(e) = Engine::check_query(text) {
                errors.push(format!("{path}: {e}"));
            }
        }
        "ru" => {
            if let Err(e) = Engine::check_update(text) {
                errors.push(format!("{path}: {e}"));
            }
        }
        "yaml" | "yml" => {
            if let Err(e) = yaml_rust2::YamlLoader::load_from_str(text) {
                errors.push(format!("{path}: {e}"));
            }
        }
        "srj" | "json" => {
            if let Err(e) = serde_json::from_str::<Value>(text) {
                errors.push(format!("{path}: {e}"));
            }
        }
        _ if format_of(path).is_some() => match parse(path, text.as_bytes(), limit) {
            Err(e) => errors.push(e.1),
            Ok(quads) => {
                let shapes = path.starts_with("shapes/")
                    || path.starts_with("rules/")
                    || (path.starts_with("tests/") && matches!(name, "shapes.ttl" | "rules.ttl"));
                if shapes {
                    let graph: tripletrail::oxigraph::model::Graph =
                        quads.into_iter().map(Triple::from).collect();
                    if let Err(e) = tripletrail::shacl::Shapes::compile(&graph) {
                        errors.push(format!("{path}: {e}"));
                    }
                }
            }
        },
        _ => {}
    }
    errors
}

/// Answers a request under `/$/workspaces` (`rest` is the path after it).
/// The most rows, triples or results a test gives on the server, when the server sets no
/// `maxRows`.
const TEST_MAX_ROWS: usize = 100_000;

pub(crate) fn route(
    state: &State,
    request: &mut Request,
    rest: &[&str],
    params: &mut Params,
    context: &mut Context,
) -> Result<Reply, HttpError> {
    // A run by a client other than the workbench goes to the workspace's history.
    let method = request.method().clone();
    let op = match rest {
        [_, s @ ("sparql" | "query" | "update" | "explain")] => Some((*s, None)),
        [_, s @ ("validate" | "infer")] => Some((*s, None)),
        [_, "tests"] if method == Method::Post => Some(("tests", None)),
        [_, "pipeline"] if method == Method::Post => Some(("pipeline", None)),
        [_, "pipeline", step, "complete"] if method == Method::Post => {
            Some(("complete", Some(*step)))
        }
        [_, "pipeline", step, s @ ("sparql" | "query" | "update")] => Some((*s, Some(*step))),
        _ => None,
    };
    let client = op.and(crate::workspace_history::client(request));
    let (Some((op, step)), Some(client)) = (op, client) else {
        return serve(state, request, rest, params, context);
    };
    context.keep_text = true;
    let started = Instant::now();
    let answer = serve(state, request, rest, params, context);
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    let Ok(ws) = state.workspaces.get(rest[0], context.caller.as_ref()) else {
        return answer;
    };
    // `sparql` is a query or an update: which, the text told.
    let op = match (op, context.operation) {
        ("sparql", Some(crate::admin::Operation::Update)) => "update",
        ("sparql", _) => "query",
        (op, _) => op,
    };
    let (answer, status, summary, result) = match answer {
        Ok(reply) if matches!(op, "validate" | "infer" | "tests") => {
            let json = reply
                .content_type
                .as_deref()
                .is_some_and(|t| t.starts_with("application/json"));
            let status = reply.status;
            let (reply, body) = crate::workspace_history::capture(reply);
            let parsed: Option<Value> = body
                .as_deref()
                .filter(|_| json)
                .and_then(|b| serde_json::from_slice(b).ok());
            let summary = crate::workspace_history::summary(op, parsed.as_ref(), body.as_deref());
            // Kept for the workbench to show: validations' and inferences' JSON answers.
            let result = body.filter(|_| json && op != "tests").map(Arc::new);
            (Ok(reply), status, summary, result)
        }
        Ok(reply) => {
            let status = reply.status;
            (Ok(reply), status, None, None)
        }
        Err(HttpError(status, message)) => {
            let summary: String = message.chars().take(300).collect();
            (Err(HttpError(status, message)), status, Some(summary), None)
        }
    };
    ws.history
        .lock_unpoisoned()
        .push(crate::workspace_history::Run {
            op: op.to_owned(),
            step: step.map(str::to_owned),
            client,
            text: context.text.clone(),
            params: crate::workspace_history::shown(&params.0),
            status,
            ms,
            summary,
            result,
        });
    answer
}

fn serve(
    state: &State,
    request: &mut Request,
    rest: &[&str],
    params: &mut Params,
    context: &mut Context,
) -> Result<Reply, HttpError> {
    let workspaces = &state.workspaces;
    if !workspaces.options.enabled {
        return Err(HttpError(
            404,
            "Workspaces are turned off on this server".to_owned(),
        ));
    }
    workspaces.sweep();
    let method = request.method().clone();
    let get = matches!(method, Method::Get | Method::Head);
    const SERVICES: [&str; 4] = ["sparql", "query", "update", "explain"];
    let protocol = matches!(rest, [_, s] if SERVICES.contains(s) || matches!(*s, "validate" | "infer" | "tests"))
        || matches!(rest, [_, "pipeline", _, "sparql" | "query"]);
    if !get && !protocol {
        may_change(state, request, context.caller.as_ref())?;
    }
    let caller = context.caller.clone();
    let caller = caller.as_ref();
    match rest {
        [] if get => {
            let mine: Vec<Arc<Workspace>> = workspaces
                .open
                .lock_unpoisoned()
                .values()
                .filter(|ws| visible(&ws.owner, caller))
                .cloned()
                .collect();
            let mut list: Vec<Value> = mine
                .iter()
                .map(|ws| ws.to_json(&ws.inner.lock_unpoisoned(), workspaces.options.ttl))
                .collect();
            list.sort_by(|a, b| a["created"].as_str().cmp(&b["created"].as_str()));
            Ok(json_reply(200, &json!({ "workspaces": list })))
        }
        [] if method == Method::Post => create(state, request, caller),
        [id] if get => {
            let ws = workspaces.get(id, caller)?;
            let inner = ws.inner.lock_unpoisoned();
            Ok(json_reply(200, &ws.to_json(&inner, workspaces.options.ttl)))
        }
        [id] if method == Method::Post || method == Method::Patch => {
            let ws = workspaces.get(id, caller)?;
            let body = json_body(state, request)?;
            let mut inner = ws.inner.lock_unpoisoned();
            if let Some(name) = body.get("name").and_then(Value::as_str) {
                inner.meta.name = name.to_owned();
            }
            if let Some(lifetime) = body.get("lifetime").and_then(Value::as_str) {
                inner.meta.lifetime = Lifetime::parse(lifetime)?;
            }
            inner.meta.used = SystemTime::now();
            ws.save_meta(&mut inner)?;
            Ok(json_reply(200, &ws.to_json(&inner, workspaces.options.ttl)))
        }
        [id] if method == Method::Delete => {
            let ws = workspaces.get(id, caller)?;
            workspaces.open.lock_unpoisoned().remove(&ws.id);
            fs::remove_dir_all(&ws.dir)?;
            Ok(Reply::empty(204))
        }
        [id, "history"] if get => {
            // `?since=N`: only the runs after run N.
            let ws = workspaces.get(id, caller)?;
            let since = params
                .get("since")
                .map(|v| v.parse::<u64>())
                .transpose()
                .map_err(|_| HttpError::bad_request("since: a run number"))?
                .unwrap_or(0);
            Ok(json_reply(200, &ws.history.lock_unpoisoned().list(since)))
        }
        [id, "history"] if method == Method::Delete => {
            let ws = workspaces.get(id, caller)?;
            ws.history.lock_unpoisoned().clear();
            Ok(Reply::empty(204))
        }
        [id, "history", run] if get => {
            let ws = workspaces.get(id, caller)?;
            let run = run
                .parse::<u64>()
                .ok()
                .and_then(|n| ws.history.lock_unpoisoned().get(n))
                .ok_or_else(|| HttpError(404, format!("No run {run} in the history")))?;
            Ok(json_reply(200, &run))
        }
        #[cfg(feature = "git")]
        [id, "pull"] if method == Method::Post => {
            let ws = workspaces.get(id, caller)?;
            pull(state, request, &ws, caller)
        }
        [id, "files", path @ ..] => {
            let ws = workspaces.get(id, caller)?;
            let path = check_path(path)?;
            file(state, request, &ws, &path)
        }
        [id, "sample"] if method == Method::Post => {
            let ws = workspaces.get(id, caller)?;
            let body = json_body(state, request)?;
            sample(state, &ws, &body, params, context)
        }
        [id, op @ ("validate" | "infer")] if get || method == Method::Post => {
            let ws = workspaces.get(id, caller)?;
            if method == Method::Post
                && crate::content_type(request).as_deref()
                    == Some("application/x-www-form-urlencoded")
            {
                let body = crate::read_body(request, state.options.max_body)?;
                params
                    .0
                    .extend(form_urlencoded::parse(body.as_bytes()).into_owned());
            }
            reason(state, request, params, &ws, op)
        }
        [id, "reason"] if get || method == Method::Post => {
            let ws = workspaces.get(id, caller)?;
            let (engine, timeout) = reasoning(state, params, &ws)?;
            crate::reason::reason(
                request,
                params,
                &engine,
                &[SHAPES_GRAPH, RULES_GRAPH],
                timeout,
            )
        }
        [id, "reason", "why"] if get || method == Method::Post => {
            let ws = workspaces.get(id, caller)?;
            let (engine, timeout) = reasoning(state, params, &ws)?;
            crate::reason::why(params, &engine, &[SHAPES_GRAPH, RULES_GRAPH], timeout)
        }
        [id, "check"] if method == Method::Post => {
            let ws = workspaces.get(id, caller)?;
            let body = json_body(state, request)?;
            let (Some(path), Some(text)) = (body["path"].as_str(), body["text"].as_str()) else {
                return Err(HttpError::bad_request(
                    "check takes {\"path\": \"…\", \"text\": \"…\"}",
                ));
            };
            let segments: Vec<&str> = path.split('/').collect();
            let path = check_path(&segments)?;
            ws.touch(&mut ws.inner.lock_unpoisoned());
            let errors = check_file(&path, text, state.workspaces.options.max_file);
            Ok(json_reply(
                200,
                &json!({ "path": path, "valid": errors.is_empty(), "errors": errors }),
            ))
        }
        [id, "tests"] if get || method == Method::Post => {
            let ws = workspaces.get(id, caller)?;
            {
                let mut inner = ws.inner.lock_unpoisoned();
                ws.touch(&mut inner);
            }
            let root = ws.files_dir();
            let base = root.join("tests");
            let found = crate::testing::discover(&base);
            let name = |dir: &std::path::Path| {
                dir.strip_prefix(&base)
                    .unwrap_or(dir)
                    .to_string_lossy()
                    .replace('\\', "/")
            };
            if get {
                let names: Vec<String> = found.iter().map(|d| name(d)).collect();
                return Ok(json_reply(200, &json!({ "tests": names })));
            }
            // Which tests: `{"tests": [...]}` in the body or `?test=`, else all.
            let body = crate::read_body(request, state.options.max_body)?;
            let mut only: Vec<String> = params.get("test").map(str::to_owned).into_iter().collect();
            if !body.trim().is_empty() {
                let body: Value = serde_json::from_str(&body)
                    .map_err(|e| HttpError::bad_request(format!("JSON body: {e}")))?;
                only.extend(
                    body.get("tests")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                );
            }
            let chosen: Vec<&std::path::PathBuf> = found
                .iter()
                .filter(|d| only.is_empty() || only.contains(&name(d)))
                .collect();
            if let Some(missing) = only.iter().find(|n| !found.iter().any(|d| &name(d) == *n)) {
                return Err(HttpError(404, format!("No test {missing} in tests/")));
            }
            let timeout = crate::limits::Limits::of(state, "").timeout(
                params
                    .get("timeout")
                    .map(crate::parse_timeout)
                    .transpose()?,
            );
            // `?record=true` writes what each test gives as its expected output.
            let record = params.get("record") == Some("true");
            if record {
                may_change(state, request, caller)?;
                if only.is_empty() {
                    return Err(HttpError::bad_request(
                        "record=true needs the tests to record: ?test=NAME or {\"tests\": [...]}",
                    ));
                }
            }
            // Bounded like a query, whatever the server's limits: a timeout (the CLI's 300 s
            // when none is set), rows, the files loaded and what is recorded.
            let max_rows = crate::limits::Limits::of(state, "")
                .max_rows
                .unwrap_or(TEST_MAX_ROWS);
            let timeout = timeout.or(Some(Duration::from_secs(300)));
            let outcomes: Vec<Value> = chosen
                .into_iter()
                .map(|d| {
                    // The room left, again for each test, as each may record.
                    let limits = crate::testing::Limits {
                        root: Some(&root),
                        timeout,
                        record,
                        max_bytes: Some(workspaces.options.max_bytes.saturating_sub(ws.bytes(""))),
                        max_input: Some(workspaces.options.max_bytes),
                        max_rows: Some(max_rows),
                    };
                    crate::testing::run(d, &base, limits).to_json()
                })
                .collect();
            let failed = outcomes.iter().filter(|o| o["passed"] != true).count();
            Ok(json_reply(
                200,
                &json!({
                    "passed": outcomes.len() - failed,
                    "failed": failed,
                    "tests": outcomes,
                }),
            ))
        }
        [id, "pipeline"] if get || method == Method::Post => {
            let ws = workspaces.get(id, caller)?;
            {
                let mut inner = ws.inner.lock_unpoisoned();
                ws.touch(&mut inner);
            }
            let root = ws.files_dir();
            if get {
                // While a run holds the pipeline, what it was before, marked as running.
                if ws
                    .pipeline_running
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    let mut view = ws
                        .pipeline_view
                        .lock_unpoisoned()
                        .clone()
                        .unwrap_or_else(|| json!({"steps": []}));
                    view["running"] = true.into();
                    return Ok(json_reply(200, &view));
                }
                let view = ws.pipeline.lock_unpoisoned().describe(&root);
                *ws.pipeline_view.lock_unpoisoned() = Some(view.clone());
                return Ok(json_reply(200, &view));
            }
            // `{"from": STEP, "to": STEP, "values": {...}}`, a step by number or name.
            let body = json_body(state, request)?;
            let step = |key: &str| match &body[key] {
                Value::Number(n) => Some(n.to_string()),
                Value::String(s) => Some(s.clone()),
                _ => None,
            };
            let values: HashMap<String, String> = body["values"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| {
                    let v = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    (k.clone(), v)
                })
                .collect();
            if ws
                .pipeline_running
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_err()
            {
                return Err(HttpError(
                    409,
                    "The pipeline of this workspace is running already".to_owned(),
                ));
            }
            struct Done<'a>(&'a std::sync::atomic::AtomicBool);
            impl Drop for Done<'_> {
                fn drop(&mut self) {
                    self.0.store(false, std::sync::atomic::Ordering::Release);
                }
            }
            let _done = Done(&ws.pipeline_running);
            // An overlay's steps start from its base (#406).
            let start = {
                let inner = ws.inner.lock_unpoisoned();
                inner.base()?.map(|b| Engine::from_store(b.overlay()))
            };
            let mut run = ws.pipeline.lock_unpoisoned();
            *ws.pipeline_view.lock_unpoisoned() = Some(run.describe(&root));
            // Bounded like the tests: the server's query timeout for each step (300 s when it
            // sets none), the store at most four times the workspace's triples, and what the
            // steps write within the room the workspace has left.
            let timeout = crate::limits::Limits::of(state, "")
                .timeout(
                    params
                        .get("timeout")
                        .map(crate::parse_timeout)
                        .transpose()?,
                )
                .or(Some(Duration::from_secs(300)));
            let options = &workspaces.options;
            let room = || options.max_bytes.saturating_sub(ws.bytes(""));
            let limits = ws.pipeline_limits(options, timeout, &room, start)?;
            let result = run.run(
                &root,
                step("from").as_deref(),
                step("to").as_deref(),
                values,
                &limits,
            );
            // Exports may have changed the workspace's files. An overlay's store keeps its
            // changes: what the exports wrote reaches it once its changes are dropped.
            {
                let mut inner = ws.inner.lock_unpoisoned();
                if inner.meta.overlay.is_none() {
                    inner.engine = None;
                }
                inner.counts = None;
            }
            result.map_err(HttpError::bad_request)?;
            let view = run.describe(&root);
            *ws.pipeline_view.lock_unpoisoned() = Some(view.clone());
            Ok(json_reply(200, &view))
        }
        [id, "pipeline", "export"] if get => {
            // `?to=airflow|python`, `one_task=true`, `location=DIR`, `dataset=NAME`.
            let ws = workspaces.get(id, caller)?;
            let root = ws.files_dir();
            let pipeline = crate::pipeline::Pipeline::read(&root.join("pipeline.yaml"))
                .map_err(HttpError::bad_request)?;
            let values = HashMap::new();
            let location = params.get("location").map(str::to_owned);
            let target = crate::pipeline_export::Target {
                file: "pipeline.yaml",
                location: Some(location.as_deref().unwrap_or("/path/to/dataset")),
                dataset: params.get("dataset").unwrap_or("ds"),
                values: &values,
            };
            let (code, name) = match params.get("to") {
                Some("airflow") | None => (
                    crate::pipeline_export::airflow(
                        &pipeline,
                        &target,
                        params.get("one_task") != Some("true"),
                    ),
                    "dag.py",
                ),
                Some("python") => {
                    let target = crate::pipeline_export::Target {
                        location: location.as_deref(),
                        ..target
                    };
                    (
                        crate::pipeline_export::python(&pipeline, &target),
                        "run_pipeline.py",
                    )
                }
                Some(other) => {
                    return Err(HttpError::bad_request(format!(
                        "to={other}: airflow or python"
                    )));
                }
            };
            let mut reply = Reply::bytes(200, "text/x-python; charset=utf-8", code.into_bytes());
            reply.headers.push((
                "Content-Disposition",
                format!("inline; filename=\"{name}\""),
            ));
            Ok(reply)
        }
        [id, "pipeline", "blame"] if get => {
            // `?subject=&predicate=&object=`, N-Triples terms.
            let ws = workspaces.get(id, caller)?;
            let root = ws.files_dir();
            let term = |k: &str| {
                params.get(k).map(str::to_owned).ok_or_else(|| {
                    HttpError::bad_request(format!("blame needs ?{k}= (an N-Triples term)"))
                })
            };
            let (subject, predicate, object) =
                (term("subject")?, term("predicate")?, term("object")?);
            let timeout = crate::limits::Limits::of(state, "")
                .timeout(None)
                .or(Some(Duration::from_secs(300)));
            let options = &workspaces.options;
            let room = || 0;
            let limits = ws.pipeline_limits(options, timeout, &room, None)?;
            let run = running(&ws)?;
            let blame = run
                .blame(&root, &subject, &predicate, &object, &limits)
                .map_err(HttpError::bad_request)?;
            Ok(json_reply(200, &blame))
        }
        [id, "pipeline", step, "complete"] if method == Method::Post => {
            // A python step waiting for its code: what that code left is the step's result.
            let ws = workspaces.get(id, caller)?;
            let root = ws.files_dir();
            let i = step
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .ok_or_else(|| HttpError::bad_request(format!("{step}: a step number")))?;
            let options = &workspaces.options;
            let room = || 0;
            let limits = ws.pipeline_limits(options, None, &room, None)?;
            let mut run = running(&ws)?;
            run.complete(&root, i, &limits)
                .map_err(HttpError::bad_request)?;
            let view = run.describe(&root);
            *ws.pipeline_view.lock_unpoisoned() = Some(view.clone());
            Ok(json_reply(200, &view))
        }
        [
            id,
            "pipeline",
            step,
            service @ ("sparql" | "query" | "update" | "data"),
        ] => {
            let ws = workspaces.get(id, caller)?;
            let index = step.parse::<usize>().ok().and_then(|n| n.checked_sub(1));
            // A python step waiting for its code serves the store it works on, read-write, as
            // a Fuseki dataset named by the step's number under `…/pipeline`.
            let pending = {
                let run = running(&ws)?;
                index.and_then(|i| run.pending(i))
            };
            if let Some(engine) = pending {
                if !get {
                    // The owner's (or an admin's) working store: they may write it, an update
                    // sent to /sparql as well. While it is over the store's bound, only a
                    // graph's removal is taken, to get back under it.
                    may_change(state, request, caller)?;
                    context.access = Some(crate::auth::Access {
                        read: true,
                        write: true,
                        ..Default::default()
                    });
                    let max = workspaces.options.max_triples.saturating_mul(4);
                    let quads = engine.store().len().unwrap_or(0);
                    if quads > max && !(*service == "data" && method == Method::Delete) {
                        return Err(HttpError(
                            413,
                            format!(
                                "The step's store holds {quads} quads, more than the {max} it may hold here: \
                                 remove a graph (DELETE …/data?graph=) or run the step again"
                            ),
                        ));
                    }
                }
                context.limits = crate::limits::Limits::of(state, "");
                if *service == "data" {
                    return crate::graph_store(
                        &engine,
                        request,
                        &method,
                        params,
                        context.limits.max_upload,
                        None,
                        None,
                        None,
                        None,
                    );
                }
                let path = format!("/$/workspaces/{}/pipeline/{step}/{service}", ws.id);
                return crate::protocol(
                    state, request, params, context, &engine, service, &path, false,
                );
            }
            if matches!(*service, "update" | "data") {
                return Err(HttpError(
                    405,
                    format!(
                        "Step {step} is not a python step waiting for its code: its store is read-only"
                    ),
                ));
            }
            let engine = {
                let run = running(&ws)?;
                index.and_then(|i| run.snapshot(i))
            }
            .ok_or_else(|| {
                HttpError(
                    404,
                    format!("No snapshot after step {step}: run the pipeline to it first"),
                )
            })?;
            context.limits = crate::limits::Limits::of(state, "");
            let path = format!("/$/workspaces/{}/pipeline/{step}/{service}", ws.id);
            crate::protocol(
                state, request, params, context, &engine, service, &path, true,
            )
        }
        [id, "overlay", rest @ ..] => {
            let ws = workspaces.get(id, caller)?;
            overlay(state, request, &ws, rest, params, context)
        }
        [id, service] if SERVICES.contains(service) => {
            let ws = workspaces.get(id, caller)?;
            let (engine, changes) = {
                let mut inner = ws.inner.lock_unpoisoned();
                ws.touch(&mut inner);
                let engine = ws.engine(&mut inner)?;
                (engine, inner.overlay_changes())
            };
            context.limits = crate::limits::Limits::of(state, "");
            let path = format!("/$/workspaces/{}/{service}", ws.id);
            // The scratch store is the files': it changes only when they do.
            // An overlay's takes updates (#406), from those who may change the workspace,
            // within its cap: checked by `protocol` once it knows the request is an update.
            let read_only = match changes {
                None => true,
                Some(changes) => {
                    context.update_gate = Some(crate::UpdateGate {
                        allowed: may_change(state, request, caller),
                        changes,
                        max: workspaces.options.max_overlay_changes,
                    });
                    false
                }
            };
            crate::protocol(
                state, request, params, context, &engine, service, &path, read_only,
            )
        }
        _ => Err(HttpError(
            if get || matches!(method, Method::Post | Method::Put | Method::Delete) {
                404
            } else {
                405
            },
            format!("No endpoint {method} /$/workspaces/{}", rest.join("/")),
        )),
    }
}

/// `/$/workspaces/{id}/overlay…` (#406): makes the workspace an overlay of a dataset, whose
/// scratch store reads the dataset's quads as they were then and keeps its own changes:
///
/// - `POST …/overlay` `{"dataset": NAME}`: takes the dataset (again: its changes go),
/// - `GET …/overlay`: the dataset, when it was taken, and about how many quads changed,
/// - `GET …/overlay/patch?format=rdf-patch|added|removed`: the changes, as an RDF Patch or the
///   added or removed quads as N-Quads (the graphs of the shapes and the rules left out),
/// - `DELETE …/overlay/changes`: drops the changes, back to the dataset as taken,
/// - `DELETE …/overlay`: a plain workspace again.
///
/// The dataset is never written, and its writers are not held up.
fn overlay(
    state: &State,
    request: &mut Request,
    ws: &Workspace,
    rest: &[&str],
    params: &Params,
    context: &mut Context,
) -> Result<Reply, HttpError> {
    let method = request.method().clone();
    let get = matches!(method, Method::Get | Method::Head);
    let describe = |inner: &Inner| {
        json_reply(
            200,
            &ws.to_json(inner, state.workspaces.options.ttl)["overlay"],
        )
    };
    match rest {
        [] if get => {
            let inner = ws.inner.lock_unpoisoned();
            Ok(describe(&inner))
        }
        [] if method == Method::Post => {
            let body = json_body(state, request)?;
            let name = body
                .get("dataset")
                .and_then(Value::as_str)
                .or_else(|| params.get("dataset"))
                .ok_or_else(|| HttpError::bad_request("The overlay needs a dataset"))?
                .to_owned();
            take_overlay(state, ws, &name, context)
        }
        [] if method == Method::Delete => {
            let mut inner = ws.inner.lock_unpoisoned();
            inner.meta.overlay = None;
            inner.base = None;
            inner.engine = None;
            ws.save_meta(&mut inner)?;
            Ok(Reply::empty(204))
        }
        ["changes"] if method == Method::Delete => {
            let mut inner = ws.inner.lock_unpoisoned();
            inner.base()?;
            if inner.meta.overlay.is_none() {
                return Err(HttpError(
                    409,
                    "This workspace is not an overlay".to_owned(),
                ));
            }
            inner.engine = None;
            Ok(Reply::empty(204))
        }
        ["patch"] if get => {
            let (base, engine) = {
                let mut inner = ws.inner.lock_unpoisoned();
                let base = inner
                    .base()?
                    .cloned()
                    .ok_or_else(|| HttpError(409, "This workspace is not an overlay".to_owned()))?;
                (base, ws.engine(&mut inner)?)
            };
            let format = params.get("format").unwrap_or("rdf-patch");
            if !matches!(format, "rdf-patch" | "added" | "removed") {
                return Err(HttpError::bad_request(format!(
                    "Unknown format {format}: rdf-patch, added or removed"
                )));
            }
            let tripletrail::store::Dataset::TripleTrail(store) = engine.store() else {
                return Err(HttpError(
                    500,
                    "The overlay is not a TripleTrail store".to_owned(),
                ));
            };
            let (added, removed) = store.changes_from(&base);
            let view = store.snapshot();
            let left_out: Vec<u64> = [SHAPES_GRAPH, RULES_GRAPH]
                .iter()
                .filter_map(|g| view.term_id(NamedNode::new_unchecked(*g).as_ref().into()))
                .map(|id| id.0)
                .collect();
            // Written while it is sent: a patch of millions of changes is not held in memory.
            let content_type = if format == "rdf-patch" {
                "application/rdf-patch"
            } else {
                "application/n-quads"
            };
            let patch = format == "rdf-patch";
            let sides: Vec<(&'static str, Vec<[tripletrail::store::TermId; 4]>)> = match format {
                "added" => vec![("", added)],
                "removed" => vec![("", removed)],
                // Removals first, then additions: each quad is in one list.
                _ => vec![("D ", removed), ("A ", added)],
            };
            let mut reply = Reply::stream(content_type, move |out| {
                let term = |id| {
                    view.term(id)
                        .map(|t| t.to_string())
                        .map_err(std::io::Error::other)
                };
                if patch {
                    out.write_all(b"TX .\n")?;
                }
                for (tag, quads) in &sides {
                    for q in quads {
                        if left_out.contains(&q[3].0) {
                            continue;
                        }
                        write!(out, "{tag}{} {} {}", term(q[0])?, term(q[1])?, term(q[2])?)?;
                        if q[3].0 != 0 {
                            write!(out, " {}", term(q[3])?)?;
                        }
                        out.write_all(b" .\n")?;
                    }
                }
                if patch {
                    out.write_all(b"TC .\n")?;
                }
                Ok(())
            })?;
            reply.headers.push((
                "Content-Disposition",
                format!(
                    "attachment; filename=\"{}-{format}.{}\"",
                    ws.id,
                    if format == "rdf-patch" { "rdfp" } else { "nq" }
                ),
            ));
            Ok(reply)
        }
        _ => Err(HttpError(
            if get || matches!(method, Method::Post | Method::Delete) {
                404
            } else {
                405
            },
            format!(
                "No endpoint {method} /$/workspaces/{}/overlay/{}",
                ws.id,
                rest.join("/")
            ),
        )),
    }
}

/// Makes `ws` an overlay of the dataset `name`, taken now.
fn take_overlay(
    state: &State,
    ws: &Workspace,
    name: &str,
    context: &mut Context,
) -> Result<Reply, HttpError> {
    let (entry, view) = {
        let registry = state.registry.read_unpoisoned();
        registry.resolve(name)
    }
    .and_then(|e| {
        let view = crate::may_list(state, context.caller.as_ref(), &[name, e.name.as_str()])?;
        Some((e, view))
    })
    .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?;
    // An overlay shares every graph of the store; a caller who reads only some (#203) samples
    // the dataset instead.
    if view.is_some() {
        return Err(HttpError(
            403,
            format!(
                "You may read only some graphs of {name}: an overlay would hold them all; \
                 take a sample of it instead (POST …/sample)"
            ),
        ));
    }
    // Taking it is a read of the dataset (a moment's, under its read lock), and a change of
    // the workspace.
    crate::use_dataset(state, context, &entry)?;
    context.operation = Some(crate::admin::Operation::Read);
    context.changes = true;
    let tripletrail::store::Dataset::TripleTrail(store) = entry.engine.store() else {
        return Err(HttpError::bad_request(format!(
            "The dataset {} is not kept in a TripleTrail store: an overlay needs one",
            entry.name
        )));
    };
    let base = store.overlay();
    let quads = base.len();
    let mut inner = ws.inner.lock_unpoisoned();
    inner.meta.overlay = Some(json!({
        "dataset": entry.name,
        "at": rfc3339(SystemTime::now()),
        "quads": quads,
    }));
    inner.base = Some(base);
    inner.engine = None;
    ws.save_meta(&mut inner)?;
    Ok(json_reply(
        200,
        &ws.to_json(&inner, state.workspaces.options.ttl)["overlay"],
    ))
}

/// The workspace's pipeline, or 409 while a run holds it.
fn running(
    ws: &Workspace,
) -> Result<std::sync::MutexGuard<'_, crate::workspace_pipeline::State>, HttpError> {
    if ws
        .pipeline_running
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(HttpError(
            409,
            "The pipeline of this workspace is running: try again when it is done".to_owned(),
        ));
    }
    Ok(ws.pipeline.lock_unpoisoned())
}

fn create(
    state: &State,
    request: &mut Request,
    caller: Option<&crate::auth::Caller>,
) -> Result<Reply, HttpError> {
    let workspaces = &state.workspaces;
    let options = &workspaces.options;
    let body = json_body(state, request)?;
    let owner = caller.and_then(|c| c.agent.clone());
    let lifetime = body
        .get("lifetime")
        .and_then(Value::as_str)
        .map(Lifetime::parse)
        .transpose()?
        .unwrap_or(Lifetime::Scratch);
    // Files given with the workspace, checked before anything is written.
    let mut files = Vec::new();
    if let Some(given) = body.get("files") {
        let given = given
            .as_object()
            .ok_or_else(|| HttpError::bad_request("files is an object of path: text"))?;
        if given.len() > options.max_files {
            return Err(too_many_files(options));
        }
        let mut triples = 0;
        let mut bytes = 0;
        for (path, text) in given {
            let segments: Vec<&str> = path.split('/').collect();
            let path = check_path(&segments)?;
            let text = text
                .as_str()
                .ok_or_else(|| HttpError::bad_request(format!("{path}: the text is a string")))?;
            if text.len() > options.max_file {
                return Err(too_large(&path, options));
            }
            bytes += text.len() as u64;
            if graph_of(&path).is_some() {
                triples += parse(&path, text.as_bytes(), options.max_file)?.len();
            }
            files.push((path, text.to_owned()));
        }
        if triples > options.max_triples {
            return Err(too_many_triples(options));
        }
        if bytes > options.max_bytes {
            return Err(too_many_bytes(options));
        }
    }
    workspaces.ready()?;
    let ws = {
        let mut open = workspaces.open.lock_unpoisoned();
        if open.len() >= options.max_workspaces {
            return Err(HttpError(
                429,
                format!(
                    "The server has its {} workspaces: delete one first",
                    options.max_workspaces
                ),
            ));
        }
        let mine = open.values().filter(|ws| ws.owner == owner).count();
        if mine >= options.max_per_owner {
            return Err(HttpError(
                429,
                format!(
                    "You have {} workspaces, the most one caller may have: delete one first",
                    options.max_per_owner
                ),
            ));
        }
        let id = new_id();
        let now = SystemTime::now();
        let meta = Meta {
            name: body
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Untitled")
                .to_owned(),
            owner: owner.clone(),
            lifetime,
            created: now,
            used: now,
            sampled: Default::default(),
            git: None,
            overlay: None,
        };
        let ws = Arc::new(Workspace {
            dir: workspaces.dir.join(&id),
            id: id.clone(),
            owner,
            max_file: options.max_file,
            inner: Mutex::new(Inner::new(meta)),
            pipeline: Mutex::default(),
            pipeline_view: Mutex::default(),
            pipeline_running: Default::default(),
            history: Mutex::default(),
            pulling: Mutex::default(),
        });
        fs::create_dir_all(ws.files_dir())?;
        for (path, text) in &files {
            let target = ws.files_dir().join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(target, text)?;
        }
        ws.save_meta(&mut ws.inner.lock_unpoisoned())?;
        open.insert(id, Arc::clone(&ws));
        ws
    };
    let inner = ws.inner.lock_unpoisoned();
    let mut reply = json_reply(201, &ws.to_json(&inner, options.ttl));
    reply
        .headers
        .push(("Location", format!("/$/workspaces/{}", ws.id)));
    Ok(reply)
}

fn too_many_files(options: &WorkspaceOptions) -> HttpError {
    HttpError(
        413,
        format!("A workspace holds at most {} files", options.max_files),
    )
}

fn too_large(path: &str, options: &WorkspaceOptions) -> HttpError {
    HttpError(
        413,
        format!("{path}: a file holds at most {} bytes", options.max_file),
    )
}

fn too_many_bytes(options: &WorkspaceOptions) -> HttpError {
    HttpError(
        413,
        format!(
            "A workspace holds at most {} bytes of files",
            options.max_bytes
        ),
    )
}

fn too_many_triples(options: &WorkspaceOptions) -> HttpError {
    HttpError(
        413,
        format!(
            "A workspace holds at most {} triples in its files",
            options.max_triples
        ),
    )
}

/// `GET`, `PUT` and `DELETE` of one file.
fn file(
    state: &State,
    request: &mut Request,
    ws: &Workspace,
    path: &str,
) -> Result<Reply, HttpError> {
    let options = &state.workspaces.options;
    let target = ws.files_dir().join(path);
    match request.method() {
        Method::Get | Method::Head => {
            let bytes = fs::read(&target).map_err(|_| HttpError(404, format!("No file {path}")))?;
            ws.touch(&mut ws.inner.lock_unpoisoned());
            Ok(Reply::bytes(200, content_type(path), bytes))
        }
        Method::Put => {
            let bytes = crate::read_bytes(request, Some(options.max_file)).map_err(|e| {
                if e.0 == 413 {
                    too_large(path, options)
                } else {
                    e
                }
            })?;
            let mut inner = ws.inner.lock_unpoisoned();
            let existed = target.is_file();
            if !existed && ws.files().len() >= options.max_files {
                return Err(too_many_files(options));
            }
            if ws.bytes(path) + bytes.len() as u64 > options.max_bytes {
                return Err(too_many_bytes(options));
            }
            let mut triples = None;
            if graph_of(path).is_some() {
                let added = parse(path, &bytes, options.max_file)?.len();
                if ws.triples(&mut inner, path)? + added > options.max_triples {
                    return Err(too_many_triples(options));
                }
                triples = Some(added);
            }
            // A sampled file stays marked through edits (an edit keeps most of the sample): only
            // its removal clears the mark.
            write_file(ws, &mut inner, path, &bytes, triples)?;
            Ok(Reply::empty(if existed { 204 } else { 201 }))
        }
        Method::Delete => {
            let mut inner = ws.inner.lock_unpoisoned();
            fs::remove_file(&target).map_err(|_| HttpError(404, format!("No file {path}")))?;
            // Empty folders go too, up to the workspace's.
            let mut dir = target.parent();
            while let Some(d) = dir
                && d != ws.files_dir()
                && fs::remove_dir(d).is_ok()
            {
                dir = d.parent();
            }
            if graph_of(path).is_some() {
                file_changed(ws, &mut inner, path);
                if let Some(counts) = &mut inner.counts {
                    counts.remove(path);
                }
            }
            inner.meta.sampled.remove(path);
            inner.meta.used = SystemTime::now();
            ws.save_meta(&mut inner)?;
            Ok(Reply::empty(204))
        }
        method => Err(HttpError(405, format!("{method} is not allowed on a file"))),
    }
}

/// Writes a file (through a temporary file, so a reader never sees half of it) and drops the
/// scratch store if the file is one of its (`triples`: those it holds).
fn write_file(
    ws: &Workspace,
    inner: &mut Inner,
    path: &str,
    bytes: &[u8],
    triples: Option<usize>,
) -> Result<(), HttpError> {
    let target = ws.files_dir().join(path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = target.with_file_name(format!(
        ".{}.writing",
        target.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, &target)?;
    if let Some(n) = triples {
        file_changed(ws, inner, path);
        if let Some(counts) = &mut inner.counts {
            counts.insert(path.to_owned(), n);
        }
    }
    inner.meta.used = SystemTime::now();
    ws.save_meta(inner)
}

/// Takes a change of the RDF file `path` to the scratch store, which is built again from the
/// files when next used. In an overlay, the graph of the shapes or of the rules is loaded
/// again in place instead, so that the overlay keeps its changes; a change of a data file
/// starts it over from its base.
fn file_changed(ws: &Workspace, inner: &mut Inner, path: &str) {
    if inner.meta.overlay.is_some()
        && let Some(engine) = &inner.engine
        && let Some(graph @ GraphName::NamedNode(_)) = graph_of(path)
        && reload_graph(ws, engine, &graph).is_ok()
    {
        return;
    }
    inner.engine = None;
}

/// Loads the files of `graph` (that of the shapes or of the rules) into the scratch store
/// again.
fn reload_graph(ws: &Workspace, engine: &Engine, graph: &GraphName) -> Result<(), HttpError> {
    let GraphName::NamedNode(g) = graph else {
        return Err(HttpError::bad_request("not a named graph"));
    };
    engine
        .update(&format!("DROP SILENT GRAPH <{}>", g.as_str()))
        .map_err(|e| HttpError(500, e.to_string()))?;
    for (path, _) in ws.files() {
        if graph_of(&path).as_ref() == Some(graph) {
            let bytes = fs::read(ws.files_dir().join(&path))?;
            let quads = parse(&path, &bytes, ws.max_file)?;
            engine.load_quads(quads.into_iter().map(|q| into_graph(q, graph)))?;
        }
    }
    Ok(())
}

/// Whether the scratch store has triples in `graph`.
fn has_triples(engine: &Engine, graph: &str) -> bool {
    let graph = NamedNode::new_unchecked(graph);
    engine
        .store()
        .quads_for_pattern(None, None, None, Some(graph.as_ref().into()))
        .next()
        .is_some()
}

/// `…/validate` and `…/infer` on the scratch store (#401). Inference reads the rules of
/// `rules/` (or, with `shapes=shapes`, or when `rules/` has none, those of `shapes/`).
/// Validation checks the data against `shapes/`, after the rules of `rules/` ran on it unless
/// `rules=false`: the data plus what they infer.
/// The workspace's engine for `…/reason` (#407), and the timeout.
fn reasoning(
    state: &State,
    params: &Params,
    ws: &Workspace,
) -> Result<(Engine, Option<std::time::Duration>), HttpError> {
    let engine = {
        let mut inner = ws.inner.lock_unpoisoned();
        ws.touch(&mut inner);
        ws.engine(&mut inner)?
    };
    let timeout = crate::limits::Limits::of(state, "").timeout(
        params
            .get("timeout")
            .map(crate::parse_timeout)
            .transpose()?,
    );
    Ok((engine, timeout))
}

fn reason(
    state: &State,
    request: &Request,
    params: &Params,
    ws: &Workspace,
    op: &str,
) -> Result<Reply, HttpError> {
    let engine = {
        let mut inner = ws.inner.lock_unpoisoned();
        ws.touch(&mut inner);
        ws.engine(&mut inner)?
    };
    let timeout = crate::limits::Limits::of(state, "").timeout(
        params
            .get("timeout")
            .map(crate::parse_timeout)
            .transpose()?,
    );
    let shapes = NamedNode::new_unchecked(SHAPES_GRAPH);
    let rules = NamedNode::new_unchecked(RULES_GRAPH);
    let has_rules = has_triples(&engine, RULES_GRAPH);
    if op == "infer" {
        let graph = match params.get("shapes") {
            Some("shapes") => shapes,
            Some("rules") => rules,
            Some(other) => {
                return Err(HttpError::bad_request(format!(
                    "shapes={other}: shapes or rules"
                )));
            }
            None if has_rules => rules,
            None => shapes,
        };
        let target = crate::reason::Target {
            engine: &engine,
            shapes: graph,
            data: GraphName::DefaultGraph,
            timeout,
        };
        return crate::reason::infer(request, params, &target, true);
    }
    if !has_rules || params.get("rules") == Some("false") {
        let target = crate::reason::Target {
            engine: &engine,
            shapes,
            data: GraphName::DefaultGraph,
            timeout,
        };
        return crate::reason::validate(request, params, &target);
    }
    // The data plus what the rules infer, with the shapes, in a store of its own. One
    // deadline covers both the rules and the validation.
    let started = Instant::now();
    let (limited, _deadline) = crate::with_deadline(&engine, timeout);
    let inferred = limited
        .infer_with(
            &rules,
            GraphName::DefaultGraph.as_ref(),
            None,
            &tripletrail::InferOptions::default(),
            &|_| {},
        )
        .map_err(|e| crate::reason::shacl_error_as(e, "The rules do not run"))?;
    let combined = Engine::new_tripletrail();
    let store = engine.store();
    combined.load_quads(
        store
            .quads_for_pattern(None, None, None, Some(GraphName::DefaultGraph.as_ref()))
            .chain(store.quads_for_pattern(None, None, None, Some(shapes.as_ref().into())))
            .filter_map(Result::ok)
            .chain(
                inferred
                    .iter()
                    .map(|t| t.clone().in_graph(GraphName::DefaultGraph)),
            ),
    )?;
    let target = crate::reason::Target {
        engine: &combined,
        shapes,
        data: GraphName::DefaultGraph,
        // What is left of it, and at least a moment, so that a spent deadline still answers
        // as a timeout.
        timeout: timeout.map(|t| {
            t.saturating_sub(started.elapsed())
                .max(Duration::from_millis(1))
        }),
    };
    crate::reason::validate(request, params, &target)
}

/// `POST …/pull` (#400): the files of a folder of a git repository (`url`, `ref`, `path`; the
/// last pull's by default) written over the workspace's, as the repository is the master;
/// files only in the workspace stay. Only for an admin: the server fetches the URL.
#[cfg(feature = "git")]
fn pull(
    state: &State,
    request: &mut Request,
    ws: &Workspace,
    caller: Option<&crate::auth::Caller>,
) -> Result<Reply, HttpError> {
    let admin = match caller {
        Some(c) => c.is_admin(),
        None => state
            .options
            .admin_token
            .as_deref()
            .is_none_or(|t| crate::admin::bearer_is(request, t)),
    };
    if !admin {
        return Err(HttpError(
            403,
            "Pulling a workspace from git needs an admin: the server fetches the repository"
                .to_owned(),
        ));
    }
    let options = &state.workspaces.options;
    let body = json_body(state, request)?;
    let last = ws.inner.lock_unpoisoned().meta.git.clone();
    let given = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .or_else(|| {
                last.as_ref()
                    .and_then(|g| g.get(key))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned)
    };
    let url = given("url").ok_or_else(|| {
        HttpError::bad_request("url: the repository to pull (none was pulled before)")
    })?;
    let reference = given("ref").unwrap_or_else(|| "HEAD".to_owned());
    let path = given("path").unwrap_or_default();
    // Only what a workspace holds: the other files of the repository are not read.
    let keep = |p: &str| check_path(&p.split('/').collect::<Vec<_>>()).is_ok();
    let source = crate::shapes_git::Source {
        url: &url,
        reference: &reference,
        path: &path,
        user: None,
        token_file: None,
        max_fetch: state.options.git_fetch,
        max_read: Some(options.max_bytes),
        max_files: Some(options.max_files),
    };
    let (commit, found) = {
        // One pull of a workspace at a time.
        let _one = ws.pulling.lock_unpoisoned();
        crate::shapes_git::fetch_files(&ws.dir.join("repository"), &source, &keep)
            .map_err(|e| HttpError(502, format!("Pulling {url}: {e}")))?
    };
    let mut inner = ws.inner.lock_unpoisoned();
    // The workspace as it would be, within its caps, before anything is written.
    let existing: BTreeMap<String, u64> = ws.files().into_iter().collect();
    let counts = ws.counts(&mut inner)?.clone();
    let mut files = existing.len();
    let mut bytes: u64 = existing.values().sum();
    let mut triples: usize = counts.values().sum();
    let mut parsed = Vec::new();
    for (path, data) in &found {
        if data.len() > options.max_file {
            return Err(too_large(path, options));
        }
        match existing.get(path) {
            Some(old) => bytes -= old,
            None => files += 1,
        }
        bytes += data.len() as u64;
        let n = match graph_of(path) {
            Some(_) => Some(parse(path, data, options.max_file)?.len()),
            None => None,
        };
        triples = triples - counts.get(path).copied().unwrap_or(0) + n.unwrap_or(0);
        parsed.push(n);
    }
    if files > options.max_files {
        return Err(too_many_files(options));
    }
    if bytes > options.max_bytes {
        return Err(too_many_bytes(options));
    }
    if triples > options.max_triples {
        return Err(too_many_triples(options));
    }
    let mut changed = Vec::new();
    for ((path, data), n) in found.iter().zip(parsed) {
        let same = existing.get(path) == Some(&(data.len() as u64))
            && fs::read(ws.files_dir().join(path)).is_ok_and(|old| &old == data);
        if same {
            continue;
        }
        inner.meta.sampled.remove(path);
        write_file(ws, &mut inner, path, data, n)?;
        changed.push(path.clone());
    }
    let git = json!({
        "url": url,
        "ref": reference,
        "path": path,
        "commit": commit,
        "at": rfc3339(SystemTime::now()),
    });
    inner.meta.git = Some(git.clone());
    inner.meta.used = SystemTime::now();
    ws.save_meta(&mut inner)?;
    Ok(json_reply(
        200,
        &json!({
            "git": git,
            "files": found.len(),
            "changed": changed,
        }),
    ))
}

/// What a sample asks for.
struct SampleSpec {
    classes: Vec<NamedNode>,
    /// Instances per class.
    instances: usize,
    /// How far from the instances: 1 is their own triples, 2 also those of the nodes they
    /// point to, and so on.
    hops: usize,
    /// Most triples read per node.
    per_node: usize,
}

struct Sampled {
    triples: Vec<Triple>,
    instances: usize,
    nodes: usize,
    truncated: bool,
}

/// `POST /$/workspaces/{id}/sample`: copies a sample of a dataset to a file of `data/`.
fn sample(
    state: &State,
    ws: &Workspace,
    body: &Value,
    params: &Params,
    context: &mut Context,
) -> Result<Reply, HttpError> {
    let options = &state.workspaces.options;
    let name = body
        .get("dataset")
        .and_then(Value::as_str)
        .ok_or_else(|| HttpError::bad_request("The sample needs a dataset"))?;
    // A caller who reads only some graphs of the dataset (#203) samples those graphs.
    let (entry, view) = {
        let registry = state.registry.read_unpoisoned();
        registry.resolve(name)
    }
    .and_then(|e| {
        let view = crate::may_list(state, context.caller.as_ref(), &[name, e.name.as_str()])?;
        Some((e, view))
    })
    .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?;
    let classes = body
        .get("classes")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| HttpError::bad_request("The sample needs classes: an array of class IRIs"))?
        .iter()
        .map(|c| {
            let c = c.as_str().unwrap_or_default();
            NamedNode::new(c)
                .map_err(|e| HttpError::bad_request(format!("<{c}> is not an IRI: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let number = |key: &str, default: usize, max: usize| -> Result<usize, HttpError> {
        match body.get(key) {
            None => Ok(default),
            Some(v) => v
                .as_u64()
                .map(|n| n as usize)
                .filter(|n| (1..=max).contains(n))
                .ok_or_else(|| {
                    HttpError::bad_request(format!("{key} must be a number from 1 to {max}"))
                }),
        }
    };
    let spec = SampleSpec {
        classes,
        instances: number("instances", 100, 100_000)?,
        hops: number("hops", 2, 5)?,
        per_node: number("perNode", 200, 100_000)?,
    };
    let file = match body.get("file").and_then(Value::as_str) {
        Some(f) => f.to_owned(),
        None => format!(
            "data/sample-{}.nt",
            entry
                .name
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                })
                .collect::<String>()
        ),
    };
    let segments: Vec<&str> = file.split('/').collect();
    let file = check_path(&segments)?;
    if !file.starts_with("data/") || format_of(&file) != Some((RdfFormat::NTriples, false)) {
        return Err(HttpError::bad_request(format!(
            "{file}: a sample is written to an .nt file of data/ (not .gz)"
        )));
    }
    // The sample is a read of the dataset: it takes a turn and its timeout like a query. It
    // writes a file of the workspace.
    crate::use_dataset(state, context, &entry)?;
    context.operation = Some(crate::admin::Operation::Read);
    context.changes = true;
    let timeout = context.limits.timeout(
        params
            .get("timeout")
            .map(crate::parse_timeout)
            .transpose()?,
    );
    if !ws.files_dir().join(&file).is_file() && ws.files().len() >= options.max_files {
        return Err(too_many_files(options));
    }
    let room = {
        let mut inner = ws.inner.lock_unpoisoned();
        options
            .max_triples
            .saturating_sub(ws.triples(&mut inner, &file)?)
    };
    let started = Instant::now();
    let sampled = take_sample(
        entry.engine.store(),
        &spec,
        room,
        timeout.map(|t| started + t),
        view.as_ref(),
    )?;
    let mut serializer = RdfSerializer::from_format(RdfFormat::NTriples).for_writer(Vec::new());
    for t in &sampled.triples {
        serializer.serialize_triple(t)?;
    }
    let bytes = serializer.finish()?;
    if bytes.len() > options.max_file {
        return Err(too_large(&file, options));
    }
    if ws.bytes(&file) + bytes.len() as u64 > options.max_bytes {
        return Err(too_many_bytes(options));
    }
    {
        let mut inner = ws.inner.lock_unpoisoned();
        inner.meta.sampled.insert(file.clone());
        write_file(ws, &mut inner, &file, &bytes, Some(sampled.triples.len()))?;
    }
    Ok(json_reply(
        200,
        &json!({
            "file": file,
            "dataset": entry.name,
            "instances": sampled.instances,
            "nodes": sampled.nodes,
            "triples": sampled.triples.len(),
            "truncated": sampled.truncated,
            "ms": started.elapsed().as_millis() as u64,
        }),
    ))
}

/// Up to `spec.instances` instances of each class, and the triples of the nodes within
/// `spec.hops` of them (outgoing: what they point to), at most `spec.per_node` per node and
/// `room` in all. Reads every graph (the `readable` ones only, when given); the sample's
/// triples have none.
fn take_sample(
    store: &tripletrail::store::Dataset,
    spec: &SampleSpec,
    room: usize,
    deadline: Option<Instant>,
    readable: Option<&tripletrail::ReadableGraphs>,
) -> Result<Sampled, HttpError> {
    let read = |quad: &Result<
        tripletrail::oxigraph::model::Quad,
        tripletrail::store::DatasetError,
    >| match (quad, readable) {
        (Ok(q), Some(r)) => r.contains(q.graph_name.as_ref()),
        _ => true,
    };
    let mut seen: HashSet<NamedOrBlankNode> = HashSet::new();
    let mut frontier = Vec::new();
    for class in &spec.classes {
        let mut taken = 0;
        for quad in store
            .quads_for_pattern(None, Some(rdf::TYPE), Some(class.as_ref().into()), None)
            .filter(read)
        {
            let quad = quad?;
            if seen.insert(quad.subject.clone()) {
                frontier.push(quad.subject);
                taken += 1;
                if taken == spec.instances {
                    break;
                }
            }
        }
    }
    let instances = frontier.len();
    let mut triples = Vec::new();
    let mut kept: HashSet<Triple> = HashSet::new();
    let mut truncated = false;
    let mut visited = 0usize;
    'hops: for _ in 0..spec.hops {
        let mut next = Vec::new();
        for node in &frontier {
            visited += 1;
            if visited.is_multiple_of(256) && deadline.is_some_and(|d| Instant::now() > d) {
                return Err(HttpError(
                    503,
                    "The sample took longer than its timeout: ask for fewer instances or hops"
                        .to_owned(),
                ));
            }
            for quad in store
                .quads_for_pattern(Some(node.as_ref()), None, None, None)
                .filter(read)
                .take(spec.per_node)
            {
                let quad = quad?;
                let object: Option<NamedOrBlankNode> = match &quad.object {
                    Term::NamedNode(n) => Some(n.clone().into()),
                    Term::BlankNode(b) => Some(b.clone().into()),
                    _ => None,
                };
                if let Some(o) = object
                    && seen.insert(o.clone())
                {
                    next.push(o);
                }
                let triple = Triple::new(quad.subject, quad.predicate, quad.object);
                if !kept.contains(&triple) {
                    if triples.len() == room {
                        truncated = true;
                        break 'hops;
                    }
                    kept.insert(triple.clone());
                    triples.push(triple);
                }
            }
        }
        frontier = next;
    }
    Ok(Sampled {
        triples,
        instances,
        nodes: visited,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(check_path(&["data", "a.ttl"]).unwrap(), "data/a.ttl");
        assert_eq!(check_path(&["pipeline.yaml"]).unwrap(), "pipeline.yaml");
        assert!(check_path(&["shapes", "sub", "x.ttl"]).is_ok());
        for bad in [
            &["..", "a.ttl"][..],
            &["data", ".."],
            &["data", ".hidden"],
            &["other", "a.ttl"],
            &["a.ttl"],
            &["data", "a b.ttl"],
            &["data"],
            &[],
        ] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn formats() {
        assert_eq!(format_of("data/a.ttl"), Some((RdfFormat::Turtle, false)));
        assert_eq!(format_of("data/a.nt.gz"), Some((RdfFormat::NTriples, true)));
        assert_eq!(format_of("data/a.csv"), None);
        assert!(parse("data/a.ttl", b"<urn:a> <urn:b> <urn:c> .", 100).is_ok());
        let e = parse("data/a.ttl", b"<urn:a> <urn:b> .", 100).unwrap_err();
        assert_eq!(e.0, 400);
        assert!(e.1.starts_with("data/a.ttl:"), "{}", e.1);
    }

    #[test]
    fn ids_differ() {
        let a = new_id();
        assert_eq!(a.len(), 32);
        assert_ne!(a, new_id());
    }
}
