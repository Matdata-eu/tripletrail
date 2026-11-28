//! The dataset registry and the administration endpoints under `/$/`, with Fuseki's paths
//! where Fuseki has one, so pipelines written against Fuseki's admin API keep working:
//!
//! - `GET /$/datasets`, `GET /$/datasets/{name}`: datasets and aliases;
//! - `POST /$/datasets?name=&location=[&mode=read-only|read-write]`: attach a dataset directory;
//! - `POST /$/datasets?name=&type=mem|disk` (or Fuseki's `dbName`, `dbType`): create an empty
//!   dataset, or with `from=<backup file>` one restored from a backup; `unionDefaultGraph=true`
//!   (also when attaching) makes its default graph the union of all its graphs;
//!   `entailment=rdfs|owl-rl[&entailmentGraph=]` keeps the entailment of its graphs in a named
//!   graph (`urn:tripletrail:inferred`), up to date with every write (#187); a graph that holds
//!   triples is refused (409) unless `replace=true`;
//! - `POST /$/datasets/{name}/entailment?entailment=rdfs|owl-rl|none[&entailmentGraph=]`: the same
//!   for a dataset that is attached;
//! - `POST /$/datasets?name=&alias-of=` and `POST /$/datasets/{alias}/swap?to=`: aliases;
//! - `DELETE /$/datasets/{name}[?delete-files=true]`: detach, once running requests finish;
//! - `POST /$/datasets/{name}/snapshot?location=`: save the dataset to a new directory;
//! - `POST /$/backup/{name}[?kind=nquads|store]`, `GET /$/backups-list`,
//!   `POST /$/backups/{file}/verify`, `GET /$/tasks[/{id}]`: backups, as gzipped N-Quads or
//!   as a copy of the dataset's store, which restores without parsing;
//! - `GET /$/stats[/{name}]`, `GET /$/server`: statistics;
//! - `POST /$/digest/{name}`: a task that computes the SHA-256 of the dataset's RDFC-1.0
//!   canonical N-Quads (#191), the same for isomorphic datasets; `/$/tasks/{id}` gives it as
//!   `sha256`; `POST /$/diff/{name}?to={other}`: a task that compares two datasets (two
//!   releases) the same way, with counts per graph, predicate and class as `diff`.
//!
//! With an admin token, every request that changes something here (`POST`, `PUT`, `DELETE`)
//! needs `Authorization: Bearer <token>`.

use crate::{HttpError, Params, Reply, State, json_reply};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tiny_http::{Method, Request};
use tripletrail::Engine;
use tripletrail::geo::sync::{LockUnpoisoned, RwLockUnpoisoned};
use tripletrail::oxigraph::io::{RdfFormat, RdfParser, RdfSerializer};
use tripletrail::oxigraph::model::NamedNode;
use tripletrail::store::entail::Regime;
use tripletrail::store::{Dataset, Verification};

/// Whether a dataset accepts writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    ReadWrite,
    /// Memory-mapped, never changed: updates and uploads answer 405.
    ReadOnly,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReadWrite => "read-write",
            Self::ReadOnly => "read-only",
        }
    }

    fn parse(s: &str) -> Result<Self, HttpError> {
        match s {
            "read-only" | "ro" | "readonly" => Ok(Self::ReadOnly),
            "read-write" | "rw" | "readwrite" => Ok(Self::ReadWrite),
            other => Err(HttpError::bad_request(format!(
                "mode must be read-only or read-write, not {other}"
            ))),
        }
    }
}

/// Opens the dataset directory `dir` (a store written by `tripletrail load`, `tripletrail serve
/// --location`, or saved by `Engine::save` / tripletrail's `Engine.save`). Read-write datasets
/// keep their routing networks next to the directory, in `<dir>.networks`; read-only ones
/// keep them in memory, so nothing on disk changes.
pub fn open_dataset(dir: &Path, mode: Mode) -> Result<Engine, tripletrail::Error> {
    match mode {
        Mode::ReadOnly => Engine::open_read_only(dir),
        Mode::ReadWrite => {
            let store = tripletrail::store::Store::open(dir)
                .map_err(tripletrail::store::DatasetError::from)?;
            Engine::with_network_dir(store, networks_dir(dir))
        }
    }
}

fn networks_dir(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_owned();
    name.push(".networks");
    dir.with_file_name(name)
}

/// Request counts of a dataset.
#[derive(Default)]
pub(crate) struct Stats {
    pub(crate) requests: AtomicU64,
    pub(crate) good: AtomicU64,
    pub(crate) bad: AtomicU64,
    pub(crate) queries: AtomicU64,
    pub(crate) updates: AtomicU64,
    pub(crate) reads: AtomicU64,
    pub(crate) uploads: AtomicU64,
    pub(crate) last_write: Mutex<Option<SystemTime>>,
}

/// What a request did, for the statistics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Query,
    Update,
    Read,
    Upload,
}

impl Stats {
    pub(crate) fn record(&self, operation: Option<Operation>, status: u16) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if status < 400 {
            self.good.fetch_add(1, Ordering::Relaxed);
        } else {
            self.bad.fetch_add(1, Ordering::Relaxed);
        }
        let counter = match operation {
            Some(Operation::Query) => &self.queries,
            Some(Operation::Update) => &self.updates,
            Some(Operation::Read) => &self.reads,
            Some(Operation::Upload) => &self.uploads,
            None => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        if status < 400 && matches!(operation, Some(Operation::Update | Operation::Upload)) {
            *self.last_write.lock_unpoisoned() = Some(SystemTime::now());
        }
    }
}

/// A dataset served by name. Requests hold an `Arc` of it while they run, so a dataset that is
/// detached or swapped out of an alias stays alive until they finish.
pub(crate) struct Entry {
    /// Unique in the server's life, for caches keyed by dataset.
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) engine: Engine,
    pub(crate) location: Option<PathBuf>,
    pub(crate) mode: Mode,
    pub(crate) attached: SystemTime,
    pub(crate) stats: Stats,
    /// Latencies and statuses, for `/$/metrics`.
    pub(crate) timings: crate::metrics::Timings,
    /// Running and waiting requests, for the dataset's limits.
    pub(crate) gate: crate::limits::Gate,
}

impl Entry {
    pub(crate) fn new(name: String, engine: Engine, location: Option<PathBuf>, mode: Mode) -> Self {
        static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Self {
            id: IDS.fetch_add(1, Ordering::Relaxed),
            name,
            engine,
            location,
            mode,
            attached: SystemTime::now(),
            stats: Stats::default(),
            timings: crate::metrics::Timings::default(),
            gate: crate::limits::Gate::default(),
        }
    }
}

/// A stable name for whichever dataset it points at.
#[derive(Clone)]
pub(crate) struct Alias {
    pub(crate) target: String,
    pub(crate) since: SystemTime,
    pub(crate) previous: Option<String>,
}

#[derive(Default)]
pub(crate) struct Registry {
    pub(crate) datasets: BTreeMap<String, Arc<Entry>>,
    pub(crate) aliases: BTreeMap<String, Alias>,
}

impl Registry {
    /// The dataset a name means: an alias's target, or the dataset of that name.
    pub(crate) fn resolve(&self, name: &str) -> Option<Arc<Entry>> {
        let name = self.aliases.get(name).map_or(name, |a| a.target.as_str());
        self.datasets.get(name).cloned()
    }

    /// The only dataset, if there is exactly one: its paths also work without its name.
    pub(crate) fn single(&self) -> Option<Arc<Entry>> {
        if self.datasets.len() == 1 {
            self.datasets.values().next().cloned()
        } else {
            None
        }
    }

    fn aliases_of(&self, name: &str) -> Vec<&str> {
        self.aliases
            .iter()
            .filter(|(_, a)| a.target == name)
            .map(|(k, _)| k.as_str())
            .collect()
    }
}

/// A background job (a backup), reported by `/$/tasks`.
pub(crate) struct Task {
    id: u64,
    kind: &'static str,
    dataset: String,
    started: SystemTime,
    state: Mutex<TaskState>,
}

#[derive(Default)]
struct TaskState {
    finished: Option<SystemTime>,
    success: Option<bool>,
    message: Option<String>,
    file: Option<String>,
    quads: u64,
    /// The digest a `Digest` task computed.
    sha256: Option<String>,
    /// The summary a `Diff` task computed.
    diff: Option<Value>,
}

#[derive(Default)]
pub(crate) struct Tasks {
    next: AtomicU64,
    list: Mutex<Vec<Arc<Task>>>,
}

/// Finished tasks kept for `/$/tasks`.
const TASKS_KEPT: usize = 200;

impl Tasks {
    fn start(&self, kind: &'static str, dataset: &str) -> Arc<Task> {
        let task = Arc::new(Task {
            id: self.next.fetch_add(1, Ordering::Relaxed) + 1,
            kind,
            dataset: dataset.to_owned(),
            started: SystemTime::now(),
            state: Mutex::default(),
        });
        let mut list = self.list.lock_unpoisoned();
        list.push(Arc::clone(&task));
        if list.len() > TASKS_KEPT
            && let Some(i) = list
                .iter()
                .position(|t| t.state.lock_unpoisoned().finished.is_some())
        {
            list.remove(i);
        }
        task
    }

    fn get(&self, id: u64) -> Option<Arc<Task>> {
        self.list
            .lock_unpoisoned()
            .iter()
            .find(|t| t.id == id)
            .cloned()
    }

    pub(crate) fn running(&self) -> usize {
        self.list
            .lock_unpoisoned()
            .iter()
            .filter(|t| t.state.lock_unpoisoned().finished.is_none())
            .count()
    }
}

impl Task {
    fn to_json(&self) -> Value {
        let s = self.state.lock_unpoisoned();
        let mut v = json!({
            "task": self.kind,
            "taskId": self.id.to_string(),
            "dataset": format!("/{}", self.dataset),
            "started": rfc3339(self.started),
            "quads": s.quads,
        });
        if let Some(f) = s.finished {
            v["finished"] = json!(rfc3339(f));
        }
        if let Some(ok) = s.success {
            v["success"] = json!(ok);
        }
        if let Some(m) = &s.message {
            v["message"] = json!(m);
        }
        if let Some(f) = &s.file {
            v["file"] = json!(f);
        }
        if let Some(d) = &s.sha256 {
            v["sha256"] = json!(d);
        }
        if let Some(d) = &s.diff {
            v["diff"] = d.clone();
        }
        v
    }
}

/// Seconds since the epoch as an RFC 3339 UTC timestamp, to the millisecond.
pub(crate) fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (y, m, day) = civil_from_days((secs / 86_400) as i64);
    let s = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        s / 3600,
        s / 60 % 60,
        s % 60,
        d.subsec_millis()
    )
}

/// The calendar date of a day number counted from 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// A name usable in a URL path segment and as a file name.
fn check_name(name: &str) -> Result<(), HttpError> {
    let ok = !name.is_empty()
        && name.len() <= 200
        && name != "$"
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(HttpError::bad_request(format!(
            "Invalid dataset name {name:?}: use letters, digits, '_', '-' and '.'"
        )))
    }
}

fn require<'a>(params: &'a Params, names: &[&str]) -> Result<&'a str, HttpError> {
    names
        .iter()
        .find_map(|n| params.get(n))
        .filter(|v| !v.is_empty())
        .ok_or_else(|| HttpError::bad_request(format!("Missing the {} parameter", names[0])))
}

fn truthy(params: &Params, name: &str) -> bool {
    params
        .get(name)
        .is_some_and(|v| matches!(v, "" | "true" | "1" | "yes"))
}

/// Total size of the files under `path`.
pub(crate) fn size_on_disk(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| size_on_disk(&e.path()))
                .sum::<u64>()
        })
        .unwrap_or(0)
}

const SERVICES: [(&str, &str, &[&str]); 5] = [
    ("query", "SPARQL Query", &["query", "sparql", ""]),
    ("update", "SPARQL Update", &["update", ""]),
    ("gsp-rw", "Graph Store Protocol", &["data"]),
    ("gsp-r", "Graph Store Protocol (Read)", &["get"]),
    (
        "explain",
        "Query plan with row counts and timings",
        &["explain"],
    ),
];

/// A dataset (or alias) as `/$/datasets` describes it: Fuseki's keys (`ds.name`, `ds.state`,
/// `ds.services`) and tripletrail's.
fn describe(registry: &Registry, name: &str, detail: bool) -> Option<Value> {
    if let Some(alias) = registry.aliases.get(name) {
        let target = registry.datasets.get(&alias.target);
        let mut v = json!({
            "ds.name": format!("/{name}"),
            "ds.state": target.is_some(),
            "ds.services": services(),
            "name": name,
            "alias": true,
            "target": alias.target,
            "since": rfc3339(alias.since),
            "previous": alias.previous,
        });
        if let Some(t) = target {
            v["mode"] = json!(t.mode.as_str());
        }
        return Some(v);
    }
    let entry = registry.datasets.get(name)?;
    let mut v = json!({
        "ds.name": format!("/{name}"),
        "ds.state": true,
        "ds.services": services(),
        "name": name,
        "alias": false,
        "mode": entry.mode.as_str(),
        "store": entry.engine.store().kind(),
        "location": entry.location.as_ref().map(|p| p.display().to_string()),
        "attached": rfc3339(entry.attached),
        "aliases": registry.aliases_of(name),
        "spatialIndexReady": entry.engine.spatial_index_ready(),
        "fileCheck": file_check(entry),
        "damagedFile": damaged_file(entry),
        "unionDefaultGraph": entry.engine.union_default_graph(),
        "entailment": match entry.engine.kept_entailment() {
            Some((graph, regime)) => {
                json!({"regime": tripletrail::regime_name(regime), "graph": graph.as_str()})
            }
            None => json!({"regime": "none"}),
        },
    });
    if detail {
        let store = entry.engine.store();
        v["triples"] = json!(store.len().ok());
        v["graphs"] = json!(store.named_graphs().ok().map(|g| g.len()));
    }
    Some(v)
}

/// A dataset's description for a caller who reads only some of its graphs (#203): without
/// where it is kept on disk.
fn limited(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.remove("location");
    }
    v
}

fn services() -> Value {
    Value::Array(
        SERVICES
            .iter()
            .map(|(t, d, e)| json!({"srv.type": t, "srv.description": d, "srv.endpoints": e}))
            .collect(),
    )
}

fn stats_json(state: &State, registry: &Registry, entry: &Entry) -> Value {
    let s = &entry.stats;
    let n = |c: &AtomicU64| c.load(Ordering::Relaxed);
    let store = entry.engine.store();
    let mapped = match store {
        Dataset::TripleTrail(s) => s.mapped_size(),
        Dataset::Oxigraph(_) => 0,
    };
    let swapped = registry
        .aliases
        .values()
        .filter(|a| a.target == entry.name)
        .map(|a| a.since)
        .max();
    let gate = entry.gate.counts();
    let memory = entry.engine.memory_use();
    json!({
        "Requests": n(&s.requests),
        "RequestsGood": n(&s.good),
        "RequestsBad": n(&s.bad),
        "endpoints": {
            "query": {"Requests": n(&s.queries), "operation": "query"},
            "update": {"Requests": n(&s.updates), "operation": "update"},
            "get": {"Requests": n(&s.reads), "operation": "gsp-r"},
            "data": {"Requests": n(&s.uploads), "operation": "gsp-rw"},
        },
        "queries": n(&s.queries),
        "updates": n(&s.updates),
        "graphStoreReads": n(&s.reads),
        "graphStoreWrites": n(&s.uploads),
        "triples": store.len().ok(),
        "graphs": store.named_graphs().ok().map(|g| g.len()),
        "mode": entry.mode.as_str(),
        "store": store.kind(),
        "location": entry.location.as_ref().map(|p| p.display().to_string()),
        "sizeOnDisk": entry.location.as_deref().map(size_on_disk),
        "mappedBytes": mapped,
        "attached": rfc3339(entry.attached),
        "lastWrite": s.last_write.lock_unpoisoned().map(rfc3339),
        "lastSwap": swapped.map(rfc3339),
        "aliases": registry.aliases_of(&entry.name),
        "spatialIndexReady": entry.engine.spatial_index_ready(),
        "fileCheck": file_check(entry),
        "damagedFile": damaged_file(entry),
        "running": gate.running,
        "queued": gate.queued,
        "queuedTotal": n(&entry.gate.queued_total),
        "rejectedBusy": n(&entry.gate.rejected),
        "rejectedQuota": n(&entry.gate.rejected_quota),
        "quotaEvictions": n(&entry.gate.evictions),
        "memoryBytes": memory.map(|m| m.total()),
        "memory": memory.map(|m| {
            m.parts()
                .into_iter()
                .map(|(k, v)| (k.to_owned(), json!(v)))
                .collect::<serde_json::Map<_, _>>()
        }),
        "limits": crate::limits::Limits::of(state, &entry.name).json(),
    })
}

/// Whether the caller may read the dataset or alias `name` (unknown names count as readable;
/// the caller learns nothing from them).
fn readable(
    state: &State,
    caller: Option<&crate::auth::Caller>,
    registry: &Registry,
    name: &str,
) -> bool {
    let target = registry.resolve(name).map(|e| e.name.clone());
    let mut names = vec![name];
    if let Some(t) = &target {
        names.push(t);
    }
    crate::may_read(state, caller, &names)
}

/// Whether the caller may read the dataset or alias `name`, or some of its graphs (#201):
/// such a dataset is listed, without its counts (see [`crate::may_list`]). `Some(true)`: all
/// of it.
fn listed(
    state: &State,
    caller: Option<&crate::auth::Caller>,
    registry: &Registry,
    name: &str,
) -> Option<bool> {
    let target = registry.resolve(name).map(|e| e.name.clone());
    let mut names = vec![name];
    if let Some(t) = &target {
        names.push(t);
    }
    crate::may_list(state, caller, &names).map(|view| view.is_none())
}

pub(crate) fn authorized(state: &State, request: &Request) -> bool {
    match &state.options.admin_token {
        Some(token) => bearer_is(request, token),
        None => true,
    }
}

/// Whether the request carries `Authorization: Bearer <token>`.
pub(crate) fn bearer_is(request: &Request, token: &str) -> bool {
    let given = crate::request_header(request, "Authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or_default();
    // Compared in full, not stopping at the first difference.
    given.len() == token.len()
        && given
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// A push webhook, which `…/shapes/pull` checks against the binding's secret itself.
fn webhook(request: &Request) -> bool {
    #[cfg(feature = "git")]
    return crate::shapes_git::is_webhook(request);
    #[cfg(not(feature = "git"))]
    {
        let _ = request;
        false
    }
}

/// Answers a request under `/$/` (`rest` is the path after it).
pub(crate) fn route(
    state: &Arc<State>,
    request: &mut Request,
    rest: &[&str],
    params: &mut Params,
    context: &mut crate::Context,
) -> Result<Reply, HttpError> {
    let owned_caller = context.caller.clone();
    let caller = owned_caller.as_ref();
    let method = request.method().clone();
    let changes = matches!(
        method,
        Method::Post | Method::Put | Method::Delete | Method::Patch
    ) && !matches!(
        rest,
        ["ping"]
            | ["validate", _]
            | ["datasets", _, "shapes", "sparql"]
            | ["datasets", _, "validate" | "infer"]
    ) && !(matches!(rest, ["datasets", _, "shapes", "pull"]) && webhook(request));
    // Refreshing proposals (#145) asks the forge with the server's review token and rewrites
    // the proposals: a change, for those who may propose, and one the audit log records.
    let refreshing = matches!(method, Method::Get | Method::Head)
        && matches!(rest, ["datasets", _, "shapes", "proposals"])
        && params
            .get("refresh")
            .is_some_and(|v| !matches!(v, "false" | "0"));
    if refreshing {
        context.changes = true;
    }
    let changes = changes || refreshing;
    // A key limited to some datasets sees nothing of the others, here included.
    if let Some(caller) = caller
        && caller.datasets.is_some()
        && !caller.is_admin()
        && !matches!(rest, ["ping" | "ready"] | ["role", _])
    {
        return Err(crate::auth::refuse(
            caller,
            "A key limited to datasets has no access to /$/",
        ));
    }
    if let Some(caller) = caller
        && !caller.is_admin()
    {
        // Detaching, backing up and compacting one dataset is open to that dataset's admins too; every
        // other change (new datasets, swaps, snapshots to a server path) to server admins.
        let dataset_admin = |name: &str| {
            state
                .auth
                .as_ref()
                .is_some_and(|a| a.access(caller, &[name]).admin)
        };
        let allowed = match (&method, rest) {
            (Method::Delete, ["datasets", name])
            | (Method::Post, ["backup", name])
            | (Method::Post, ["compact", name])
            | (Method::Post, ["digest", name]) => dataset_admin(name),
            (Method::Post, ["diff", name]) => {
                dataset_admin(name) && params.get("to").is_some_and(dataset_admin)
            }
            // Binding a repository names files and URLs of the server: server admins only.
            (_, ["datasets", _, "shapes", "git"]) if changes => false,
            // A dataset's shape editors propose changes (and refresh the proposals) as its
            // admins do; the review on the forge decides (#145).
            (_, ["datasets", name, "shapes", "proposals"]) if changes => {
                dataset_admin(name)
                    || state
                        .auth
                        .as_ref()
                        .is_some_and(|a| a.edits_shapes(caller, &[name]))
            }
            // A dataset's admins keep its shapes.
            (_, ["datasets", name, "shapes", ..]) if changes => dataset_admin(name),
            _ => !changes,
        };
        if !allowed {
            return Err(crate::auth::refuse(
                caller,
                "This needs the group tripletrail:admin (or, for one dataset, its admins)",
            ));
        }
        // With policies, the server-wide pages name every dataset.
        let monitor = caller.in_group(crate::auth::MONITOR);
        if !changes
            && !monitor
            && state.auth.as_ref().is_some_and(|a| a.policies_on())
            && matches!(
                rest,
                [
                    "server"
                        | "metrics"
                        | "status"
                        | "tasks"
                        | "backups-list"
                        | "limits"
                        | "license"
                ] | ["tasks", _]
            )
        {
            return Err(crate::auth::refuse(
                caller,
                "This needs the group tripletrail:admin or tripletrail:monitor",
            ));
        }
    }
    if caller.is_none() && changes && !authorized(state, request) {
        return Err(HttpError(
            401,
            "This needs the admin token: Authorization: Bearer <token>".to_owned(),
        ));
    }
    // A form body adds parameters, as in Fuseki's admin API.
    // (Not a webhook's: its signature is over the body.)
    if method == Method::Post
        && !matches!(rest, ["datasets", _, "shapes", "pull"])
        && crate::content_type(request).as_deref() == Some("application/x-www-form-urlencoded")
    {
        let body = crate::read_body(request, state.options.max_body)?;
        params
            .0
            .extend(form_urlencoded::parse(body.as_bytes()).into_owned());
    }
    let get = matches!(method, Method::Get | Method::Head);
    match rest {
        ["ping"] => Ok(Reply::bytes(200, "text/plain", b"ok\n".to_vec())),
        ["ready"] => Ok(ready(state, params.has("report"))),
        ["role", name] if get => Ok(role(state, name)),
        ["status"] => Ok(status(state)),
        #[cfg(feature = "replication")]
        ["replication", name, more @ ..] => {
            crate::replication::route(state, request, name, more, params, caller)
        }
        ["server"] if get => Ok(server_info(state)),
        #[cfg(feature = "enterprise")]
        ["license"] if get => {
            let mut license = state.options.license_status().to_json();
            let in_use = state
                .auth
                .as_ref()
                .is_some_and(crate::auth::Auth::has_row_rules);
            license["rowPolicies"] = crate::row_policies::to_json(&state.options, in_use);
            Ok(json_reply(200, &license))
        }
        ["access", more @ ..] => {
            let viewing_as = context.viewer.is_some();
            crate::access::route(state, request, more, params, caller, viewing_as)
        }
        ["metrics"] if get => Ok(crate::metrics::render(state)),
        ["limits"] if get => {
            let file = limits_file(state)?;
            Ok(json_reply(200, &file.raw()))
        }
        ["limits"] if method == Method::Put => {
            let file = limits_file(state)?;
            let body = json_body(state, request)?;
            file.replace(body).map_err(HttpError::bad_request)?;
            Ok(json_reply(200, &file.raw()))
        }
        ["limits", name] if get => {
            let registry = state.registry.read_unpoisoned();
            if !readable(state, caller, &registry, name) {
                return Err(HttpError(404, format!("No dataset {name}")));
            }
            let entry = registry
                .resolve(name)
                .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?;
            let written = state
                .limits
                .as_ref()
                .and_then(|f| f.raw().get("datasets")?.get(&entry.name).cloned());
            Ok(json_reply(
                200,
                &json!({
                    "dataset": entry.name,
                    "written": written,
                    "applied": crate::limits::Limits::of(state, &entry.name).json(),
                }),
            ))
        }
        ["limits", name] if method == Method::Put => {
            let file = limits_file(state)?;
            let body = json_body(state, request)?;
            let name = limits_name(state, name);
            file.set_dataset(&name, Some(body))
                .map_err(HttpError::bad_request)?;
            Ok(json_reply(
                200,
                &crate::limits::Limits::of(state, &name).json(),
            ))
        }
        ["limits", name] if method == Method::Delete => {
            let file = limits_file(state)?;
            file.set_dataset(&limits_name(state, name), None)
                .map_err(HttpError::bad_request)?;
            Ok(Reply::empty(204))
        }
        ["datasets"] if get => {
            let registry = state.registry.read_unpoisoned();
            let names: Vec<&String> = registry
                .datasets
                .keys()
                .chain(registry.aliases.keys())
                .collect();
            let list: Vec<Value> = names
                .iter()
                .filter_map(|n| {
                    let all = listed(state, caller, &registry, n)?;
                    describe(&registry, n, false).map(|v| if all { v } else { limited(v) })
                })
                .collect();
            Ok(json_reply(200, &json!({ "datasets": list })))
        }
        ["datasets"] if method == Method::Post => add_dataset(state, params),
        ["datasets", name] if get => {
            let registry = state.registry.read_unpoisoned();
            // A caller who reads only some graphs (#203) gets no counts of the whole dataset.
            let Some(all) = listed(state, caller, &registry, name) else {
                return Err(HttpError(404, format!("No dataset {name}")));
            };
            #[cfg_attr(not(feature = "replication"), allow(unused_mut))]
            let mut v = describe(&registry, name, all)
                .map(|v| if all { v } else { limited(v) })
                .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?;
            #[cfg(feature = "replication")]
            if all
                && let Some(entry) = registry.resolve(name)
                && let Some(replica) = state.replicas.lock_unpoisoned().get(&entry.name)
            {
                v["replica"] = replica.json();
            }
            Ok(json_reply(200, &v))
        }
        // What the dataset contains, for generated shapes (#137).
        ["datasets", name, "profile"] if get => {
            let entry = {
                let registry = state.registry.read_unpoisoned();
                if !readable(state, caller, &registry, name) {
                    return Err(HttpError(404, format!("No dataset {name}")));
                }
                registry
                    .resolve(name)
                    .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?
            };
            crate::profile::reply(state, &entry, params).map(|v| json_reply(200, &v))
        }
        // The shapes a reader reads, and the dataset's own set (#137).
        ["datasets", name, "shapes", more @ ..] => {
            let entry = {
                let registry = state.registry.read_unpoisoned();
                if !readable(state, caller, &registry, name) {
                    return Err(HttpError(404, format!("No dataset {name}")));
                }
                registry
                    .resolve(name)
                    .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?
            };
            match more {
                [] if get => crate::shapes::reply(state, &entry, request, params),
                [] => Err(HttpError(405, "The shapes are read with GET".to_owned())),
                // A pack from git names files and URLs of the server, as a binding does:
                // only server admins may install one (#148).
                more => {
                    let server_admin = caller.is_none_or(crate::auth::Caller::is_admin);
                    let by = caller.and_then(|c| c.agent.clone());
                    crate::shapes::route(
                        state,
                        &entry,
                        request,
                        more,
                        params,
                        server_admin,
                        by.as_deref(),
                    )
                }
            }
        }
        // SHACL validation and inference on the dataset, reading only (#401).
        ["datasets", name, op @ ("validate" | "infer")] if get || method == Method::Post => {
            let entry = {
                let registry = state.registry.read_unpoisoned();
                if !readable(state, caller, &registry, name) {
                    return Err(HttpError(404, format!("No dataset {name}")));
                }
                registry
                    .resolve(name)
                    .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?
            };
            let shapes = params
                .get("shapes")
                .ok_or_else(|| HttpError::bad_request("Give the shapes graph: shapes=<iri>"))?;
            let shapes = tripletrail::oxigraph::model::NamedNode::new(shapes)
                .map_err(|e| HttpError::bad_request(format!("shapes: {e}")))?;
            let data = match params.get("graph") {
                None | Some("default") => tripletrail::oxigraph::model::GraphName::DefaultGraph,
                Some(g) => tripletrail::oxigraph::model::NamedNode::new(g)
                    .map_err(|e| HttpError::bad_request(format!("graph: {e}")))?
                    .into(),
            };
            let asked = params
                .get("timeout")
                .map(crate::parse_timeout)
                .transpose()?;
            // A read of the dataset: it takes a turn under the dataset's limits, like a query.
            crate::use_dataset(state, context, &entry)?;
            context.operation = Some(Operation::Read);
            let target = crate::reason::Target {
                engine: &entry.engine,
                shapes,
                data,
                timeout: context.limits.timeout(asked),
            };
            if *op == "validate" {
                crate::reason::validate(request, params, &target)
            } else {
                crate::reason::infer(request, params, &target, false)
            }
        }
        ["datasets", name] if method == Method::Delete => detach(state, name, params),
        ["datasets", name, "swap"] if method == Method::Post => swap(state, name, params),
        ["datasets", name, "snapshot"] if method == Method::Post => snapshot(state, name, params),
        ["datasets", name, "entailment"] if method == Method::Post => {
            entailment(state, name, params)
        }
        ["backup", name] if method == Method::Post => backup(state, name, params),
        ["compact", name] if method == Method::Post => compact(state, name),
        ["sleep"] if method == Method::Post => sleep(state, params),
        ["logs"] if get => Ok(Reply::bytes(
            200,
            "text/plain; charset=utf-8",
            b"TripleTrail writes its log to stderr, and its request log with --request-log; see /$/metrics and /$/status for its state.\n".to_vec(),
        )),
        ["validate", kind] if get || method == Method::Post => {
            crate::validators::route(kind, params)
        }
        ["digest", name] if method == Method::Post => digest(state, name),
        ["diff", name] if method == Method::Post => diff(state, name, params),
        ["backups-list"] if get => backups_list(state),
        ["backups", file, "verify"] if method == Method::Post => verify_backup(state, file),
        ["tasks"] if get => {
            let tasks: Vec<Value> = state
                .tasks
                .list
                .lock_unpoisoned()
                .iter()
                .map(|t| t.to_json())
                .collect();
            Ok(json_reply(200, &Value::Array(tasks)))
        }
        ["tasks", id] if get => {
            let task = id
                .parse::<u64>()
                .ok()
                .and_then(|id| state.tasks.get(id))
                .ok_or_else(|| HttpError(404, format!("No task {id}")))?;
            Ok(json_reply(200, &task.to_json()))
        }
        ["stats"] if get => {
            let registry = state.registry.read_unpoisoned();
            let datasets: serde_json::Map<String, Value> = registry
                .datasets
                .values()
                .filter(|e| readable(state, caller, &registry, &e.name))
                .map(|e| (format!("/{}", e.name), stats_json(state, &registry, e)))
                .collect();
            Ok(json_reply(200, &json!({ "datasets": datasets })))
        }
        ["stats", name] if get => {
            let registry = state.registry.read_unpoisoned();
            let entry = registry
                .resolve(name)
                .filter(|_| readable(state, caller, &registry, name))
                .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?;
            let mut datasets = serde_json::Map::new();
            datasets.insert(format!("/{name}"), stats_json(state, &registry, &entry));
            Ok(json_reply(200, &json!({ "datasets": datasets })))
        }
        ["ui", ..] if get => crate::ui::serve(state, request, &rest[1..]),
        _ => Err(HttpError(
            if get || method == Method::Post || method == Method::Delete {
                404
            } else {
                405
            },
            format!("No admin endpoint {method} /$/{}", rest.join("/")),
        )),
    }
}

/// Where the check of a dataset's store files stands (#436, #440): "checking", "verified",
/// "damaged" or "stopped"; `None` for Oxigraph's store, which is not checked this way.
fn file_check(entry: &Entry) -> Option<&'static str> {
    Some(match entry.engine.store().verification()? {
        Verification::Checking => "checking",
        Verification::Verified => "verified",
        Verification::Damaged(_) => "damaged",
        Verification::Stopped => "stopped",
    })
}

/// The store file of a dataset that failed its check, if one did.
fn damaged_file(entry: &Entry) -> Option<String> {
    match entry.engine.store().verification()? {
        Verification::Damaged(file) => Some(file),
        _ => None,
    }
}

/// `/$/ready`: 200 once the server has started and every dataset's spatial index is built.
/// A dataset whose files failed their check does not hold readiness back (it answers
/// queries, and its index may never build): it is listed under `damaged` and reported per
/// dataset with the file. The check of a large store's files runs in the background and does
/// not hold readiness back either.
/// `GET /$/role/{name}` (#217), open like `/$/ready`, for a load balancer to send writes to
/// the server that takes them: 200 `leader` when this server takes writes to the dataset
/// (and serves its log to replicas, when on disk), 503 `follower` for a read replica and
/// `read-only` for a dataset served read-only, 404 for no such dataset.
fn role(state: &State, name: &str) -> Reply {
    let Some(entry) = state.registry.read_unpoisoned().resolve(name) else {
        return Reply::bytes(404, "text/plain", b"no such dataset\n".to_vec());
    };
    #[cfg(feature = "replication")]
    let follower = state.replicas.lock_unpoisoned().contains_key(&entry.name);
    #[cfg(not(feature = "replication"))]
    let follower = false;
    let (status, role): (u16, &[u8]) = if follower {
        (503, b"follower\n")
    } else if entry.mode == Mode::ReadOnly {
        (503, b"read-only\n")
    } else {
        (200, b"leader\n")
    };
    Reply::bytes(status, "text/plain", role.to_vec())
}

/// `/$/ready`: 200 when every dataset is open and indexed, 503 before. With `report` it answers
/// 200 either way, with the same body, for a page that shows the state: a browser logs a 503
/// as a failed request (#420).
fn ready(state: &State, report: bool) -> Reply {
    let registry = state.registry.read_unpoisoned();
    let starting = state.starting.load(Ordering::Relaxed);
    let stopping = state.stopping.load(Ordering::Relaxed);
    let datasets: Vec<(&String, bool, Option<&'static str>, Option<String>)> = registry
        .datasets
        .iter()
        .map(|(k, e)| {
            (
                k,
                e.engine.spatial_index_ready(),
                file_check(e),
                damaged_file(e),
            )
        })
        .collect();
    // A read replica not served yet (its first base still being copied).
    #[cfg(feature = "replication")]
    let copying: Vec<String> = state
        .replicas
        .lock_unpoisoned()
        .iter()
        .filter(|(_, r)| !r.serving())
        .map(|(k, _)| k.clone())
        .collect();
    #[cfg(not(feature = "replication"))]
    let copying: Vec<String> = Vec::new();
    let ready = !starting
        && !stopping
        && copying.is_empty()
        && datasets
            .iter()
            .all(|(_, index, _, damaged)| *index || damaged.is_some());
    let damaged: Vec<&String> = datasets
        .iter()
        .filter(|(.., d)| d.is_some())
        .map(|(k, ..)| *k)
        .collect();
    let body = json!({
        "ready": ready,
        "starting": starting,
        "stopping": stopping,
        "damaged": damaged,
        "copying": copying,
        "datasets": datasets
            .iter()
            .map(|(k, index, check, damaged)| {
                let mut v = json!({"spatial_index": index});
                if let Some(check) = check {
                    v["files"] = json!(check);
                }
                if let Some(file) = damaged {
                    v["damaged_file"] = json!(file);
                }
                ((*k).clone(), v)
            })
            .collect::<serde_json::Map<_, _>>(),
    });
    json_reply(if ready || report { 200 } else { 503 }, &body)
}

/// `/$/status`: whether each dataset's spatial index is built (always 200).
fn status(state: &State) -> Reply {
    let registry = state.registry.read_unpoisoned();
    let datasets: Vec<(String, bool)> = registry
        .datasets
        .iter()
        .map(|(k, e)| (k.clone(), e.engine.spatial_index_ready()))
        .collect();
    // Written by hand to keep the key order clients of earlier versions match on.
    #[cfg(feature = "replication")]
    let replicas = state.replicas.lock_unpoisoned().clone();
    let fields: Vec<String> = datasets
        .iter()
        .map(|(k, r)| {
            #[cfg(feature = "replication")]
            if let Some(replica) = replicas.get(k) {
                return format!(
                    "{}:{{\"spatial_index\":{r},\"replica\":{}}}",
                    json!(k),
                    replica.json()
                );
            }
            format!("{}:{{\"spatial_index\":{r}}}", json!(k))
        })
        .collect();
    let json = format!(
        "{{\"ready\":{},\"datasets\":{{{}}}}}",
        !state.starting.load(Ordering::Relaxed) && datasets.iter().all(|(_, r)| *r),
        fields.join(",")
    );
    Reply::bytes(200, "application/json", json.into_bytes())
}

fn server_info(state: &State) -> Reply {
    let registry = state.registry.read_unpoisoned();
    let datasets: Vec<Value> = registry
        .datasets
        .keys()
        .chain(registry.aliases.keys())
        .filter_map(|n| describe(&registry, n, false))
        .collect();
    json_reply(
        200,
        &json!({
            "version": env!("CARGO_PKG_VERSION"),
            "built": "tripletrail",
            "edition": crate::EDITION,
            "startDateTime": rfc3339(state.started),
            "uptime": state.started.elapsed().map(|d| d.as_secs()).unwrap_or(0),
            "timeoutSeconds": state.options.timeout.map(|t| t.as_secs_f64()),
            "adminToken": state.options.admin_token.is_some(),
            "backupDir": state.options.backup_dir.as_ref().map(|p| p.display().to_string()),
            "location": state.options.location.as_ref().map(|p| p.display().to_string()),
            "configFile": state.options.config_file.as_ref().map(|p| p.display().to_string()),
            "runningTasks": state.tasks.running(),
            "datasets": datasets,
        }),
    )
}

/// Attaches a directory, creates an empty dataset, restores one from a backup, or adds an
/// alias, depending on the parameters.
fn add_dataset(state: &State, params: &Params) -> Result<Reply, HttpError> {
    // The directory of a new disk dataset, removed again (with its networks) if the request
    // fails after creating it, so that a retry does not find it "exists already". Dropped
    // after the engine, which is declared after it.
    let mut created = Created::default();
    let name = require(params, &["name", "dbName"])?;
    let name = name.strip_prefix('/').unwrap_or(name).to_owned();
    check_name(&name)?;
    // The settings are read before anything is created, so that a refused request leaves no
    // directory behind.
    let union = match params
        .get("unionDefaultGraph")
        .or_else(|| params.get("union-default-graph"))
    {
        None => None,
        Some(u) => Some(match u.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => true,
            "false" | "0" | "no" => false,
            other => {
                return Err(HttpError::bad_request(format!(
                    "unionDefaultGraph must be true or false, not {other}"
                )));
            }
        }),
    };
    let entailment = entailment_setting(params)?;
    {
        let registry = state.registry.read_unpoisoned();
        if registry.datasets.contains_key(&name) || registry.aliases.contains_key(&name) {
            return Err(HttpError(
                409,
                format!("A dataset or alias {name} exists already"),
            ));
        }
    }
    if let Some(target) = params.get("alias-of").or_else(|| params.get("aliasOf")) {
        return set_alias(state, &name, target, false);
    }
    let mode = params.get("mode").map(Mode::parse).transpose()?;
    let (engine, location, mode) = if let Some(location) = params.get("location") {
        let dir = PathBuf::from(location);
        if !dir.join("CURRENT").is_file() && mode == Some(Mode::ReadOnly) {
            return Err(HttpError(
                404,
                format!("No dataset directory at {location}"),
            ));
        }
        let mode = mode.unwrap_or_default();
        let engine = open_dataset(&dir, mode)?;
        (engine, Some(dir.to_path_buf()), mode)
    } else {
        if mode == Some(Mode::ReadOnly) {
            return Err(HttpError::bad_request(
                "A new dataset starts empty, so it cannot be read-only; attach a directory with location=",
            ));
        }
        let kind = params
            .get("type")
            .or_else(|| params.get("dbType"))
            .unwrap_or("mem")
            .to_ascii_lowercase();
        let restores_a_store = matches!(kind.as_str(), "disk" | "tdb" | "tdb1" | "tdb2")
            && params.get("from").is_some_and(is_store_backup);
        if params.get("until").is_some() && !restores_a_store {
            return Err(HttpError::bad_request(
                "until= restores a store backup (from=<name>.store) into a type=disk dataset",
            ));
        }
        let (engine, location) = match kind.as_str() {
            "mem" | "memory" => (Engine::new_tripletrail(), None),
            "disk" | "tdb" | "tdb1" | "tdb2" => {
                let base = state.options.location.as_ref().ok_or_else(|| {
                    HttpError::bad_request(
                        "On-disk datasets need the server's --location; or attach a directory with location=",
                    )
                })?;
                let dir = base.join(&name);
                // Claimed for this request until it ends: a concurrent create of the same
                // name answers 409 here rather than racing it (and cleaning up its directory).
                let claim = Claim::new(&dir)
                    .ok_or_else(|| HttpError(409, format!("{} is being created", dir.display())))?;
                if dir.exists() {
                    return Err(HttpError(409, format!("{} exists already", dir.display())));
                }
                created.claim = Some(claim);
                created.dir = Some(dir.to_path_buf());
                // A store backup becomes the dataset's directory as it is: nothing to parse.
                // With until=, the archived write-ahead logs are replayed onto it.
                if let Some(file) = params.get("from").filter(|f| is_store_backup(f)) {
                    match params.get("until") {
                        Some(until) => restore_until(state, file, until, &dir)?,
                        None => restore_store(state, file, &dir)?,
                    }
                }
                (
                    open_dataset(&dir, Mode::ReadWrite)?,
                    Some(dir.to_path_buf()),
                )
            }
            other => {
                return Err(HttpError::bad_request(format!(
                    "type must be mem or disk, not {other}"
                )));
            }
        };
        if let Some(file) = params.get("from")
            && !(location.is_some() && is_store_backup(file))
        {
            restore(state, &engine, file)?;
        } else if params.get("from").is_some() {
            // The store backup is the dataset's data, written without the engine: nothing
            // derived from it exists yet, and the audit log (#207) learns its quads are not
            // counted.
            engine.invalidate();
        }
        (engine, location, Mode::ReadWrite)
    };
    engine.set_indexed_predicates(state.options.indexed.clone());
    if let Some(union) = union {
        engine.set_union_default_graph(union);
    }
    if let Some(kept) = entailment {
        if kept.is_some() && mode == Mode::ReadOnly {
            return Err(HttpError(
                409,
                "entailment needs a writable dataset".to_owned(),
            ));
        }
        let into = kept.as_ref().map(|(g, _)| g);
        check_replace(&engine, into, params)?;
        match kept {
            Some((into, regime)) => engine.set_entailment(Some(into), regime)?,
            None => engine.set_rdfs_entailment(None)?,
        };
    }
    engine.warm_up();
    let mut registry = state.registry.write_unpoisoned();
    if registry.datasets.contains_key(&name) || registry.aliases.contains_key(&name) {
        return Err(HttpError(
            409,
            format!("A dataset or alias {name} exists already"),
        ));
    }
    registry.datasets.insert(
        name.clone(),
        Arc::new(Entry::new(
            name.clone(),
            state.prepare(&name, engine),
            location,
            mode,
        )),
    );
    created.dir = None;
    let v = describe(&registry, &name, false);
    Ok(json_reply(200, &v.unwrap_or(Value::Null)))
}

/// See `add_dataset`: the directory a create made, removed on drop unless the create
/// succeeded (`dir` taken), and the claim on it, released after.
#[derive(Default)]
struct Created {
    dir: Option<PathBuf>,
    claim: Option<Claim>,
}

impl Drop for Created {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_dir_all(networks_dir(&dir));
        }
        // `claim` is dropped after this: the name is free only once the cleanup is done.
    }
}

/// The dataset directories requests are creating now.
static CREATING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// A dataset directory one request is creating.
struct Claim(PathBuf);

impl Claim {
    /// `None` when another request is creating it.
    fn new(dir: &Path) -> Option<Self> {
        let mut creating = CREATING.lock_unpoisoned();
        if creating.iter().any(|d| d == dir) {
            return None;
        }
        creating.push(dir.to_owned());
        Some(Self(dir.to_owned()))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        CREATING.lock_unpoisoned().retain(|d| *d != self.0);
    }
}

fn backup_dir(state: &State) -> Result<&Path, HttpError> {
    state
        .options
        .backup_dir
        .as_deref()
        .ok_or_else(|| HttpError::bad_request("The server has no backup directory (--backup-dir)"))
}

/// The path of a backup of the backup directory, by the name `/$/backups-list` gives.
fn backup_path(state: &State, file: &str) -> Result<PathBuf, HttpError> {
    if file.contains(['/', '\\']) || file.starts_with('.') || file.is_empty() {
        return Err(HttpError::bad_request(format!(
            "from= takes a file name of /$/backups-list, not {file}"
        )));
    }
    Ok(backup_dir(state)?.join(file))
}

/// Whether a backup is a copy of a store (a directory), rather than an RDF file.
fn is_store_backup(file: &str) -> bool {
    file.ends_with(STORE_SUFFIX)
}

/// The name ending of store backups.
const STORE_SUFFIX: &str = ".store";

/// Copies a store backup to `dir`, a new on-disk dataset's directory (which must not exist):
/// the files are copied, never linked, so the dataset's writes leave the backup unchanged.
fn restore_store(state: &State, file: &str, dir: &Path) -> Result<(), HttpError> {
    let from = backup_path(state, file)?;
    if !from.join("CURRENT").is_file() {
        return Err(HttpError(404, format!("No store backup {file}")));
    }
    tripletrail::store::Store::copy_dir(&from, dir)
        .map_err(|e| HttpError(500, format!("Restoring {file}: {e}")))?;
    Ok(())
}

/// Restores a store backup into `dir` with the write-ahead logs written since replayed onto
/// it, up to `until` (an RFC 3339 time, or `latest`). The logs are read from the archive
/// (`--wal-archive`) of the backup's dataset, then from that dataset's own directory when it is
/// attached (its current log).
fn restore_until(state: &State, file: &str, until: &str, dir: &Path) -> Result<(), HttpError> {
    let base = backup_path(state, file)?;
    let cutoff = match until {
        "latest" | "now" => None,
        _ => Some(parse_time(until).ok_or_else(|| {
            HttpError::bad_request(format!(
                "until must be an RFC 3339 time such as 2026-11-10T08:52:13Z, or latest, not {until}"
            ))
        })?),
    };
    let dataset = BackupName::parse(file)
        .map(|b| b.dataset.to_owned())
        .ok_or_else(|| HttpError::bad_request(format!("{file} is not named as a backup")))?;
    let mut logs = Vec::new();
    if let Some(archive) = &state.options.wal_archive {
        logs.push(archive.join(&dataset));
    }
    if let Some(location) = state
        .registry
        .read_unpoisoned()
        .datasets
        .get(&dataset)
        .and_then(|e| e.location.clone())
    {
        logs.push(location);
    }
    if logs.is_empty() {
        return Err(HttpError(
            409,
            format!(
                "No write-ahead logs of {dataset} to replay: start the server with --wal-archive"
            ),
        ));
    }
    tripletrail::store::Store::restore(&base, &logs, cutoff, dir).map_err(|e| match e {
        tripletrail::store::StoreError::Unsupported(m) => HttpError(409, m),
        e => HttpError(500, format!("Restoring {file}: {e}")),
    })?;
    Ok(())
}

/// An RFC 3339 time (`2026-11-10T08:52:13Z`, `2026-11-10T09:52:13.250+01:00`).
pub(crate) fn parse_time(text: &str) -> Option<SystemTime> {
    let b = text.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let s = text.get(r)?;
        s.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| s.parse().ok())?
    };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let padded: String = frac[..digits]
            .chars()
            .chain("000".chars())
            .take(3)
            .collect();
        millis = padded.parse().ok()?;
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let o = rest.get(1..)?;
            if o.len() != 5 || o.as_bytes()[2] != b':' {
                return None;
            }
            let oh: i64 = o[..2].parse().ok()?;
            let om: i64 = o[3..].parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    let days = days_from_civil(y, mo as u32, d as u32);
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset;
    let millis = secs.checked_mul(1000)? + millis;
    u64::try_from(millis)
        .ok()
        .map(|m| UNIX_EPOCH + Duration::from_millis(m))
}

/// The day number counted from 1970-01-01 of a calendar date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Loads a backup of the backup directory into `engine`: gzipped N-Quads (or any RDF format
/// by its extension), or the quads of a store backup.
fn restore(state: &State, engine: &Engine, file: &str) -> Result<(), HttpError> {
    let path = backup_path(state, file)?;
    if is_store_backup(file) {
        let backup = Engine::open_read_only(&path)
            .map_err(|e| HttpError(404, format!("No store backup {file}: {e}")))?;
        let mut quads = backup.store().iter();
        let mut error = None;
        let batches = std::iter::from_fn(|| {
            let mut batch = Vec::with_capacity(1 << 16);
            for q in quads.by_ref() {
                match q {
                    Ok(q) => batch.push(q),
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
                if batch.len() == batch.capacity() {
                    break;
                }
            }
            (!batch.is_empty()).then_some(batch)
        });
        for batch in batches {
            engine.load_quads(batch)?;
        }
        if let Some(e) = error {
            return Err(HttpError(500, format!("Reading {file}: {e}")));
        }
        return Ok(());
    }
    let input =
        std::fs::File::open(&path).map_err(|e| HttpError(404, format!("No backup {file}: {e}")))?;
    let input = std::io::BufReader::with_capacity(1 << 20, input);
    let (stem, gz) = match file.strip_suffix(".gz") {
        Some(stem) => (stem, true),
        None => (file, false),
    };
    let format = Path::new(stem)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(RdfFormat::from_extension)
        .unwrap_or(RdfFormat::NQuads);
    if gz {
        engine.load(format, flate2::read::MultiGzDecoder::new(input))?;
    } else {
        engine.load(format, input)?;
    }
    Ok(())
}

fn set_alias(state: &State, alias: &str, target: &str, replace: bool) -> Result<Reply, HttpError> {
    let target = target.strip_prefix('/').unwrap_or(target);
    let mut registry = state.registry.write_unpoisoned();
    if registry.datasets.contains_key(alias) {
        return Err(HttpError(
            409,
            format!("{alias} is a dataset, not an alias; detach it or pick another alias name"),
        ));
    }
    if !registry.datasets.contains_key(target) {
        return Err(HttpError(
            404,
            format!("No dataset {target} (an alias points at a dataset, not at another alias)"),
        ));
    }
    if !replace && registry.aliases.contains_key(alias) {
        return Err(HttpError(409, format!("The alias {alias} exists already")));
    }
    let previous = registry
        .aliases
        .get(alias)
        .map(|a| a.target.clone())
        .filter(|p| p != target);
    registry.aliases.insert(
        alias.to_owned(),
        Alias {
            target: target.to_owned(),
            since: SystemTime::now(),
            previous: previous.clone(),
        },
    );
    let v = describe(&registry, alias, false).unwrap_or(Value::Null);
    Ok(json_reply(200, &v))
}

/// Points an alias at another dataset (creating the alias if needed). New requests on the
/// alias go to the new target at once; requests already running finish on the old one. With
/// `release=true` the old target is detached once they have.
fn swap(state: &Arc<State>, alias: &str, params: &Params) -> Result<Reply, HttpError> {
    let target = require(params, &["to", "target"])?;
    let reply = set_alias(state, alias, target, true)?;
    carry_shapes(state, alias, target);
    if truthy(params, "release") {
        let previous = state
            .registry
            .read_unpoisoned()
            .aliases
            .get(alias)
            .and_then(|a| a.previous.clone());
        if let Some(previous) = previous {
            let state = Arc::clone(state);
            let delete = truthy(params, "delete-files");
            thread::spawn(move || {
                if let Err(HttpError(_, e)) = detach_now(&state, &previous, delete, false) {
                    eprintln!("releasing {previous} after a swap: {e}");
                }
            });
        }
    }
    Ok(reply)
}

/// After a swap, gives the new target the old one's shapes when it has none of its own, so
/// the readers of the alias keep theirs.
fn carry_shapes(state: &State, alias: &str, target: &str) {
    let (previous, next) = {
        let registry = state.registry.read_unpoisoned();
        let previous = registry
            .aliases
            .get(alias)
            .and_then(|a| a.previous.clone())
            .and_then(|p| registry.datasets.get(&p).cloned());
        (previous, registry.datasets.get(target).cloned())
    };
    let (Some(previous), Some(next)) = (previous, next) else {
        return;
    };
    if let Some(from) = crate::shapes::own_folder(state, &previous)
        && crate::shapes::own_folder(state, &next).is_none()
        && let Some(to) = crate::shapes::folder_of(state, &next)
    {
        let _ = std::fs::remove_dir_all(&to);
        if let Err(e) = crate::shapes::copy_folder(&from, &to) {
            eprintln!(
                "carrying the shapes of {} to {}: {e}",
                previous.name, next.name
            );
        }
    }
}

/// How long a detach waits for the requests running on the dataset.
const DETACH_WAIT: Duration = Duration::from_secs(3600);

fn detach(state: &Arc<State>, name: &str, params: &Params) -> Result<Reply, HttpError> {
    {
        let mut registry = state.registry.write_unpoisoned();
        if registry.aliases.remove(name).is_some() {
            return Ok(json_reply(200, &json!({"removed": name, "alias": true})));
        }
    }
    let force = truthy(params, "force");
    detach_now(state, name, truthy(params, "delete-files"), !force)?;
    Ok(json_reply(200, &json!({"removed": name, "alias": false})))
}

/// Takes `name` out of the registry, waits until no request holds it, and deletes its
/// directory if asked. With `keep_aliased`, refuses while an alias points at it.
fn detach_now(
    state: &Arc<State>,
    name: &str,
    delete_files: bool,
    keep_aliased: bool,
) -> Result<(), HttpError> {
    let entry = {
        let mut registry = state.registry.write_unpoisoned();
        let aliases: Vec<String> = registry
            .aliases_of(name)
            .into_iter()
            .map(str::to_owned)
            .collect();
        if keep_aliased && !aliases.is_empty() {
            return Err(HttpError(
                409,
                format!(
                    "{name} is the target of {}: swap the alias first, or add force=true",
                    aliases.join(", ")
                ),
            ));
        }
        for a in &aliases {
            registry.aliases.remove(a);
        }
        registry
            .datasets
            .remove(name)
            .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?
    };
    // Running requests hold the entry; wait for them.
    let started = Instant::now();
    while Arc::strong_count(&entry) > 1 && started.elapsed() < DETACH_WAIT {
        thread::sleep(Duration::from_millis(20));
    }
    if let Dataset::TripleTrail(store) = entry.engine.store()
        && entry.mode == Mode::ReadWrite
    {
        let _ = store.checkpoint();
    }
    let location = entry.location.clone();
    drop(entry);
    // An in-memory dataset's shapes go with it, so a later dataset of that name starts afresh.
    if location.is_none()
        && let Some(shapes) = crate::shapes::memory_folder(state, name)
    {
        let _ = std::fs::remove_dir_all(shapes);
    }
    if delete_files && let Some(dir) = location {
        // Only a store directory, never an arbitrary one.
        if dir.join("CURRENT").is_file()
            || std::fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_none())
        {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| HttpError(500, format!("Removing {}: {e}", dir.display())))?;
            let _ = std::fs::remove_dir_all(networks_dir(&dir));
        } else {
            return Err(HttpError(
                409,
                format!(
                    "{} does not look like a dataset; left in place",
                    dir.display()
                ),
            ));
        }
    }
    Ok(())
}

/// `entailment=rdfs|none` and `entailmentGraph=`: the graph to keep the RDFS entailment in
/// (`Some(None)`: none), or `None` when the request does not say.
fn entailment_setting(params: &Params) -> Result<Option<Option<Kept>>, HttpError> {
    let Some(regime) = params.get("entailment") else {
        return Ok(None);
    };
    let regime = match regime.to_ascii_lowercase().as_str() {
        "none" => return Ok(Some(None)),
        "rdfs" => Regime::Rdfs,
        "owl-rl" => Regime::OwlRl,
        other => {
            return Err(HttpError::bad_request(format!(
                "entailment must be rdfs, owl-rl or none, not {other}"
            )));
        }
    };
    let graph = params
        .get("entailmentGraph")
        .unwrap_or("urn:tripletrail:inferred");
    NamedNode::new(graph)
        .map(|g| Some(Some((g, regime))))
        .map_err(|e| HttpError::bad_request(format!("entailmentGraph {graph:?}: {e}")))
}

/// A kept entailment: the graph it is kept in and its rules.
type Kept = (NamedNode, Regime);

/// Refuses (409) to keep the entailment in a graph that holds triples and is not the graph
/// it is kept in now, unless `replace=true`: entailing replaces the graph's content, which a
/// typo in `entailmentGraph=` would otherwise lose without a word. After a restart, a graph
/// that holds the old entailment is passed with `replace=true`.
fn check_replace(
    engine: &Engine,
    into: Option<&NamedNode>,
    params: &Params,
) -> Result<(), HttpError> {
    let Some(into) = into else {
        return Ok(());
    };
    if engine.rdfs_entailment().as_ref() == Some(into)
        || params
            .get("replace")
            .is_some_and(|r| matches!(r.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
    {
        return Ok(());
    }
    let held = engine
        .store()
        .quads_for_pattern(None, None, None, Some(into.as_ref().into()))
        .next()
        .is_some();
    if held {
        return Err(HttpError(
            409,
            format!(
                "Graph <{}> holds triples, which keeping the entailment there would replace; \
                 give replace=true to replace them, or another entailmentGraph",
                into.as_str()
            ),
        ));
    }
    Ok(())
}

/// `POST /$/datasets/{name}/entailment`: starts (entailing once) or stops keeping the RDFS or OWL 2 RL
/// entailment of an attached dataset. Moving it to another graph empties the one it was kept
/// in; stopping leaves the graph as it is, as data.
fn entailment(state: &State, name: &str, params: &Params) -> Result<Reply, HttpError> {
    let kept = entailment_setting(params)?
        .ok_or_else(|| HttpError::bad_request("entailment=rdfs, owl-rl or none is required"))?;
    let into = kept.as_ref().map(|(g, _)| g.clone());
    let entry = state
        .registry
        .read_unpoisoned()
        .resolve(name)
        .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?;
    if into.is_some() && entry.mode == Mode::ReadOnly {
        return Err(HttpError(
            409,
            format!("Dataset {name} is read-only: an entailment writes"),
        ));
    }
    check_replace(&entry.engine, into.as_ref(), params)?;
    let started = Instant::now();
    let done = match &kept {
        Some((g, regime)) => entry.engine.set_entailment(Some(g.clone()), *regime)?,
        None => entry.engine.set_rdfs_entailment(None)?,
    };
    Ok(json_reply(
        200,
        &json!({
            "dataset": entry.name,
            "entailment": kept.as_ref().map_or("none", |(_, r)| tripletrail::regime_name(*r)),
            "graph": into.as_ref().map(NamedNode::as_str),
            "triples": done.map(|d| d.triples),
            "added": done.map(|d| d.added),
            "removed": done.map(|d| d.removed),
            "seconds": started.elapsed().as_secs_f64(),
        }),
    ))
}

fn snapshot(state: &State, name: &str, params: &Params) -> Result<Reply, HttpError> {
    let location = require(params, &["location"])?;
    let entry = state
        .registry
        .read_unpoisoned()
        .resolve(name)
        .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?;
    let started = Instant::now();
    entry.engine.save(location).map_err(|e| match e {
        tripletrail::Error::Dataset(tripletrail::store::DatasetError::Store(
            tripletrail::store::StoreError::Unsupported(m),
        )) => HttpError(409, m),
        e => e.into(),
    })?;
    // The shapes go with the data.
    if let Some(shapes) = crate::shapes::own_folder(state, &entry) {
        crate::shapes::copy_folder(&shapes, &Path::new(location).join("shapes"))?;
    }
    Ok(json_reply(
        200,
        &json!({
            "dataset": entry.name,
            "location": location,
            "seconds": started.elapsed().as_secs_f64(),
        }),
    ))
}

/// Starts a backup of `name` to the backup directory as a background task. `kind=nquads`
/// (the default) writes gzipped N-Quads of the dataset as it is when the request arrives
/// (later writes are not in it): the portable export. `kind=store` writes a copy of the
/// dataset's store (a checkpoint and an empty log, as `snapshot` does) as the dataset is when
/// the task starts; writes wait while it is written, and restoring it parses nothing.
fn backup(state: &State, name: &str, params: &Params) -> Result<Reply, HttpError> {
    let store = match params.get("kind").map(str::to_ascii_lowercase).as_deref() {
        None | Some("nquads" | "nq" | "n-quads") => false,
        Some("store") => true,
        Some(other) => {
            return Err(HttpError::bad_request(format!(
                "kind must be nquads or store, not {other}"
            )));
        }
    };
    let dir = backup_dir(state)?.to_owned();
    let entry = state
        .registry
        .read_unpoisoned()
        .resolve(name)
        .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?;
    std::fs::create_dir_all(&dir)?;
    let task = state.tasks.start("Backup", &entry.name);
    let stamp = rfc3339(task.started)
        .replace(':', "-")
        .replace('T', "_")
        .chars()
        .take(19)
        .collect::<String>();
    let file = if store {
        format!("{}_{stamp}{STORE_SUFFIX}", entry.name)
    } else {
        format!("{}_{stamp}.nq.gz", entry.name)
    };
    task.state.lock_unpoisoned().file = Some(file.clone());
    let worker = Arc::clone(&task);
    let keep = state.options.backup_keep;
    let dataset = entry.name.clone();
    let archive = state.options.wal_archive.as_ref().map(|a| a.join(&dataset));
    let written: Box<dyn FnOnce() -> Result<u64, BoxError> + Send> = if store {
        let target = dir.join(&file);
        let shapes = crate::shapes::own_folder(state, &entry);
        Box::new(move || {
            // A backup of the same second is replaced, as an N-Quads one is.
            if target.exists() {
                std::fs::remove_dir_all(&target)?;
            }
            // From an on-disk store, a base backup: it records where it starts in the chain
            // of write-ahead logs, for restores to a point in time.
            match entry.engine.store() {
                Dataset::TripleTrail(store) => store.save_base(&target)?,
                Dataset::Oxigraph(_) => entry.engine.save(&target)?,
            }
            // The shapes go with the data, and come back with a restore.
            if let Some(shapes) = shapes {
                crate::shapes::copy_folder(&shapes, &target.join("shapes"))?;
            }
            Ok(entry.engine.store().len()? as u64)
        })
    } else {
        // The iterator is a snapshot: taken here, before answering.
        let quads = entry.engine.store().iter();
        let (dir, file, worker) = (dir.to_path_buf(), file.clone(), Arc::clone(&worker));
        Box::new(move || {
            let tmp = dir.join(format!(".{file}.part"));
            let written = (|| -> Result<u64, BoxError> {
                let out = std::fs::File::create(&tmp)?;
                let gz = flate2::write::GzEncoder::new(
                    std::io::BufWriter::with_capacity(1 << 20, out),
                    flate2::Compression::fast(),
                );
                let mut serializer = RdfSerializer::from_format(RdfFormat::NQuads).for_writer(gz);
                let mut n = 0u64;
                for quad in quads {
                    serializer.serialize_quad(&quad?)?;
                    n += 1;
                    if n.is_multiple_of(100_000) {
                        worker.state.lock_unpoisoned().quads = n;
                    }
                }
                let gz = serializer.finish()?;
                let mut out = gz.finish()?;
                out.flush()?;
                out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
                std::fs::rename(&tmp, dir.join(&file))?;
                Ok(n)
            })();
            if written.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            written
        })
    };
    thread::spawn(move || {
        let written = written();
        // The old backups go before the task is finished, so a client that waits for it
        // lists what is kept.
        if written.is_ok()
            && let Some(keep) = keep
        {
            if let Err(e) = prune_backups(&dir, &dataset, keep) {
                eprintln!("removing old backups of {dataset}: {e}");
            } else if let Some(archive) = &archive
                && let Err(e) = prune_archive(&dir, &dataset, archive)
            {
                eprintln!("removing archived logs of {dataset}: {e}");
            }
        }
        let mut s = worker.state.lock_unpoisoned();
        match written {
            Ok(n) => {
                s.quads = n;
                s.success = Some(true);
            }
            Err(e) => {
                s.success = Some(false);
                s.message = Some(e.to_string());
            }
        }
        s.finished = Some(SystemTime::now());
    });
    Ok(json_reply(
        202,
        &json!({"taskId": task.id.to_string(), "requestId": task.id, "file": task.state.lock_unpoisoned().file}),
    ))
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// `POST /$/compact/{name}` (#831), Fuseki's TDB2 compaction: TripleTrail folds its write-ahead
/// log by itself, and this folds it now, in a task, so that scripts written for Fuseki keep
/// working. Fuseki's `deleteOld` has nothing to do: a fold leaves no old generation behind.
fn compact(state: &State, name: &str) -> Result<Reply, HttpError> {
    let entry = resolve(state, name)?;
    let task = state.tasks.start("Compact", &entry.name);
    let worker = Arc::clone(&task);
    thread::spawn(move || {
        let done: Result<u64, BoxError> = (|| {
            if let Dataset::TripleTrail(store) = entry.engine.store() {
                store.checkpoint_if_changed()?;
            }
            Ok(entry.engine.store().len()? as u64)
        })();
        let mut s = worker.state.lock_unpoisoned();
        match done {
            Ok(n) => {
                s.quads = n;
                s.success = Some(true);
            }
            Err(e) => {
                s.success = Some(false);
                s.message = Some(e.to_string());
            }
        }
        s.finished = Some(SystemTime::now());
    });
    Ok(json_reply(
        202,
        &json!({"taskId": task.id.to_string(), "requestId": task.id}),
    ))
}

/// `POST /$/sleep?interval=ms` (#831): Fuseki's test task, which waits `interval`
/// milliseconds (1 second by default, 10 minutes at most).
fn sleep(state: &State, params: &Params) -> Result<Reply, HttpError> {
    let ms = match params.get("interval") {
        None => 1000,
        Some(v) => v.parse::<u64>().map_err(|_| {
            HttpError::bad_request(format!("interval is a number of milliseconds, not {v}"))
        })?,
    }
    .min(600_000);
    let task = state.tasks.start("Sleep", "");
    let worker = Arc::clone(&task);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(ms));
        let mut s = worker.state.lock_unpoisoned();
        s.success = Some(true);
        s.finished = Some(SystemTime::now());
    });
    Ok(json_reply(
        202,
        &json!({"taskId": task.id.to_string(), "requestId": task.id}),
    ))
}

/// `POST /$/digest/{name}`: canonicalizes a snapshot of the dataset (RDFC-1.0) in a task and
/// keeps the SHA-256 of its canonical N-Quads.
fn digest(state: &State, name: &str) -> Result<Reply, HttpError> {
    let entry = resolve(state, name)?;
    // The iterator is a snapshot: taken here, before answering.
    let quads = entry.engine.store().iter();
    canon_task(state, "Digest", &entry.name, move || {
        let canonical = canonical(quads)?;
        Ok(TaskResult {
            quads: canonical.lines.len() as u64,
            sha256: Some(canonical.digest()),
            diff: None,
        })
    })
}

/// `POST /$/diff/{name}?to={other}`: what changed from snapshots of one dataset to another
/// (two releases), blank nodes matched through their RDFC-1.0 canonical labels: a task whose
/// `diff` holds the counts per graph, predicate and class of the subject, as `tripletrail diff`
/// prints them.
fn diff(state: &State, name: &str, params: &Params) -> Result<Reply, HttpError> {
    let to = params
        .get("to")
        .ok_or_else(|| HttpError::bad_request("give the dataset to compare with as to="))?;
    let a = resolve(state, name)?;
    let b = resolve(state, to)?;
    let (qa, qb) = (a.engine.store().iter(), b.engine.store().iter());
    let label = format!("{} -> {}", a.name, b.name);
    canon_task(state, "Diff", &label, move || {
        let (a, b) = (canonical(qa)?, canonical(qb)?);
        let d = tripletrail::canon::diff(&a, &b);
        let summary = d.summary(&a, &b);
        let counts = |m: &BTreeMap<String, tripletrail::canon::Counts>| {
            m.iter()
                .map(|(k, c)| json!({ "iri": k, "added": c.added, "removed": c.removed }))
                .collect::<Vec<_>>()
        };
        Ok(TaskResult {
            quads: (d.added.len() + d.removed.len()) as u64,
            sha256: None,
            diff: Some(json!({
                "a": { "sha256": a.digest(), "quads": a.lines.len() },
                "b": { "sha256": b.digest(), "quads": b.lines.len() },
                "added": summary.total.added,
                "removed": summary.total.removed,
                "graphs": counts(&summary.graphs),
                "predicates": counts(&summary.predicates),
                "classes": counts(&summary.classes),
            })),
        })
    })
}

fn resolve(state: &State, name: &str) -> Result<Arc<Entry>, HttpError> {
    state
        .registry
        .read_unpoisoned()
        .resolve(name)
        .ok_or_else(|| HttpError(404, format!("No dataset {name}")))
}

fn canonical(
    quads: tripletrail::store::QuadIter,
) -> Result<tripletrail::canon::Canonical, BoxError> {
    let quads = quads.collect::<Result<Vec<_>, _>>()?;
    Ok(tripletrail::canon::canonicalize(quads)?)
}

/// What a digest or diff task found.
struct TaskResult {
    quads: u64,
    sha256: Option<String>,
    diff: Option<Value>,
}

/// Runs `work` as a task of `kind`. It holds datasets in memory, so one such task runs at a
/// time (409 otherwise).
fn canon_task(
    state: &State,
    kind: &'static str,
    dataset: &str,
    work: impl FnOnce() -> Result<TaskResult, BoxError> + Send + 'static,
) -> Result<Reply, HttpError> {
    if CANONICALIZING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(HttpError(
            409,
            "A digest or diff is already running (see /$/tasks); one runs at a time".into(),
        ));
    }
    let task = state.tasks.start(kind, dataset);
    let worker = Arc::clone(&task);
    thread::spawn(move || {
        // Cleared however the task ends, a panic included.
        struct Done;
        impl Drop for Done {
            fn drop(&mut self) {
                CANONICALIZING.store(false, Ordering::Release);
            }
        }
        let done = Done;
        let result = work();
        // Before the task shows as finished, so a client that waits for it may start another.
        drop(done);
        let mut s = worker.state.lock_unpoisoned();
        match result {
            Ok(r) => {
                s.quads = r.quads;
                s.sha256 = r.sha256;
                s.diff = r.diff;
                s.success = Some(true);
            }
            Err(e) => {
                s.success = Some(false);
                s.message = Some(e.to_string());
            }
        }
        s.finished = Some(SystemTime::now());
    });
    Ok(json_reply(
        202,
        &json!({"taskId": task.id.to_string(), "requestId": task.id}),
    ))
}

/// Whether a digest or diff task is running, in this process.
static CANONICALIZING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A backup of the backup directory, by its name: `{dataset}_{YYYY-MM-DD_HH-MM-SS}.nq.gz` or
/// `….store`, as `POST /$/backup` names them.
struct BackupName<'a> {
    dataset: &'a str,
    /// `2026-11-10T08:52:13Z`.
    time: String,
}

impl<'a> BackupName<'a> {
    fn parse(file: &'a str) -> Option<Self> {
        let stem = match file.strip_suffix(STORE_SUFFIX) {
            Some(stem) => stem,
            None => file.strip_suffix(".nq.gz")?,
        };
        // The stamp is the last 19 characters, after a `_`.
        let split = stem.len().checked_sub(20)?;
        let (dataset, stamp) = (stem.get(..split)?, stem.get(split..)?);
        let stamp = stamp.strip_prefix('_')?;
        let b = stamp.as_bytes();
        let shape = b.len() == 19
            && b.iter().enumerate().all(|(i, c)| match i {
                4 | 7 | 13 | 16 => *c == b'-',
                10 => *c == b'_',
                _ => c.is_ascii_digit(),
            });
        (shape && !dataset.is_empty()).then(|| Self {
            dataset,
            time: format!(
                "{}T{}:{}:{}Z",
                &stamp[..10],
                &stamp[11..13],
                &stamp[14..16],
                &stamp[17..19]
            ),
        })
    }
}

/// Deletes the backups of `dataset` in `dir` beyond the `keep` newest (of both kinds).
fn prune_backups(dir: &Path, dataset: &str, keep: usize) -> std::io::Result<()> {
    let mut backups: Vec<(String, String)> = std::fs::read_dir(dir)?
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let parsed = BackupName::parse(&file)?;
            (parsed.dataset == dataset).then(|| (parsed.time.clone(), file))
        })
        .collect();
    backups.sort();
    let excess = backups.len().saturating_sub(keep);
    for (_, file) in backups.into_iter().take(excess) {
        let path = dir.join(&file);
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Deletes the archived logs of `dataset` that none of its store backups left in `dir` needs
/// (#222): with the old backups gone, nothing restores from before the oldest one kept.
fn prune_archive(dir: &Path, dataset: &str, archive: &Path) -> Result<usize, String> {
    let bases: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            is_store_backup(&file) && BackupName::parse(&file).is_some_and(|b| b.dataset == dataset)
        })
        .map(|e| e.path())
        .collect();
    tripletrail::store::Store::prune_wal_archive(archive, &bases).map_err(|e| e.to_string())
}

/// The size of a file, or of the files of a directory tree.
fn size_of(path: &Path) -> u64 {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::read_dir(path)
            .map(|d| d.flatten().map(|e| size_of(&e.path())).sum())
            .unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

fn backups_list(state: &State) -> Result<Reply, HttpError> {
    let dir = backup_dir(state)?;
    let mut files: Vec<(String, u64, Option<SystemTime>)> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let meta = e.metadata().ok()?;
                let listed = meta.is_file() || (meta.is_dir() && is_store_backup(&name));
                (listed && !name.starts_with('.'))
                    .then(|| (name, size_of(&e.path()), meta.modified().ok()))
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    files.sort();
    Ok(json_reply(
        200,
        &json!({
            "backups": files.iter().map(|(n, _, _)| n).collect::<Vec<_>>(),
            "files": files.iter().map(|(n, size, modified)| {
                let parsed = BackupName::parse(n);
                json!({
                    "name": n,
                    "size": size,
                    "modified": modified.map(rfc3339),
                    "kind": if is_store_backup(n) { "store" } else { "nquads" },
                    "dataset": parsed.as_ref().map(|p| p.dataset),
                    "time": parsed.as_ref().map(|p| p.time.clone()),
                })
            }).collect::<Vec<_>>(),
        }),
    ))
}

/// Checks a backup as a background task: a store backup is opened read-only, which checks
/// every checkpoint file against its checksum; an RDF backup is read through. The task's
/// `quads` is what the backup holds.
fn verify_backup(state: &State, file: &str) -> Result<Reply, HttpError> {
    let path = backup_path(state, file)?;
    if !path.exists() {
        return Err(HttpError(404, format!("No backup {file}")));
    }
    let dataset = BackupName::parse(file).map_or("", |b| b.dataset).to_owned();
    let task = state.tasks.start("Verify", &dataset);
    task.state.lock_unpoisoned().file = Some(file.to_owned());
    let worker = Arc::clone(&task);
    let file = file.to_owned();
    thread::spawn(move || {
        let counted = (|| -> Result<u64, BoxError> {
            if is_store_backup(&file) {
                let backup = Engine::open_read_only(&path)?;
                return Ok(backup.store().len()? as u64);
            }
            let input = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(&path)?);
            let (stem, gz) = match file.strip_suffix(".gz") {
                Some(stem) => (stem, true),
                None => (file.as_str(), false),
            };
            let format = Path::new(stem)
                .extension()
                .and_then(|e| e.to_str())
                .and_then(RdfFormat::from_extension)
                .unwrap_or(RdfFormat::NQuads);
            let input: Box<dyn std::io::Read> = if gz {
                Box::new(flate2::read::MultiGzDecoder::new(input))
            } else {
                Box::new(input)
            };
            let mut n = 0u64;
            for quad in RdfParser::from_format(format).for_reader(input) {
                quad?;
                n += 1;
            }
            Ok(n)
        })();
        let mut s = worker.state.lock_unpoisoned();
        s.finished = Some(SystemTime::now());
        match counted {
            Ok(n) => {
                s.quads = n;
                s.success = Some(true);
            }
            Err(e) => {
                s.success = Some(false);
                s.message = Some(e.to_string());
            }
        }
    });
    Ok(json_reply(
        202,
        &json!({"taskId": task.id.to_string(), "requestId": task.id, "file": task.state.lock_unpoisoned().file}),
    ))
}

/// Reads RDF of a known format into `engine` (for tests and the CLI).
#[allow(dead_code)]
pub(crate) fn parser_for(path: &Path) -> Option<RdfParser> {
    path.extension()
        .and_then(|e| e.to_str())
        .and_then(RdfFormat::from_extension)
        .map(RdfParser::from_format)
}

/// The name limits are kept under: an alias's dataset, or the name as given (a dataset not
/// attached yet may have limits too).
fn limits_name(state: &State, name: &str) -> String {
    state
        .registry
        .read_unpoisoned()
        .resolve(name)
        .map_or_else(|| name.to_owned(), |e| e.name.clone())
}

/// The limits file, or 409 when the server runs without one.
fn limits_file(state: &State) -> Result<&crate::limits::LimitsFile, HttpError> {
    state.limits.as_ref().ok_or_else(|| {
        HttpError(
            409,
            "The server has no limits file: start it with --limits FILE (the file may hold {})"
                .to_owned(),
        )
    })
}

fn json_body(state: &State, request: &mut Request) -> Result<Value, HttpError> {
    let body = crate::read_body(request, state.options.max_body)?;
    serde_json::from_str(&body).map_err(|e| HttpError::bad_request(format!("Invalid JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names() {
        let b = BackupName::parse("rinf_2026-11-10_08-52-13.store").unwrap();
        assert_eq!(
            (b.dataset, b.time.as_str()),
            ("rinf", "2026-11-10T08:52:13Z")
        );
        let b = BackupName::parse("my_data_2026-01-02_03-04-05.nq.gz").unwrap();
        assert_eq!(
            (b.dataset, b.time.as_str()),
            ("my_data", "2026-01-02T03:04:05Z")
        );
        assert!(BackupName::parse("upload.ttl").is_none());
        assert!(BackupName::parse("x_2026-11-10T08-52-13.nq.gz").is_none());
        assert!(BackupName::parse("_2026-11-10_08-52-13.nq.gz").is_none());
    }

    #[test]
    fn keeps_the_newest_backups() {
        let dir = tripletrail_testdir::tempdir("prune");
        for f in [
            "ds_2026-01-01_00-00-00.nq.gz",
            "ds_2026-01-02_00-00-00.nq.gz",
            "other_2026-01-01_00-00-00.nq.gz",
            "notes.txt",
        ] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        std::fs::create_dir(dir.join("ds_2026-01-03_00-00-00.store")).unwrap();
        prune_backups(&dir, "ds", 2).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            left,
            [
                "ds_2026-01-02_00-00-00.nq.gz",
                "ds_2026-01-03_00-00-00.store",
                "notes.txt",
                "other_2026-01-01_00-00-00.nq.gz"
            ]
        );
    }

    #[test]
    fn parses_rfc3339_times() {
        let t = |s: &str| parse_time(s).map(rfc3339);
        assert_eq!(
            t("2026-11-07T00:08:39Z").unwrap(),
            "2026-11-07T00:08:39.000Z"
        );
        assert_eq!(
            t("2026-11-07T01:08:39.839+01:00").unwrap(),
            "2026-11-07T00:08:39.839Z"
        );
        assert_eq!(
            t("2000-02-29T00:00:00.5Z").unwrap(),
            "2000-02-29T00:00:00.500Z"
        );
        for bad in [
            "2026-11-07",
            "2026-13-01T00:00:00Z",
            "2026-11-07T00:08:39",
            "soon",
        ] {
            assert!(parse_time(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + Duration::from_millis(1_794_010_119_839);
        assert_eq!(rfc3339(t), "2026-11-07T00:08:39.839Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(rfc3339(leap), "2000-02-29T00:00:00.000Z");
    }
}
