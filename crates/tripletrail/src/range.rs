//! Range scans on numeric literals (issue #71).
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

/// The numeric objects of one predicate in the default graph, ordered by value.
#[derive(Clone)]
pub(crate) struct NumericColumn {
    values: Vec<f64>,
    literals: Vec<Literal>,
    /// `before[i]`: triples whose object is one of `literals[..i]`.
    before: Vec<u64>,
    /// Triples of the predicate, numeric or not.
    total: u64,
}

impl NumericColumn {
    /// `None` if reading the store fails: an incomplete column would drop rows.
    fn build(engine: &Engine, predicate: &NamedNode) -> Option<Self> {
        let mut counts: HashMap<Literal, u64> = HashMap::new();
        let mut total = 0;
        for quad in engine.store.quads_for_pattern(
            None,
            Some(predicate.as_ref()),
            None,
            Some(GraphNameRef::DefaultGraph),
        ) {
            total += 1;
            if let Term::Literal(l) = quad.ok()?.object
                && numeric_value(&l).is_some()
            {
                *counts.entry(l).or_default() += 1;
            }
        }
        let mut entries: Vec<(f64, Literal, u64)> = counts
            .into_iter()
            .filter_map(|(l, n)| Some((numeric_value(&l)?, l, n)))
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
        let Some(v) = numeric_value(l).filter(|v| !v.is_nan()) else {
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
#[derive(Default)]
pub(crate) struct RangeIndexes {
    columns: Mutex<HashMap<NamedNode, Arc<NumericColumn>>>,
    /// Counts the invalidations.
    generation: AtomicU64,
}

impl RangeIndexes {
    fn column(&self, engine: &Engine, predicate: &NamedNode) -> Option<Arc<NumericColumn>> {
        if let Some(c) = self.columns.lock_unpoisoned().get(predicate) {
            return Some(Arc::clone(c));
        }
        // Built outside the lock, so other predicates' lookups do not wait. A write between
        // the build and the insert would leave a stale column: the generation guards that.
        let generation = self.generation.load(Ordering::Acquire);
        let built = Arc::new(NumericColumn::build(engine, predicate)?);
        let mut columns = self.columns.lock_unpoisoned();
        if self.generation.load(Ordering::Acquire) == generation {
            columns.insert(predicate.clone(), Arc::clone(&built));
        }
        Some(built)
    }

    /// Brings the built columns up to date with the quads a write added (`true`) or removed
    /// (`false`), each listed once: patched in place (a query still holding a column keeps
    /// its own copy), or dropped when the changes for a predicate are many against its
    /// column or don't match it. Columns not built yet need nothing.
    pub(crate) fn apply(&self, changes: impl IntoIterator<Item = (bool, Quad)>) {
        let mut columns = self.columns.lock_unpoisoned();
        self.generation.fetch_add(1, Ordering::AcqRel);
        if columns.is_empty() {
            return;
        }
        let mut by_predicate: HashMap<NamedNode, Vec<(bool, Term)>> = HashMap::new();
        for (added, q) in changes {
            if q.graph_name.is_default_graph() && columns.contains_key(&q.predicate) {
                by_predicate
                    .entry(q.predicate)
                    .or_default()
                    .push((added, q.object));
            }
        }
        for (predicate, changes) in by_predicate {
            let Some(column) = columns.get_mut(&predicate) else {
                continue;
            };
            let patched = changes.len() <= column.literals.len() / PATCH_RATIO + 64 && {
                let c = Arc::make_mut(column);
                changes
                    .iter()
                    .all(|(added, o)| c.apply(*added, o).is_some())
            };
            if !patched {
                columns.remove(&predicate);
            }
        }
    }

    /// Drops the indexes of these predicates (after a write whose quads aren't known).
    pub(crate) fn invalidate<'a>(&self, predicates: impl IntoIterator<Item = &'a str>) {
        let mut columns = self.columns.lock_unpoisoned();
        self.generation.fetch_add(1, Ordering::AcqRel);
        for p in predicates {
            columns.remove(&NamedNode::new_unchecked(p));
        }
    }

    /// Roughly the heap bytes of the built columns (#225): a numeric literal's lexical form
    /// is short, so each value is counted with its literal's struct and datatype IRI.
    pub(crate) fn heap_size(&self) -> usize {
        const PER_VALUE: usize = size_of::<f64>() + size_of::<u64>() + size_of::<Literal>() + 56;
        self.columns
            .lock_unpoisoned()
            .values()
            .map(|c| size_of::<NumericColumn>() + c.literals.len() * PER_VALUE)
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

/// A numeric constant of an expression.
fn constant(e: &Expression) -> Option<f64> {
    match e {
        Expression::Literal(l) => numeric_value(l).filter(|v| !v.is_nan()),
        _ => None,
    }
}

/// Bounds on variables from the conjuncts of a FILTER: `(low, high)` per variable.
fn bounds(e: &Expression, out: &mut HashMap<Variable, (f64, f64)>) {
    let mut add = |v: &Variable, low: f64, high: f64| {
        let b = out
            .entry(v.clone())
            .or_insert((f64::NEG_INFINITY, f64::INFINITY));
        b.0 = b.0.max(low);
        b.1 = b.1.min(high);
    };
    match e {
        Expression::And(a, b) => {
            bounds(a, out);
            bounds(b, out);
        }
        Expression::Greater(a, b) | Expression::GreaterOrEqual(a, b) => match (&**a, &**b) {
            (Expression::Variable(v), c) => {
                if let Some(c) = constant(c) {
                    add(v, c, f64::INFINITY)
                }
            }
            (c, Expression::Variable(v)) => {
                if let Some(c) = constant(c) {
                    add(v, f64::NEG_INFINITY, c)
                }
            }
            _ => {}
        },
        Expression::Less(a, b) | Expression::LessOrEqual(a, b) => match (&**a, &**b) {
            (Expression::Variable(v), c) => {
                if let Some(c) = constant(c) {
                    add(v, f64::NEG_INFINITY, c)
                }
            }
            (c, Expression::Variable(v)) => {
                if let Some(c) = constant(c) {
                    add(v, c, f64::INFINITY)
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
        for (var, (low, high)) in &ranges {
            if low > high || (low.is_infinite() && high.is_infinite()) {
                continue;
            }
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
            let Some(column) = self.engine.spatial.ranges.column(self.engine, &predicate) else {
                continue;
            };
            let range = column.range(*low, *high);
            let rows = column.before[range.end] - column.before[range.start];
            if best.as_ref().is_none_or(|b| rows < b.rows) {
                best = Some(Candidate {
                    var: var.clone(),
                    predicate,
                    low: *low,
                    high: *high,
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
            "range {var} in [{low}, {high}] on {predicate}: {values} values in {rows} triples, {why}: {}",
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
/// returns what it decided, for `explain`.
pub(crate) fn rewrite(query: Query, engine: &Engine) -> (Query, Vec<String>) {
    // `#pragma tt.range-rewrite off` (#238).
    if crate::Hints::current().range_rewrite_off {
        return (query, Vec::new());
    }
    let mut pass = Pass {
        engine,
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
        let held = ranges.column(&engine, &km).unwrap();
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
            assert!(ranges.columns.lock_unpoisoned().contains_key(&km), "{w}");
            let patched = ranges.column(&engine, &km).unwrap();
            let rebuilt = NumericColumn::build(&engine, &km).unwrap();
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
            assert!(ranges.columns.lock_unpoisoned().contains_key(&km));
            let patched = ranges.column(&engine, &km).unwrap();
            let rebuilt = NumericColumn::build(&engine, &km).unwrap();
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
        assert!(!ranges.columns.lock_unpoisoned().contains_key(&km));
    }
}
