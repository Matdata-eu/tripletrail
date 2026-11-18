//! Read replicas by shipping the write-ahead log (#217).
//!
//! A leader serves, to the admin only:
//!
//! - `GET /$/replication/{name}`: where the dataset's log stands (its timeline, the
//!   generation of the log being written, and its length on disk);
//! - `GET /$/replication/{name}/log?timeline=…&generation=…&offset=…[&max=…][&wait=…]`: the
//!   bytes of that log from `offset`, as far as they are on disk. `X-TripleTrail-Log-End` is the
//!   log's length, `X-TripleTrail-Log-Retired: true` says a checkpoint retired it (the next
//!   generation follows), `X-TripleTrail-Store-Format` is the leader's store format (#221). 409 when the dataset is on another timeline now (recreated or
//!   restored), 410 when the log is gone; either way a follower starts again from a base.
//!   With `wait` (milliseconds, up to 30 s), a read with nothing new waits for a commit;
//! - `GET /$/replication/{name}/base`: a base backup of the dataset, as a tar archive;
//! - `POST /$/replication/{name}/promote`, on a follower: makes it the leader (see
//!   [`promote`]).
//!
//! A follower's read of the log names the follower (`follower=…`) and the position it has
//! applied, so the leader knows how far each follower is. With `--sync-replicas N`, a write
//! answers once N followers have read past it (see [`Acks`]).
//!
//! A follower (`tripletrail serve --follow NAME=URL`) copies a base, then reads the log and
//! applies each transaction, as the leader committed it, in a commit of its own. Its dataset
//! answers queries but no writes, and `/$/status` and `/$/datasets/{name}` report how far
//! behind it is.

use crate::admin::{Entry, Mode};
use crate::{Handle, HttpError, Params, Reply, State, json_reply};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tiny_http::{Method, Request};
use tripletrail::Engine;
use tripletrail::geo::sync::{LockUnpoisoned, RwLockUnpoisoned};
use tripletrail::store::{Dataset, LogEntry, LogRead, Store};

/// What a read of the log returns at most, unless asked for less (or for more, to get a
/// transaction larger than that whole).
const MAX_READ: usize = 16 << 20;

/// The longest a log read waits for a commit.
const MAX_WAIT: Duration = Duration::from_secs(30);

/// How long the leader remembers a follower it has not heard from.
const FORGET_AFTER: Duration = Duration::from_secs(600);

/// A follower's timeline, generation and offset, and when it said so.
type Held = (String, u64, u64, Instant);

/// Where each follower of each dataset stands, as its last read of the log said (#217): a
/// follower asks for the log from the position it has applied (and saved, its commit synced),
/// so a read from past a write says the follower holds the write.
#[derive(Default)]
pub(crate) struct Acks {
    state: Mutex<AcksState>,
    moved: std::sync::Condvar,
    /// Counts the writes this server answered, so that a read of the log waiting for a
    /// commit wakes at once rather than at its next look.
    commits: Mutex<u64>,
    committed: std::sync::Condvar,
}

#[derive(Default)]
struct AcksState {
    /// By dataset and follower: the timeline, generation and offset it asked from, and when.
    positions: std::collections::HashMap<(String, String), Held>,
    /// By dataset, for --sync-replicas: whether writes stopped waiting (too few replicas
    /// answered in time), and where the last write that answered left the log.
    sync: std::collections::HashMap<String, SyncWrites>,
}

#[derive(Default)]
struct SyncWrites {
    degraded: bool,
    last: Option<(String, u64, u64)>,
}

impl AcksState {
    /// How many followers of `dataset` hold the log up to `generation:length` on `timeline`.
    fn holding(&self, dataset: &str, (timeline, generation, length): (&str, u64, u64)) -> usize {
        self.positions
            .iter()
            .filter(|((d, _), (t, g, o, _))| {
                d == dataset && t == timeline && (*g, *o) >= (generation, length)
            })
            .count()
    }
}

impl Acks {
    fn note(&self, dataset: &str, follower: &str, timeline: &str, generation: u64, offset: u64) {
        let now = Instant::now();
        let mut state = self.state.lock_unpoisoned();
        state
            .positions
            .retain(|_, p| now.duration_since(p.3) < FORGET_AFTER);
        state.positions.insert(
            (dataset.to_owned(), follower.to_owned()),
            (timeline.to_owned(), generation, offset, now),
        );
        drop(state);
        self.moved.notify_all();
    }

    /// Notes that a write is on disk: reads of the log waiting for one look again.
    pub(crate) fn note_commit(&self) {
        *self.commits.lock_unpoisoned() += 1;
        self.committed.notify_all();
    }

    /// The writes noted so far.
    fn commits(&self) -> u64 {
        *self.commits.lock_unpoisoned()
    }

    /// Waits until a write after the first `seen` is noted, for up to `timeout`.
    fn wait_commit(&self, seen: u64, timeout: Duration) {
        let commits = self.commits.lock_unpoisoned();
        if *commits == seen {
            let _ = self.committed.wait_timeout(commits, timeout);
        }
    }

    /// Waits until `replicas` followers of `dataset` hold the log up to `position`
    /// (timeline, generation, length), for up to `timeout`; returns how many do.
    ///
    /// When too few do in time, the dataset's writes stop waiting (#764): each answers at
    /// once with how many hold it, until enough followers hold the last write that answered
    /// so; then they wait again. Both changes are logged once.
    pub(crate) fn wait(
        &self,
        dataset: &str,
        position: (&str, u64, u64),
        replicas: usize,
        timeout: Duration,
    ) -> usize {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock_unpoisoned();
        let owned = Some((position.0.to_owned(), position.1, position.2));
        let sync = state.sync.entry(dataset.to_owned()).or_default();
        if sync.degraded {
            let last = sync.last.clone();
            let caught_up =
                last.is_some_and(|(t, g, o)| state.holding(dataset, (&t, g, o)) >= replicas);
            if !caught_up {
                let held = state.holding(dataset, position);
                state.sync.entry(dataset.to_owned()).or_default().last = owned;
                return held;
            }
            state.sync.entry(dataset.to_owned()).or_default().degraded = false;
            eprintln!("{dataset}: {replicas} replica(s) caught up: writes wait for them again");
        }
        loop {
            let held = state.holding(dataset, position);
            let now = Instant::now();
            if held >= replicas || now >= deadline {
                let sync = state.sync.entry(dataset.to_owned()).or_default();
                sync.last = owned;
                if held < replicas && !sync.degraded {
                    sync.degraded = true;
                    eprintln!(
                        "{dataset}: only {held} of {replicas} replica(s) held a write after {timeout:?}: writes answer without waiting until they catch up"
                    );
                }
                return held;
            }
            state = self
                .moved
                .wait_timeout(state, deadline - now)
                .map_or_else(|e| e.into_inner().0, |(g, _)| g);
        }
    }

    /// For `/$/replication/{name}`: whether writes wait for replicas, and the followers.
    pub(crate) fn json(&self, dataset: &str, replicas: usize) -> Value {
        let state = self.state.lock_unpoisoned();
        let now = Instant::now();
        let followers: Vec<Value> = state
            .positions
            .iter()
            .filter(|((d, _), _)| d == dataset)
            .map(|((_, id), (t, g, o, seen))| {
                json!({
                    "follower": id,
                    "timeline": t,
                    "generation": g,
                    "offset": o,
                    "secondsAgo": now.duration_since(*seen).as_secs_f64(),
                })
            })
            .collect();
        let degraded = state.sync.get(dataset).is_some_and(|s| s.degraded);
        json!({
            "syncReplicas": replicas,
            "waiting": replicas > 0 && !degraded,
            "followers": followers,
        })
    }
}

/// Answers `/$/replication/…` (`rest` is the path after `replication/{name}`).
pub(crate) fn route(
    state: &Arc<State>,
    request: &Request,
    name: &str,
    rest: &[&str],
    params: &Params,
    caller: Option<&crate::auth::Caller>,
) -> Result<Reply, HttpError> {
    // The log and the base are the whole dataset: for the server's admins only.
    let admin = match caller {
        Some(caller) => caller.is_admin(),
        None => crate::admin::authorized(state, request),
    };
    if !admin {
        return Err(match caller {
            Some(caller) => {
                crate::auth::refuse(caller, "Replication needs the group tripletrail:admin")
            }
            None => HttpError(
                401,
                "Replication needs the admin token: Authorization: Bearer <token>".to_owned(),
            ),
        });
    }
    if rest == ["promote"] {
        if *request.method() != Method::Post {
            return Err(HttpError(405, "Promote with a POST".to_owned()));
        }
        return promote(state, name);
    }
    if !matches!(request.method(), Method::Get | Method::Head) {
        return Err(HttpError(405, "Replication answers GET only".to_owned()));
    }
    let entry = state
        .registry
        .read_unpoisoned()
        .resolve(name)
        .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?;
    let Dataset::TripleTrail(store) = entry.engine.store() else {
        return Err(HttpError(
            409,
            format!("{name} is not on the tripletrail store: it has no write-ahead log to follow"),
        ));
    };
    let Some(position) = store.log_position() else {
        return Err(HttpError(
            409,
            format!(
                "{name} is not a writable dataset on disk: it has no write-ahead log to follow"
            ),
        ));
    };
    match rest {
        [] => Ok(json_reply(
            200,
            &json!({
                "dataset": entry.name,
                "timeline": position.timeline,
                "generation": position.generation,
                "length": position.length,
                "role": if state.replicas.lock_unpoisoned().contains_key(&entry.name) {
                    "follower"
                } else {
                    "leader"
                },
                "history": entry.location.as_deref().map(history).unwrap_or_default(),
                "sync": state.acks.json(&entry.name, state.options.sync_replicas),
            }),
        )),
        ["log"] => {
            // Noted as the read comes in, before it waits for a commit: the position asked
            // from is what the follower holds.
            if params.get("timeline") == Some(&position.timeline)
                && let (Some(follower), Some(g), offset) = (
                    params.get("follower"),
                    number(params, "generation")?,
                    number(params, "offset")?.unwrap_or(0),
                )
            {
                state
                    .acks
                    .note(&entry.name, follower, &position.timeline, g, offset);
            }
            log(state, store, &position.timeline, params)
        }
        ["base"] => base(state, &entry),
        _ => Err(HttpError(404, "Not found".to_owned())),
    }
}

/// The file of a store directory that lists the timelines it was promoted from (#217): a
/// JSON array, each entry the `parent` timeline and the position on it (`generation`,
/// `offset`) the replica had applied when it was promoted, and the position on its own
/// `timeline` (`fromGeneration`, `fromLength`) where its log goes on from there. A copy of a
/// base (taken by a follower) carries it on.
const HISTORY: &str = "HISTORY";

/// The promotions in the history of the store directory `dir`, oldest first.
fn history(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join(HISTORY))
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok())
        .unwrap_or_default()
}

/// Written to a temporary file, synced, and renamed over the old one.
fn write_history(dir: &Path, history: &[Value]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = dir.join("HISTORY.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(serde_json::to_string_pretty(history)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(HISTORY))
}

/// Where a follower at `p` goes on reading on `target`, by the promotions of `history`: a
/// promotion from its timeline at its very position, then (for a chain) one from that
/// timeline at the position that one started from, and so on. `None` when no chain of them
/// starts where the follower is: it was behind or ahead of the replica promoted.
fn continued(history: &[Value], p: &Position, target: &str) -> Option<(String, u64, u64)> {
    if p.skip > 0 {
        return None;
    }
    let mut at = (p.timeline.clone(), p.generation, p.offset);
    for _ in 0..history.len() {
        let next = history.iter().find(|h| {
            h["parent"] == at.0.as_str()
                && h["generation"].as_u64() == Some(at.1)
                && h["offset"].as_u64() == Some(at.2)
        })?;
        at = (
            next["timeline"].as_str()?.to_owned(),
            next["fromGeneration"].as_u64()?,
            next["fromLength"].as_u64()?,
        );
        if at.0 == target {
            return Some(at);
        }
    }
    None
}

/// `POST /$/replication/{name}/promote` on a follower (#217): makes its copy the leader. The
/// follower stops reading its leader's log, its copy (which has a timeline of its own) takes
/// writes, and the copy's history notes the promotion: the leader's timeline and the
/// position on it that the copy holds, and where the copy's own log stands. Followers of the
/// old leader that hold exactly that position go on from there (see [`Follower::find_leader`]);
/// the others copy a new base. The old leader must not take writes again: stop it first.
fn promote(state: &Arc<State>, name: &str) -> Result<Reply, HttpError> {
    let not_replica = || {
        HttpError(
            409,
            format!("{name} is not a read replica: there is nothing to promote"),
        )
    };
    let status = state
        .replicas
        .lock_unpoisoned()
        .get(name)
        .cloned()
        .ok_or_else(not_replica)?;
    if !status.serving() {
        return Err(HttpError(
            409,
            format!("{name} is still copying its first base: there is nothing to promote yet"),
        ));
    }
    // The follower waits for this lock to apply a read; it stops once it sees `stop`, set
    // when the promotion is written down. A promotion that fails leaves it following.
    let _applying = status.applying.lock_unpoisoned();
    // Another promotion may have been first.
    if !state
        .replicas
        .lock_unpoisoned()
        .get(name)
        .is_some_and(|s| Arc::ptr_eq(s, &status))
    {
        return Err(not_replica());
    }
    let mut position = Position::read(&status.location, name)
        .ok_or_else(|| HttpError(500, format!("{name} has no replica position on disk")))?;
    let dir = status.location.join(&position.dir);
    let entry = state
        .registry
        .read_unpoisoned()
        .datasets
        .get(name)
        .cloned()
        .filter(|e| e.location.as_deref() == Some(dir.as_path()))
        .ok_or_else(not_replica)?;
    let own = match entry.engine.store() {
        Dataset::TripleTrail(store) => store.log_position(),
        _ => None,
    }
    .ok_or_else(|| HttpError(500, format!("{name}'s copy has no write-ahead log")))?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let mut promotions = history(&dir);
    let promotion = json!({
        "parent": position.timeline,
        "generation": position.generation,
        // A follower still skipping transactions its base held matches no other's position.
        "offset": (position.skip == 0).then_some(position.offset),
        "timeline": own.timeline,
        "fromGeneration": own.generation,
        "fromLength": own.length,
        "promoted": millis,
    });
    promotions.push(promotion.clone());
    let failed = |e: std::io::Error| HttpError(500, format!("promoting {name}: {e}"));
    write_history(&dir, &promotions).map_err(failed)?;
    position.promoted = true;
    if let Err(e) = position.write(&status.location, name) {
        promotions.pop();
        let _ = write_history(&dir, &promotions);
        return Err(failed(e));
    }
    status.stop.store(true, Ordering::SeqCst);
    state.replicas.lock_unpoisoned().remove(name);
    state.registry.write_unpoisoned().datasets.insert(
        name.to_owned(),
        Arc::new(Entry::new(
            name.to_owned(),
            entry.engine.clone(),
            Some(dir),
            Mode::ReadWrite,
        )),
    );
    eprintln!(
        "{name}: promoted to a leader on timeline {}, from {}:{} on timeline {}",
        own.timeline, position.generation, position.offset, position.timeline
    );
    Ok(json_reply(
        200,
        &json!({
            "dataset": name,
            "timeline": own.timeline,
            "generation": own.generation,
            "length": own.length,
            "promotion": promotion,
        }),
    ))
}

fn number(params: &Params, name: &str) -> Result<Option<u64>, HttpError> {
    params
        .get(name)
        .map(|v| {
            v.parse()
                .map_err(|_| HttpError::bad_request(format!("{name} takes a number, not {v:?}")))
        })
        .transpose()
}

fn log(state: &State, store: &Store, timeline: &str, params: &Params) -> Result<Reply, HttpError> {
    let asked = params
        .get("timeline")
        .ok_or_else(|| HttpError::bad_request("give the timeline"))?;
    if asked != timeline {
        return Err(HttpError(
            409,
            format!("the dataset is on timeline {timeline} now, not {asked}: copy a new base"),
        ));
    }
    let generation = number(params, "generation")?
        .ok_or_else(|| HttpError::bad_request("give the generation"))?;
    let offset = number(params, "offset")?.unwrap_or(0);
    let max = number(params, "max")?.map_or(MAX_READ, |m| m.clamp(1, 1 << 30) as usize);
    let wait = number(params, "wait")?
        .map_or(Duration::ZERO, |ms| Duration::from_millis(ms).min(MAX_WAIT));
    let deadline = Instant::now() + wait;
    loop {
        let seen = state.acks.commits();
        let read = store
            .read_log(generation, offset, max)
            .map_err(|e| HttpError(500, e.to_string()))?;
        match read {
            LogRead::Gone => {
                return Err(HttpError(
                    410,
                    format!(
                        "the log of generation {generation} is gone (or shorter than {offset} bytes): copy a new base"
                    ),
                ));
            }
            LogRead::Bytes {
                bytes,
                end,
                retired,
            } => {
                if bytes.is_empty()
                    && !retired
                    && Instant::now() < deadline
                    && !state.stopping.load(Ordering::Relaxed)
                {
                    // Woken by the next write this server answers; and looks again every
                    // 20 ms anyway, for writes that come another way.
                    state.acks.wait_commit(seen, Duration::from_millis(20));
                    continue;
                }
                let mut reply = Reply::bytes(200, "application/octet-stream", bytes);
                reply
                    .headers
                    .push(("X-TripleTrail-Log-End", end.to_string()));
                // A follower of an older format stops rather than misread the entries (#221).
                reply.headers.push((
                    "X-TripleTrail-Store-Format",
                    tripletrail::store::STORE_FORMAT.to_string(),
                ));
                reply
                    .headers
                    .push(("X-TripleTrail-Log-Retired", retired.to_string()));
                return Ok(reply);
            }
        }
    }
}

/// A base backup of the dataset, as a tar archive written while it is sent: saved into a
/// directory of its own first (next to the backups, or the datasets), deleted once sent.
fn base(state: &State, entry: &Entry) -> Result<Reply, HttpError> {
    let Dataset::TripleTrail(store) = entry.engine.store() else {
        unreachable!("checked");
    };
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = state
        .options
        .backup_dir
        .clone()
        .or_else(|| state.options.location.clone())
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&parent).map_err(|e| HttpError(500, e.to_string()))?;
    let dir = parent.join(format!(
        ".replica-base-{}-{}-{}",
        entry.name,
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = store.save_base(&dir) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(HttpError(
            500,
            format!("saving a base of {}: {e}", entry.name),
        ));
    }
    let (reader, writer) = std::io::pipe().map_err(|e| HttpError(500, e.to_string()))?;
    std::thread::spawn(move || {
        let mut tar = tar::Builder::new(writer);
        tar.follow_symlinks(false);
        // A follower that hangs up ends the copy.
        let _ = tar.append_dir_all(".", &dir).and_then(|()| tar.finish());
        drop(tar);
        let _ = std::fs::remove_dir_all(&dir);
    });
    Ok(Reply {
        status: 200,
        content_type: Some("application/x-tar".to_owned()),
        body: Box::new(reader),
        length: None,
        headers: Vec::new(),
    })
}

/// Waits until the read replica `name` has applied its leader's log up to `version` (a
/// `TripleTrail-Min-Version` header: `timeline:generation:length`, as the leader's
/// `TripleTrail-Version` gave it after a write), for up to 10 s (503 then). Datasets that are
/// not replicas answer at once: they hold every write made to them.
pub(crate) fn await_version(state: &State, name: &str, version: &str) -> Result<(), HttpError> {
    let Some(status) = state.replicas.lock_unpoisoned().get(name).cloned() else {
        return Ok(());
    };
    let parsed = version.split(':').collect::<Vec<_>>();
    let (timeline, generation, length) = match parsed.as_slice() {
        [t, g, l] => match (g.parse::<u64>(), l.parse::<u64>()) {
            (Ok(g), Ok(l)) if !t.is_empty() => (*t, g, l),
            _ => return Err(bad_version(version)),
        },
        _ => return Err(bad_version(version)),
    };
    // What the replica knows now: done, or a version it cannot reach by waiting.
    let check = || -> Option<Result<(), HttpError>> {
        let s = status.inner.lock_unpoisoned();
        if !s.serving {
            return None;
        }
        if s.timeline != timeline {
            return Some(Err(HttpError(
                503,
                format!(
                    "{name} follows timeline {} of its leader, not {timeline}: the leader's dataset was replaced",
                    s.timeline
                ),
            )));
        }
        if (s.generation, s.offset) >= (generation, length) {
            return Some(Ok(()));
        }
        None
    };
    if let Some(done) = check() {
        return done;
    }
    // Each wait holds a worker thread: at most half of the server's wait, on all its replicas
    // together; the others answer at once.
    let cap = (state.options.threads / 2).max(1);
    struct Waiting<'a>(&'a std::sync::atomic::AtomicUsize);
    impl Drop for Waiting<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    if state.replica_waits.fetch_add(1, Ordering::SeqCst) >= cap {
        state.replica_waits.fetch_sub(1, Ordering::SeqCst);
        return Err(HttpError(
            503,
            format!(
                "{name} has not applied the leader's log up to {version} yet, and enough requests wait for it already"
            ),
        ));
    }
    let _waiting = Waiting(&state.replica_waits);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(done) = check() {
            return done;
        }
        if Instant::now() >= deadline || state.stopping.load(Ordering::Relaxed) {
            return Err(HttpError(
                503,
                format!("{name} has not applied the leader's log up to {version} yet"),
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn bad_version(version: &str) -> HttpError {
    HttpError::bad_request(format!(
        "TripleTrail-Min-Version takes timeline:generation:length (the TripleTrail-Version of a write), not {version:?}"
    ))
}

/// A dataset to follow: served as `name`, copied from the leader's dataset at one of `urls`
/// (`http://leader:7878/ds`). With more than one, the follower reads from the first that
/// answers, and moves to another when that one is down or on another timeline, as long as
/// the other is the leader of its timeline or of one promoted from it (see [`promote`]).
#[derive(Clone, Debug)]
pub struct Follow {
    pub name: String,
    pub urls: Vec<String>,
    /// The URL read from now.
    current: usize,
    /// The leader's admin token (or an API key of its admin group).
    pub token: Option<String>,
    /// How long a read of the leader's log waits for a commit before the follower asks again.
    pub wait: Duration,
}

impl Follow {
    /// `NAME=URL`.
    pub fn parse(arg: &str, token: Option<String>, wait: Duration) -> Result<Self, String> {
        let (name, url) = arg
            .split_once('=')
            .filter(|(n, u)| !n.is_empty() && !u.is_empty())
            .ok_or_else(|| format!("--follow {arg:?}: give NAME=URL (http://leader:7878/ds)"))?;
        let mut follow = Self {
            name: name.to_owned(),
            urls: Vec::new(),
            current: 0,
            token,
            wait,
        };
        follow.add_url(url)?;
        Ok(follow)
    }

    /// Adds another URL of the leader's dataset (a server that is, or may be promoted to,
    /// the leader): `--follow kg=URL1,URL2`.
    pub fn add_url(&mut self, url: &str) -> Result<(), String> {
        let url = url.trim_end_matches('/');
        if !url.rsplit_once('/').is_some_and(|(base, d)| {
            !d.is_empty() && (base.starts_with("http://") || base.starts_with("https://"))
        }) {
            return Err(format!(
                "--follow {}={url}: the URL is the leader's dataset, as http://leader:7878/ds",
                self.name
            ));
        }
        if !self.urls.iter().any(|u| u == url) {
            self.urls.push(url.to_owned());
        }
        Ok(())
    }

    /// The URL read from now.
    pub fn url(&self) -> &str {
        &self.urls[self.current]
    }

    fn get(&self, agent: &ureq::Agent, path: &str) -> ureq::Request {
        self.get_at(agent, self.current, path)
    }

    /// A GET of `/$/replication/{dataset}{path}` on the server of the `i`th URL.
    fn get_at(&self, agent: &ureq::Agent, i: usize, path: &str) -> ureq::Request {
        let (base, dataset) = self.urls[i].rsplit_once('/').expect("checked by add_url");
        let request = agent.get(&format!("{base}/$/replication/{dataset}{path}"));
        match &self.token {
            Some(t) => request.set("Authorization", &format!("Bearer {t}")),
            None => request,
        }
    }
}

/// How a follower is doing, for `/$/status` and `/$/datasets/{name}`.
#[derive(Debug)]
pub(crate) struct Status {
    inner: Mutex<StatusInner>,
    /// Where the follower keeps its copy and position.
    location: PathBuf,
    /// Set by a promotion: the follower stops.
    stop: std::sync::atomic::AtomicBool,
    /// Held while the follower applies a read and writes its position down, so that a
    /// promotion sees neither half done.
    applying: Mutex<()>,
}

#[derive(Debug, Default)]
struct StatusInner {
    /// The URL of the leader read from.
    leader: String,
    state: &'static str,
    timeline: String,
    generation: u64,
    offset: u64,
    /// The length of the log being read, as the leader last said.
    end: u64,
    retired: bool,
    /// Transactions applied since the server started, and bases copied.
    applied: u64,
    bases: u64,
    /// The commit time (by the leader's clock) of the last transaction applied.
    last_commit: Option<u64>,
    last_contact: Option<SystemTime>,
    /// When the follower last had everything the leader had on disk.
    caught_up: Option<SystemTime>,
    error: Option<String>,
    /// Whether a copy is served (none until the first base is in place).
    serving: bool,
}

impl Status {
    fn set(&self, f: impl FnOnce(&mut StatusInner)) {
        f(&mut self.inner.lock_unpoisoned());
    }

    /// Whether a copy is served yet.
    pub(crate) fn serving(&self) -> bool {
        self.inner.lock_unpoisoned().serving
    }

    pub(crate) fn json(&self) -> Value {
        let s = self.inner.lock_unpoisoned();
        let behind = !(s.offset >= s.end && !s.retired);
        let seconds = |t: Option<SystemTime>| {
            t.and_then(|t| SystemTime::now().duration_since(t).ok())
                .map(|d| d.as_secs_f64())
        };
        let millis = |t: Option<SystemTime>| {
            t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
        };
        json!({
            "leader": s.leader,
            "state": s.state,
            "timeline": s.timeline,
            "generation": s.generation,
            "offset": s.offset,
            "leaderLength": s.end,
            "behindBytes": if s.retired { None } else { Some(s.end.saturating_sub(s.offset)) },
            "lagSeconds": if behind { seconds(s.caught_up) } else { Some(0.0) },
            "applied": s.applied,
            "bases": s.bases,
            "lastCommit": s.last_commit,
            "lastContact": millis(s.last_contact),
            "error": s.error,
        })
    }
}

/// Where a follower is, kept in `<location>/<name>.replica` so that a restart goes on from
/// there: the store directory holding the copy, and the position in the leader's logs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Position {
    /// The directory's name in the location.
    dir: String,
    timeline: String,
    generation: u64,
    offset: u64,
    /// Transactions at the start of the log that the base holds already.
    skip: u64,
    /// Promoted to a leader ([`promote`]): served writable, no longer following.
    promoted: bool,
}

impl Position {
    fn file(location: &Path, name: &str) -> PathBuf {
        location.join(format!("{name}.replica"))
    }

    fn read(location: &Path, name: &str) -> Option<Self> {
        let text = std::fs::read_to_string(Self::file(location, name)).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        Some(Self {
            dir: v["dir"].as_str()?.to_owned(),
            timeline: v["timeline"].as_str()?.to_owned(),
            generation: v["generation"].as_u64()?,
            offset: v["offset"].as_u64()?,
            skip: v["skip"].as_u64()?,
            promoted: v["promoted"].as_bool().unwrap_or(false),
        })
    }

    /// Written to a temporary file, synced, and renamed over the old one. Applying again what
    /// a crash kept from being written down changes nothing (see [`Engine::apply_logged`]).
    fn write(&self, location: &Path, name: &str) -> std::io::Result<()> {
        use std::io::Write;
        let path = Self::file(location, name);
        let tmp = path.with_extension("replica.tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(
            json!({
                "dir": self.dir,
                "timeline": self.timeline,
                "generation": self.generation,
                "offset": self.offset,
                "skip": self.skip,
                "promoted": self.promoted,
            })
            .to_string()
            .as_bytes(),
        )?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A follower: opened or copied, then kept up to date on a thread of its own.
struct Follower {
    follow: Follow,
    location: PathBuf,
    handle: Handle,
    agent: ureq::Agent,
    status: Arc<Status>,
    position: Position,
    engine: Engine,
    max: usize,
    /// Names this follower to the leader, which counts it for `--sync-replicas`.
    id: String,
}

/// Stops the server's followers between two reads, for a shutdown: waits for one that is
/// applying a read to finish it, and none applies another or copies a new base, so their
/// copies are not written while the datasets are checkpointed or after the server stopped.
pub(crate) fn stop_followers(state: &State) {
    let replicas: Vec<Arc<Status>> = state.replicas.lock_unpoisoned().values().cloned().collect();
    for status in replicas {
        let _applying = status.applying.lock_unpoisoned();
        status.stop.store(true, Ordering::SeqCst);
    }
}

/// Starts following `follow` into `location`: opens the copy a previous run left, or copies a
/// base from the leader (which can take long; the dataset is served once it is in place),
/// then reads the leader's log on a thread of its own.
pub fn follow(handle: &Handle, location: &Path, follow: Follow) -> Result<(), BoxError> {
    std::fs::create_dir_all(location)?;
    // Promoted by an earlier run: a leader now, whatever the command line says.
    if serve_promoted(handle, location, &follow.name, Mode::ReadWrite)? {
        eprintln!(
            "{}: promoted to a leader earlier, so not following {}",
            follow.name,
            follow.url()
        );
        return Ok(());
    }
    let status = Arc::new(Status {
        inner: Mutex::new(StatusInner {
            leader: follow.url().to_owned(),
            state: "starting",
            ..Default::default()
        }),
        location: location.to_owned(),
        stop: Default::default(),
        applying: Mutex::new(()),
    });
    handle
        .state
        .replicas
        .lock_unpoisoned()
        .insert(follow.name.clone(), Arc::clone(&status));
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .build();
    let handle = handle.clone();
    let location = location.to_owned();
    // On a thread of its own: the leader may be down, or still opening its datasets, when
    // the follower starts (both started together, say), and the other datasets are served
    // meanwhile. `/$/ready` answers 503 until the copy is in place.
    std::thread::Builder::new()
        .name(format!("follow-{}", follow.name))
        .spawn(move || {
            let mut follow = follow;
            let mut backoff = Duration::from_millis(500);
            let (position, engine) = loop {
                if handle.state.stopping.load(Ordering::Relaxed) {
                    return;
                }
                match open_or_copy(&follow, &location, &agent, &status) {
                    Ok(opened) => break opened,
                    Err(e) => {
                        fail(
                            &follow,
                            &status,
                            &format!("copying a base: {e}"),
                            &mut backoff,
                        );
                        // Then from the next of the leader's URLs.
                        follow.current = (follow.current + 1) % follow.urls.len();
                        let url = follow.url().to_owned();
                        status.set(|s| s.leader = url);
                    }
                }
            };
            remove_other_copies(&location, &follow.name, &position.dir);
            // The spatial index is built in the background from here on, as for any dataset
            // opened at start-up (see /$/ready).
            engine.warm_up();
            handle.attach_as(
                &follow.name,
                engine.clone(),
                Some(location.join(&position.dir)),
                Mode::ReadOnly,
            );
            status.set(|s| {
                s.state = "following";
                s.error = None;
                s.serving = true;
                s.timeline = position.timeline.clone();
                s.generation = position.generation;
                s.offset = position.offset;
            });
            eprintln!("{}: following {}", follow.name, follow.url());
            Follower {
                follow,
                location,
                handle,
                agent,
                status,
                position,
                engine,
                max: MAX_READ,
                id: follower_id(),
            }
            .run();
        })?;
    Ok(())
}

/// The datasets of `location` that were read replicas promoted to leaders (#217): each
/// `NAME.replica` there that says so. A start serves them ([`serve_promoted`]) with or without
/// their `--follow`.
pub fn promoted_replicas(location: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(location) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().into_string().ok()?;
            let name = file.strip_suffix(".replica")?;
            Position::read(location, name)
                .is_some_and(|p| p.promoted)
                .then(|| name.to_owned())
        })
        .collect();
    names.sort();
    names
}

/// Serves the copy of the replica `name` in `location` as a leader (`mode` read-write, unless
/// the server is read-only) if it was promoted ([`promote`]); `false` if it was not.
pub fn serve_promoted(
    handle: &Handle,
    location: &Path,
    name: &str,
    mode: Mode,
) -> Result<bool, BoxError> {
    let Some(p) = Position::read(location, name).filter(|p| p.promoted) else {
        return Ok(false);
    };
    let dir = location.join(&p.dir);
    let engine = crate::admin::open_dataset(&dir, mode)?;
    engine.warm_up();
    handle.attach_as(name, engine, Some(dir.clone()), mode);
    eprintln!(
        "{name}: serving the replica promoted to a leader ({}, {})",
        dir.display(),
        if mode == Mode::ReadOnly {
            "read-only"
        } else {
            "read-write"
        }
    );
    Ok(true)
}

/// A name for a follower, different for every follower the leader may see at once.
fn follower_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    format!("{:016x}{:08x}", nanos, std::process::id())
}

/// Opens the copy a previous run left, or copies a base from the leader.
fn open_or_copy(
    follow: &Follow,
    location: &Path,
    agent: &ureq::Agent,
    status: &Status,
) -> Result<(Position, Engine), BoxError> {
    let opened = Position::read(location, &follow.name).and_then(|p| {
        let dir = location.join(&p.dir);
        if !dir.join("CURRENT").is_file() {
            return None;
        }
        match crate::admin::open_dataset(&dir, Mode::ReadWrite) {
            Ok(engine) => Some((p, engine)),
            Err(e) => {
                eprintln!(
                    "{}: opening the copy in {}: {e}; copying a new base",
                    follow.name,
                    dir.display()
                );
                None
            }
        }
    });
    match opened {
        Some(opened) => Ok(opened),
        None => copy_base(follow, location, agent, status, None),
    }
}

/// Notes a failure in the status and waits, longer each time (up to 30 s).
fn fail(follow: &Follow, status: &Status, error: &str, backoff: &mut Duration) {
    eprintln!("{}: following {}: {error}", follow.name, follow.url());
    status.set(|s| {
        s.state = "error";
        s.error = Some(error.to_owned());
    });
    std::thread::sleep(*backoff);
    *backoff = (*backoff * 2).min(Duration::from_secs(30));
}

/// Copies a base of the leader's dataset into a new directory of the location and opens it.
fn copy_base(
    follow: &Follow,
    location: &Path,
    agent: &ureq::Agent,
    status: &Status,
    previous: Option<&str>,
) -> Result<(Position, Engine), BoxError> {
    status.set(|s| s.state = "copying");
    let started = Instant::now();
    // A directory name not in use: `<name>.r<n>`.
    let mut n = previous
        .and_then(|p| p.rsplit_once(".r")?.1.parse::<u64>().ok())
        .map_or(1, |n| n + 1);
    let dir_name = loop {
        let candidate = format!("{}.r{n}", follow.name);
        if !location.join(&candidate).exists() {
            break candidate;
        }
        n += 1;
    };
    let dir = location.join(&dir_name);
    let copied = (|| -> Result<(Position, Engine), BoxError> {
        // Of several URLs, only the leader's: a replica's copy lags behind.
        if follow.urls.len() > 1 {
            let v: Value = serde_json::from_str(
                &follow
                    .get(agent, "")
                    .timeout(Duration::from_secs(5))
                    .call()?
                    .into_string()?,
            )?;
            if v["role"] != "leader" {
                return Err(format!("{} is not the leader", follow.url()).into());
            }
        }
        let response = follow.get(agent, "/base").call()?;
        tar::Archive::new(response.into_reader()).unpack(&dir)?;
        let (timeline, generation, skip) =
            Store::base_position(&dir)?.ok_or("the leader sent a base without a BASE file")?;
        let engine = crate::admin::open_dataset(&dir, Mode::ReadWrite)?;
        Ok((
            Position {
                dir: dir_name.clone(),
                timeline,
                generation,
                offset: 0,
                skip,
                promoted: false,
            },
            engine,
        ))
    })();
    let (position, engine) = match copied {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    position.write(location, &follow.name)?;
    status.set(|s| s.bases += 1);
    eprintln!(
        "{}: copied a base of {} in {:.1} s",
        follow.name,
        follow.url(),
        started.elapsed().as_secs_f64()
    );
    Ok((position, engine))
}

/// Deletes the copies of earlier bases (`<name>.r<n>` other than `keep`, and their networks).
fn remove_other_copies(location: &Path, name: &str, keep: &str) {
    let Ok(entries) = std::fs::read_dir(location) else {
        return;
    };
    let prefix = format!("{name}.r");
    for entry in entries.flatten() {
        let file = entry.file_name();
        let file = file.to_string_lossy();
        let Some(rest) = file.strip_prefix(&prefix) else {
            continue;
        };
        let n = rest.strip_suffix(".networks").unwrap_or(rest);
        if n.parse::<u64>().is_err() || file == keep || file == format!("{keep}.networks") {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

/// What the follower must do after a read.
enum Next {
    /// Read again at once: there is more.
    More,
    /// Read again (the read waited for a commit already).
    Wait,
    /// Copy a new base: the leader's dataset is another now, or its log is gone.
    Rebase(String),
    /// Stop: promoted.
    Stopped,
}

impl Follower {
    fn run(&mut self) {
        let mut backoff = Duration::from_millis(500);
        loop {
            if self.handle.state.stopping.load(Ordering::Relaxed)
                || self.status.stop.load(Ordering::SeqCst)
                || !self.still_served()
            {
                return;
            }
            let next = match self.step() {
                // Another of the leader's URLs may be the leader now.
                Err(e) if self.follow.urls.len() > 1 => match self.find_leader(false) {
                    Ok(Some(next)) => Ok(next),
                    Ok(None) => Err(e),
                    Err(other) => {
                        Err(format!("{e} (and looking for another leader: {other})").into())
                    }
                },
                next => next,
            };
            match next {
                Ok(Next::More) | Ok(Next::Wait) => {
                    backoff = Duration::from_millis(500);
                }
                Ok(Next::Stopped) => return,
                Ok(Next::Rebase(why)) => {
                    eprintln!("{}: {why}; copying a new base", self.follow.name);
                    let status = Arc::clone(&self.status);
                    let applying = status.applying.lock_unpoisoned();
                    if self.status.stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let rebased = self.rebase();
                    drop(applying);
                    if let Err(e) = rebased {
                        self.failed(&format!("copying a new base: {e}"), &mut backoff);
                    }
                }
                Err(e) => self.failed(&e.to_string(), &mut backoff),
            }
        }
    }

    /// Asks each of the leader's URLs (the one read from first) where its dataset stands,
    /// for a leader to go on with: one on this follower's timeline, or one promoted from it
    /// at this follower's position (its history says where), whose log the follower then
    /// reads from where the promotion started it. `Next::Rebase` names a leader promoted from
    /// this timeline at another position (the follower was behind, or ahead, of the replica
    /// promoted): it copies a new base from that one. `None` when no URL is such a leader.
    fn find_leader(&mut self, conflict: bool) -> Result<Option<Next>, BoxError> {
        let n = self.follow.urls.len();
        let mut successor = None;
        // After a failed read, only another URL: the one read from may answer its status
        // while its log fails. On a 409 it answered: it is on another timeline, and may
        // have been promoted.
        let skip = usize::from(!conflict);
        for i in (skip..n).map(|k| (self.follow.current + k) % n) {
            let call = self
                .follow
                .get_at(&self.agent, i, "")
                .timeout(Duration::from_secs(5))
                .call();
            let Ok(v) = call
                .map_err(BoxError::from)
                .and_then(|r| Ok(serde_json::from_str::<Value>(&r.into_string()?)?))
            else {
                continue;
            };
            if v["role"] != "leader" {
                continue;
            }
            let Some(timeline) = v["timeline"].as_str() else {
                continue;
            };
            let history = v["history"].as_array().map_or(&[][..], Vec::as_slice);
            let p = &self.position;
            if timeline == p.timeline {
                if i == self.follow.current {
                    continue;
                }
                self.switch_to(i, None)?;
                return Ok(Some(Next::More));
            }
            if let Some(at) = continued(history, p, timeline) {
                self.switch_to(i, Some(at))?;
                return Ok(Some(Next::More));
            }
            if successor.is_none() && history.iter().any(|h| h["parent"] == p.timeline.as_str()) {
                successor = Some(i);
            }
        }
        Ok(match successor {
            Some(i) => {
                self.follow.current = i;
                let url = self.follow.url().to_owned();
                self.status.set(|s| s.leader = url.clone());
                Some(Next::Rebase(format!(
                    "{url} was promoted from this follower's timeline, but at another position than this follower's"
                )))
            }
            None => None,
        })
    }

    /// Reads from the `i`th URL from now on, from `at` (timeline, generation, offset) when
    /// it is on another timeline.
    fn switch_to(&mut self, i: usize, at: Option<(String, u64, u64)>) -> Result<(), BoxError> {
        let status = Arc::clone(&self.status);
        let _applying = status.applying.lock_unpoisoned();
        self.follow.current = i;
        if let Some((timeline, generation, offset)) = at {
            self.position.timeline = timeline;
            self.position.generation = generation;
            self.position.offset = offset;
            self.position.skip = 0;
            self.position.write(&self.location, &self.follow.name)?;
        }
        let (url, p) = (self.follow.url().to_owned(), self.position.clone());
        eprintln!(
            "{}: following {url}, on timeline {} from {}:{}",
            self.follow.name, p.timeline, p.generation, p.offset
        );
        self.status.set(|s| {
            s.leader = url;
            s.timeline = p.timeline;
            s.generation = p.generation;
            s.offset = p.offset;
            s.end = 0;
            s.retired = false;
        });
        Ok(())
    }

    fn failed(&self, error: &str, backoff: &mut Duration) {
        fail(&self.follow, &self.status, error, backoff);
    }

    /// Whether the dataset served under the name is still this copy (not detached, or replaced
    /// through the admin API).
    fn still_served(&self) -> bool {
        let dir = self.location.join(&self.position.dir);
        self.handle
            .state
            .registry
            .read_unpoisoned()
            .datasets
            .get(&self.follow.name)
            .is_some_and(|e| e.location.as_deref() == Some(dir.as_path()))
    }

    fn step(&mut self) -> Result<Next, BoxError> {
        let p = &self.position;
        let wait = self.follow.wait.as_millis().to_string();
        let request = self
            .follow
            .get(&self.agent, "/log")
            .query("timeline", &p.timeline)
            .query("generation", &p.generation.to_string())
            .query("offset", &p.offset.to_string())
            .query("max", &self.max.to_string())
            .query("wait", &wait)
            .query("follower", &self.id)
            // A leader that went away without closing the connection (its machine down) is
            // given up on soon after its wait, so that another of its URLs can be tried.
            .timeout(self.follow.wait + Duration::from_secs(5));
        let response = match request.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code @ (409 | 410), r)) => {
                let why = r.into_string().unwrap_or_default();
                // On another timeline: the leader (or another of its URLs) may have been
                // promoted from this follower's timeline, at this follower's position.
                if code == 409
                    && let Some(next) = self.find_leader(true)?
                {
                    return Ok(next);
                }
                return Ok(Next::Rebase(format!(
                    "the leader answered {code}: {}",
                    why.trim()
                )));
            }
            Err(e) => return Err(e.into()),
        };
        let header = |name: &str| response.header(name).and_then(|v| v.parse::<u64>().ok());
        let end =
            header("X-TripleTrail-Log-End").ok_or("no X-TripleTrail-Log-End from the leader")?;
        // A leader upgraded to a newer store format writes log entries this build may not
        // read: nothing is applied, and the read is tried again (#221).
        if let Some(format) = header("X-TripleTrail-Store-Format")
            && format > u64::from(tripletrail::store::STORE_FORMAT)
        {
            return Err(format!(
                "the leader's log is in store format {format}, written by a newer TripleTrail: this one ({}) reads up to format {}. Upgrade this replica",
                env!("CARGO_PKG_VERSION"),
                tripletrail::store::STORE_FORMAT
            )
            .into());
        }
        let retired = response.header("X-TripleTrail-Log-Retired") == Some("true");
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes)?;
        let read_whole = bytes.len() >= self.max;
        let _applying = self.status.applying.lock_unpoisoned();
        if self.status.stop.load(Ordering::SeqCst) {
            return Ok(Next::Stopped);
        }
        // Applies every complete transaction of the read, in order, in one commit (#217): one
        // commit per transaction kept the follower far behind a leader taking a few thousand
        // small writes a second. The commit holds what the transactions one after the other
        // do, and nothing of it is applied if it fails, so the position stays where it was.
        let mut changes = Vec::new();
        let mut load = None;
        let mut applied = 0;
        let mut last_commit = None;
        let mut skip = self.position.skip;
        let consumed = tripletrail::store::scan_log(&bytes, |entry| match entry {
            LogEntry::Transaction(time, quads) => {
                if skip > 0 {
                    skip -= 1;
                } else {
                    changes.extend(quads);
                }
                applied += 1;
                last_commit = time.or(last_commit);
                true
            }
            LogEntry::Load(time) => {
                load = Some(time);
                false
            }
        })?;
        if !changes.is_empty() {
            self.engine
                .apply_logged(&changes)
                .map_err(|e| format!("applying transactions of the leader: {e}"))?;
        }
        self.position.offset += consumed;
        self.position.skip = skip;
        let moved = consumed > 0;
        if load.is_some() {
            return Ok(Next::Rebase(
                "the leader bulk loaded data, which its log does not hold".to_owned(),
            ));
        }
        let at_end = self.position.offset >= end;
        let next_log = retired && at_end;
        if next_log {
            self.position.generation += 1;
            self.position.offset = 0;
            self.position.skip = 0;
        }
        if moved || next_log {
            self.position.write(&self.location, &self.follow.name)?;
        }
        // A transaction larger than a whole read: read more at once.
        if !moved && read_whole {
            self.max = self.max.saturating_mul(2).min(1 << 30);
        } else if moved {
            self.max = MAX_READ.max(self.max / 2);
        }
        let now = SystemTime::now();
        let position = self.position.clone();
        self.status.set(|s| {
            s.state = "following";
            s.error = None;
            s.timeline = position.timeline.clone();
            s.generation = position.generation;
            s.offset = position.offset;
            s.end = if next_log { 0 } else { end };
            s.retired = retired && !next_log;
            s.applied += applied;
            s.last_commit = last_commit.or(s.last_commit);
            s.last_contact = Some(now);
            if at_end && !retired {
                s.caught_up = Some(now);
            }
        });
        Ok(if moved || next_log || read_whole {
            Next::More
        } else {
            Next::Wait
        })
    }

    /// Copies a new base, serves it in place of the copy, and deletes the old copy.
    fn rebase(&mut self) -> Result<(), BoxError> {
        let (position, engine) = copy_base(
            &self.follow,
            &self.location,
            &self.agent,
            &self.status,
            Some(&self.position.dir),
        )?;
        let old = std::mem::replace(&mut self.position, position);
        self.engine = engine.clone();
        engine.warm_up();
        self.handle.attach_as(
            &self.follow.name,
            engine,
            Some(self.location.join(&self.position.dir)),
            Mode::ReadOnly,
        );
        self.max = MAX_READ;
        let position = self.position.clone();
        self.status.set(|s| {
            s.state = "following";
            s.error = None;
            s.timeline = position.timeline.clone();
            s.generation = position.generation;
            s.offset = 0;
            s.end = 0;
            s.retired = false;
        });
        // Requests still reading the old copy keep it open until they end, and a merge may
        // still run in it: deleted a minute later (or, if that fails, at the next start).
        let location = self.location.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(60));
            let _ = std::fs::remove_dir_all(location.join(&old.dir));
            let _ = std::fs::remove_dir_all(location.join(format!("{}.networks", old.dir)));
        });
        Ok(())
    }
}
