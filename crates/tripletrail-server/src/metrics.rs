//! `GET /$/metrics`: the server's counters in the Prometheus text exposition format (#220), for
//! a Prometheus scrape job or anything that reads it (Grafana Agent, VictoriaMetrics, ...).
//!
//! Per dataset: requests by operation and status, a latency histogram per operation, triples,
//! named graphs, memory-mapped bytes, the result cache, and whether the spatial index is
//! built. For on-disk tripletrail stores: the write-ahead log, commits, checkpoints, runs and
//! merges, and the check of the files. For read replicas: their lag, and on a leader the
//! replicas it hears from (#217). Server-wide: requests in flight, uptime, the process's
//! resident memory, and what became of the BIND chains of update WHEREs worth evaluating in
//! parallel.

use crate::admin::{Entry, Operation};
use crate::{Reply, State};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tripletrail::geo::sync::{LockUnpoisoned, RwLockUnpoisoned};
use tripletrail::store::Dataset;

/// Upper bounds of the latency buckets, in seconds.
const BUCKETS: [f64; 14] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

/// A latency histogram: a count per bucket (not cumulative; summed when written), the count
/// and the sum.
#[derive(Default)]
pub(crate) struct Histogram {
    buckets: [AtomicU64; BUCKETS.len()],
    count: AtomicU64,
    micros: AtomicU64,
}

impl Histogram {
    fn observe(&self, elapsed: Duration) {
        let seconds = elapsed.as_secs_f64();
        if let Some(i) = BUCKETS.iter().position(|&b| seconds <= b) {
            self.buckets[i].fetch_add(1, Ordering::Relaxed);
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.micros.fetch_add(
            u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

/// What a dataset's requests took, by operation and status.
#[derive(Default)]
pub(crate) struct Timings {
    /// Per operation, in [`OPERATIONS`] order.
    latency: [Histogram; 5],
    /// Requests by (operation, status code).
    statuses: Mutex<BTreeMap<(&'static str, u16), u64>>,
    /// Requests stopped or cut by a budget (#189), by [`Trip`].
    trips: Mutex<BTreeMap<Trip, u64>>,
}

/// The budget a request ran into (#189).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Trip {
    /// Stopped by its timeout (503).
    Timeout,
    /// Stopped by the query memory limit (503).
    Memory,
    /// Refused for answering more rows than `max-rows` (413).
    Rows,
    /// Cut at `max-rows` with `partial=true` (200 with `X-Result-Incomplete`).
    Partial,
}

impl Trip {
    /// The `budget` label values of `tripletrail_budget_trips_total`.
    const ALL: [(Trip, &'static str); 3] = [
        (Trip::Memory, "memory"),
        (Trip::Rows, "rows"),
        (Trip::Partial, "partial"),
    ];

    /// The budget behind an error answer, from its status and message.
    pub(crate) fn of_error(status: u16, message: &str) -> Option<Self> {
        match status {
            503 if message.contains("more memory than its limit") => Some(Self::Memory),
            503 if message.contains("timeout") => Some(Self::Timeout),
            413 if message.contains("the most this server answers") => Some(Self::Rows),
            _ => None,
        }
    }
}

/// The `operation` label values; `other` is a request the router did not attribute (a bad
/// path under the dataset, a service description, ...).
const OPERATIONS: [&str; 5] = ["query", "update", "read", "upload", "other"];

fn slot(operation: Option<Operation>) -> usize {
    match operation {
        Some(Operation::Query) => 0,
        Some(Operation::Update) => 1,
        Some(Operation::Read) => 2,
        Some(Operation::Upload) => 3,
        None => 4,
    }
}

impl Timings {
    pub(crate) fn record(&self, operation: Option<Operation>, status: u16, elapsed: Duration) {
        let i = slot(operation);
        self.latency[i].observe(elapsed);
        *self
            .statuses
            .lock_unpoisoned()
            .entry((OPERATIONS[i], status))
            .or_default() += 1;
    }

    pub(crate) fn trip(&self, trip: Trip) {
        *self.trips.lock_unpoisoned().entry(trip).or_default() += 1;
    }

    fn trips(&self, trip: Trip) -> u64 {
        self.trips
            .lock_unpoisoned()
            .get(&trip)
            .copied()
            .unwrap_or_default()
    }
}

/// One metric family being written: its `# HELP` / `# TYPE` lines come once, before its first
/// sample.
struct Out {
    text: String,
}

impl Out {
    fn family(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.text, "# HELP {name} {help}\n# TYPE {name} {kind}");
    }

    fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: impl std::fmt::Display) {
        self.text.push_str(name);
        if !labels.is_empty() {
            self.text.push('{');
            for (i, (k, v)) in labels.iter().enumerate() {
                if i > 0 {
                    self.text.push(',');
                }
                let _ = write!(self.text, "{k}=\"{}\"", escape(v));
            }
            self.text.push('}');
        }
        let _ = writeln!(self.text, " {value}");
    }
}

/// A label value with `\`, `"` and line ends escaped.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(unix)]
fn page_size() -> u64 {
    // SAFETY: sysconf reads a constant of the running kernel.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size).ok().filter(|&s| s > 0).unwrap_or(4096)
}

#[cfg(not(unix))]
fn page_size() -> u64 {
    4096
}

/// The resident memory of this process, in bytes (Linux only).
fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * page_size())
}

/// The families of the writable on-disk tripletrail stores (#220): their write-ahead log, commits,
/// checkpoints, runs and merges, and the check of their files.
fn storage(out: &mut Out, entries: &[std::sync::Arc<Entry>]) {
    let stores: Vec<(&str, &tripletrail::store::Store)> = entries
        .iter()
        .filter_map(|e| match e.engine.store() {
            // Writable on disk: it has a write-ahead log (asked without the writer's lock,
            // which a commit or checkpoint holds).
            Dataset::TripleTrail(s) if s.log_position().is_some() => Some((e.name.as_str(), s)),
            _ => None,
        })
        .collect();
    let logs: Vec<_> = stores
        .iter()
        .filter_map(|(name, s)| Some((*name, s.log_position()?)))
        .collect();
    out.family(
        "tripletrail_wal_bytes",
        "gauge",
        "Bytes of the write-ahead log being written that are synced (writable on-disk stores); a checkpoint retires it.",
    );
    for (name, p) in &logs {
        out.sample("tripletrail_wal_bytes", &[("dataset", name)], p.length);
    }
    out.family(
        "tripletrail_wal_generation",
        "gauge",
        "Generation of the write-ahead log being written: one more at each checkpoint.",
    );
    for (name, p) in &logs {
        out.sample(
            "tripletrail_wal_generation",
            &[("dataset", name)],
            p.generation,
        );
    }
    let writes: Vec<_> = stores.iter().map(|(n, s)| (*n, s.write_stats())).collect();
    for (family, kind, help, value) in [
        (
            "tripletrail_commits_total",
            "counter",
            "Transactions that changed the store since it opened.",
            (|w: &tripletrail::store::WriteStats| w.commits as f64) as fn(&_) -> f64,
        ),
        (
            "tripletrail_commit_seconds_total",
            "counter",
            "Time those transactions took to commit, until durable.",
            |w| w.commit_seconds,
        ),
        (
            "tripletrail_slow_commits_total",
            "counter",
            "Commits that took a second or more (each logged with where the time went).",
            |w| w.slow_commits as f64,
        ),
        (
            "tripletrail_checkpoints_total",
            "counter",
            "Checkpoints written since the store opened.",
            |w| w.checkpoints as f64,
        ),
        (
            "tripletrail_checkpoint_seconds_total",
            "counter",
            "Time spent writing checkpoints.",
            |w| w.checkpoint_seconds,
        ),
        (
            "tripletrail_last_checkpoint_seconds",
            "gauge",
            "Time the last checkpoint took.",
            |w| w.last_checkpoint_seconds,
        ),
    ] {
        out.family(family, kind, help);
        for (name, w) in &writes {
            out.sample(family, &[("dataset", name)], value(w));
        }
    }
    out.family(
        "tripletrail_runs",
        "gauge",
        "Sorted runs of recent writes not merged into the segments yet.",
    );
    for (name, s) in &stores {
        out.sample("tripletrail_runs", &[("dataset", name)], s.run_count());
    }
    out.family(
        "tripletrail_merge_running",
        "gauge",
        "1 while runs are merged into the segments in the background.",
    );
    for (name, s) in &stores {
        out.sample(
            "tripletrail_merge_running",
            &[("dataset", name)],
            u8::from(s.is_merging()),
        );
    }
    out.family(
        "tripletrail_file_check",
        "gauge",
        "1 for the state of the check of the store's files against their sums: checking, verified, damaged or stopped.",
    );
    for (name, s) in &stores {
        use tripletrail::store::Verification;
        let now = match s.verification() {
            Verification::Checking => "checking",
            Verification::Verified => "verified",
            Verification::Damaged(_) => "damaged",
            Verification::Stopped => "stopped",
        };
        for state in ["checking", "verified", "damaged", "stopped"] {
            out.sample(
                "tripletrail_file_check",
                &[("dataset", name), ("state", state)],
                u8::from(state == now),
            );
        }
    }
}

/// The families of read replication (#217): on a replica, its lag and what it applied; on
/// a leader, the replicas it heard from and whether writes wait for them.
#[cfg(feature = "replication")]
fn replication(out: &mut Out, state: &State, entries: &[std::sync::Arc<Entry>]) {
    let replicas: Vec<(String, serde_json::Value)> = state
        .replicas
        .lock_unpoisoned()
        .iter()
        .map(|(name, s)| (name.clone(), s.json()))
        .collect();
    out.family(
        "tripletrail_replica_following",
        "gauge",
        "1 while the replica reads its leader's log; 0 while it copies a base or fails (see /$/status).",
    );
    for (name, r) in &replicas {
        out.sample(
            "tripletrail_replica_following",
            &[("dataset", name)],
            u8::from(r["state"] == "following"),
        );
    }
    for (family, kind, help, key) in [
        (
            "tripletrail_replica_lag_seconds",
            "gauge",
            "Seconds since the replica last had everything its leader had on disk (0 when it has).",
            "lagSeconds",
        ),
        (
            "tripletrail_replica_behind_bytes",
            "gauge",
            "Bytes of the leader's log the replica has not applied yet.",
            "behindBytes",
        ),
        (
            "tripletrail_replica_applied_total",
            "counter",
            "Transactions of the leader the replica applied since the server started.",
            "applied",
        ),
        (
            "tripletrail_replica_bases_total",
            "counter",
            "Bases the replica copied from its leader since the server started.",
            "bases",
        ),
    ] {
        out.family(family, kind, help);
        for (name, r) in &replicas {
            if let Some(v) = r[key].as_f64() {
                out.sample(family, &[("dataset", name)], v);
            }
        }
    }
    let leaders: Vec<(&str, serde_json::Value)> = entries
        .iter()
        .filter(|e| !replicas.iter().any(|(n, _)| *n == e.name))
        .filter(|e| match e.engine.store() {
            Dataset::TripleTrail(s) => s.log_position().is_some(),
            _ => false,
        })
        .map(|e| {
            (
                e.name.as_str(),
                state.acks.json(&e.name, state.options.sync_replicas),
            )
        })
        .collect();
    out.family(
        "tripletrail_replication_followers",
        "gauge",
        "Replicas of the dataset this leader heard from in the last 10 minutes.",
    );
    for (name, sync) in &leaders {
        out.sample(
            "tripletrail_replication_followers",
            &[("dataset", name)],
            sync["followers"].as_array().map_or(0, Vec::len),
        );
    }
    out.family(
        "tripletrail_sync_replicas_waiting",
        "gauge",
        "With --sync-replicas: 1 while writes wait for the replicas; 0 while too few hold the writes and they answer without waiting.",
    );
    if state.options.sync_replicas > 0 {
        for (name, sync) in &leaders {
            out.sample(
                "tripletrail_sync_replicas_waiting",
                &[("dataset", name)],
                u8::from(sync["waiting"] == true),
            );
        }
    }
}

/// The exposition of every attached dataset and the server.
pub(crate) fn render(state: &State) -> Reply {
    let entries: Vec<std::sync::Arc<Entry>> = state
        .registry
        .read_unpoisoned()
        .datasets
        .values()
        .cloned()
        .collect();
    let mut out = Out {
        text: String::with_capacity(4096),
    };

    out.family(
        "tripletrail_requests_total",
        "counter",
        "Requests answered, by dataset, operation and HTTP status.",
    );
    for e in &entries {
        for ((operation, status), n) in e.timings.statuses.lock_unpoisoned().iter() {
            out.sample(
                "tripletrail_requests_total",
                &[
                    ("dataset", &e.name),
                    ("operation", operation),
                    ("status", &status.to_string()),
                ],
                n,
            );
        }
    }

    out.family(
        "tripletrail_request_duration_seconds",
        "histogram",
        "Time to answer a request, body included, by dataset and operation.",
    );
    for e in &entries {
        for (i, operation) in OPERATIONS.iter().enumerate() {
            let h = &e.timings.latency[i];
            let count = h.count.load(Ordering::Relaxed);
            if count == 0 {
                continue;
            }
            let mut cumulative = 0;
            for (b, bound) in BUCKETS.iter().enumerate() {
                cumulative += h.buckets[b].load(Ordering::Relaxed);
                out.sample(
                    "tripletrail_request_duration_seconds_bucket",
                    &[
                        ("dataset", &e.name),
                        ("operation", operation),
                        ("le", &bound.to_string()),
                    ],
                    cumulative,
                );
            }
            let labels = [("dataset", e.name.as_str()), ("operation", operation)];
            out.sample(
                "tripletrail_request_duration_seconds_bucket",
                &[labels[0], labels[1], ("le", "+Inf")],
                count,
            );
            out.sample(
                "tripletrail_request_duration_seconds_sum",
                &labels,
                h.micros.load(Ordering::Relaxed) as f64 / 1e6,
            );
            out.sample("tripletrail_request_duration_seconds_count", &labels, count);
        }
    }

    out.family(
        "tripletrail_timeouts_total",
        "counter",
        "Queries and updates stopped by their timeout (answered 503), by dataset.",
    );
    for e in &entries {
        let n = e.timings.trips(Trip::Timeout);
        out.sample("tripletrail_timeouts_total", &[("dataset", &e.name)], n);
    }
    out.family(
        "tripletrail_budget_trips_total",
        "counter",
        "Queries stopped by the memory limit (memory), refused for more rows than max-rows \
         (rows) or cut at max-rows with partial=true (partial), by dataset.",
    );
    for e in &entries {
        for (trip, budget) in Trip::ALL {
            let n = e.timings.trips(trip);
            out.sample(
                "tripletrail_budget_trips_total",
                &[("dataset", &e.name), ("budget", budget)],
                n,
            );
        }
    }

    #[cfg(feature = "enterprise")]
    if let Some(ship) = state.audit.as_ref().and_then(|a| a.shipper()) {
        out.family(
            "tripletrail_audit_syslog_dropped_total",
            "counter",
            "Audit lines not sent to syslog (--audit-syslog): the queue was full or the \
             collector could not be reached. The audit file has them.",
        );
        out.sample(
            "tripletrail_audit_syslog_dropped_total",
            &[],
            ship.dropped(),
        );
    }

    // Only for the tripletrail store, which keeps these counts: on Oxigraph's store each is a
    // scan, too slow to repeat at every scrape.
    out.family("tripletrail_triples", "gauge", "Quads stored, by dataset.");
    for e in &entries {
        if let Dataset::TripleTrail(s) = e.engine.store() {
            out.sample("tripletrail_triples", &[("dataset", &e.name)], s.len());
        }
    }
    out.family("tripletrail_graphs", "gauge", "Named graphs, by dataset.");
    for e in &entries {
        if let Dataset::TripleTrail(s) = e.engine.store() {
            out.sample(
                "tripletrail_graphs",
                &[("dataset", &e.name)],
                s.named_graphs().len(),
            );
        }
    }
    out.family(
        "tripletrail_mapped_bytes",
        "gauge",
        "Bytes of the dataset's files mapped into memory (on-disk tripletrail stores).",
    );
    for e in &entries {
        if let Dataset::TripleTrail(s) = e.engine.store() {
            out.sample(
                "tripletrail_mapped_bytes",
                &[("dataset", &e.name)],
                s.mapped_size(),
            );
        }
    }
    out.family(
        "tripletrail_dataset_memory_bytes",
        "gauge",
        "Roughly the heap bytes a dataset holds, by part (what its memory quota counts; tripletrail stores).",
    );
    for e in &entries {
        if let Some(m) = e.engine.memory_use() {
            for (part, bytes) in m.parts() {
                out.sample(
                    "tripletrail_dataset_memory_bytes",
                    &[("dataset", &e.name), ("part", part)],
                    bytes,
                );
            }
        }
    }
    storage(&mut out, &entries);
    #[cfg(feature = "replication")]
    replication(&mut out, state, &entries);
    out.family(
        "tripletrail_spatial_index_ready",
        "gauge",
        "1 once the dataset's spatial index is built.",
    );
    for e in &entries {
        out.sample(
            "tripletrail_spatial_index_ready",
            &[("dataset", &e.name)],
            u8::from(e.engine.spatial_index_ready()),
        );
    }

    let caches: Vec<_> = entries
        .iter()
        .map(|e| (e, e.engine.result_cache_stats()))
        .collect();
    out.family(
        "tripletrail_result_cache_hits_total",
        "counter",
        "SELECT and ASK queries answered from the result cache.",
    );
    for (e, c) in &caches {
        out.sample(
            "tripletrail_result_cache_hits_total",
            &[("dataset", &e.name)],
            c.hits,
        );
    }
    out.family(
        "tripletrail_result_cache_misses_total",
        "counter",
        "Cacheable queries that had to be evaluated.",
    );
    for (e, c) in &caches {
        out.sample(
            "tripletrail_result_cache_misses_total",
            &[("dataset", &e.name)],
            c.misses,
        );
    }
    out.family(
        "tripletrail_result_cache_bytes",
        "gauge",
        "Memory held by the result cache.",
    );
    for (e, c) in &caches {
        out.sample(
            "tripletrail_result_cache_bytes",
            &[("dataset", &e.name)],
            c.bytes,
        );
    }

    out.family(
        "tripletrail_dataset_requests_running",
        "gauge",
        "Requests holding a turn on the dataset (see the limits file).",
    );
    for e in &entries {
        out.sample(
            "tripletrail_dataset_requests_running",
            &[("dataset", &e.name)],
            e.gate.counts().running,
        );
    }
    out.family(
        "tripletrail_dataset_requests_queued",
        "gauge",
        "Requests waiting for a turn on the dataset.",
    );
    for e in &entries {
        out.sample(
            "tripletrail_dataset_requests_queued",
            &[("dataset", &e.name)],
            e.gate.counts().queued,
        );
    }
    out.family(
        "tripletrail_dataset_requests_rejected_total",
        "counter",
        "Requests answered with 429 because the dataset was at its concurrency limit.",
    );
    for e in &entries {
        out.sample(
            "tripletrail_dataset_requests_rejected_total",
            &[("dataset", &e.name)],
            e.gate.rejected.load(Ordering::Relaxed),
        );
    }

    out.family(
        "tripletrail_dataset_writes_rejected_quota_total",
        "counter",
        "Writes refused with 507 because the dataset was over its disk or memory quota.",
    );
    for e in &entries {
        out.sample(
            "tripletrail_dataset_writes_rejected_quota_total",
            &[("dataset", &e.name)],
            e.gate.rejected_quota.load(Ordering::Relaxed),
        );
    }
    out.family(
        "tripletrail_caller_requests_rejected_total",
        "counter",
        "Requests answered with 429 by the callers limits, by reason (rate or concurrency).",
    );
    out.sample(
        "tripletrail_caller_requests_rejected_total",
        &[("reason", "rate")],
        state.callers.rejected_rate.load(Ordering::Relaxed),
    );
    out.sample(
        "tripletrail_caller_requests_rejected_total",
        &[("reason", "concurrency")],
        state.callers.rejected_concurrency.load(Ordering::Relaxed),
    );

    let binds = tripletrail::parallel_bind_stats();
    out.family(
        "tripletrail_update_bind_chains_total",
        "counter",
        "BIND chains of update WHEREs worth evaluating in parallel, by outcome: parallel, or left \
         to the evaluator because the rows under them held too many values or a blank node.",
    );
    for (outcome, n) in [
        ("parallel", binds.taken),
        ("too_many_values", binds.too_many_values),
        ("blank_node", binds.blank_nodes),
    ] {
        out.sample(
            "tripletrail_update_bind_chains_total",
            &[("outcome", outcome)],
            n,
        );
    }

    out.family(
        "tripletrail_requests_in_flight",
        "gauge",
        "Requests being answered, this scrape included.",
    );
    out.sample(
        "tripletrail_requests_in_flight",
        &[],
        state.in_flight.load(Ordering::SeqCst),
    );
    out.family("tripletrail_datasets", "gauge", "Datasets attached.");
    out.sample("tripletrail_datasets", &[], entries.len());
    out.family(
        "tripletrail_uptime_seconds",
        "gauge",
        "Seconds since the server started.",
    );
    out.sample(
        "tripletrail_uptime_seconds",
        &[],
        SystemTime::now()
            .duration_since(state.started)
            .unwrap_or_default()
            .as_secs(),
    );
    if let Some(rss) = resident_bytes() {
        out.family(
            "process_resident_memory_bytes",
            "gauge",
            "Resident memory size in bytes.",
        );
        out.sample("process_resident_memory_bytes", &[], rss);
    }
    out.family(
        "tripletrail_build_info",
        "gauge",
        "The server's version, as a label; always 1.",
    );
    out.sample(
        "tripletrail_build_info",
        &[("version", env!("CARGO_PKG_VERSION"))],
        1,
    );
    Reply::bytes(
        200,
        "text/plain; version=0.0.4; charset=utf-8",
        out.text.into_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histograms_count_each_request_once() {
        let t = Timings::default();
        t.record(Some(Operation::Query), 200, Duration::from_millis(3));
        t.record(Some(Operation::Query), 200, Duration::from_secs(100));
        t.record(None, 404, Duration::from_millis(1));
        let h = &t.latency[0];
        assert_eq!(h.count.load(Ordering::Relaxed), 2);
        // 3 ms is in the 5 ms bucket; 100 s is past every bound (only in +Inf).
        assert_eq!(h.buckets[1].load(Ordering::Relaxed), 1);
        let in_buckets: u64 = h.buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum();
        assert_eq!(in_buckets, 1);
        assert_eq!(t.statuses.lock_unpoisoned()[&("other", 404)], 1);
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
