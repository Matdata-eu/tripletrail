//! A bounded cache of whole query results (issue #126): `SELECT` and `ASK` results kept
//! under a memory budget, keyed by the query as parsed (its algebra, written out), the
//! query options and the engine settings that steer evaluation, and valid while the store's
//! write generation is the one they were computed at. Any write therefore invalidates every
//! entry. Only on the tripletrail store (Oxigraph's has no write generation).
//!
//! Results are collected as they are read: a result larger than an entry may be is handed
//! on without being cached, and so is one that fails part way or is not read to the end.

use oxigraph::model::Term;
use oxigraph::sparql::{QueryEvaluationError, QueryResults, QuerySolutionIter, Variable};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tripletrail_geo::sync::LockUnpoisoned;

type Rows = Arc<[Vec<Option<Term>>]>;

#[derive(Clone)]
enum Cached {
    Boolean(bool),
    /// An answer on term ids (#11), kept as it is.
    Ids(Arc<crate::IdSolutions>),
    Solutions {
        variables: Arc<[Variable]>,
        rows: Rows,
    },
}

struct Entry {
    value: Cached,
    bytes: usize,
    used: u64,
}

#[derive(Default)]
struct Entries {
    /// The write generation the entries were computed at: a different one empties the cache.
    generation: u64,
    map: HashMap<String, Entry>,
    bytes: usize,
    clock: u64,
}

/// Hits and misses of the result cache, and what it holds now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResultCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
    pub bytes: usize,
    /// The memory budget, in bytes (0: the cache is off).
    pub budget: usize,
}

/// Whether a query was answered from the result cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheStatus {
    /// Answered from the cache.
    Hit,
    /// Evaluated, and kept if it fits.
    Miss,
    /// Not cacheable: the cache is off, the store has no write generation, or the query is
    /// not a `SELECT` or `ASK` or uses a function whose value changes from call to call.
    Bypass,
}

impl CacheStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "HIT",
            Self::Miss => "MISS",
            Self::Bypass => "BYPASS",
        }
    }
}

/// The budget of the result cache of engines made from now on (see
/// [`crate::Engine::set_default_result_cache`]).
pub(crate) static DEFAULT_BUDGET: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct ResultCache {
    budget: AtomicUsize,
    entries: Mutex<Entries>,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// Overhead counted per row and per term, besides the terms' strings.
const ROW_BYTES: usize = 48;
const TERM_BYTES: usize = 40;
/// Entries kept at most, whatever their size.
const MAX_ENTRIES: usize = 10_000;

pub(crate) fn term_bytes(t: &Term) -> usize {
    TERM_BYTES
        + match t {
            Term::NamedNode(n) => n.as_str().len(),
            Term::BlankNode(b) => b.as_str().len(),
            Term::Literal(l) => {
                l.value().len() + l.datatype().as_str().len() + l.language().map_or(0, str::len)
            }
            Term::Triple(t) => {
                t.subject.to_string().len() + t.predicate.as_str().len() + term_bytes(&t.object)
            }
        }
}

fn row_bytes(row: &[Option<Term>]) -> usize {
    ROW_BYTES + row.iter().flatten().map(term_bytes).sum::<usize>()
}

/// The parser names the variables and blank nodes it adds (for aggregates and property
/// paths, say) with a random number in hex: they are renamed in order of appearance, so that
/// the same query gets the same key.
pub(crate) fn normalize(algebra: &str) -> String {
    let bytes = algebra.as_bytes();
    let mut names: HashMap<(&str, &str), usize> = HashMap::new();
    let mut out = String::with_capacity(algebra.len());
    let mut i = 0;
    let mut copied = 0;
    while i < bytes.len() {
        // Names inside literals and IRIs are left alone (renaming them there could make two
        // different queries one key); leaving a name alone only costs a miss.
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                i += if bytes[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'<'
            && bytes
                .get(i + 1)
                .is_some_and(|b| !b.is_ascii_whitespace() && *b != b'=')
        {
            while i < bytes.len() && bytes[i] != b'>' {
                i += 1;
            }
            continue;
        }
        let prefix = if bytes[i] == b'?' {
            "?"
        } else if bytes[i..].starts_with(b"_:") {
            "_:"
        } else {
            i += 1;
            continue;
        };
        let start = i + prefix.len();
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        let name = &algebra[start..end];
        // A random u128 in hex: up to 32 digits (fewer with leading zeros).
        if (16..=32).contains(&name.len())
            && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            out.push_str(&algebra[copied..i]);
            let n = names.len();
            let n = *names.entry((prefix, name)).or_insert(n);
            out.push_str(prefix);
            out.push_str(&format!("__mg{n}"));
            copied = end;
        }
        i = end;
    }
    out.push_str(&algebra[copied..]);
    out
}

/// Whether the query's results may be reused: it reads only the store (no `SERVICE` but the
/// engine's own) and calls no function whose value changes between evaluations.
pub(crate) fn cacheable(algebra: &str) -> bool {
    const VOLATILE: [&str; 5] = ["RAND(", "NOW(", "UUID(", "STRUUID(", "BNODE("];
    if VOLATILE.iter().any(|f| algebra.contains(f)) {
        return false;
    }
    let mut rest = algebra;
    while let Some(i) = rest.find("SERVICE ") {
        rest = &rest[i + "SERVICE ".len()..];
        let iri = rest.strip_prefix("SILENT ").unwrap_or(rest);
        if !iri.starts_with("<http://w3id.org/tripletrail/") {
            return false;
        }
    }
    true
}

impl Default for ResultCache {
    fn default() -> Self {
        Self {
            budget: AtomicUsize::new(DEFAULT_BUDGET.load(Ordering::Relaxed)),
            entries: Mutex::default(),
            hits: AtomicU64::default(),
            misses: AtomicU64::default(),
        }
    }
}

impl ResultCache {
    pub(crate) fn budget(&self) -> usize {
        self.budget.load(Ordering::Relaxed)
    }

    pub(crate) fn set_budget(&self, bytes: usize) {
        self.budget.store(bytes, Ordering::Relaxed);
        let mut entries = self.entries.lock_unpoisoned();
        Self::evict(&mut entries, bytes);
    }

    pub(crate) fn stats(&self) -> ResultCacheStats {
        let entries = self.entries.lock_unpoisoned();
        ResultCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            entries: entries.map.len(),
            bytes: entries.bytes,
            budget: self.budget(),
        }
    }

    pub(crate) fn clear(&self) {
        let mut entries = self.entries.lock_unpoisoned();
        entries.map.clear();
        entries.bytes = 0;
    }

    /// The cached results of `key` at write generation `generation`.
    pub(crate) fn get(&self, key: &str, generation: u64) -> Option<QueryResults<'static>> {
        let mut entries = self.entries.lock_unpoisoned();
        if entries.generation != generation {
            entries.map.clear();
            entries.bytes = 0;
            entries.generation = generation;
        }
        entries.clock += 1;
        let clock = entries.clock;
        let Some(entry) = entries.map.get_mut(key) else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        entry.used = clock;
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(match entry.value.clone() {
            Cached::Boolean(b) => QueryResults::Boolean(b),
            Cached::Ids(answer) => crate::direct::results(answer),
            Cached::Solutions { variables, rows } => solutions(variables, rows, 0),
        })
    }

    fn insert(self: &Arc<Self>, key: String, generation: u64, value: Cached, bytes: usize) {
        let budget = self.budget();
        let mut entries = self.entries.lock_unpoisoned();
        if entries.generation != generation || bytes > Self::entry_limit(budget) {
            return;
        }
        entries.clock += 1;
        let used = entries.clock;
        entries.bytes += bytes;
        if let Some(old) = entries.map.insert(key, Entry { value, bytes, used }) {
            entries.bytes -= old.bytes;
        }
        Self::evict(&mut entries, budget);
    }

    /// The largest result kept: a quarter of the budget, so one result never empties it.
    fn entry_limit(budget: usize) -> usize {
        budget / 4
    }

    /// Drops the least recently used entries until the cache fits `budget`.
    fn evict(entries: &mut Entries, budget: usize) {
        while entries.bytes > budget || entries.map.len() > MAX_ENTRIES {
            let Some(oldest) = entries
                .map
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(e) = entries.map.remove(&oldest) {
                entries.bytes -= e.bytes;
            }
        }
    }

    /// `results`, kept under `key` once read to the end if they fit.
    pub(crate) fn store(
        self: &Arc<Self>,
        key: String,
        generation: u64,
        results: QueryResults<'static>,
    ) -> QueryResults<'static> {
        match results {
            QueryResults::Boolean(b) => {
                let bytes = ROW_BYTES + key_bytes(&key);
                self.insert(key, generation, Cached::Boolean(b), bytes);
                QueryResults::Boolean(b)
            }
            QueryResults::Solutions(solutions)
                if let Some(answer) = solutions
                    .attachment()
                    .and_then(|a| Arc::clone(a).downcast::<crate::IdSolutions>().ok()) =>
            {
                // Already whole: kept as it is, and still written from its ids.
                let bytes = key_bytes(&key) + ROW_BYTES + answer.bytes();
                self.insert(key, generation, Cached::Ids(answer), bytes);
                QueryResults::Solutions(solutions)
            }
            QueryResults::Solutions(solutions) => {
                let variables: Arc<[Variable]> = solutions.variables().into();
                let mut collector = Collector {
                    cache: Arc::clone(self),
                    key,
                    generation,
                    variables: Arc::clone(&variables),
                    rows: Some(Vec::new()),
                    bytes: 0,
                    limit: Self::entry_limit(self.budget()),
                };
                collector.bytes = key_bytes(&collector.key);
                let width = variables.len();
                let mut inner = solutions;
                QueryResults::Solutions(QuerySolutionIter::from_tuples(
                    variables,
                    std::iter::from_fn(move || {
                        let next = inner.next();
                        match &next {
                            Some(Ok(s)) => {
                                let row: Vec<Option<Term>> =
                                    s.values().iter().take(width).cloned().collect();
                                collector.push(&row);
                                Some(Ok(row))
                            }
                            Some(Err(_)) => {
                                collector.rows = None;
                                next.map(|r| r.map(|_| unreachable!()))
                            }
                            None => {
                                collector.finish();
                                None
                            }
                        }
                    }),
                ))
            }
            graph @ QueryResults::Graph(_) => graph,
        }
    }
}

fn key_bytes(key: &str) -> usize {
    key.len() + 64
}

fn solutions(variables: Arc<[Variable]>, rows: Rows, start: usize) -> QueryResults<'static> {
    let mut i = start;
    QueryResults::Solutions(QuerySolutionIter::from_tuples(
        variables,
        std::iter::from_fn(move || {
            let row = rows.get(i)?.clone();
            i += 1;
            Some(Ok::<_, QueryEvaluationError>(row))
        }),
    ))
}

/// The rows of a result as they are read, handed to the cache when the last one is.
struct Collector {
    cache: Arc<ResultCache>,
    key: String,
    generation: u64,
    variables: Arc<[Variable]>,
    /// `None` once the result is known not to be kept.
    rows: Option<Vec<Vec<Option<Term>>>>,
    bytes: usize,
    limit: usize,
}

impl Collector {
    fn push(&mut self, row: &[Option<Term>]) {
        let Some(rows) = &mut self.rows else {
            return;
        };
        self.bytes += row_bytes(row);
        if self.bytes > self.limit {
            self.rows = None;
        } else {
            rows.push(row.to_vec());
        }
    }

    fn finish(&mut self) {
        if let Some(rows) = self.rows.take() {
            self.cache.insert(
                std::mem::take(&mut self.key),
                self.generation,
                Cached::Solutions {
                    variables: Arc::clone(&self.variables),
                    rows: rows.into(),
                },
                self.bytes,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn generated_variable_names_are_renamed() {
        let a = super::normalize("SELECT (?0f3a9c11d2e4b5a6c7d8e9f0a1b2c3d AS ?n) { ?s ?p ?o }");
        let b = super::normalize("SELECT (?5819ae9c63cccfb1fd649c7298de694b AS ?n) { ?s ?p ?o }");
        assert_eq!(a, b);
        assert_eq!(a, "SELECT (?__mg0 AS ?n) { ?s ?p ?o }");
        assert_eq!(super::normalize("?abc ?cafe"), "?abc ?cafe");
        let literal =
            r#"FILTER(?x = "?0f3a9c11d2e4b5a6c7d8e9f0a1b2c3d" && <http://e/?0f3a9c11d2e4b5a6>)"#;
        assert_eq!(super::normalize(literal), literal);
        assert_eq!(
            super::normalize(
                "?s <p> _:f81ad7bb0880f5d0331fbec3e140b8be ._:f81ad7bb0880f5d0331fbec3e140b8be <q> ?w"
            ),
            "?s <p> _:__mg0 ._:__mg0 <q> ?w"
        );
    }
}
