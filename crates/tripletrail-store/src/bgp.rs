//! Basic graph patterns joined on term ids, without Oxigraph's evaluator: for callers that
//! read many rows and want ids (an aggregate over every feature, the geometries of a side of
//! a spatial join), where the evaluator spends most of its time on one row at a time.
//!
//! The triples are joined one after another from the smallest, each by the cheapest of an
//! index nested loop (a lookup per row), a merge join when the triple shares one variable
//! with the rows (both sides sorted on it, unless they come sorted, then merged run by run)
//! and a hash join on the shared variables otherwise. [`Snapshot::count_ids`] counts the
//! solutions without building the rows of the last join (issue #167).

use crate::{GraphSelector, Snapshot, TermId};
use oxigraph::model::{BlankNode, TermRef};
use rustc_hash::FxHashMap;
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern, Variable};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A position of an [`IdTriple`]: a term id, or a variable by its column in the rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdSlot {
    Const(TermId),
    Var(usize),
}

/// A triple pattern over term ids.
#[derive(Clone, Copy, Debug)]
pub struct IdTriple {
    pub s: IdSlot,
    pub p: IdSlot,
    pub o: IdSlot,
}

impl IdTriple {
    fn slots(&self) -> [IdSlot; 3] {
        [self.s, self.p, self.o]
    }
}

/// Rows of term ids, `width` per row; a column of a variable no triple binds is `TermId(0)`.
#[derive(Clone, Debug, Default)]
pub struct IdRows {
    pub width: usize,
    pub ids: Vec<TermId>,
}

impl IdRows {
    pub fn len(&self) -> usize {
        self.ids.len().checked_div(self.width).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn rows(&self) -> std::slice::ChunksExact<'_, TermId> {
        self.ids.chunks_exact(self.width.max(1))
    }
}

/// A basic graph pattern on term ids (see [`Snapshot::compile_bgp`]).
#[derive(Clone, Debug)]
pub struct CompiledBgp {
    pub triples: Vec<IdTriple>,
    /// The variable of each column (`None`: a blank node of the pattern).
    pub columns: Vec<Option<Variable>>,
    /// A constant of the pattern is not in the store: it has no solutions.
    pub empty: bool,
}

/// Relative costs, measured on the block index: a lookup seeks a block and decodes it, a
/// scanned quad is a few varints.
const LOOKUP: f64 = 40.0;
const SCANNED: f64 = 1.0;
const PROBED: f64 = 2.0;
/// Per row of a merge join: a sort when the rows don't come in key order, then one step.
const MERGED: f64 = 1.0;
/// Per row of a merge join whose scan comes sorted by the key: a seek past the blocks
/// between two keys (#169), measured at about half a lookup.
const SOUGHT: f64 = 20.0;
/// Memory, in bytes, that the intermediate rows of one query's id joins may take by default
/// (#479): 1 GiB, as before the budget became a setting.
pub const DEFAULT_JOIN_MEMORY: usize = 1 << 30;
/// Ids (8 bytes each) the intermediate rows of one join may take: past it the join is given
/// up on and the caller evaluates the pattern another way. Set with [`set_join_memory`].
static MAX_IDS: AtomicUsize = AtomicUsize::new(DEFAULT_JOIN_MEMORY / 8);
/// Whether [`set_join_memory`] set a limit for the process, rather than the default.
static PROCESS_LIMIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Sets, for the whole process, the memory in bytes that the intermediate rows of one
/// query's id joins may take (#479). Past it a join is given up on and the query is
/// evaluated row by row instead, which takes little memory and is cancelled on time.
/// 0 goes back to [`DEFAULT_JOIN_MEMORY`], with no limit set.
pub fn set_join_memory(bytes: usize) {
    let ids = if bytes == 0 {
        DEFAULT_JOIN_MEMORY
    } else {
        bytes
    } / 8;
    MAX_IDS.store(ids.max(1 << 10), Ordering::Relaxed);
    PROCESS_LIMIT.store(bytes != 0, Ordering::Relaxed);
}

/// The memory in bytes that the intermediate rows of one query's id joins may take: on this
/// thread, the lower of [`set_join_memory`] and [`with_join_memory`].
pub fn join_memory() -> usize {
    max_ids() * 8
}

/// The memory limit of a query on this thread, when one was set for the process
/// ([`set_join_memory`] with any value but 0, [`DEFAULT_JOIN_MEMORY`] included) or for the query
/// ([`with_join_memory`]): the lower of the two. `None` with neither, where the id joins'
/// default of 1 GiB is a guard rather than a limit, and work that fits no limit may go on.
pub fn memory_limit() -> Option<usize> {
    (query_ids() > 0 || PROCESS_LIMIT.load(Ordering::Relaxed)).then(join_memory)
}

thread_local! {
    /// Ids of one query's limit on this thread ([`with_join_memory`]); 0 for none.
    static QUERY_IDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Runs `f` with the id joins on this thread limited to `bytes` (#189), or to the process's
/// limit when it is lower; 0 sets no limit of its own. Then puts back the limit before.
/// Only work run on this thread before `f` returns sees it, and the helper threads of
/// [`crate::par::chunks`] it starts: work on another thread, or left for later, has the
/// process's limit unless it is given its own scope.
pub fn with_join_memory<R>(bytes: usize, f: impl FnOnce() -> R) -> R {
    let ids = if bytes == 0 {
        0
    } else {
        (bytes / 8).max(1 << 10)
    };
    with_query_ids(ids, f)
}

/// The ids of this thread's query limit ([`with_join_memory`]), 0 for none: for
/// [`crate::par`] to give its helper threads the same limit.
pub(crate) fn query_ids() -> usize {
    QUERY_IDS.with(std::cell::Cell::get)
}

/// [`with_join_memory`] in ids.
pub(crate) fn with_query_ids<R>(ids: usize, f: impl FnOnce() -> R) -> R {
    struct Restore(usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            QUERY_IDS.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(QUERY_IDS.with(|c| c.replace(ids)));
    f()
}

/// Ids the intermediate rows of one join may take ([`set_join_memory`], [`with_join_memory`]).
#[inline]
fn max_ids() -> usize {
    let process = MAX_IDS.load(Ordering::Relaxed);
    match query_ids() {
        0 => process,
        query => process.min(query),
    }
}
/// A seek in a sorted scan costs about as much as reading this many quads in order.
const SEEKS: usize = 8;
/// Rows a join makes between two calls of `cancelled` (#467).
const CHECK_ROWS: usize = 1 << 16;

/// The rows a join makes from `input` (#467): with the input's, at most [`max_ids`] ids, also
/// in capacity (a `Vec` doubling past it would take twice the memory), and `cancelled` asked
/// every [`CHECK_ROWS`] rows, so a cross product whose rows come from a few input rows stops
/// as soon as it is too large or the query's time is up, not once the next input row is
/// reached.
struct Out<'c> {
    ids: Vec<TermId>,
    width: usize,
    /// Ids this join may make.
    max: usize,
    made: usize,
    cancelled: &'c (dyn Fn() -> bool + Sync),
}

impl<'c> Out<'c> {
    /// For about `rows` rows from `input`.
    fn new(input: &IdRows, rows: usize, cancelled: &'c (dyn Fn() -> bool + Sync)) -> Self {
        Self::with_used(input.width, input.ids.len(), rows, cancelled)
    }

    /// For about `rows` rows of `width` ids, with `used` ids held already.
    fn with_used(
        width: usize,
        used: usize,
        rows: usize,
        cancelled: &'c (dyn Fn() -> bool + Sync),
    ) -> Self {
        let max = max_ids().saturating_sub(used);
        Self {
            ids: Vec::with_capacity(rows.saturating_mul(width).min(max)),
            width,
            max,
            made: 0,
            cancelled,
        }
    }

    /// `row` with the triple's variables (`slots`) set to the values of `q`. `None` once too
    /// large or cancelled.
    #[inline]
    fn push(&mut self, row: &[TermId], slots: &[IdSlot; 3], q: &[TermId; 3]) -> Option<()> {
        self.made += 1;
        if self.ids.len() + self.width > self.max
            || (self.made.is_multiple_of(CHECK_ROWS) && (self.cancelled)())
        {
            return None;
        }
        if self.ids.capacity() - self.ids.len() < self.width {
            let extra = self
                .ids
                .len()
                .max(4096 * self.width)
                .min(self.max - self.ids.len())
                .max(self.width);
            self.ids.reserve_exact(extra);
        }
        let start = self.ids.len();
        self.ids.extend_from_slice(row);
        for (s, v) in slots.iter().zip(q) {
            if let IdSlot::Var(c) = s {
                self.ids[start + c] = *v;
            }
        }
        Some(())
    }

    fn rows(self) -> IdRows {
        IdRows {
            width: self.width,
            ids: self.ids,
        }
    }
}

/// What [`Snapshot::count_ids`] counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgpCount {
    /// Solutions.
    Rows,
    /// Distinct values of a column.
    Distinct(usize),
}

enum Joined {
    Rows(IdRows),
    Count(u64),
}

#[derive(Clone, Copy)]
enum Step {
    Scan,
    Nested,
    Hash,
    Merge(usize),
}

impl Snapshot {
    /// `patterns` on term ids, with the columns of `first` first (in that order), then the
    /// pattern's other variables and blank nodes. `None` for a pattern with a triple term.
    pub fn compile_bgp(
        &self,
        patterns: &[TriplePattern],
        first: &[Variable],
    ) -> Option<CompiledBgp> {
        let mut columns: Vec<Option<Variable>> = first.iter().cloned().map(Some).collect();
        let mut blanks: Vec<(usize, BlankNode)> = Vec::new();
        let mut empty = false;
        let mut triples = Vec::with_capacity(patterns.len());
        for t in patterns {
            let mut slot = |t: &TermPattern| -> Option<IdSlot> {
                let term: TermRef<'_> = match t {
                    TermPattern::Variable(v) => {
                        let c = match columns.iter().position(|c| c.as_ref() == Some(v)) {
                            Some(c) => c,
                            None => {
                                columns.push(Some(v.clone()));
                                columns.len() - 1
                            }
                        };
                        return Some(IdSlot::Var(c));
                    }
                    TermPattern::BlankNode(b) => {
                        let c = match blanks.iter().find(|(_, x)| x == b) {
                            Some((c, _)) => *c,
                            None => {
                                columns.push(None);
                                blanks.push((columns.len() - 1, b.clone()));
                                columns.len() - 1
                            }
                        };
                        return Some(IdSlot::Var(c));
                    }
                    TermPattern::NamedNode(n) => n.as_ref().into(),
                    TermPattern::Literal(l) => l.as_ref().into(),
                    #[allow(unreachable_patterns)]
                    _ => return None,
                };
                Some(match self.term_id(term) {
                    Some(id) => IdSlot::Const(id),
                    None => {
                        empty = true;
                        IdSlot::Const(TermId(0))
                    }
                })
            };
            let s = slot(&t.subject)?;
            let o = slot(&t.object)?;
            let p = match &t.predicate {
                NamedNodePattern::NamedNode(n) => slot(&TermPattern::NamedNode(n.clone()))?,
                NamedNodePattern::Variable(v) => slot(&TermPattern::Variable(v.clone()))?,
            };
            // A constant the view's rows hide everywhere (#204): no quad matches.
            if let Some(rows) = self.rows() {
                let id = |slot: &IdSlot| match slot {
                    IdSlot::Const(t) => Some(t.0),
                    IdSlot::Var(_) => None,
                };
                empty |= rows.hides_all(id(&s), id(&p), id(&o));
            }
            triples.push(IdTriple { s, p, o });
        }
        Some(CompiledBgp {
            triples,
            columns,
            empty,
        })
    }

    /// Whether `triples` only read one result table's columns on one row variable, each
    /// into a variable of its own: then [`Snapshot::join_ids`] and [`Snapshot::count_ids`]
    /// read the table whole. Other patterns with table columns are answered only during
    /// evaluation (the joins here do not read tables).
    pub fn reads_table_whole(&self, triples: &[IdTriple], width: usize) -> bool {
        crate::table::is_whole_rows(&self.tables, triples, width)
    }

    /// The solutions of `triples` in `graph`, as rows of `width` columns (every variable index
    /// in the triples is below `width`). `None` once `cancelled` says so.
    pub fn join_ids(
        &self,
        triples: &[IdTriple],
        width: usize,
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<IdRows> {
        match self.join(triples, width, graph, cancelled, false, None)? {
            Joined::Rows(rows) => Some(rows),
            Joined::Count(_) => unreachable!("rows asked for"),
        }
    }

    /// The solutions of `triples` compatible with `seed`, whose rows bind the first `bound`
    /// columns (and only those): each seed row extended by the solutions with its values, as
    /// [`Snapshot::join_ids`] joins (a few seed rows look the triples up by their values
    /// instead of scanning them).
    pub fn join_ids_from(
        &self,
        triples: &[IdTriple],
        seed: IdRows,
        bound: usize,
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<IdRows> {
        let width = seed.width;
        match self.join(triples, width, graph, cancelled, false, Some((seed, bound)))? {
            Joined::Rows(rows) => Some(rows),
            Joined::Count(_) => unreachable!("rows asked for"),
        }
    }

    /// The number of solutions of `triples` in `graph` ([`BgpCount::Rows`]), or of distinct
    /// values of one column ([`BgpCount::Distinct`]), joined as [`Snapshot::join_ids`] does
    /// but without building the rows of the last join. `None` once `cancelled` says so, or
    /// when an intermediate result would be too large to hold.
    pub fn count_ids(
        &self,
        triples: &[IdTriple],
        width: usize,
        graph: GraphSelector,
        count: BgpCount,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<u64> {
        match count {
            BgpCount::Rows if let Some(n) = self.count_star(triples, graph, cancelled) => n,
            BgpCount::Rows => match self.join(triples, width, graph, cancelled, true, None)? {
                Joined::Count(n) => Some(n),
                Joined::Rows(rows) => Some(rows.len() as u64),
            },
            BgpCount::Distinct(c) => {
                let Joined::Rows(rows) =
                    self.join(triples, width, graph, cancelled, false, None)?
                else {
                    unreachable!("rows asked for")
                };
                let mut values: Vec<u64> = rows.rows().map(|r| r[c].0).collect();
                values.sort_unstable();
                values.dedup();
                Some(values.len() as u64)
            }
        }
    }

    /// The solutions of a star: triples that all have one variable (the centre) once, and
    /// whose other variables are each in one triple only, once (`?s a ?c . ?s ex:f ?f`).
    /// They are the sum over the centre's values of the product of each triple's matches
    /// for it, so the rows are never built (issue #179): each triple's matches are counted
    /// per centre value (a scan sorted on the centre), and the counts merged. `None` for
    /// other patterns, for counts the block index can't give exactly (tables, named graphs
    /// only), and when the smallest triple's centre values are few and the rows not many
    /// (the usual join then seeks those values instead of reading every quad): joined as
    /// usual. `Some(None)` once `cancelled` says so.
    fn count_star(
        &self,
        triples: &[IdTriple],
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<Option<u64>> {
        let centre = star_centre(triples, graph)?;
        let at = |t: &IdTriple| centre_at(t, centre);
        let c = |s: IdSlot| match s {
            IdSlot::Const(id) => Some(id),
            IdSlot::Var(_) => None,
        };
        let runs = |t: &IdTriple| self.centre_runs(t, centre, graph, cancelled);
        // From the smallest triple's counts, multiplied by each other triple's for the same
        // centre values (the smaller first, so fewer values are left for the larger).
        let mut order: Vec<(usize, usize)> = triples
            .iter()
            .map(|t| self.estimate_slots(t, graph))
            .enumerate()
            .collect();
        order.sort_by_key(|&(_, size)| size);
        let Some(mut counts) = runs(&triples[order[0].0]) else {
            return Some(None);
        };
        // The quads per centre value of each other triple: their mean over a few values.
        let per_value = |t: &IdTriple, counts: &[(u64, u64)]| -> f64 {
            let i = at(t);
            let sample: Vec<usize> = counts
                .iter()
                .step_by((counts.len() / 8).max(1))
                .take(8)
                .map(|(v, _)| {
                    let mut slots = [c(t.s), c(t.p), c(t.o)];
                    slots[i] = Some(TermId(*v));
                    self.estimate_ids(slots[0], slots[1], slots[2], graph)
                })
                .collect();
            sample.iter().sum::<usize>() as f64 / sample.len().max(1) as f64
        };
        // With centre values much fewer than another triple's quads, the usual join seeks
        // them in its scan instead of reading it all: leave it that when it builds no more
        // rows than there are quads.
        let total: usize = order.iter().map(|(_, size)| size).sum();
        if order[1..]
            .iter()
            .any(|&(_, size)| counts.len().saturating_mul(SEEKS) < size)
        {
            let rows = counts.iter().map(|(_, n)| *n as f64).sum::<f64>()
                * order[1..]
                    .iter()
                    .map(|&(k, _)| per_value(&triples[k], &counts))
                    .product::<f64>();
            if rows <= total as f64 {
                return None;
            }
        }
        for &(k, size) in &order[1..] {
            if counts.is_empty() {
                break;
            }
            // Many quads per centre value left: one scan of the triple walked forward from
            // value to value reads less than all of it (#165).
            if counts.len().saturating_mul(WALK_COST) < size
                && let Some(mut walk) = self.centre_walk(&triples[k], centre, graph)
            {
                let mut kept = Vec::with_capacity(counts.len());
                for (i, &(v, n)) in counts.iter().enumerate() {
                    if i % 1024 == 0 && cancelled() {
                        return Some(None);
                    }
                    let m = walk.count(v);
                    if m > 0 {
                        kept.push((v, n.saturating_mul(m)));
                    }
                }
                counts = kept;
                continue;
            }
            let Some(other) = runs(&triples[k]) else {
                return Some(None);
            };
            counts = merge_products(&counts, &other);
        }
        Some(Some(
            counts.iter().fold(0u64, |s, (_, n)| s.saturating_add(*n)),
        ))
    }

    /// A scan of the matches of `t` in the order of the centre value, to count the matches
    /// of increasing centre values ([`CentreWalk::count`]). `None` when the store can't give
    /// them in that order.
    fn centre_walk(&self, t: &IdTriple, centre: usize, graph: GraphSelector) -> Option<CentreWalk> {
        let at = centre_at(t, centre);
        let c = |s: IdSlot| match s {
            IdSlot::Const(id) => Some(id),
            IdSlot::Var(_) => None,
        };
        let scan = self
            .quads
            .scan_sorted(
                crate::Pattern::of_ids(c(t.s), c(t.p), c(t.o), graph),
                Some(at),
            )
            .with_rows(self.rows());
        if scan.sorted_by() != Some(at) {
            return None;
        }
        // A repeated variable (`?x p ?x`) would need the other position checked too.
        if !repeated_positions(&t.slots()).is_empty() {
            return None;
        }
        Some(CentreWalk {
            quads: Chunked {
                scan,
                buf: Vec::new(),
                pos: 0,
                at,
                single: 0,
            },
            next: None,
            done: false,
        })
    }

    /// The (centre, key) values of the matches of `t`, in the order of the centre value
    /// (then of the key). `None` past [`max_ids`] matches or once `cancelled` says so.
    fn centre_key_pairs(
        &self,
        t: &IdTriple,
        centre: usize,
        key: usize,
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<Vec<(u64, u64)>> {
        let (i, k) = (centre_at(t, centre), centre_at(t, key));
        let c = |s: IdSlot| match s {
            IdSlot::Const(id) => Some(id),
            IdSlot::Var(_) => None,
        };
        let mut scan = self
            .quads
            .scan_sorted(
                crate::Pattern::of_ids(c(t.s), c(t.p), c(t.o), graph),
                Some(i),
            )
            .with_rows(self.rows());
        let sorted = scan.sorted_by() == Some(i);
        let mut pairs: Vec<(u64, u64)> = Vec::new();
        let mut chunk = Vec::new();
        loop {
            chunk.clear();
            if !scan.next_chunk(&mut chunk) {
                break;
            }
            if cancelled() || pairs.len() > max_ids() {
                return None;
            }
            pairs.extend(chunk.iter().map(|q| (q[i], q[k])));
        }
        if !sorted {
            pairs.sort_unstable();
        }
        Some(pairs)
    }

    /// `(centre value, matches)` of a triple of a star ([`star_centre`]), sorted by value.
    /// `None` once `cancelled` says so or past [`max_ids`] matches.
    fn centre_runs(
        &self,
        t: &IdTriple,
        centre: usize,
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<Vec<(u64, u64)>> {
        let i = centre_at(t, centre);
        let c = |s: IdSlot| match s {
            IdSlot::Const(id) => Some(id),
            IdSlot::Var(_) => None,
        };
        let mut scan = self
            .quads
            .scan_sorted(
                crate::Pattern::of_ids(c(t.s), c(t.p), c(t.o), graph),
                Some(i),
            )
            .with_rows(self.rows());
        let sorted = scan.sorted_by() == Some(i);
        let mut keys: Vec<u64> = Vec::new();
        let mut chunk = Vec::new();
        loop {
            chunk.clear();
            if !scan.next_chunk(&mut chunk) {
                break;
            }
            if cancelled() || keys.len() > max_ids() {
                return None;
            }
            keys.extend(chunk.iter().map(|q| q[i]));
        }
        if !sorted {
            keys.sort_unstable();
        }
        let mut out: Vec<(u64, u64)> = Vec::new();
        for k in keys {
            match out.last_mut() {
                Some((v, n)) if *v == k => *n += 1,
                _ => out.push((k, 1)),
            }
        }
        Some(out)
    }

    /// `(value of variable key, matches)` of one triple pattern, sorted by value: the
    /// solutions of the triple per value of a variable it has once, without building them.
    /// `None` for a triple with a repeated variable or a column predicate, for all named graphs,
    /// once `cancelled` says so, or past [`max_ids`] matches.
    pub fn key_counts(
        &self,
        t: &IdTriple,
        key: usize,
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<Vec<(u64, u64)>> {
        if !keyable(t, key, graph) {
            return None;
        }
        self.centre_runs(t, key, graph, cancelled)
    }

    /// [`Self::key_counts_of`] by one scan in the order of `key`, moved forward from value to
    /// value (`values` sorted and distinct): cheaper than a lookup per value when the values
    /// are many, and than reading every match when they are fewer than the matches (#165).
    /// `None` when the store can't give the matches in that order.
    pub fn key_counts_walked(
        &self,
        t: &IdTriple,
        key: usize,
        values: &[u64],
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<Option<Vec<(u64, u64)>>> {
        if !keyable(t, key, graph) {
            return None;
        }
        let mut walk = self.centre_walk(t, key, graph)?;
        let mut out = Vec::new();
        for (i, &v) in values.iter().enumerate() {
            if i % 1024 == 0 && cancelled() {
                return Some(None);
            }
            let n = walk.count(v);
            if n > 0 {
                out.push((v, n));
            }
        }
        Some(Some(out))
    }

    /// [`Self::key_counts`] for the given values of `key` only (sorted), each looked up in the
    /// indexes: for a few values of a triple with many matches. Values without matches are
    /// left out.
    pub fn key_counts_of(
        &self,
        t: &IdTriple,
        key: usize,
        values: &[u64],
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<Vec<(u64, u64)>> {
        if !keyable(t, key, graph) {
            return None;
        }
        let at = |s: IdSlot, v: u64| match s {
            IdSlot::Var(x) if x == key => IdSlot::Const(TermId(v)),
            s => s,
        };
        let mut out = Vec::new();
        for (i, &v) in values.iter().enumerate() {
            if i % 1024 == 0 && cancelled() {
                return None;
            }
            let bound = IdTriple {
                s: at(t.s, v),
                p: at(t.p, v),
                o: at(t.o, v),
            };
            let n = self.count_matches(&bound, graph, cancelled)?;
            if n > 0 {
                out.push((v, n));
            }
        }
        Some(out)
    }

    /// The solutions of a star ([`Self::count_star`]) per value of `key`, a variable of the
    /// star, without building them (issue #179): for the centre, the product of each triple's
    /// matches for that value; for another variable, the sum over its triple's matches of the
    /// product of the other triples' matches for their centre value. Groups in the order of
    /// their ids. `None` for other patterns or keys, and when there are more than `max_groups`
    /// groups; `Some(None)` once `cancelled` says so.
    pub fn count_star_groups(
        &self,
        triples: &[IdTriple],
        graph: GraphSelector,
        key: usize,
        max_groups: usize,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<Option<Vec<(TermId, u64)>>> {
        let centre = star_centre(triples, graph)?;
        // This reads every match of every triple: when one triple matches far fewer quads
        // than another, the usual join seeks its values in the others instead.
        let sizes: Vec<usize> = triples
            .iter()
            .map(|t| self.estimate_slots(t, graph))
            .collect();
        let (smallest, largest) = (
            sizes.iter().copied().min().unwrap_or(0),
            sizes.iter().copied().max().unwrap_or(0),
        );
        // The triple holding the key, when it isn't the centre: left out of the product.
        let holder = if key == centre {
            None
        } else {
            Some(
                triples
                    .iter()
                    .position(|t| t.slots().contains(&IdSlot::Var(key)))?,
            )
        };
        if smallest.saturating_mul(SEEKS) < largest {
            return self.count_star_groups_seeking(
                triples, graph, centre, key, holder, &sizes, max_groups, cancelled,
            );
        }
        let mut product: Option<Vec<(u64, u64)>> = None;
        for (k, t) in triples.iter().enumerate() {
            if Some(k) == holder {
                continue;
            }
            if product.as_ref().is_some_and(Vec::is_empty) {
                break;
            }
            let Some(runs) = self.centre_runs(t, centre, graph, cancelled) else {
                return Some(None);
            };
            product = Some(match product {
                None => runs,
                Some(counts) => merge_products(&counts, &runs),
            });
        }
        let product = product.unwrap_or_default();
        let Some(h) = holder else {
            if product.len() > max_groups {
                return None;
            }
            return Some(Some(
                product.into_iter().map(|(v, n)| (TermId(v), n)).collect(),
            ));
        };
        // Each match of the key's triple adds the product of its centre value.
        let t = &triples[h];
        let (ci, ki) = (centre_at(t, centre), centre_at(t, key));
        let c = |s: IdSlot| match s {
            IdSlot::Const(id) => Some(id),
            IdSlot::Var(_) => None,
        };
        let mut scan = self
            .quads
            .scan_sorted(
                crate::Pattern::of_ids(c(t.s), c(t.p), c(t.o), graph),
                Some(ci),
            )
            .with_rows(self.rows());
        let mut groups: FxHashMap<u64, u64> = FxHashMap::default();
        let mut chunk = Vec::new();
        loop {
            chunk.clear();
            if !scan.next_chunk(&mut chunk) {
                break;
            }
            if cancelled() {
                return Some(None);
            }
            for q in &chunk {
                if let Ok(at) = product.binary_search_by_key(&q[ci], |(v, _)| *v) {
                    let n = groups.entry(q[ki]).or_insert(0);
                    *n = n.saturating_add(product[at].1);
                }
            }
            if groups.len() > max_groups {
                return None;
            }
        }
        let mut out: Vec<(TermId, u64)> = groups.into_iter().map(|(v, n)| (TermId(v), n)).collect();
        out.sort_unstable();
        Some(Some(out))
    }

    /// [`Self::count_star_groups`] when one triple matches far fewer quads than another: the
    /// centre values come from the smallest triple, and each other triple's matches for them
    /// are counted by seeking (as the usual join would find them, without building its rows).
    #[expect(clippy::too_many_arguments)]
    fn count_star_groups_seeking(
        &self,
        triples: &[IdTriple],
        graph: GraphSelector,
        centre: usize,
        key: usize,
        holder: Option<usize>,
        sizes: &[usize],
        max_groups: usize,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<Option<Vec<(TermId, u64)>>> {
        let small = (0..triples.len()).min_by_key(|&k| sizes[k])?;
        // The key's triple is the smallest: its (centre, key) pairs, read once in the order of
        // the centre, give both the centre values and the groups (#165). Seeking each centre
        // value in it again cost a seek per value.
        let pairs = if Some(small) == holder {
            let Some(pairs) = self.centre_key_pairs(&triples[small], centre, key, graph, cancelled)
            else {
                return Some(None);
            };
            Some(pairs)
        } else {
            None
        };
        let mut product = match &pairs {
            Some(pairs) => {
                let mut runs: Vec<(u64, u64)> = Vec::new();
                for &(v, _) in pairs {
                    if runs.last().is_none_or(|(last, _)| *last != v) {
                        runs.push((v, 1));
                    }
                }
                runs
            }
            None => match self.centre_runs(&triples[small], centre, graph, cancelled) {
                Some(runs) => runs,
                None => return Some(None),
            },
        };
        let bind = |t: &IdTriple, v: u64| {
            let at = |s: IdSlot| match s {
                IdSlot::Var(x) if x == centre => IdSlot::Const(TermId(v)),
                s => s,
            };
            IdTriple {
                s: at(t.s),
                p: at(t.p),
                o: at(t.o),
            }
        };
        for (k, t) in triples.iter().enumerate() {
            if k == small || Some(k) == holder {
                continue;
            }
            let mut kept = Vec::with_capacity(product.len());
            // Few matches per centre value: one scan in the order of the centre, moved
            // forward from value to value, instead of a new seek from the top for each (#165).
            if sizes[k] / product.len().max(1) <= RUN_WALK
                && let Some(mut walk) = self.centre_walk(t, centre, graph)
            {
                for (i, &(v, n)) in product.iter().enumerate() {
                    if i % 1024 == 0 && cancelled() {
                        return Some(None);
                    }
                    let m = walk.count(v);
                    if m > 0 {
                        kept.push((v, n.saturating_mul(m)));
                    }
                }
                product = kept;
                if product.is_empty() {
                    break;
                }
                continue;
            }
            for (i, &(v, n)) in product.iter().enumerate() {
                // count_matches does not look at `cancelled` on its block-index path.
                if i % 1024 == 0 && cancelled() {
                    return Some(None);
                }
                let Some(m) = self.count_matches(&bind(t, v), graph, cancelled) else {
                    return Some(None);
                };
                if m > 0 {
                    kept.push((v, n.saturating_mul(m)));
                }
            }
            product = kept;
            if product.is_empty() {
                break;
            }
        }
        let Some(h) = holder else {
            if product.len() > max_groups {
                return None;
            }
            return Some(Some(
                product.into_iter().map(|(v, n)| (TermId(v), n)).collect(),
            ));
        };
        if let Some(pairs) = pairs {
            let mut groups: FxHashMap<u64, u64> = FxHashMap::default();
            let mut at = 0;
            for (v, k) in pairs {
                // Both are in the order of the centre value.
                while at < product.len() && product[at].0 < v {
                    at += 1;
                }
                if at == product.len() {
                    break;
                }
                if product[at].0 == v {
                    let g = groups.entry(k).or_insert(0);
                    *g = g.saturating_add(product[at].1);
                }
            }
            if groups.len() > max_groups {
                return None;
            }
            let mut out: Vec<(TermId, u64)> =
                groups.into_iter().map(|(v, n)| (TermId(v), n)).collect();
            out.sort_unstable();
            return Some(Some(out));
        }
        let t = &triples[h];
        let ki = centre_at(t, key);
        let c = |s: IdSlot| match s {
            IdSlot::Const(id) => Some(id),
            IdSlot::Var(_) => None,
        };
        let mut groups: FxHashMap<u64, u64> = FxHashMap::default();
        let mut chunk = Vec::new();
        for &(v, n) in &product {
            if cancelled() {
                return Some(None);
            }
            let b = bind(t, v);
            let mut scan = self
                .quads
                .scan_sorted(crate::Pattern::of_ids(c(b.s), c(b.p), c(b.o), graph), None)
                .with_rows(self.rows());
            loop {
                chunk.clear();
                if !scan.next_chunk(&mut chunk) {
                    break;
                }
                for q in &chunk {
                    let g = groups.entry(q[ki]).or_insert(0);
                    *g = g.saturating_add(n);
                }
            }
            if groups.len() > max_groups {
                return None;
            }
        }
        let mut out: Vec<(TermId, u64)> = groups.into_iter().map(|(v, n)| (TermId(v), n)).collect();
        out.sort_unstable();
        Some(Some(out))
    }

    fn join(
        &self,
        triples: &[IdTriple],
        width: usize,
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
        count_last: bool,
        seed: Option<(IdRows, usize)>,
    ) -> Option<Joined> {
        // A pattern that only reads a result table's columns: its rows, read straight off the
        // table instead of a nested loop per column.
        if seed.is_none()
            && graph == GraphSelector::Default
            && let Some(rows) = crate::table::whole_rows(&self.tables, triples, width, count_last)
        {
            return Some(match rows {
                Ok(rows) => Joined::Rows(rows),
                Err(n) => Joined::Count(n),
            });
        }
        let mut bound = vec![false; width];
        let started_with_seed = seed.is_some();
        let mut rows = match seed {
            Some((rows, k)) => {
                bound[..k].fill(true);
                if rows.is_empty() {
                    return Some(Joined::Rows(rows));
                }
                rows
            }
            None => IdRows {
                width,
                ids: Vec::new(),
            },
        };
        // A triple without variables only checks that it is in the graph: it keeps all the
        // rows or none. Scanned, it would give rows without columns, which `IdRows` can't
        // count (`:s :p :o . :t :p :o1` gave no row).
        let mut left: Vec<IdTriple> = Vec::with_capacity(triples.len());
        for t in triples {
            if t.slots().iter().all(|s| matches!(s, IdSlot::Const(_))) {
                if self.count_matches(t, graph, cancelled)? == 0 {
                    rows.ids.clear();
                    return Some(if count_last {
                        Joined::Count(0)
                    } else {
                        Joined::Rows(rows)
                    });
                }
            } else {
                left.push(*t);
            }
        }
        if left.is_empty() && count_last {
            return Some(Joined::Count(if started_with_seed {
                rows.len() as u64
            } else {
                1
            }));
        }
        let mut started = started_with_seed;
        while !left.is_empty() {
            // The next triple: one sharing a variable with the rows, cheapest first.
            let n = rows.len();
            let mut best: Option<(usize, f64, Step)> = None;
            for (i, t) in left.iter().enumerate() {
                let size = self.estimate_slots(t, graph) as f64;
                let shared = t
                    .slots()
                    .iter()
                    .filter_map(|s| match s {
                        IdSlot::Var(v) if bound[*v] => Some(*v),
                        _ => None,
                    })
                    .fold(Vec::new(), |mut a, v| {
                        if !a.contains(&v) {
                            a.push(v);
                        }
                        a
                    });
                if started && shared.is_empty() && left.iter().any(|u| self.shares(u, &bound)) {
                    continue;
                }
                let (cost, step) = if !started {
                    (size * SCANNED, Step::Scan)
                } else {
                    let nested = n as f64 * LOOKUP;
                    // One shared variable: a merge join on it, after sorting both sides
                    // (free when they come sorted). More: a hash join on all of them.
                    let (bulk, step) = if shared.len() == 1 {
                        // A scan sorted by the key reads at most a block per row.
                        let read = if self.scans_sorted_by(t, shared[0], graph) {
                            (size * SCANNED).min(n as f64 * SOUGHT)
                        } else {
                            size * SCANNED
                        };
                        (read + n as f64 * MERGED, Step::Merge(shared[0]))
                    } else {
                        (size * SCANNED + n as f64 * PROBED, Step::Hash)
                    };
                    // The rows the step makes, which every later step reads: without
                    // them, `?s a ?c . ?o a ?c` (a merge on a key of few values) was
                    // taken before `?s ex:p ?o`, which binds ?o for it.
                    let made = n as f64 * self.fanout(t, &shared, &rows, graph);
                    let (cost, step) = if !shared.is_empty() && nested < bulk {
                        (nested, Step::Nested)
                    } else {
                        (bulk, step)
                    };
                    (cost + made * MERGED, step)
                };
                if best.as_ref().is_none_or(|(_, c, _)| cost < *c) {
                    best = Some((i, cost, step));
                }
            }
            let (i, _, step) = best?;
            let t = left.swap_remove(i);
            let last = count_last && left.is_empty();
            let joined = match step {
                Step::Scan => {
                    started = true;
                    if last {
                        return Some(Joined::Count(self.count_matches(&t, graph, cancelled)?));
                    }
                    // Sorted for the merge join that likely follows: on the subject when
                    // another triple has its variable (a star), else on the object's.
                    let by = [0, 2].into_iter().find(|&i| match t.slots()[i] {
                        IdSlot::Var(v) => left.iter().any(|u| u.slots().contains(&IdSlot::Var(v))),
                        IdSlot::Const(_) => false,
                    });
                    Joined::Rows(self.scan_rows(&t, width, graph, cancelled, by)?)
                }
                Step::Nested => self.nested_join(&rows, &t, &bound, graph, cancelled, last)?,
                Step::Hash => self.hash_join(&rows, &t, &bound, graph, cancelled, last)?,
                Step::Merge(v) => self.merge_join(rows, &t, v, graph, cancelled, last)?,
            };
            rows = match joined {
                Joined::Count(n) => return Some(Joined::Count(n)),
                Joined::Rows(r) => r,
            };
            for s in t.slots() {
                if let IdSlot::Var(v) = s {
                    bound[v] = true;
                }
            }
            if rows.is_empty() {
                return Some(Joined::Rows(rows));
            }
        }
        if !started {
            // The empty pattern: one empty solution.
            rows.ids = vec![TermId(0); width];
        }
        Some(Joined::Rows(rows))
    }

    /// Whether the scan of `t` comes sorted by variable `var`, so a merge join on it can seek
    /// ([`Snapshot::seek_join`]).
    fn scans_sorted_by(&self, t: &IdTriple, var: usize, graph: GraphSelector) -> bool {
        let slots = t.slots();
        let Some(at) = (0..3).find(|&i| slots[i] == IdSlot::Var(var)) else {
            return false;
        };
        let c = |s: IdSlot| match s {
            IdSlot::Const(id) => Some(id),
            IdSlot::Var(_) => None,
        };
        self.quads
            .scan_sorted(
                crate::Pattern::of_ids(c(t.s), c(t.p), c(t.o), graph),
                Some(at),
            )
            .sorted_by()
            == Some(at)
    }

    /// About how many solutions `triples` have, from the block index and the per-predicate
    /// fanouts kept with it, without joining: the smallest triple's matches, times the
    /// matches per value of each next triple sharing a variable with those before (one
    /// when it shares two), times all the matches of a triple sharing none. For choosing
    /// between joining a pattern on its own and looking it up per value of another's rows
    /// (`?s a ?c . ?o a ?c` alone is every pair of things of a class).
    pub fn estimate_rows(&self, triples: &[IdTriple], graph: GraphSelector) -> f64 {
        let mut left: Vec<IdTriple> = triples.to_vec();
        let Some(first) = (0..left.len()).min_by_key(|&i| self.estimate_slots(&left[i], graph))
        else {
            return 1.0;
        };
        let t = left.swap_remove(first);
        let mut rows = self.estimate_slots(&t, graph) as f64;
        let mut bound: Vec<usize> = Vec::new();
        let vars = |t: &IdTriple| -> Vec<(usize, usize)> {
            t.slots()
                .iter()
                .enumerate()
                .filter_map(|(i, s)| match s {
                    IdSlot::Var(v) => Some((i, *v)),
                    IdSlot::Const(_) => None,
                })
                .collect()
        };
        bound.extend(vars(&t).into_iter().map(|(_, v)| v));
        while !left.is_empty() && rows > 0.0 {
            // The next triple: one sharing a variable, the one with fewest matches per value.
            let per_value = |t: &IdTriple| -> Option<f64> {
                let shared: Vec<usize> = vars(t)
                    .into_iter()
                    .filter(|(_, v)| bound.contains(v))
                    .map(|(i, _)| i)
                    .collect();
                if shared.is_empty() {
                    return None;
                }
                let size = self.estimate_slots(t, graph) as f64;
                Some(match (shared.as_slice(), t.p) {
                    ([0], IdSlot::Const(p)) | ([2], IdSlot::Const(p))
                        if p.kind() != crate::Kind::Column =>
                    {
                        self.per_value(p, shared[0] == 0, graph).min(size)
                    }
                    ([_], _) => size,
                    _ => size.min(1.0),
                })
            };
            let next = (0..left.len())
                .filter_map(|i| per_value(&left[i]).map(|n| (i, n)))
                .min_by(|a, b| a.1.total_cmp(&b.1));
            let (i, factor) = match next {
                Some(n) => n,
                None => (0, self.estimate_slots(&left[0], graph) as f64),
            };
            let t = left.swap_remove(i);
            rows *= factor;
            bound.extend(vars(&t).into_iter().map(|(_, v)| v));
        }
        rows
    }

    /// The quads with predicate `p` per value of their subject (`subject`) or object, as
    /// a value bound from the data meets them: kept with the indexes, or else the mean over
    /// the values of a few quads.
    fn per_value(&self, p: TermId, subject: bool, graph: GraphSelector) -> f64 {
        if let Some(n) = self.predicate_fanout(p, subject) {
            return n as f64;
        }
        const SAMPLES: usize = 16;
        let (mut sum, mut taken) = (0usize, 0usize);
        for q in self
            .quad_ids_for_pattern(None, Some(p), None, graph)
            .take(SAMPLES)
        {
            let v = if subject { q[0] } else { q[2] };
            sum += if subject {
                self.estimate_ids(Some(v), Some(p), None, graph)
            } else {
                self.estimate_ids(None, Some(p), Some(v), graph)
            };
            taken += 1;
        }
        sum as f64 / taken.max(1) as f64
    }

    /// The mean matches of `t` per row of `rows` once the `shared` variables take the row's
    /// values, from the exact counts of a few rows spread over them.
    fn fanout(&self, t: &IdTriple, shared: &[usize], rows: &IdRows, graph: GraphSelector) -> f64 {
        let n = rows.len();
        if n == 0 || shared.is_empty() {
            return self.estimate_slots(t, graph) as f64;
        }
        const SAMPLES: usize = 8;
        let step = (n / SAMPLES).max(1);
        let mut sum = 0usize;
        let mut taken = 0usize;
        for row in rows.rows().step_by(step).take(SAMPLES) {
            let value = |s: IdSlot| match s {
                IdSlot::Const(id) => Some(id),
                IdSlot::Var(v) if shared.contains(&v) => Some(row[v]),
                IdSlot::Var(_) => None,
            };
            sum += self.estimate_ids(value(t.s), value(t.p), value(t.o), graph);
            taken += 1;
        }
        sum as f64 / taken as f64
    }

    fn shares(&self, t: &IdTriple, bound: &[bool]) -> bool {
        t.slots()
            .iter()
            .any(|s| matches!(s, IdSlot::Var(v) if bound[*v]))
    }

    fn estimate_slots(&self, t: &IdTriple, graph: GraphSelector) -> usize {
        let c = |s: IdSlot| match s {
            IdSlot::Const(id) => Some(id),
            IdSlot::Var(_) => None,
        };
        self.estimate_ids(c(t.s), c(t.p), c(t.o), graph)
    }

    /// The quads matching `t` with `row`'s values for its bound variables, checked against
    /// the row and each other (a variable twice in the triple), sorted by position `by` when
    /// a quad index gives that order ([`Snapshot::quad_ids_sorted`]).
    fn matches<'r>(
        &self,
        t: &IdTriple,
        row: Option<&'r [TermId]>,
        bound: &[bool],
        graph: GraphSelector,
        by: Option<usize>,
    ) -> impl Iterator<Item = [TermId; 3]> + use<'r> {
        let value = |s: IdSlot| match s {
            IdSlot::Const(id) => Some(id),
            IdSlot::Var(v) if bound.get(v).copied().unwrap_or(false) => row.map(|r| r[v]),
            IdSlot::Var(_) => None,
        };
        let slots = t.slots();
        self.quad_ids_by(value(t.s), value(t.p), value(t.o), graph, by)
            .map(|q| [q[0], q[1], q[2]])
            .filter(move |q| {
                // A variable repeated in the triple takes one value.
                (0..3).all(|i| match slots[i] {
                    IdSlot::Var(v) => (0..i).all(|j| slots[j] != IdSlot::Var(v) || q[j] == q[i]),
                    IdSlot::Const(_) => true,
                })
            })
    }

    /// The rows of the first triple, sorted by position `by` when an index gives that order.
    fn scan_rows(
        &self,
        t: &IdTriple,
        width: usize,
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
        by: Option<usize>,
    ) -> Option<IdRows> {
        let mut ids = Vec::with_capacity((self.estimate_slots(t, graph) * width).min(max_ids()));
        let slots = t.slots();
        let same = repeated_positions(&slots);
        if let Some(quads) = self.self_loops(t, graph, cancelled) {
            let quads = quads?;
            if quads.len().saturating_mul(width) > max_ids() {
                return None;
            }
            for q in quads {
                let start = ids.len();
                ids.resize(start + width, TermId(0));
                for (i, s) in slots.iter().enumerate() {
                    if let IdSlot::Var(c) = s {
                        ids[start + c] = TermId(q[i]);
                    }
                }
            }
            return Some(IdRows { width, ids });
        }
        {
            // Chunks of the scan straight into the rows (#169), a repeated variable checked on
            // each quad.
            let c = |s: IdSlot| match s {
                IdSlot::Const(id) => Some(id),
                IdSlot::Var(_) => None,
            };
            let mut scan = self
                .quads
                .scan_sorted(crate::Pattern::of_ids(c(t.s), c(t.p), c(t.o), graph), by)
                .with_rows(self.rows());
            let vars: Vec<(usize, usize)> = (0..3)
                .filter_map(|i| match slots[i] {
                    IdSlot::Var(v) => Some((i, v)),
                    IdSlot::Const(_) => None,
                })
                .collect();
            let mut chunk = Vec::new();
            let mut row = vec![TermId(0); width];
            let mut k = 0usize;
            loop {
                chunk.clear();
                if !scan.next_chunk(&mut chunk) {
                    break;
                }
                let before = k;
                k += chunk.len();
                if (before == 0 || before / 4096 != k / 4096) && cancelled()
                    || k.saturating_mul(width) > max_ids()
                {
                    return None;
                }
                for q in &chunk {
                    if !same.iter().all(|&(i, j)| q[i] == q[j]) {
                        continue;
                    }
                    for &(i, v) in &vars {
                        row[v] = TermId(q[i]);
                    }
                    ids.extend_from_slice(&row);
                }
            }
            Some(IdRows { width, ids })
        }
    }

    fn count_matches(
        &self,
        t: &IdTriple,
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<u64> {
        // Without a variable twice in the triple, the block index counts it exactly (except
        // over named graphs only), without reading the quads.
        let slots = t.slots();
        let repeated = (0..3)
            .any(|i| matches!(slots[i], IdSlot::Var(v) if slots[..i].contains(&IdSlot::Var(v))));
        let column = matches!(t.p, IdSlot::Const(p) if p.kind() == crate::Kind::Column);
        // A row filter (#202) hides quads the block index counts: then they are read.
        if !repeated && !column && graph != GraphSelector::AnyNamed && self.rows().is_none() {
            return Some(self.estimate_slots(t, graph) as u64);
        }
        if let Some(quads) = self.self_loops(t, graph, cancelled) {
            return Some(quads?.len() as u64);
        }
        // Chunks of the scan (#169), a repeated variable checked on each quad.
        let c = |s: IdSlot| match s {
            IdSlot::Const(id) => Some(id),
            IdSlot::Var(_) => None,
        };
        let mut scan = self
            .quads
            .scan_sorted(crate::Pattern::of_ids(c(t.s), c(t.p), c(t.o), graph), None)
            .with_rows(self.rows());
        let same = repeated_positions(&slots);
        let mut chunk = Vec::new();
        let (mut n, mut k) = (0u64, 0usize);
        loop {
            chunk.clear();
            if !scan.next_chunk(&mut chunk) {
                break;
            }
            let before = k;
            k += chunk.len();
            if (before == 0 || before / 4096 != k / 4096) && cancelled() {
                return None;
            }
            n += chunk
                .iter()
                .filter(|q| same.iter().all(|&(i, j)| q[i] == q[j]))
                .count() as u64;
        }
        Some(n)
    }

    /// The quads of `?x <p> ?x`, the same variable as subject and object of a constant
    /// predicate (`FILTER(?s = ?o)` on `?s <p> ?o`, #165), found without reading all of the
    /// predicate's quads: a leapfrog of its quads by object and by subject, which skips from
    /// one value to the next one both have (the few classes of `rdf:type` against its many
    /// subjects). `None` for other triples, or when the two orders interleave so much that
    /// reading the quads is cheaper; `Some(None)` once `cancelled`.
    fn self_loops(
        &self,
        t: &IdTriple,
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Option<Option<Vec<[u64; 4]>>> {
        let (IdSlot::Var(x), IdSlot::Const(p), IdSlot::Var(y)) = (t.s, t.p, t.o) else {
            return None;
        };
        if x != y {
            return None;
        }
        let scan = |by: usize| {
            let scan = self
                .quads
                .scan_sorted(crate::Pattern::of_ids(None, Some(p), None, graph), Some(by))
                .with_rows(self.rows());
            (scan.sorted_by() == Some(by)).then_some(Chunked {
                scan,
                buf: Vec::new(),
                pos: 0,
                at: by,
                single: 0,
            })
        };
        let (mut by_o, mut by_s) = (scan(2)?, scan(0)?);
        // Past this many steps, a step per quad or so, reading the quads is cheaper.
        let budget = self.estimate_slots(t, graph) / 8 + 64;
        let mut out = Vec::new();
        let (Some(mut a), Some(mut b)) = (by_o.next(), by_s.next()) else {
            return Some(Some(out));
        };
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > budget {
                return None;
            }
            if steps.is_multiple_of(1024) && cancelled() {
                return Some(None);
            }
            let (o, s) = (a[2], b[0]);
            if o < s {
                by_o.seek(s);
                let Some(next) = by_o.next() else { break };
                a = next;
            } else if s < o {
                by_s.seek(o);
                let Some(next) = by_s.next() else { break };
                b = next;
            } else {
                // The quads of subject `s`, those with `s` as object too.
                loop {
                    if b[2] == s {
                        out.push(b);
                    }
                    match by_s.next() {
                        Some(next) if next[0] == s => b = next,
                        Some(next) => {
                            b = next;
                            break;
                        }
                        None => return Some(Some(out)),
                    }
                }
                by_o.seek(s + 1);
                let Some(next) = by_o.next() else { break };
                a = next;
            }
        }
        Some(Some(out))
    }

    fn nested_join(
        &self,
        rows: &IdRows,
        t: &IdTriple,
        bound: &[bool],
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
        count: bool,
    ) -> Option<Joined> {
        let slots = t.slots();
        let mut out = Out::new(rows, 0, cancelled);
        let mut n = 0;
        for (k, row) in rows.rows().enumerate() {
            if k % 1024 == 0 && cancelled() {
                return None;
            }
            if count {
                n += self.matches(t, Some(row), bound, graph, None).count() as u64;
                continue;
            }
            for q in self.matches(t, Some(row), bound, graph, None) {
                out.push(row, &slots, &q)?;
            }
        }
        Some(if count {
            Joined::Count(n)
        } else {
            Joined::Rows(out.rows())
        })
    }

    fn hash_join(
        &self,
        rows: &IdRows,
        t: &IdTriple,
        bound: &[bool],
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
        count: bool,
    ) -> Option<Joined> {
        let width = rows.width;
        let slots = t.slots();
        // Positions of the triple whose variable the rows bind (the key), and the others.
        let keyed: Vec<usize> = (0..3)
            .filter(|&i| matches!(slots[i], IdSlot::Var(v) if bound[v]))
            .collect();
        let key_of = |q: &[TermId; 3]| -> [u64; 3] {
            let mut k = [0; 3];
            for (j, &i) in keyed.iter().enumerate() {
                k[j] = q[i].0;
            }
            k
        };
        let row_key = |row: &[TermId]| -> [u64; 3] {
            let mut key = [0; 3];
            for (j, &i) in keyed.iter().enumerate() {
                let IdSlot::Var(v) = slots[i] else {
                    unreachable!()
                };
                key[j] = row[v].0;
            }
            key
        };
        // The rows by key, as chains through `next` (no allocation per row), with their
        // length (a count adds it), probed by one scan of the triple. Without a key (a cross
        // product) every row is in one chain.
        const END: usize = usize::MAX;
        let n = rows.len();
        let mut heads: FxHashMap<[u64; 3], (usize, u64)> = FxHashMap::default();
        heads.reserve(n);
        let mut next = vec![END; n];
        for (r, row) in rows.rows().enumerate().rev() {
            let head = heads.entry(row_key(row)).or_insert((END, 0));
            next[r] = head.0;
            *head = (r, head.1 + 1);
        }
        let none = vec![false; width];
        let mut out = Out::new(rows, 0, cancelled);
        let mut matched = 0;
        for (k, q) in self.matches(t, None, &none, graph, None).enumerate() {
            if k % 4096 == 0 && cancelled() {
                return None;
            }
            let Some(&(head, len)) = heads.get(&key_of(&q)) else {
                continue;
            };
            if count {
                matched += len;
                continue;
            }
            let mut r = head;
            while r != END {
                out.push(&rows.ids[r * width..(r + 1) * width], &slots, &q)?;
                r = next[r];
            }
        }
        Some(if count {
            Joined::Count(matched)
        } else {
            Joined::Rows(out.rows())
        })
    }

    /// [`Snapshot::merge_join`] when the scan of `t` comes sorted by `var` (at position `at`
    /// of the triple): the rows in key order, and for each key the scan sought to it, so
    /// blocks between the keys of the rows are not read. `None` when the scan has another
    /// order; `Some(None)` once cancelled or too large.
    #[allow(clippy::too_many_arguments)]
    fn seek_join(
        &self,
        rows: &IdRows,
        t: &IdTriple,
        var: usize,
        at: usize,
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
        count: bool,
    ) -> Option<Option<Joined>> {
        let slots = t.slots();
        let c = |s: IdSlot| match s {
            IdSlot::Const(id) => Some(id),
            IdSlot::Var(_) => None,
        };
        let pattern = crate::Pattern::of_ids(c(t.s), c(t.p), c(t.o), graph);
        let open = || {
            self.quads
                .scan_sorted(pattern, Some(at))
                .with_rows(self.rows())
        };
        if open().sorted_by() != Some(at) {
            return None;
        }
        let width = rows.width;
        let n = rows.len();
        let key = |r: usize| rows.ids[r * width + var].0;
        // The rows in key order: as they are when already sorted, else each row's key and
        // number, sorted (equal keys by number, so in row order, as a stable sort gives).
        let order: Option<Vec<(u64, u32)>> = if (1..n).all(|r| key(r - 1) <= key(r)) {
            None
        } else {
            let mut order: Vec<(u64, u32)> = (0..n).map(|r| (key(r), r as u32)).collect();
            crate::par::sort_unstable(&mut order);
            Some(order)
        };
        let row_at = |i: usize| order.as_ref().map_or(i, |o| o[i].1 as usize);
        let key = |i: usize| order.as_ref().map_or_else(|| key(i), |o| o[i].0);
        // Many rows on several cores (#4): each chunk of rows, its ends moved to the next
        // change of key so that no key is split, walks a scan of its own; the chunks' rows
        // are put back in chunk order, which is the order one walk gives.
        let made = AtomicUsize::new(rows.ids.len());
        let edge = |x: usize| {
            (x..n)
                .find(|&y| y == 0 || key(y) != key(y - 1))
                .unwrap_or(n)
        };
        let parts = crate::par::chunks(n, |range| {
            // Chunks are copied into one table at the end: their rows may take half the
            // limit, so that the copy fits in the rest (#234).
            let merged = range.len() < n;
            let (lo, hi) = (edge(range.start), edge(range.end));
            self.seek_run(
                rows,
                &slots,
                at,
                open(),
                merged,
                lo..hi,
                &key,
                &row_at,
                &made,
                cancelled,
                count,
            )
        });
        // A chunk cancelled or too large: the join is.
        let Some(parts) = parts.into_iter().collect::<Option<Vec<_>>>() else {
            return Some(None);
        };
        let matched: u64 = parts.iter().map(|(_, m)| m).sum();
        let total: usize = parts.iter().map(|(part, _)| part.ids.len()).sum();
        let mut parts = parts.into_iter().map(|(part, _)| part);
        let Some(mut out) = parts.next() else {
            return Some(None);
        };
        out.ids.reserve_exact(total - out.ids.len());
        for part in parts {
            out.ids.extend_from_slice(&part.ids);
        }
        Some(Some(if count {
            Joined::Count(matched)
        } else {
            Joined::Rows(out)
        }))
    }

    /// The rows `range` (in key order: `key` and `row_at`) of [`Snapshot::seek_join`] joined
    /// with the quads of `scan`, sorted by position `at`; with `count`, only how many. `None`
    /// once cancelled or too large: the ids made by every chunk, counted in `made`, past
    /// [`max_ids`].
    #[allow(clippy::too_many_arguments)]
    fn seek_run(
        &self,
        rows: &IdRows,
        slots: &[IdSlot; 3],
        at: usize,
        scan: crate::index::Scan,
        merged: bool,
        range: Range<usize>,
        key: &(dyn Fn(usize) -> u64 + Sync),
        row_at: &(dyn Fn(usize) -> usize + Sync),
        made: &AtomicUsize,
        cancelled: &(dyn Fn() -> bool + Sync),
        count: bool,
    ) -> Option<(IdRows, u64)> {
        // A variable repeated in the triple takes one value.
        let keep = |q: &[u64; 4]| {
            (0..3).all(|i| match slots[i] {
                IdSlot::Var(v) => (0..i).all(|j| slots[j] != IdSlot::Var(v) || q[j] == q[i]),
                IdSlot::Const(_) => true,
            })
        };
        let width = rows.width;
        let n = range.end;
        // About one output row per row is the common case (a star on the subject).
        let mut out = Out::new(rows, if count { 0 } else { range.len() }, cancelled);
        let max = if merged { out.max / 2 } else { out.max }.saturating_add(rows.ids.len());
        // The ids of this chunk already added to `made` (its capacity: a `Vec`'s room is
        // memory too); false once past the limit.
        let mut counted = 0;
        let within = |now: usize, counted: &mut usize| {
            let more = now - *counted;
            *counted = now;
            made.fetch_add(more, Ordering::Relaxed) + more <= max
        };
        let mut matched = 0u64;
        let mut run: Vec<[TermId; 3]> = Vec::new();
        let mut scan = Chunked {
            scan,
            buf: Vec::new(),
            pos: 0,
            at,
            single: 0,
        };
        let mut i = range.start;
        if i < n {
            scan.seek(key(i));
        }
        let mut cur = scan.next();
        let mut steps = 0usize;
        while i < n {
            let Some(q) = cur else { break };
            steps += 1;
            // The other chunks' rows count against the same limit.
            if steps.is_multiple_of(1024)
                && (cancelled() || !within(out.ids.capacity(), &mut counted))
            {
                return None;
            }
            let a = key(i);
            // Runs of a key are short: step through them.
            let i2 = (i..n).find(|&x| key(x) != a).unwrap_or(n);
            if q[at] < a {
                scan.seek(a);
                cur = scan.next();
                continue;
            }
            if q[at] > a {
                // No quad for this key: on to the first row key at or past the quad's.
                i = gallop(i, n, |x| key(x) < q[at]);
                continue;
            }
            run.clear();
            while let Some(q) = cur.filter(|q| q[at] == a) {
                if keep(&q) {
                    run.push([TermId(q[0]), TermId(q[1]), TermId(q[2])]);
                }
                cur = scan.next();
            }
            if count {
                matched += ((i2 - i) * run.len()) as u64;
            } else {
                for x in i..i2 {
                    let r = row_at(x);
                    let row = &rows.ids[r * width..(r + 1) * width];
                    for q in &run {
                        out.push(row, slots, q)?;
                    }
                }
            }
            i = i2;
        }
        within(out.ids.capacity(), &mut counted).then(|| (out.rows(), matched))
    }

    /// Joins `rows` with `t` on the one variable `var` they share: both sides sorted by it
    /// (each only if it doesn't come sorted already), then merged run by run.
    fn merge_join(
        &self,
        rows: IdRows,
        t: &IdTriple,
        var: usize,
        graph: GraphSelector,
        cancelled: &(dyn Fn() -> bool + Sync),
        count: bool,
    ) -> Option<Joined> {
        let width = rows.width;
        let slots = t.slots();
        let at = (0..3)
            .find(|&i| slots[i] == IdSlot::Var(var))
            .expect("the shared variable is in the triple");
        // Sorted by the key when an index gives that order (PSOG for `?s <p> ?o` on `?s`):
        // then the scan is read along the rows, seeking past keys no row has (#169).
        if let Some(joined) = self.seek_join(&rows, t, var, at, graph, cancelled, count) {
            return joined;
        }
        let mut quads: Vec<[TermId; 3]> = Vec::new();
        for (k, q) in self.matches(t, None, &[], graph, Some(at)).enumerate() {
            if k % 4096 == 0 && cancelled() {
                return None;
            }
            quads.push(q);
        }
        if !quads.is_sorted_by_key(|q| q[at]) {
            quads.sort_unstable_by_key(|q| q[at]);
        }
        if cancelled() {
            return None;
        }
        // The rows in key order: as they are when already sorted, else through an order.
        let n = rows.len();
        let key = |r: usize| rows.ids[r * width + var];
        let order: Option<Vec<u32>> = if (1..n).all(|r| key(r - 1) <= key(r)) {
            None
        } else {
            let mut keyed: Vec<(TermId, u32)> = (0..n).map(|r| (key(r), r as u32)).collect();
            keyed.sort_unstable();
            Some(keyed.into_iter().map(|(_, r)| r).collect())
        };
        let row_at = |i: usize| order.as_ref().map_or(i, |o| o[i] as usize);
        let mut out = Out::new(&rows, 0, cancelled);
        let mut matched = 0u64;
        let (mut i, mut j) = (0, 0);
        let mut steps = 0usize;
        while i < n && j < quads.len() {
            steps += 1;
            if steps.is_multiple_of(4096) && cancelled() {
                return None;
            }
            let (a, b) = (key(row_at(i)), quads[j][at]);
            if a < b {
                i += 1;
            } else if a > b {
                j += 1;
            } else {
                let i2 = (i..n).find(|&x| key(row_at(x)) != a).unwrap_or(n);
                let j2 = (j..quads.len())
                    .find(|&x| quads[x][at] != a)
                    .unwrap_or(quads.len());
                if count {
                    matched += ((i2 - i) * (j2 - j)) as u64;
                } else {
                    for x in i..i2 {
                        let r = row_at(x);
                        let row = &rows.ids[r * width..(r + 1) * width];
                        for q in &quads[j..j2] {
                            out.push(row, &slots, q)?;
                        }
                    }
                }
                i = i2;
                j = j2;
            }
        }
        Some(if count {
            Joined::Count(matched)
        } else {
            Joined::Rows(out.rows())
        })
    }
}

/// Matches per centre value below which [`Snapshot::count_star_groups_seeking`] walks one
/// sorted scan forward instead of seeking each value from the top.
const RUN_WALK: usize = 64;

/// A value found by walking a sorted scan forward costs about as much as reading this many
/// quads in order.
const WALK_COST: usize = 4;

/// See [`Snapshot::centre_walk`].
struct CentreWalk {
    quads: Chunked,
    /// The first quad not yet counted.
    next: Option<[u64; 4]>,
    done: bool,
}

impl CentreWalk {
    /// The matches whose centre is `value`; each call's `value` above the last one's.
    fn count(&mut self, value: u64) -> u64 {
        let at = self.quads.at;
        if self.done {
            return 0;
        }
        if self.next.is_none_or(|q| q[at] < value) {
            self.quads.seek(value);
            self.next = self.quads.next();
        }
        let mut n = 0;
        while let Some(q) = self.next {
            if q[at] != value {
                break;
            }
            n += 1;
            self.next = self.quads.next();
        }
        if self.next.is_none() {
            self.done = true;
        }
        n
    }
}

/// A sorted scan read a chunk at a time (#169), with seeks inside the chunk.
struct Chunked {
    scan: crate::index::Scan,
    buf: Vec<[u64; 4]>,
    pos: usize,
    /// The position the scan comes sorted by.
    at: usize,
    /// Quads to read one by one before the next chunk: right after a seek, which often
    /// finds only a few.
    single: usize,
}

impl Chunked {
    #[inline]
    fn next(&mut self) -> Option<[u64; 4]> {
        if self.pos == self.buf.len() {
            if self.single > 0 {
                self.single -= 1;
                return self.scan.next();
            }
            self.buf.clear();
            self.pos = 0;
            if !self.scan.next_chunk(&mut self.buf) {
                return None;
            }
        }
        self.pos += 1;
        Some(self.buf[self.pos - 1])
    }

    /// Skips to the first quad whose sorted position is at least `value`.
    fn seek(&mut self, value: u64) {
        let at = self.at;
        let rest = &self.buf[self.pos..];
        if rest.last().is_some_and(|k| k[at] >= value) {
            self.pos += rest.partition_point(|k| k[at] < value);
        } else {
            self.buf.clear();
            self.pos = 0;
            self.single = 4;
            self.scan.seek(value);
        }
    }
}

/// The first index in `from..to` where `before` turns false (it holds for a prefix): an
/// exponential search from `from`, so a target near it costs a few probes.
fn gallop(from: usize, to: usize, before: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (from, to);
    let mut step = 1;
    loop {
        let probe = from + step - 1;
        if probe >= to {
            break;
        }
        if !before(probe) {
            hi = probe;
            break;
        }
        lo = probe + 1;
        step *= 2;
    }
    // The answer is in `lo..=hi`: `hi` is `to` or fails `before`.
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if before(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// The centre of a star: a variable every triple has once, the triples' other variables each
/// in one triple only, once (`?s a ?c . ?s ex:f ?f`). `None` for other patterns, fewer than
/// two triples, named graphs only, and tables.
fn star_centre(triples: &[IdTriple], graph: GraphSelector) -> Option<usize> {
    if triples.len() < 2 || graph == GraphSelector::AnyNamed {
        return None;
    }
    let vars = |t: &IdTriple| -> Vec<usize> {
        t.slots()
            .iter()
            .filter_map(|s| match s {
                IdSlot::Var(v) => Some(*v),
                IdSlot::Const(_) => None,
            })
            .collect()
    };
    let centre = vars(&triples[0]).into_iter().find(|c| {
        triples
            .iter()
            .all(|t| vars(t).iter().filter(|v| *v == c).count() == 1)
    })?;
    // Every other variable once in the whole pattern.
    let mut others: Vec<usize> = triples
        .iter()
        .flat_map(|t| vars(t).into_iter().filter(|v| *v != centre))
        .collect();
    let n = others.len();
    others.sort_unstable();
    others.dedup();
    if others.len() != n
        || triples
            .iter()
            .any(|t| matches!(t.p, IdSlot::Const(p) if p.kind() == crate::Kind::Column))
    {
        return None;
    }
    Some(centre)
}

/// Whether [`Snapshot::key_counts`] counts `t` per value of `key`: a variable of the triple,
/// whose variables are all different, with no column predicate, outside all named graphs.
fn keyable(t: &IdTriple, key: usize, graph: GraphSelector) -> bool {
    let vars: Vec<usize> = t
        .slots()
        .iter()
        .filter_map(|s| match s {
            IdSlot::Var(v) => Some(*v),
            IdSlot::Const(_) => None,
        })
        .collect();
    let distinct = vars.iter().enumerate().all(|(i, v)| !vars[..i].contains(v));
    graph != GraphSelector::AnyNamed
        && distinct
        && vars.contains(&key)
        && !matches!(t.p, IdSlot::Const(p) if p.kind() == crate::Kind::Column)
}

/// The position of variable `v` in a triple that has it.
fn centre_at(t: &IdTriple, v: usize) -> usize {
    (0..3).find(|&i| t.slots()[i] == IdSlot::Var(v)).unwrap()
}

/// The values both sorted runs have, with the product of their counts.
fn merge_products(a: &[(u64, u64)], b: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut merged = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                merged.push((a[i].0, a[i].1.saturating_mul(b[j].1)));
                i += 1;
                j += 1;
            }
        }
    }
    merged
}

/// The positions (`i < j`, of subject, predicate and object) of a triple that hold the same
/// variable: a quad matches only with the same id at both.
fn repeated_positions(slots: &[IdSlot; 3]) -> Vec<(usize, usize)> {
    let mut same = Vec::new();
    for j in 0..3 {
        for i in 0..j {
            if matches!(slots[j], IdSlot::Var(_)) && slots[i] == slots[j] {
                same.push((i, j));
            }
        }
    }
    same
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use oxigraph::model::{GraphNameRef, Literal, NamedNode, QuadRef};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn n(i: &str) -> NamedNode {
        NamedNode::new(format!("http://e/{i}")).unwrap()
    }

    #[test]
    fn gallop_finds_the_end_of_a_prefix() {
        for n in 0..40 {
            for k in 0..=n {
                for from in 0..=k {
                    assert_eq!(gallop(from, n, |x| x < k), k, "{n} {k} {from}");
                }
            }
        }
    }

    #[test]
    fn joins_triples_on_ids_by_both_strategies() {
        let store = Store::new();
        let (a, has, wkt, other) = (n("A"), n("has"), n("wkt"), n("Other"));
        let ty = NamedNode::new("http://www.w3.org/1999/02/22-rdf-syntax-ns#type").unwrap();
        for i in 0..300 {
            let (f, g) = (n(&format!("f{i}")), n(&format!("g{i}")));
            let class = if i % 3 == 0 { &a } else { &other };
            let w = Literal::new_simple_literal(format!("w{i}"));
            for q in [
                QuadRef::new(&f, &ty, class, GraphNameRef::DefaultGraph),
                QuadRef::new(&f, &has, &g, GraphNameRef::DefaultGraph),
            ] {
                store.insert(q).unwrap();
            }
            store
                .insert(QuadRef::new(&g, &wkt, &w, GraphNameRef::DefaultGraph))
                .unwrap();
        }
        // A self-loop, for a variable twice in a triple.
        store
            .insert(QuadRef::new(&a, &has, &a, GraphNameRef::DefaultGraph))
            .unwrap();
        let snap = store.snapshot();
        let id = |t: &NamedNode| IdSlot::Const(snap.term_id(t.as_ref().into()).unwrap());
        let triples = [
            IdTriple {
                s: IdSlot::Var(0),
                p: id(&ty),
                o: id(&a),
            },
            IdTriple {
                s: IdSlot::Var(0),
                p: id(&has),
                o: IdSlot::Var(1),
            },
            IdTriple {
                s: IdSlot::Var(1),
                p: id(&wkt),
                o: IdSlot::Var(2),
            },
        ];
        let rows = snap
            .join_ids(&triples, 3, GraphSelector::Default, &|| false)
            .unwrap();
        assert_eq!(rows.len(), 100);
        for row in rows.rows() {
            let f = snap.term(row[0]).unwrap().to_string();
            let w = snap.term(row[2]).unwrap().to_string();
            assert_eq!(
                f.trim_start_matches("<http://e/f").trim_end_matches('>'),
                w.trim_matches('"').trim_start_matches('w')
            );
        }
        let looped = [IdTriple {
            s: IdSlot::Var(0),
            p: id(&has),
            o: IdSlot::Var(0),
        }];
        let rows = snap
            .join_ids(&looped, 1, GraphSelector::Default, &|| false)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            snap.join_ids(&triples, 3, GraphSelector::Default, &|| true)
                .is_none()
        );
        // One triple is counted from the block index (or by a scan, for a variable twice):
        // the same as its rows.
        for t in triples.iter().chain(&looped) {
            for graph in [
                GraphSelector::Default,
                GraphSelector::All,
                GraphSelector::AnyNamed,
            ] {
                let rows = snap.join_ids(std::slice::from_ref(t), 3, graph, &|| false);
                let count =
                    snap.count_ids(std::slice::from_ref(t), 3, graph, BgpCount::Rows, &|| false);
                assert_eq!(count, rows.map(|r| r.len() as u64), "{t:?} {graph:?}");
            }
        }
    }

    #[test]
    fn a_cycle_joins_the_triple_that_binds_its_key_first() {
        // `?s a ?c . ?s ex:p ?o . ?o a ?c`, everything of one class: the two type triples
        // joined on ?c first make 4M rows; ex:p first binds ?o, and the rows stay 6,000.
        let store = Store::new();
        let (a, p, c) = (n("type"), n("p"), n("C"));
        let things = 2000;
        for i in 0..things {
            let s = n(&format!("s{i}"));
            store
                .insert(QuadRef::new(&s, &a, &c, GraphNameRef::DefaultGraph))
                .unwrap();
            for k in 1..=3 {
                let o = n(&format!("s{}", (i + k) % things));
                store
                    .insert(QuadRef::new(&s, &p, &o, GraphNameRef::DefaultGraph))
                    .unwrap();
            }
        }
        let snap = store.snapshot();
        let id = |t: &NamedNode| IdSlot::Const(snap.term_id(t.as_ref().into()).unwrap());
        let (a, p) = (id(&a), id(&p));
        let (s, c, o) = (IdSlot::Var(0), IdSlot::Var(1), IdSlot::Var(2));
        let classes = [IdTriple { s, p: a, o: c }, IdTriple { s: o, p: a, o: c }];
        let triples = [classes[0], IdTriple { s, p, o }, classes[1]];
        assert!(snap.estimate_rows(&classes, GraphSelector::Default) >= 1e6);
        assert!(snap.estimate_rows(&triples, GraphSelector::Default) <= 1e5);
        // Rows are made between calls of `cancelled`: 4M rows would call it dozens of times.
        let calls = AtomicUsize::new(0);
        let cancelled = || {
            calls.fetch_add(1, AtomicOrdering::Relaxed);
            false
        };
        let count = snap.count_ids(
            &triples,
            3,
            GraphSelector::Default,
            BgpCount::Rows,
            &cancelled,
        );
        assert_eq!(count, Some(6000));
        assert!(calls.load(AtomicOrdering::Relaxed) < 20, "{calls:?}");
    }

    #[test]
    fn cross_products_count_without_rows_and_stop_when_cancelled() {
        // 200 triples: a cross product of three is 8M rows (#467).
        let store = Store::new();
        let p = n("p");
        for i in 0..200 {
            let (s, o) = (n(&format!("s{i}")), n(&format!("o{i}")));
            store
                .insert(QuadRef::new(&s, &p, &o, GraphNameRef::DefaultGraph))
                .unwrap();
        }
        let snap = store.snapshot();
        let p = IdSlot::Const(snap.term_id(p.as_ref().into()).unwrap());
        let triples: Vec<IdTriple> = (0..3)
            .map(|i| IdTriple {
                s: IdSlot::Var(2 * i),
                p,
                o: IdSlot::Var(2 * i + 1),
            })
            .collect();
        // Counted from the length of the one chain of rows, not row by row.
        let count = snap.count_ids(&triples, 6, GraphSelector::Default, BgpCount::Rows, &|| {
            false
        });
        assert_eq!(count, Some(8_000_000));
        // Cancelled while one input row makes its 40,000 rows: asked again within them, so
        // it stops after a few calls instead of after the 8M rows.
        let calls = AtomicUsize::new(0);
        let cancelled = || calls.fetch_add(1, AtomicOrdering::Relaxed) >= 3;
        assert!(
            snap.join_ids(&triples, 6, GraphSelector::Default, &cancelled)
                .is_none()
        );
        assert!(calls.load(AtomicOrdering::Relaxed) < 10);
        // Too large: the rows stop at the cap, also with a cap in capacity.
        let mut out = Out::with_used(6, max_ids() - 60, 0, &|| false);
        let row = [TermId(1); 6];
        let q = [TermId(2); 3];
        let slots = [IdSlot::Var(0), IdSlot::Var(1), IdSlot::Var(2)];
        for _ in 0..10 {
            assert!(out.push(&row, &slots, &q).is_some());
        }
        assert!(out.push(&row, &slots, &q).is_none());
        assert!(out.ids.capacity() <= 60);
    }

    #[test]
    fn chunked_scans_and_seek_joins_match_a_naive_join() {
        // Enough quads for many blocks; selective predicates make the joins seek.
        let store = Store::new();
        let p = |i: u64| n(&format!("p{i}"));
        let quads: Vec<oxigraph::model::Quad> = (0..20_000u64)
            .flat_map(|i| {
                let s = n(&format!("s{}", i % 5000));
                let mut v = vec![oxigraph::model::Quad::new(
                    s.clone(),
                    p(i / 5000),
                    Literal::from(i as i64),
                    GraphNameRef::DefaultGraph,
                )];
                for m in [7, 300] {
                    if i < 5000 && i % m == 0 {
                        v.push(oxigraph::model::Quad::new(
                            s.clone(),
                            n(&format!("sel{m}")),
                            Literal::from(i as i64),
                            GraphNameRef::DefaultGraph,
                        ));
                    }
                }
                v
            })
            .collect();
        store.bulk_load_quads(quads).unwrap();
        for with_delta in [false, true] {
            if with_delta {
                for i in 0..50u64 {
                    store
                        .insert(&oxigraph::model::Quad::new(
                            n(&format!("s{}", i * 97)),
                            n("sel7"),
                            Literal::from(-(i as i64)),
                            GraphNameRef::DefaultGraph,
                        ))
                        .unwrap();
                }
            }
            let snap = store.snapshot();
            let id = |x: NamedNode| snap.term_id(x.as_ref().into()).unwrap();
            let (v0, v1, v2) = (IdSlot::Var(0), IdSlot::Var(1), IdSlot::Var(2));
            for (a, b) in [("sel7", "p1"), ("sel300", "p2"), ("p0", "p3")] {
                let triples = [
                    IdTriple {
                        s: v0,
                        p: IdSlot::Const(id(n(a))),
                        o: v1,
                    },
                    IdTriple {
                        s: v0,
                        p: IdSlot::Const(id(n(b))),
                        o: v2,
                    },
                ];
                let mut got: Vec<Vec<TermId>> = snap
                    .join_ids(&triples, 3, GraphSelector::Default, &|| false)
                    .unwrap()
                    .rows()
                    .map(<[TermId]>::to_vec)
                    .collect();
                got.sort();
                let side = |p: &str| -> Vec<[TermId; 4]> {
                    snap.quad_ids_for_pattern(None, Some(id(n(p))), None, GraphSelector::Default)
                        .collect()
                };
                let right = side(b);
                let mut expected: Vec<Vec<TermId>> = side(a)
                    .iter()
                    .flat_map(|l| {
                        right
                            .iter()
                            .filter(move |r| r[0] == l[0])
                            .map(move |r| vec![l[0], l[2], r[2]])
                    })
                    .collect();
                expected.sort();
                assert!(!expected.is_empty());
                assert_eq!(got, expected, "{a} {b} delta={with_delta}");
            }
        }
    }
}
