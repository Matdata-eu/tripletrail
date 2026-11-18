//! Per-dataset limits (#225): how many requests a dataset answers at once, how many may wait
//! for a turn, its timeout, row and upload limits, and its disk and memory quotas. They come from a JSON file
//! (`--limits`) that is read again when it changes, so a limit can be changed without a
//! restart.
//!
//! ```json
//! {
//!   "default": { "maxConcurrent": 4, "maxQueued": 8, "queueTimeout": 10 },
//!   "datasets": {
//!     "kg": { "maxConcurrent": 2, "timeout": 30, "maxTimeout": 300, "maxRows": 100000 }
//!   }
//! }
//! ```
//!
//! A request over a dataset's `maxConcurrent` waits (holding a worker thread) while fewer than
//! `maxQueued` others wait, for at most `queueTimeout` seconds; otherwise it is answered
//! with `429 Too Many Requests` and `Retry-After`. With a limit below the server's worker
//! count, a flood on one dataset leaves workers free for the others.
//!
//! `callers` limits who calls, across datasets: `maxConcurrent` requests at once and
//! `requestsPerMinute` (a token bucket holding `burst` requests). `agents` entries apply to one
//! agent IRI, `groups` entries to all members of a group together, `anonymous` to each client
//! address without an identity, and `default` to each caller no other entry names.
//!
//! ```json
//! {
//!   "callers": {
//!     "default": { "maxConcurrent": 4, "requestsPerMinute": 600 },
//!     "anonymous": { "maxConcurrent": 2, "requestsPerMinute": 60, "burst": 10 },
//!     "groups": { "partners": { "requestsPerMinute": 1200 } }
//!   }
//! }
//! ```

use crate::admin::Entry;
use crate::{HttpError, State};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tripletrail::geo::sync::LockUnpoisoned;

/// How often the file's modification time is looked at.
const RELOAD_CHECK: Duration = Duration::from_secs(1);

/// How long a queued request waits for a turn when the file does not say.
const QUEUE_TIMEOUT: Duration = Duration::from_secs(10);

/// One dataset's limits as written in the file: unset fields fall back to `default`, then to
/// the server's options.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Written {
    max_concurrent: Option<usize>,
    max_queued: Option<usize>,
    queue_timeout: Option<Duration>,
    timeout: Option<Duration>,
    max_timeout: Option<Duration>,
    max_rows: Option<usize>,
    max_upload: Option<usize>,
    max_disk: Option<u64>,
    max_memory: Option<u64>,
    query_memory: Option<u64>,
}

impl Written {
    fn or(self, other: Self) -> Self {
        Self {
            max_concurrent: self.max_concurrent.or(other.max_concurrent),
            max_queued: self.max_queued.or(other.max_queued),
            queue_timeout: self.queue_timeout.or(other.queue_timeout),
            timeout: self.timeout.or(other.timeout),
            max_timeout: self.max_timeout.or(other.max_timeout),
            max_rows: self.max_rows.or(other.max_rows),
            max_upload: self.max_upload.or(other.max_upload),
            max_disk: self.max_disk.or(other.max_disk),
            max_memory: self.max_memory.or(other.max_memory),
            query_memory: self.query_memory.or(other.query_memory),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Config {
    default: Written,
    datasets: BTreeMap<String, Written>,
    callers: Callers,
}

/// Limits on one caller, or on a group's members together.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct CallerLimits {
    pub(crate) max_concurrent: Option<usize>,
    pub(crate) per_minute: Option<f64>,
    /// Requests the bucket holds: how many may come at once after a quiet spell.
    pub(crate) burst: Option<f64>,
}

impl CallerLimits {
    fn is_empty(&self) -> bool {
        self.max_concurrent.is_none() && self.per_minute.is_none()
    }

    fn burst(&self) -> f64 {
        self.burst
            .or(self.per_minute.map(|r| (r / 10.0).ceil()))
            .unwrap_or(1.0)
            .max(1.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Callers {
    default: Option<CallerLimits>,
    anonymous: Option<CallerLimits>,
    agents: BTreeMap<String, CallerLimits>,
    groups: BTreeMap<String, CallerLimits>,
}

struct Loaded {
    config: Config,
    /// The file as written, for the admin API.
    raw: Value,
    modified: Option<SystemTime>,
    checked: Instant,
}

/// The limits file, read again when it changes.
pub(crate) struct LimitsFile {
    path: PathBuf,
    loaded: RwLock<Loaded>,
}

impl LimitsFile {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let (config, raw) = read(path)?;
        Ok(Self {
            path: path.to_owned(),
            loaded: RwLock::new(Loaded {
                config,
                raw,
                modified: modified(path),
                checked: Instant::now(),
            }),
        })
    }

    /// Reads the file again if its modification time changed, looking at most once per
    /// `RELOAD_CHECK`.
    fn reload_if_changed(&self) {
        {
            let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
            if loaded.checked.elapsed() < RELOAD_CHECK {
                return;
            }
        }
        let mut loaded = self.loaded.write().unwrap_or_else(|e| e.into_inner());
        if loaded.checked.elapsed() < RELOAD_CHECK {
            return;
        }
        loaded.checked = Instant::now();
        let now = modified(&self.path);
        if now == loaded.modified {
            return;
        }
        loaded.modified = now;
        match read(&self.path) {
            Ok((config, raw)) => {
                loaded.config = config;
                loaded.raw = raw;
                eprintln!("tripletrail: reloaded {}", self.path.display());
            }
            Err(e) => eprintln!(
                "tripletrail: {} is invalid, keeping the previous one: {e}",
                self.path.display()
            ),
        }
    }

    /// The file's contents, as JSON.
    pub(crate) fn raw(&self) -> Value {
        self.reload_if_changed();
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .raw
            .clone()
    }

    /// Replaces the file's contents with `raw`, once it is checked, and applies it at once.
    /// The file is written to a temporary file next to it, then renamed over it.
    pub(crate) fn replace(&self, raw: Value) -> Result<(), String> {
        let mut loaded = self.loaded.write().unwrap_or_else(|e| e.into_inner());
        self.write(&mut loaded, raw)
    }

    /// Sets the entry of one dataset (`None` removes it), keeping the rest of the file.
    pub(crate) fn set_dataset(&self, name: &str, entry: Option<Value>) -> Result<(), String> {
        // Reads the file again, so an edit made by hand since the last look is kept. Its
        // modification time is not enough: an edit within the same tick would not show.
        let mut loaded = self.loaded.write().unwrap_or_else(|e| e.into_inner());
        let mut raw = match read(&self.path) {
            Ok((_, raw)) => raw,
            Err(_) => loaded.raw.clone(),
        };
        let Value::Object(root) = &mut raw else {
            return Err("the limits file is not a JSON object".to_owned());
        };
        let datasets = root
            .entry("datasets")
            .or_insert_with(|| Value::Object(Default::default()));
        let Value::Object(datasets) = datasets else {
            return Err("datasets must be an object of dataset names".to_owned());
        };
        match entry {
            Some(entry) => {
                datasets.insert(name.to_owned(), entry);
            }
            None => {
                datasets.remove(name);
            }
        }
        self.write(&mut loaded, raw)
    }

    fn write(&self, loaded: &mut Loaded, raw: Value) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&raw).map_err(|e| e.to_string())?;
        let config = parse(&text)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, format!("{text}\n"))
            .and_then(|()| std::fs::rename(&tmp, &self.path))
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        loaded.config = config;
        loaded.raw = raw;
        loaded.modified = modified(&self.path);
        loaded.checked = Instant::now();
        Ok(())
    }

    fn written(&self, dataset: &str) -> Written {
        self.reload_if_changed();
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        let config = &loaded.config;
        config
            .datasets
            .get(dataset)
            .copied()
            .unwrap_or_default()
            .or(config.default)
    }
}

impl LimitsFile {
    /// The limits that apply to a caller: its own (keyed by agent IRI, or by client address
    /// for an anonymous one) and those of its groups (keyed by group, shared by the members).
    fn caller_limits(
        &self,
        agent: Option<&str>,
        groups: &[String],
        address: Option<IpAddr>,
    ) -> Vec<(String, CallerLimits)> {
        self.reload_if_changed();
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        let c = &loaded.config.callers;
        let mut out = Vec::new();
        let own = match agent {
            Some(agent) => c
                .agents
                .get(agent)
                .copied()
                .or(c.default)
                .map(|l| (format!("agent {agent}"), l)),
            None => c.anonymous.or(c.default).map(|l| {
                let address = address.map_or_else(|| "unknown".to_owned(), |a| a.to_string());
                (format!("address {address}"), l)
            }),
        };
        out.extend(own);
        for g in groups {
            if let Some(l) = c.groups.get(g) {
                out.push((format!("group {g}"), *l));
            }
        }
        out.retain(|(_, l)| !l.is_empty());
        out
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn read(path: &Path) -> Result<(Config, Value), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let config = parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let raw = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok((config, raw))
}

fn parse(text: &str) -> Result<Config, String> {
    let root: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let Value::Object(root) = root else {
        return Err("expected a JSON object".to_owned());
    };
    let mut config = Config::default();
    for (key, value) in &root {
        match key.as_str() {
            "default" => config.default = written(value).map_err(|e| format!("default: {e}"))?,
            "datasets" => {
                let Value::Object(datasets) = value else {
                    return Err("datasets must be an object of dataset names".to_owned());
                };
                for (name, value) in datasets {
                    let limits = written(value).map_err(|e| format!("datasets.{name}: {e}"))?;
                    config.datasets.insert(name.clone(), limits);
                }
            }
            "callers" => config.callers = callers(value).map_err(|e| format!("callers: {e}"))?,
            other => return Err(format!("unknown key {other}")),
        }
    }
    Ok(config)
}

fn callers(value: &Value) -> Result<Callers, String> {
    let Value::Object(fields) = value else {
        return Err("expected an object".to_owned());
    };
    let named = |key: &str, v: &Value| -> Result<BTreeMap<String, CallerLimits>, String> {
        let Value::Object(entries) = v else {
            return Err(format!("{key} must be an object"));
        };
        entries
            .iter()
            .map(|(name, v)| {
                Ok((
                    name.clone(),
                    caller(v).map_err(|e| format!("{key}.{name}: {e}"))?,
                ))
            })
            .collect()
    };
    let mut c = Callers::default();
    for (key, v) in fields {
        match key.as_str() {
            "default" => c.default = Some(caller(v).map_err(|e| format!("default: {e}"))?),
            "anonymous" => c.anonymous = Some(caller(v).map_err(|e| format!("anonymous: {e}"))?),
            "agents" => c.agents = named(key, v)?,
            "groups" => c.groups = named(key, v)?,
            other => return Err(format!("unknown key {other}")),
        }
    }
    Ok(c)
}

fn caller(value: &Value) -> Result<CallerLimits, String> {
    let Value::Object(fields) = value else {
        return Err("expected an object".to_owned());
    };
    let positive = |key: &str, v: &Value| {
        v.as_f64()
            .filter(|n| n.is_finite() && *n > 0.0)
            .ok_or_else(|| format!("{key} must be a number above 0"))
    };
    let mut l = CallerLimits::default();
    for (key, v) in fields {
        match key.as_str() {
            "maxConcurrent" => {
                l.max_concurrent = Some(
                    v.as_u64()
                        .filter(|n| *n > 0)
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or("maxConcurrent must be a whole number of at least 1")?,
                )
            }
            "requestsPerMinute" => l.per_minute = Some(positive(key, v)?),
            "burst" => l.burst = Some(positive(key, v)?),
            other => return Err(format!("unknown limit {other}")),
        }
    }
    Ok(l)
}

fn written(value: &Value) -> Result<Written, String> {
    let Value::Object(fields) = value else {
        return Err("expected an object".to_owned());
    };
    let count = |key: &str, v: &Value| {
        v.as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| format!("{key} must be a whole number of at least 0"))
    };
    let seconds = |key: &str, v: &Value| {
        v.as_f64()
            .and_then(|s| Duration::try_from_secs_f64(s).ok())
            .ok_or_else(|| format!("{key} must be a number of seconds"))
    };
    let mut w = Written::default();
    for (key, v) in fields {
        match key.as_str() {
            "maxConcurrent" => {
                let n = count(key, v)?;
                if n == 0 {
                    return Err("maxConcurrent must be at least 1".to_owned());
                }
                w.max_concurrent = Some(n);
            }
            "maxQueued" => w.max_queued = Some(count(key, v)?),
            "queueTimeout" => w.queue_timeout = Some(seconds(key, v)?),
            "timeout" => w.timeout = Some(seconds(key, v)?),
            "maxTimeout" => w.max_timeout = Some(seconds(key, v)?),
            "maxRows" => w.max_rows = Some(count(key, v)?),
            "maxUpload" => w.max_upload = Some(count(key, v)?),
            "maxDisk" => w.max_disk = Some(bytes(key, v)?),
            "maxMemory" => w.max_memory = Some(bytes(key, v)?),
            "queryMemory" => w.query_memory = Some(bytes(key, v)?),
            other => return Err(format!("unknown limit {other}")),
        }
    }
    Ok(w)
}

/// A size in bytes: a whole number, or a string such as `"512MB"` or `"2 GiB"` (KB, MB, GB
/// and TB are powers of 1000, KiB, MiB, GiB and TiB of 1024).
pub(crate) fn bytes(key: &str, v: &Value) -> Result<u64, String> {
    let invalid = || format!("{key} must be a number of bytes or a size such as \"10GB\"");
    if let Some(n) = v.as_u64() {
        return Ok(n);
    }
    let text = v.as_str().ok_or_else(invalid)?.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().map_err(|_| invalid())?;
    let factor: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return Err(invalid()),
    };
    let n = number * factor;
    if !n.is_finite() || n < 0.0 || n > u64::MAX as f64 {
        return Err(invalid());
    }
    Ok(n as u64)
}

/// The limits a request on one dataset runs under: the file's, with the server's options
/// where it says nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Limits {
    pub(crate) max_concurrent: Option<usize>,
    pub(crate) max_queued: usize,
    pub(crate) queue_timeout: Duration,
    /// Used when the request asks for no timeout.
    pub(crate) timeout: Option<Duration>,
    /// The longest timeout a request may ask for.
    pub(crate) max_timeout: Option<Duration>,
    pub(crate) max_rows: Option<usize>,
    pub(crate) max_upload: Option<usize>,
    /// Writes are refused while the dataset's files take more than this.
    pub(crate) max_disk: Option<u64>,
    /// Writes are refused while the dataset holds more than this in memory.
    pub(crate) max_memory: Option<u64>,
    /// The memory the rows each hash join of one query holds may take (#189), below the
    /// server's `--query-memory`; a request may ask for less with `query-memory`.
    pub(crate) query_memory: Option<u64>,
}

impl Limits {
    /// The limits of the dataset `name`.
    pub(crate) fn of(state: &State, name: &str) -> Self {
        let w = state
            .limits
            .as_ref()
            .map(|f| f.written(name))
            .unwrap_or_default();
        let options = &state.options;
        // A dataset that sets either timeout replaces the server's, which is both the default
        // and the cap.
        let (timeout, max_timeout) = if w.timeout.is_some() || w.max_timeout.is_some() {
            let cap = w.max_timeout.or(w.timeout);
            (w.timeout.or(cap).min_opt(cap), cap)
        } else {
            (options.timeout, options.timeout)
        };
        Self {
            max_concurrent: w.max_concurrent,
            max_queued: w.max_queued.unwrap_or(0),
            queue_timeout: w.queue_timeout.unwrap_or(QUEUE_TIMEOUT),
            timeout,
            max_timeout,
            max_rows: w.max_rows.or(options.max_rows),
            max_upload: w.max_upload.or(options.max_upload),
            max_disk: w.max_disk,
            max_memory: w.max_memory,
            query_memory: w.query_memory,
        }
    }

    /// The query memory of a request that asked for `asked` bytes: never more than the
    /// dataset's `queryMemory`. A `queryMemory` of 0 sets none.
    pub(crate) fn query_memory(&self, asked: Option<u64>) -> Option<u64> {
        match (asked, self.query_memory.filter(|&cap| cap > 0)) {
            (Some(asked), Some(cap)) => Some(asked.min(cap)),
            (asked, cap) => asked.or(cap),
        }
    }

    /// The timeout of a request that asked for `asked`: never more than the cap.
    pub(crate) fn timeout(&self, asked: Option<Duration>) -> Option<Duration> {
        match (asked, self.max_timeout) {
            (Some(asked), Some(cap)) => Some(asked.min(cap)),
            (Some(asked), None) => Some(asked),
            (None, _) => self.timeout,
        }
    }

    pub(crate) fn json(&self) -> Value {
        serde_json::json!({
            "maxConcurrent": self.max_concurrent,
            "maxQueued": self.max_queued,
            "queueTimeout": self.queue_timeout.as_secs_f64(),
            "timeout": self.timeout.map(|t| t.as_secs_f64()),
            "maxTimeout": self.max_timeout.map(|t| t.as_secs_f64()),
            "maxRows": self.max_rows,
            "maxUpload": self.max_upload,
            "maxDisk": self.max_disk,
            "maxMemory": self.max_memory,
            "queryMemory": self.query_memory,
        })
    }
}

trait MinOpt {
    fn min_opt(self, cap: Self) -> Self;
}

impl MinOpt for Option<Duration> {
    fn min_opt(self, cap: Self) -> Self {
        match (self, cap) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// A dataset's running and waiting requests.
#[derive(Default)]
pub(crate) struct Gate {
    counts: Mutex<Counts>,
    freed: Condvar,
    /// Requests that waited for a turn.
    pub(crate) queued_total: AtomicU64,
    /// Requests answered with 429 because the dataset was busy.
    pub(crate) rejected: AtomicU64,
    /// Writes refused because the dataset was over its disk or memory quota.
    pub(crate) rejected_quota: AtomicU64,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Counts {
    pub(crate) running: usize,
    pub(crate) queued: usize,
}

impl Gate {
    pub(crate) fn counts(&self) -> Counts {
        *self.counts.lock_unpoisoned()
    }
}

/// A turn on a dataset: released when dropped, after the answer is sent.
pub(crate) struct Permit(Arc<Entry>);

impl Drop for Permit {
    fn drop(&mut self) {
        let gate = &self.0.gate;
        gate.counts.lock_unpoisoned().running -= 1;
        gate.freed.notify_one();
    }
}

/// Waits for a turn on `entry` under `limits`, or refuses with 429 when it is busy.
pub(crate) fn enter(entry: &Arc<Entry>, limits: &Limits) -> Result<Permit, HttpError> {
    let gate = &entry.gate;
    let mut counts = gate.counts.lock_unpoisoned();
    let free = |c: &Counts| limits.max_concurrent.is_none_or(|max| c.running < max);
    if !free(&counts) {
        if counts.queued >= limits.max_queued {
            gate.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(busy(entry, limits));
        }
        counts.queued += 1;
        gate.queued_total.fetch_add(1, Ordering::Relaxed);
        let deadline = Instant::now() + limits.queue_timeout;
        while !free(&counts) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                counts.queued -= 1;
                gate.rejected.fetch_add(1, Ordering::Relaxed);
                return Err(busy(entry, limits));
            }
            counts = gate
                .freed
                .wait_timeout(counts, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        counts.queued -= 1;
    }
    counts.running += 1;
    Ok(Permit(Arc::clone(entry)))
}

fn busy(entry: &Entry, limits: &Limits) -> HttpError {
    HttpError(
        429,
        format!(
            "Dataset {} is busy ({} requests at once); try again later",
            entry.name,
            limits.max_concurrent.unwrap_or_default()
        ),
    )
}

/// Most callers tracked before idle ones are forgotten.
const CALLERS_KEPT: usize = 10_000;

/// Running requests and rate buckets of callers and groups.
#[derive(Default)]
pub(crate) struct CallerGate {
    states: Mutex<HashMap<String, CallerState>>,
    /// Requests refused for going over `requestsPerMinute`.
    pub(crate) rejected_rate: AtomicU64,
    /// Requests refused for going over a caller's `maxConcurrent`.
    pub(crate) rejected_concurrency: AtomicU64,
}

struct CallerState {
    running: usize,
    tokens: f64,
    refilled: Instant,
}

/// A refusal: the error, and when to retry, in seconds.
pub(crate) struct Refused(pub(crate) HttpError, pub(crate) u64);

/// The caller's turns, released when dropped.
pub(crate) struct CallerPermit {
    gate: Arc<CallerGate>,
    keys: Vec<String>,
}

impl Drop for CallerPermit {
    fn drop(&mut self) {
        let mut states = self.gate.states.lock_unpoisoned();
        for key in &self.keys {
            if let Some(s) = states.get_mut(key) {
                s.running -= 1;
            }
        }
    }
}

/// Takes a turn and a request token for the caller and each of its limited groups, or refuses
/// with 429 and the seconds to wait. Nothing is taken when any of them refuses.
pub(crate) fn admit_caller(
    state: &State,
    agent: Option<&str>,
    groups: &[String],
    address: Option<IpAddr>,
) -> Result<Option<CallerPermit>, Refused> {
    let Some(file) = &state.limits else {
        return Ok(None);
    };
    let limits = file.caller_limits(agent, groups, address);
    if limits.is_empty() {
        return Ok(None);
    }
    let gate = &state.callers;
    let now = Instant::now();
    let mut states = gate.states.lock_unpoisoned();
    if states.len() > CALLERS_KEPT {
        states.retain(|_, s| {
            s.running > 0 || now.duration_since(s.refilled) < Duration::from_secs(600)
        });
    }
    // Check every limit first, then take from all of them.
    for (key, l) in &limits {
        let s = states.entry(key.clone()).or_insert_with(|| CallerState {
            running: 0,
            tokens: l.burst(),
            refilled: now,
        });
        if let Some(rate) = l.per_minute {
            let per_second = rate / 60.0;
            s.tokens = (s.tokens + now.duration_since(s.refilled).as_secs_f64() * per_second)
                .min(l.burst());
            s.refilled = now;
            if s.tokens < 1.0 {
                gate.rejected_rate.fetch_add(1, Ordering::Relaxed);
                let wait = ((1.0 - s.tokens) / per_second).ceil().max(1.0) as u64;
                return Err(Refused(
                    HttpError(
                        429,
                        format!("Too many requests for {key} ({rate} a minute); try again later"),
                    ),
                    wait,
                ));
            }
        }
        if let Some(max) = l.max_concurrent
            && s.running >= max
        {
            gate.rejected_concurrency.fetch_add(1, Ordering::Relaxed);
            return Err(Refused(
                HttpError(
                    429,
                    format!("Too many requests at once for {key} ({max}); try again later"),
                ),
                1,
            ));
        }
    }
    let mut keys = Vec::with_capacity(limits.len());
    for (key, l) in limits {
        let s = states.get_mut(&key).expect("added above");
        if l.per_minute.is_some() {
            s.tokens -= 1.0;
        }
        s.running += 1;
        keys.push(key);
    }
    Ok(Some(CallerPermit {
        gate: Arc::clone(gate),
        keys,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        let b = |v: Value| bytes("maxDisk", &v);
        assert_eq!(b(serde_json::json!(1234)), Ok(1234));
        assert_eq!(b(serde_json::json!("512MB")), Ok(512_000_000));
        assert_eq!(b(serde_json::json!("2 GiB")), Ok(2 << 30));
        assert_eq!(b(serde_json::json!("1.5kb")), Ok(1500));
        assert!(b(serde_json::json!("10 parsecs")).is_err());
        assert!(b(serde_json::json!(-1)).is_err());
        assert!(b(serde_json::json!("GB")).is_err());
    }

    #[test]
    fn parses_and_merges() {
        let config = parse(
            r#"{"default": {"maxConcurrent": 4, "queueTimeout": 2.5},
                "datasets": {"kg": {"maxConcurrent": 1, "timeout": 30, "maxRows": 10}}}"#,
        )
        .unwrap();
        let kg = config.datasets["kg"].or(config.default);
        assert_eq!(kg.max_concurrent, Some(1));
        assert_eq!(kg.queue_timeout, Some(Duration::from_secs_f64(2.5)));
        assert_eq!(kg.timeout, Some(Duration::from_secs(30)));
        assert_eq!(kg.max_rows, Some(10));
        assert!(parse(r#"{"default": {"maxConcurent": 1}}"#).is_err());
        assert!(parse(r#"{"default": {"maxConcurrent": 0}}"#).is_err());
        assert!(parse(r#"{"dataset": {}}"#).is_err());
        assert!(parse(r#"{"default": {"timeout": -1}}"#).is_err());
    }

    #[test]
    fn parses_callers() {
        let config = parse(
            r#"{"callers": {"default": {"maxConcurrent": 2},
                "anonymous": {"requestsPerMinute": 60, "burst": 5},
                "agents": {"urn:a": {"requestsPerMinute": 6}},
                "groups": {"g": {"maxConcurrent": 3}}}}"#,
        )
        .unwrap();
        let c = &config.callers;
        assert_eq!(c.default.unwrap().max_concurrent, Some(2));
        assert_eq!(c.anonymous.unwrap().burst(), 5.0);
        assert_eq!(c.agents["urn:a"].burst(), 1.0);
        assert_eq!(c.groups["g"].max_concurrent, Some(3));
        assert!(parse(r#"{"callers": {"users": {}}}"#).is_err());
        assert!(parse(r#"{"callers": {"default": {"requestsPerMinute": 0}}}"#).is_err());
    }

    #[test]
    fn timeouts() {
        let limits = Limits {
            timeout: Some(Duration::from_secs(10)),
            max_timeout: Some(Duration::from_secs(60)),
            ..Limits::default()
        };
        assert_eq!(limits.timeout(None), Some(Duration::from_secs(10)));
        assert_eq!(
            limits.timeout(Some(Duration::from_secs(30))),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            limits.timeout(Some(Duration::from_secs(600))),
            Some(Duration::from_secs(60))
        );
        assert_eq!(Limits::default().timeout(None), None);
    }
}

/// What a dataset takes: the size of its files, and the memory its store and result cache
/// hold (`None` where it is not known, as for Oxigraph's store).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Usage {
    pub(crate) disk: Option<u64>,
    pub(crate) memory: Option<u64>,
}

impl Usage {
    pub(crate) fn of(entry: &Entry) -> Self {
        Self {
            disk: entry.location.as_deref().map(crate::admin::size_on_disk),
            memory: Self::memory(entry),
        }
    }

    /// The heap memory the dataset's engine holds (see `Engine::memory_use`; memory-mapped
    /// checkpoint files are not counted: the operating system pages them in and out).
    pub(crate) fn memory(entry: &Entry) -> Option<u64> {
        entry.engine.memory_use().map(|m| m.total() as u64)
    }
}

/// Refuses a write with 507 while the dataset is over its disk or memory quota. A write is
/// checked before it starts: the one that crosses a quota completes, and the ones after it
/// are refused until data is removed or the quota raised. Reads are never refused, but a
/// read that builds an index (spatial, full-text, range) adds to the memory counted.
pub(crate) fn check_quota(entry: &Entry, limits: &Limits) -> Result<(), HttpError> {
    check_quota_unless(entry, limits, || false)
}

/// Like [`check_quota`], but lets a write through while over a quota when `exempt` says so
/// (an update that only removes data). `exempt` runs only when the dataset is over a quota,
/// so the common case never pays for it.
pub(crate) fn check_quota_unless(
    entry: &Entry,
    limits: &Limits,
    exempt: impl FnOnce() -> bool,
) -> Result<(), HttpError> {
    if limits.max_disk.is_none() && limits.max_memory.is_none() {
        return Ok(());
    }
    let usage = Usage::of(entry);
    let over = |what: &str, used: Option<u64>, quota: Option<u64>| match (used, quota) {
        (Some(used), Some(quota)) if used >= quota && what == "disk" => Some(format!(
            "Dataset {} is over its disk quota ({used} of {quota} bytes): writes are \
             refused until data is removed or the quota is raised; reads still work",
            entry.name
        )),
        // The indexes that queries build count too: a read can take a dataset over.
        (Some(used), Some(quota)) if used >= quota => Some(format!(
            "Dataset {0} is over its memory quota ({used} of {quota} bytes, with the indexes \
             queries built: see `memory` in /$/stats/{0}): writes are refused until data is \
             removed, the quota is raised or the server restarts (which drops the indexes \
             until a query needs them); reads still work",
            entry.name
        )),
        _ => None,
    };
    let refusal = over("disk", usage.disk, limits.max_disk)
        .or_else(|| over("memory", usage.memory, limits.max_memory));
    match refusal {
        Some(_) if exempt() => Ok(()),
        Some(message) => {
            entry.gate.rejected_quota.fetch_add(1, Ordering::Relaxed);
            Err(HttpError(507, message))
        }
        None => Ok(()),
    }
}
