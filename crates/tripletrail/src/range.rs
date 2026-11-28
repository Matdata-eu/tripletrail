//! Range scans on numeric literals (issue #71), and on dates and date-times (#312).
//!
//! A numeric `FILTER(?km >= a && ?km <= b)` on a variable bound by a triple `?lr ex:km ?km` is
//! evaluated row by row: every value of `ex:km` the rest of the pattern reaches is decoded and
//! compared. When nothing else in the pattern narrows `?lr` down, that is every triple of
//! `ex:km`. This pass answers such a filter from a value-ordered index of the predicate's
//! numeric objects instead: the literals in `[a, b]` are found by binary search and put in a
//! `VALUES ?km { ... }` block next to the pattern, which then starts from them with one index
//! lookup per value. The FILTER stays, so the result is exactly the same: the block only has
//! to hold every literal that can pass it (it is widened a little against rounding).
//!
//! The index is built per predicate on first use. Writes whose quads the engine knows
//! (SPARQL updates, single inserts and removes on the tripletrail store) patch it in place
//! (#212); other writes, or more changes than a patch is worth, drop it. It covers the
//! default graph, so the pass leaves patterns inside `GRAPH` and
//! queries with a dataset (`FROM`) alone.
//!
//! The block is not always a gain: a pattern that already starts from a selective constant
//! (`?lr ex:line <h84>`, 3.5 k rows) is faster without a block of 1.6 k values to join with.
//! The pass therefore compares how many triples the range holds with the estimates of the
//! pattern's other triple patterns that have a constant subject or object.
//!
//! Dates and date-times are columns of their own, ordered by instant: XPath compares an
//! `xsd:date` only with dates and an `xsd:dateTime` (or `xsd:dateTimeStamp`) only with
//! date-times, so a date constant's block holds dates alone. A value without a timezone is
//! placed as if it were UTC, and every temporal block is widened by 14 hours on each side, the
//! most two timezones can move an instant apart: the FILTER decides the values near the bounds.

use crate::Engine;
use oxigraph::model::{GraphNameRef, Literal, NamedNode, Quad, Term};
use oxigraph::sparql::Variable;
use spargebra::Query;
use spargebra::algebra::{Expression, GraphPattern};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern, TriplePattern};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tripletrail_geo::sync::LockUnpoisoned;
use tripletrail_store::{Dataset, Snapshot, Store};

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";
/// Numeric datatypes, by local name.
const NUMERIC: [&str; 16] = [
    "integer",
    "decimal",
    "double",
    "float",
    "long",
    "int",
    "short",
    "byte",
    "nonNegativeInteger",
    "positiveInteger",
    "negativeInteger",
    "nonPositiveInteger",
    "unsignedLong",
    "unsignedInt",
    "unsignedShort",
    "unsignedByte",
];
/// The values a column orders: numbers, dates, or date-times (with date-time stamps).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Space {
    Number,
    Date,
    DateTime,
}

/// 14 hours, in seconds: the widest that two timezones move one instant.
const TIMEZONES: f64 = 14.0 * 3600.0;

/// Most distinct values put in one block.
const MAX_VALUES: usize = 100_000;
/// The block is used when the range holds at most this share of what the best other triple
/// pattern of the group is estimated to match (one lookup per value against a scan).
const OTHER_RATIO: f64 = 0.25;
/// Without estimates (cost model off): at most this share of the predicate's triples.
const ALONE_RATIO: f64 = 0.1;

/// Changes for one predicate beyond this share of its distinct values drop the column
/// rather than patch it (each patch shifts the arrays after it).
const PATCH_RATIO: usize = 8;

/// The numeric (or date, or date-time) objects of one predicate in the default graph, ordered
/// by value.
#[derive(Clone)]
pub(crate) struct NumericColumn {
    space: Space,
    values: Vec<f64>,
    literals: Vec<Literal>,
    /// `before[i]`: triples whose object is one of `literals[..i]`.
    before: Vec<u64>,
    /// Triples of the predicate, numeric or not.
    total: u64,
}

impl NumericColumn {
    /// From `at` (a snapshot of the tripletrail store), or from the engine's current data.
    /// `None` if reading the store fails: an incomplete column would drop rows.
    fn build(
        engine: &Engine,
        predicate: &NamedNode,
        space: Space,
        at: Option<&Snapshot>,
    ) -> Option<Self> {
        let mut counts: HashMap<Literal, u64> = HashMap::new();
        let mut total = 0;
        let mut count = |object: Term| {
            total += 1;
            if let Term::Literal(l) = object
                && value(space, &l).is_some()
            {
                *counts.entry(l).or_default() += 1;
            }
        };
        let (p, g) = (Some(predicate.as_ref()), Some(GraphNameRef::DefaultGraph));
        match at {
            Some(at) => at
                .quads_for_pattern(None, p, None, g)
                .for_each(|q| count(q.object)),
            None => {
                for quad in engine.store.quads_for_pattern(None, p, None, g) {
                    count(quad.ok()?.object);
                }
            }
        }
        let mut entries: Vec<(f64, Literal, u64)> = counts
            .into_iter()
            .filter_map(|(l, n)| Some((value(space, &l)?, l, n)))
            .filter(|(v, _, _)| !v.is_nan())
            .collect();
        entries.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut before = Vec::with_capacity(entries.len() + 1);
        let mut sum = 0;
        before.push(0);
        let mut values = Vec::with_capacity(entries.len());
        let mut literals = Vec::with_capacity(entries.len());
        for (v, l, n) in entries {
            sum += n;
            before.push(sum);
            values.push(v);
            literals.push(l);
        }
        Some(Self {
            space,
            values,
            literals,
            before,
            total,
        })
    }

    /// Applies the object of one default-graph quad of the predicate, added or removed.
    /// `None` when a removed literal isn't in the column (it doesn't match the data).
    fn apply(&mut self, added: bool, object: &Term) -> Option<()> {
        if added {
            self.total += 1;
        } else {
            self.total = self.total.checked_sub(1)?;
        }
        let Term::Literal(l) = object else {
            return Some(());
        };
        let Some(v) = value(self.space, l).filter(|v| !v.is_nan()) else {
            return Some(());
        };
        // The literal among the run of equal values, or where it goes.
        let mut i = self.values.partition_point(|x| *x < v);
        while i < self.values.len() && self.values[i] == v && self.literals[i] != *l {
            i += 1;
        }
        let found = i < self.values.len() && self.literals[i] == *l;
        if added {
            if !found {
                self.values.insert(i, v);
                self.literals.insert(i, l.clone());
                self.before.insert(i + 1, self.before[i]);
            }
            for b in &mut self.before[i + 1..] {
                *b += 1;
            }
        } else {
            if !found {
                return None;
            }
            for b in &mut self.before[i + 1..] {
                *b -= 1;
            }
            if self.before[i + 1] == self.before[i] {
                self.values.remove(i);
                self.literals.remove(i);
                self.before.remove(i + 1);
            }
        }
        Some(())
    }

    /// The positions of the values in `[low, high]` (bounds widened against rounding).
    fn range(&self, low: f64, high: f64) -> std::ops::Range<usize> {
        let start = self.values.partition_point(|v| *v < widen(low, -1.0));
        let end = self.values.partition_point(|v| *v <= widen(high, 1.0));
        start..end.max(start)
    }

    /// The positions of the values that hold at least `triples` of the predicate's triples
    /// (all of them when there are fewer), from the smallest value or from the largest
    /// (`descending`), and every value equal to the last of them (bounds widened against
    /// rounding): every triple whose value orders before or with theirs is among them (#297).
    /// `None` when some of the predicate's objects are not numbers: those order apart.
    pub(crate) fn first(&self, triples: u64, descending: bool) -> Option<std::ops::Range<usize>> {
        let numeric = *self.before.last()?;
        if numeric != self.total || self.values.is_empty() {
            return None;
        }
        let n = self.values.len();
        Some(if descending {
            // The last position `i` with at least `triples` from it to the end.
            let i = self
                .before
                .partition_point(|&b| b <= numeric.saturating_sub(triples))
                .saturating_sub(1)
                .min(n - 1);
            let low = widen(self.values[i], -1.0);
            self.values.partition_point(|v| *v < low)..n
        } else {
            // The first position `i` with at least `triples` up to it.
            let i = self.before[1..]
                .partition_point(|&b| b < triples)
                .min(n - 1);
            let high = widen(self.values[i], 1.0);
            0..self.values.partition_point(|v| *v <= high)
        })
    }

    /// The triples whose objects are the values at `range`.
    pub(crate) fn triples(&self, range: &std::ops::Range<usize>) -> u64 {
        self.before[range.end] - self.before[range.start]
    }

    /// The distinct numbers.
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    /// The triples of the predicate.
    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// The literal at `i`.
    pub(crate) fn literal(&self, i: usize) -> &Literal {
        &self.literals[i]
    }
}

/// Applies a write's changes for one predicate to its column; `false` when they are many
/// against it or don't match it (the column is dropped then).
fn patch(column: &mut Arc<NumericColumn>, changes: &[(bool, Term)]) -> bool {
    changes.len() <= column.literals.len() / PATCH_RATIO + 64 && {
        let c = Arc::make_mut(column);
        changes
            .iter()
            .all(|(added, o)| c.apply(*added, o).is_some())
    }
}

/// `bound` moved a little outwards: decimals and doubles of the same value can round to
/// neighbouring doubles, and the FILTER makes the exact decision anyway.
fn widen(bound: f64, direction: f64) -> f64 {
    if bound.is_infinite() {
        return bound;
    }
    bound + direction * (bound.abs() * 1e-12 + f64::MIN_POSITIVE)
}

/// Value-ordered indexes of numeric objects, per predicate, built on first use.
///
/// On the tripletrail store each column is kept with the [`Store::generation`] whose data it
/// holds (#328): a query reads a column only for the generation of its own snapshot, or for
/// one whose commits since (or before) did not touch the predicate. Otherwise the column is
/// built again from the query's snapshot, so that a write between the snapshot and the read
/// never takes values from it (top-k, #297) or from its value blocks (the range rewrite).
#[derive(Default)]
pub(crate) struct RangeIndexes {
    /// With the generation of the store whose data the column holds; `None` on Oxigraph's
    /// store, whose columns follow its current data.
    columns: Mutex<HashMap<(NamedNode, Space), Held>>,
    /// Counts the invalidations.
    generation: AtomicU64,
}

/// A column, with the generation of the store whose data it holds (`None`: the current data).
type Held = (Option<u64>, Arc<NumericColumn>);

/// Whether no commit after generation `from`, up to and with `to` (either order), changed a
/// quad of `predicate`: `false` when that is not known.
fn untouched(store: &Store, predicate: &NamedNode, from: u64, to: u64) -> bool {
    let (from, to) = (from.min(to), from.max(to));
    if from == to {
        return true;
    }
    let Some((_, predicates)) = store.touched_between(from, to) else {
        return false;
    };
    // A predicate the dictionary does not know was in no commit.
    store
        .term_id(predicate.as_ref().into())
        .is_none_or(|id| predicates.binary_search(&id.0).is_err())
}

impl RangeIndexes {
    /// The numeric column of `predicate` for a query reading `at` (the tripletrail store), or
    /// the current data (Oxigraph's store, `at: None`).
    pub(crate) fn column(
        &self,
        engine: &Engine,
        predicate: &NamedNode,
        at: Option<&Snapshot>,
    ) -> Option<Arc<NumericColumn>> {
        self.column_of(engine, predicate, Space::Number, at)
    }

    /// The column of `predicate`'s values in `space`, as [`Self::column`].
    fn column_of(
        &self,
        engine: &Engine,
        predicate: &NamedNode,
        space: Space,
        at: Option<&Snapshot>,
    ) -> Option<Arc<NumericColumn>> {
        let store = match &engine.store {
            Dataset::TripleTrail(store) => Some(store),
            Dataset::Oxigraph(_) => None,
        };
        let (Some(store), Some(at)) = (store, at) else {
            return self.current_column(engine, predicate, space);
        };
        // A transaction's view has no generation: built for it alone.
        let Some(wanted) = at.generation() else {
            return NumericColumn::build(engine, predicate, space, Some(at)).map(Arc::new);
        };
        let key = (predicate.clone(), space);
        if let Some((generation, c)) = self.columns.lock_unpoisoned().get_mut(&key)
            && let Some(held) = *generation
            && (held == wanted || untouched(store, predicate, held, wanted))
        {
            // The same data at the later generation too.
            *generation = Some(held.max(wanted));
            return Some(Arc::clone(c));
        }
        let built = Arc::new(NumericColumn::build(engine, predicate, space, Some(at))?);
        let mut columns = self.columns.lock_unpoisoned();
        // Kept unless a newer one came in meanwhile (an older query's column is its own).
        if columns
            .get(&key)
            .is_none_or(|(g, _)| g.is_none_or(|g| g < wanted))
        {
            columns.insert(key, (Some(wanted), Arc::clone(&built)));
        }
        Some(built)
    }

    /// The column of the current data, for Oxigraph's store.
    fn current_column(
        &self,
        engine: &Engine,
        predicate: &NamedNode,
        space: Space,
    ) -> Option<Arc<NumericColumn>> {
        let key = (predicate.clone(), space);
        if let Some((_, c)) = self.columns.lock_unpoisoned().get(&key) {
            return Some(Arc::clone(c));
        }
        // Built outside the lock, so other predicates' lookups do not wait. A write between
        // the build and the insert would leave a stale column: the generation guards that.
        let generation = self.generation.load(Ordering::Acquire);
        let built = Arc::new(NumericColumn::build(engine, predicate, space, None)?);
        let mut columns = self.columns.lock_unpoisoned();
        if self.generation.load(Ordering::Acquire) == generation {
            columns.insert(key, (None, Arc::clone(&built)));
        }
        Some(built)
    }

    /// Brings the built columns up to date with the quads a write added (`true`) or removed
    /// (`false`), each listed once: patched in place (a query still holding a column keeps
    /// its own copy), or dropped when the changes for a predicate are many against its
    /// column or don't match it. Columns not built yet need nothing. `committed`: the
    /// tripletrail store and the generation the write's commit made, if known. A column of an
    /// earlier generation is patched only if the commits between left its predicate alone
    /// (they may be brought in after this one); one a write of unknown generation changes is
    /// dropped.
    pub(crate) fn apply(
        &self,
        changes: impl IntoIterator<Item = (bool, Quad)>,
        committed: Option<(&Store, Option<u64>)>,
    ) {
        let mut columns = self.columns.lock_unpoisoned();
        self.generation.fetch_add(1, Ordering::AcqRel);
        if columns.is_empty() {
            return;
        }
        let held: std::collections::HashSet<NamedNode> =
            columns.keys().map(|(p, _)| p.clone()).collect();
        let mut by_predicate: HashMap<NamedNode, Vec<(bool, Term)>> = HashMap::new();
        for (added, q) in changes {
            if q.graph_name.is_default_graph() && held.contains(&q.predicate) {
                by_predicate
                    .entry(q.predicate)
                    .or_default()
                    .push((added, q.object));
            }
        }
        if let Some((store, generation)) = committed {
            columns.retain(|(predicate, _), (held, column)| {
                let changes = by_predicate.get(predicate);
                let (Some(held_at), Some(g)) = (*held, generation) else {
                    // Untouched columns of the tripletrail store keep their generation: a
                    // query checks the commits since.
                    return held.is_some() && changes.is_none();
                };
                if held_at >= g {
                    // Built from a snapshot that holds this commit already.
                    return true;
                }
                if !untouched(store, predicate, held_at, g - 1) {
                    return false;
                }
                *held = Some(g);
                changes.is_none_or(|changes| patch(column, changes))
            });
            return;
        }
        columns.retain(|(predicate, _), (_, column)| {
            by_predicate
                .get(predicate)
                .is_none_or(|changes| patch(column, changes))
        });
    }

    /// Drops the indexes of these predicates (after a write whose quads aren't known).
    pub(crate) fn invalidate<'a>(&self, predicates: impl IntoIterator<Item = &'a str>) {
        let mut columns = self.columns.lock_unpoisoned();
        self.generation.fetch_add(1, Ordering::AcqRel);
        let predicates: std::collections::HashSet<&str> = predicates.into_iter().collect();
        columns.retain(|(p, _), _| !predicates.contains(p.as_str()));
    }

    /// Roughly the heap bytes of the built columns (#225): a numeric literal's lexical form
    /// is short, so each value is counted with its literal's struct and datatype IRI.
    pub(crate) fn heap_size(&self) -> usize {
        const PER_VALUE: usize = size_of::<f64>() + size_of::<u64>() + size_of::<Literal>() + 56;
        self.columns
            .lock_unpoisoned()
            .values()
            .map(|(_, c)| size_of::<NumericColumn>() + c.literals.len() * PER_VALUE)
            .sum()
    }

    pub(crate) fn clear(&self) {
        let mut columns = self.columns.lock_unpoisoned();
        self.generation.fetch_add(1, Ordering::AcqRel);
        columns.clear();
    }
}

fn numeric_value(l: &Literal) -> Option<f64> {
    let local = l.datatype().as_str().strip_prefix(XSD)?;
    if !NUMERIC.contains(&local) {
        return None;
    }
    let s = l.value().trim();
    match s {
        "INF" | "+INF" => Some(f64::INFINITY),
        "-INF" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        _ if s
            .bytes()
            .any(|b| b.is_ascii_alphabetic() && b != b'e' && b != b'E') =>
        {
            None
        }
        _ => s.parse().ok(),
    }
}

/// A literal's value in `space`: a number, or for dates and date-times the seconds from
/// 1970-01-01T00:00:00Z of the instant (of the start of the day for a date), read as
/// SPARQL's evaluator reads it; a value without a timezone is placed as if in UTC.
fn value(space: Space, l: &Literal) -> Option<f64> {
    use oxsdatatypes::{Date, DateTime, DayTimeDuration, Double};
    let dt = l.datatype().as_str().strip_prefix(XSD)?;
    let at = match (space, dt) {
        (Space::Number, _) => return numeric_value(l),
        (Space::Date, "date") => DateTime::try_from(l.value().parse::<Date>().ok()?).ok()?,
        (Space::DateTime, "dateTime" | "dateTimeStamp") => l.value().parse::<DateTime>().ok()?,
        _ => return None,
    };
    // Days from 1970-01-01 of the proleptic Gregorian date (Howard Hinnant's days_from_civil).
    let (y, m, d) = (
        at.year() - i64::from(at.month() <= 2),
        i64::from(at.month()),
        i64::from(at.day()),
    );
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let seconds = f64::from(Double::from(at.second()));
    let offset = at.timezone_offset().map_or(0.0, |o| {
        f64::from(Double::from(DayTimeDuration::from(o).as_seconds()))
    });
    Some(
        days as f64 * 86_400.0
            + f64::from(at.hour()) * 3600.0
            + f64::from(at.minute()) * 60.0
            + seconds
            - offset,
    )
}

/// A numeric, date or date-time constant of an expression, with its space.
fn constant(e: &Expression) -> Option<(Space, f64)> {
    let Expression::Literal(l) = e else {
        return None;
    };
    [Space::Number, Space::Date, Space::DateTime]
        .into_iter()
        .find_map(|s| Some((s, value(s, l).filter(|v| !v.is_nan())?)))
}

/// What the conjuncts of a FILTER bound a variable to: `(space, low, high)`, or `None` when
/// they compare it with constants of different spaces.
type Bound = Option<(Space, f64, f64)>;

/// Bounds on variables from the conjuncts of a FILTER, per variable.
fn bounds(e: &Expression, out: &mut HashMap<Variable, Bound>) {
    let mut add = |v: &Variable, (space, c): (Space, f64), above: bool| {
        let (low, high) = if above {
            (c, f64::INFINITY)
        } else {
            (f64::NEG_INFINITY, c)
        };
        let b = out
            .entry(v.clone())
            .or_insert(Some((space, f64::NEG_INFINITY, f64::INFINITY)));
        *b = b
            .filter(|b| b.0 == space)
            .map(|b| (space, b.1.max(low), b.2.min(high)));
    };
    match e {
        Expression::And(a, b) => {
            bounds(a, out);
            bounds(b, out);
        }
        Expression::Greater(a, b) | Expression::GreaterOrEqual(a, b) => match (&**a, &**b) {
            (Expression::Variable(v), c) => {
                if let Some(c) = constant(c) {
                    add(v, c, true)
                }
            }
            (c, Expression::Variable(v)) => {
                if let Some(c) = constant(c) {
                    add(v, c, false)
                }
            }
            _ => {}
        },
        Expression::Less(a, b) | Expression::LessOrEqual(a, b) => match (&**a, &**b) {
            (Expression::Variable(v), c) => {
                if let Some(c) = constant(c) {
                    add(v, c, false)
                }
            }
            (c, Expression::Variable(v)) => {
                if let Some(c) = constant(c) {
                    add(v, c, true)
                }
            }
            _ => {}
        },
        _ => {}
    }
}

/// The triple patterns of a group made of basic graph patterns and joins of them.
fn group_triples<'a>(p: &'a GraphPattern, out: &mut Vec<&'a TriplePattern>) -> bool {
    match p {
        GraphPattern::Bgp { patterns } => {
            out.extend(patterns);
            true
        }
        GraphPattern::Join { left, right } => group_triples(left, out) && group_triples(right, out),
        _ => false,
    }
}

/// A range FILTER the index can answer.
struct Candidate {
    var: Variable,
    predicate: NamedNode,
    low: f64,
    high: f64,
    /// Positions of its values in `column`.
    range: std::ops::Range<usize>,
    /// Triples with these values.
    rows: u64,
    column: Arc<NumericColumn>,
}

struct Pass<'a> {
    engine: &'a Engine,
    /// The snapshot the query reads, on the tripletrail store.
    at: Option<&'a Snapshot>,
    decisions: Vec<String>,
}

impl Pass<'_> {
    fn walk(&mut self, p: GraphPattern) -> GraphPattern {
        use GraphPattern as G;
        match p {
            G::Filter { expr, inner } => {
                let inner = self.walk(*inner);
                let inner = self.filtered(&expr, inner);
                G::Filter {
                    expr,
                    inner: Box::new(inner),
                }
            }
            // The index covers the default graph only.
            G::Graph { .. } | G::Service { .. } => p,
            G::Join { left, right } => G::Join {
                left: Box::new(self.walk(*left)),
                right: Box::new(self.walk(*right)),
            },
            G::Lateral { left, right } => G::Lateral {
                left: Box::new(self.walk(*left)),
                right: Box::new(self.walk(*right)),
            },
            G::LeftJoin {
                left,
                right,
                expression,
            } => G::LeftJoin {
                left: Box::new(self.walk(*left)),
                right: Box::new(self.walk(*right)),
                expression,
            },
            G::Union { left, right } => G::Union {
                left: Box::new(self.walk(*left)),
                right: Box::new(self.walk(*right)),
            },
            G::Minus { left, right } => G::Minus {
                left: Box::new(self.walk(*left)),
                right: Box::new(self.walk(*right)),
            },
            G::Extend {
                inner,
                variable,
                expression,
            } => G::Extend {
                inner: Box::new(self.walk(*inner)),
                variable,
                expression,
            },
            G::Project { inner, variables } => G::Project {
                inner: Box::new(self.walk(*inner)),
                variables,
            },
            G::Distinct { inner } => G::Distinct {
                inner: Box::new(self.walk(*inner)),
            },
            G::Reduced { inner } => G::Reduced {
                inner: Box::new(self.walk(*inner)),
            },
            G::Slice {
                inner,
                start,
                length,
            } => G::Slice {
                inner: Box::new(self.walk(*inner)),
                start,
                length,
            },
            G::OrderBy { inner, expression } => G::OrderBy {
                inner: Box::new(self.walk(*inner)),
                expression,
            },
            G::Group {
                inner,
                variables,
                aggregates,
            } => G::Group {
                inner: Box::new(self.walk(*inner)),
                variables,
                aggregates,
            },
            other => other,
        }
    }

    /// `inner` of `FILTER(expr)`, with a block of values for a range of `expr` when it pays.
    fn filtered(&mut self, expr: &Expression, inner: GraphPattern) -> GraphPattern {
        let mut ranges = HashMap::new();
        bounds(expr, &mut ranges);
        if ranges.is_empty() {
            return inner;
        }
        let mut triples = Vec::new();
        if !group_triples(&inner, &mut triples) {
            return inner;
        }
        let mut best: Option<Candidate> = None;
        for (var, bound) in &ranges {
            let Some((space, low, high)) = *bound else {
                continue;
            };
            if low > high || (low.is_infinite() && high.is_infinite()) {
                continue;
            }
            // Instants without a timezone, or with another, move by up to 14 hours.
            let (low, high) = match space {
                Space::Number => (low, high),
                Space::Date | Space::DateTime => (low - TIMEZONES, high + TIMEZONES),
            };
            let Some(predicate) = triples
                .iter()
                .find_map(|t| match (&t.predicate, &t.object) {
                    // A table's column (tt:search's score, say) holds no triples to range
                    // over (#188).
                    (NamedNodePattern::NamedNode(p), TermPattern::Variable(o))
                        if o == var && !tripletrail_store::is_table_column(p.as_str()) =>
                    {
                        Some(p.clone())
                    }
                    _ => None,
                })
            else {
                continue;
            };
            let Some(column) =
                self.engine
                    .spatial
                    .ranges
                    .column_of(self.engine, &predicate, space, self.at)
            else {
                continue;
            };
            let range = column.range(low, high);
            let rows = column.before[range.end] - column.before[range.start];
            if best.as_ref().is_none_or(|b| rows < b.rows) {
                best = Some(Candidate {
                    var: var.clone(),
                    predicate,
                    low,
                    high,
                    range,
                    rows,
                    column,
                });
            }
        }
        let Some(Candidate {
            var,
            predicate,
            low,
            high,
            range,
            rows,
            column,
        }) = best
        else {
            return inner;
        };
        let values = range.len();
        let (inject, why) = if values > MAX_VALUES {
            (false, format!("more than {MAX_VALUES} values"))
        } else if let Some(other) = self.best_other(&triples, &var) {
            (
                (rows as f64) <= other * OTHER_RATIO,
                format!("the best other triple pattern matches about {other:.0}"),
            )
        } else {
            (
                (rows as f64) <= column.total as f64 * ALONE_RATIO,
                format!("{} has {} triples", predicate, column.total),
            )
        };
        self.decisions.push(format!(
            "range {var} in [{low}, {high}]{} on {predicate}: {values} values in {rows} triples, {why}: {}",
            match column.space {
                Space::Number => "",
                Space::Date => " (dates, seconds from 1970)",
                Space::DateTime => " (date-times, seconds from 1970)",
            },
            if inject { "inject" } else { "filter" }
        ));
        if !inject {
            return inner;
        }
        let bindings = column.literals[range]
            .iter()
            .map(|l| vec![Some(GroundTerm::Literal(l.clone()))])
            .collect();
        GraphPattern::Join {
            left: Box::new(GraphPattern::Values {
                variables: vec![var],
                bindings,
            }),
            right: Box::new(inner),
        }
    }

    /// The smallest estimate among the group's triple patterns with a constant subject or
    /// object, other than those binding `var`; `None` without the cost model or such patterns.
    fn best_other(&self, triples: &[&TriplePattern], var: &Variable) -> Option<f64> {
        if !self.engine.spatial.costed.load(Ordering::Relaxed) {
            return None;
        }
        let constant = |t: &TermPattern| match t {
            TermPattern::NamedNode(n) => Some(Term::NamedNode(n.clone())),
            TermPattern::Literal(l) => Some(Term::Literal(l.clone())),
            _ => None,
        };
        triples
            .iter()
            .filter(|t| !matches!(&t.object, TermPattern::Variable(v) if v == var))
            .filter_map(|t| {
                let s = constant(&t.subject);
                let o = constant(&t.object);
                if s.is_none() && o.is_none() {
                    return None;
                }
                let p = match &t.predicate {
                    NamedNodePattern::NamedNode(p) => Some(p.clone()),
                    NamedNodePattern::Variable(_) => None,
                };
                self.engine
                    .estimate_triples(s.as_ref(), p.as_ref(), o.as_ref(), false)
                    .map(|n| n as f64)
            })
            .reduce(f64::min)
    }
}

/// Adds value blocks for numeric ranges the index answers better than the FILTER, and
/// returns what it decided, for `explain`. `at`: the snapshot the query is evaluated on, on
/// the tripletrail store (the blocks hold its values).
pub(crate) fn rewrite(
    query: Query,
    engine: &Engine,
    at: Option<&Snapshot>,
) -> (Query, Vec<String>) {
    // `#pragma tt.range-rewrite off` (#238).
    if crate::Hints::current().range_rewrite_off {
        return (query, Vec::new());
    }
    let mut pass = Pass {
        engine,
        at,
        decisions: Vec::new(),
    };
    let query = match query {
        Query::Select {
            dataset: None,
            pattern,
            base_iri,
        } => Query::Select {
            dataset: None,
            pattern: pass.walk(pattern),
            base_iri,
        },
        Query::Construct {
            template,
            dataset: None,
            pattern,
            base_iri,
        } => Query::Construct {
            template,
            dataset: None,
            pattern: pass.walk(pattern),
            base_iri,
        },
        Query::Ask {
            dataset: None,
            pattern,
            base_iri,
        } => Query::Ask {
            dataset: None,
            pattern: pass.walk(pattern),
            base_iri,
        },
        other => other,
    };
    (query, pass.decisions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::io::RdfFormat;
    use oxigraph::model::{GraphName, NamedNode, Quad};

    /// The store's current snapshot, as a query reads it.
    fn now(engine: &Engine) -> Snapshot {
        let Dataset::TripleTrail(store) = &engine.store else {
            panic!("the tripletrail store")
        };
        store.snapshot()
    }

    /// A column as (value, literal, quads) in a fixed order, and the total.
    fn entries(c: &NumericColumn) -> (Vec<(u64, String, u64)>, u64) {
        let mut v: Vec<(u64, String, u64)> = (0..c.values.len())
            .map(|i| {
                (
                    c.values[i].to_bits(),
                    c.literals[i].to_string(),
                    c.before[i + 1] - c.before[i],
                )
            })
            .collect();
        v.sort();
        // Sorted by value, as `range` needs.
        assert!(c.values.is_sorted_by(|a, b| a <= b));
        assert_eq!(c.before.len(), c.values.len() + 1);
        (v, c.total)
    }

    #[test]
    fn the_first_values_hold_enough_triples_and_every_equal_value() {
        let engine = Engine::new_tripletrail();
        // Values 0..10, value i on i + 1 triples; 5 also written as 5.0.
        let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
        for i in 0..10 {
            for j in 0..=i {
                ttl += &format!("ex:s{i}_{j} ex:v {i} .\n");
            }
        }
        ttl += "ex:x ex:v 5.0 .\n";
        engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
        let v = NamedNode::new("http://example.org/v").unwrap();
        let c = engine
            .spatial
            .ranges
            .column(&engine, &v, Some(&now(&engine)))
            .unwrap();
        let values = |r: std::ops::Range<usize>| -> Vec<String> {
            r.map(|i| c.literal(i).value().to_owned()).collect()
        };
        // The largest: 9 alone holds 10 triples, 9 and 8 hold 19.
        assert_eq!(values(c.first(1, true).unwrap()), ["9"]);
        assert_eq!(values(c.first(10, true).unwrap()), ["9"]);
        assert_eq!(values(c.first(11, true).unwrap()), ["8", "9"]);
        // The smallest: 0, 1 and 2 hold 6; 3 more need 3.
        assert_eq!(values(c.first(6, false).unwrap()), ["0", "1", "2"]);
        assert_eq!(values(c.first(7, false).unwrap()), ["0", "1", "2", "3"]);
        // Down to 5: both of its spellings.
        let r = c.first(10 + 9 + 8 + 7 + 1, true).unwrap();
        assert_eq!(c.triples(&r), 10 + 9 + 8 + 7 + 6 + 1);
        assert_eq!(values(r).len(), 6);
        // More than there are: all.
        assert_eq!(c.first(1000, false).unwrap(), 0..c.len());
        assert_eq!(c.first(1000, true).unwrap(), 0..c.len());
        // A string among the objects: they don't order as numbers alone.
        engine
            .update("PREFIX ex: <http://example.org/> INSERT DATA { ex:y ex:v \"text\" }")
            .unwrap();
        let c = engine
            .spatial
            .ranges
            .column(&engine, &v, Some(&now(&engine)))
            .unwrap();
        assert!(c.first(1, true).is_none());
    }

    #[test]
    fn a_patched_column_equals_a_rebuilt_one() {
        let engine = Engine::new_tripletrail();
        let mut ttl = String::from(
            "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
        );
        for i in 0..400 {
            ttl += &format!("ex:s{i} ex:km {} ; ex:other {i} .\n", i % 50);
        }
        ttl += "ex:t ex:km \"text\" , \"NaN\"^^xsd:double , \"7.0\"^^xsd:decimal .\n";
        engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
        let km = NamedNode::new("http://example.org/km").unwrap();
        let ranges = &engine.spatial.ranges;
        assert_eq!(ranges.heap_size(), 0);
        let held = ranges.column(&engine, &km, Some(&now(&engine))).unwrap();
        let before = entries(&held);
        // Counted in the dataset's memory quota (#225).
        assert!(ranges.heap_size() > held.literals.len() * 16);
        assert_eq!(
            engine.memory_use().unwrap().range_indexes,
            ranges.heap_size()
        );
        let p = "PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> ";
        let writes = [
            // A new value, a new spelling of an old value, more of an old literal, text.
            "INSERT DATA { ex:a ex:km 1000 . ex:b ex:km \"3.0\"^^xsd:decimal . ex:c ex:km 3 . ex:d ex:km \"x\" }",
            // The last quad of a literal, and some of another.
            "DELETE WHERE { ex:s49 ex:km ?k }",
            "DELETE DATA { ex:s0 ex:km 0 . ex:s3 ex:km 3 . ex:t ex:km \"text\" }",
            // Already there, not there, named graphs, removed and added again: no change.
            "INSERT DATA { ex:s1 ex:km 1 . GRAPH ex:g { ex:g1 ex:km 5000 } }",
            "DELETE DATA { ex:none ex:km 12 }",
            "DELETE DATA { ex:s2 ex:km 2 } ; INSERT DATA { ex:s2 ex:km 2 }",
        ];
        for w in writes {
            engine.update(&format!("{p}{w}")).unwrap();
            // Patched, not dropped to be built again.
            assert!(
                ranges
                    .columns
                    .lock_unpoisoned()
                    .contains_key(&(km.clone(), Space::Number)),
                "{w}"
            );
            let patched = ranges.column(&engine, &km, Some(&now(&engine))).unwrap();
            let rebuilt = NumericColumn::build(&engine, &km, Space::Number, None).unwrap();
            assert_eq!(entries(&patched), entries(&rebuilt), "{w}");
        }
        let quad = Quad::new(
            NamedNode::new("http://example.org/e").unwrap(),
            km.clone(),
            oxigraph::model::Literal::from(-5),
            GraphName::DefaultGraph,
        );
        for added in [true, true, false, false] {
            if added {
                engine.insert(&quad).unwrap();
            } else {
                engine.remove(&quad).unwrap();
            }
            assert!(
                ranges
                    .columns
                    .lock_unpoisoned()
                    .contains_key(&(km.clone(), Space::Number))
            );
            let patched = ranges.column(&engine, &km, Some(&now(&engine))).unwrap();
            let rebuilt = NumericColumn::build(&engine, &km, Space::Number, None).unwrap();
            assert_eq!(entries(&patched), entries(&rebuilt), "added {added}");
        }
        // A query that held the column before the writes still sees its version.
        assert_eq!(entries(&held), before);
        // A bulk load drops it.
        engine
            .load(
                RdfFormat::Turtle,
                "<http://example.org/f> <http://example.org/km> 9 .".as_bytes(),
            )
            .unwrap();
        assert!(
            !ranges
                .columns
                .lock_unpoisoned()
                .contains_key(&(km.clone(), Space::Number))
        );
    }
}
