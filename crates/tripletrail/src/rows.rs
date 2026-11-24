//! Joins, `OPTIONAL`, `MINUS`, `FILTER (NOT) EXISTS` and `UNION` over basic graph patterns
//! evaluated on term ids, with the rows handed back to the evaluator (issue #167).
//!
//! The evaluator joins one row at a time: a large-large join is an index nested loop with a
//! block seek per row, `OPTIONAL` a lookup per left row. [`crate::idtree`] evaluates the same
//! operators on whole tables of ids with hash joins. This pass evaluates the largest parts of
//! a query it covers that way, registers the rows as a result table
//! ([`tripletrail_store::register_table`]), and puts in their place a pattern that reads the
//! table back (`?row <urn:tripletrail:table:N:C> ?v`, inside `OPTIONAL` for a column that may be
//! unbound). What is around them (other operators, expressions, the projection) is evaluated
//! as usual, on the same rows.
//!
//! Only parts with a `MINUS` or `FILTER` are taken, and only when they give at most two thirds
//! as many rows as their triple patterns match: reading rows back costs about as much as the
//! evaluator's own join takes to make them, so plain joins and `OPTIONAL`s gain nothing.
//!
//! `ORDER BY` of variables under a `LIMIT` is also done on ids, when the ordered pattern is
//! covered: only the first rows are read back, so the evaluator orders a few rows instead of
//! all of them (decoding the values at each comparison).
//!
//! Left alone: parts under a `LIMIT` without an
//! order (the evaluator stops early there), parts inside `GRAPH`, `SERVICE`, `EXISTS` or the
//! right side of `LATERAL`, and anything [`crate::idtree`] can't evaluate exactly.

use crate::idtree::{Eval, Table};
use oxigraph::sparql::Variable;
use spargebra::Query;
use spargebra::algebra::{Expression, GraphPattern};
use spargebra::term::{TermPattern, TriplePattern};
use std::collections::HashMap;
use tripletrail_store::Snapshot;
use tripletrail_store::TermId;

/// The rows read back must be at most this fraction of the quads the part's triple patterns
/// match.
const KEEP: f64 = 1.5;

/// Replaces the parts of `query` it evaluates on ids, and says how many.
pub(crate) fn rewrite(
    query: Query,
    snapshot: &Snapshot,
    graph: tripletrail_store::GraphSelector,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> (Query, usize) {
    // `#pragma tt.id-rewrites off` (#238).
    if crate::Hints::current().id_rewrites_off {
        return (query, 0);
    }
    let mut pass = Pass {
        eval: Eval {
            snapshot,
            cancelled,
            graph,
        },
        done: 0,
    };
    let query = match query {
        Query::Select {
            dataset: None,
            pattern,
            base_iri,
        } => Query::Select {
            dataset: None,
            pattern: pass.walk(pattern, false, true),
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
            pattern: pass.walk(pattern, false, true),
            base_iri,
        },
        other => other,
    };
    (query, pass.done)
}

struct Pass<'a> {
    eval: Eval<'a>,
    done: usize,
}

impl Pass<'_> {
    /// `p` with its largest covered parts replaced; `limited`: a `LIMIT` without an order is
    /// above, so the evaluator may stop before reading all rows; `alone`: no other part is
    /// joined with `p`'s rows. A part with a path is taken only alone: joined with others,
    /// the evaluator searches the path from the values they give it, which may be few.
    fn walk(&mut self, p: GraphPattern, limited: bool, alone: bool) -> GraphPattern {
        use GraphPattern as G;
        if !limited
            && (alone || !crate::idtree::has_path(&p))
            && covered(&p)
            && worth(&p)
            && let Some(t) = self.eval.eval(&p)
            // Reading rows back costs about as much as the evaluator's own join makes them:
            // it pays where many rows read give few (MINUS, EXISTS, a restrictive FILTER).
            && (t.len() as f64) * KEEP <= self.eval.input_estimate(&p)
            && let Some(replaced) = read_back(t)
        {
            self.done += 1;
            return replaced;
        }
        if let Some(replaced) = self.top(&p) {
            self.done += 1;
            return replaced;
        }
        let mut w = |p: Box<G>, limited: bool, alone: bool| Box::new(self.walk(*p, limited, alone));
        match p {
            G::Join { left, right } => G::Join {
                left: w(left, limited, false),
                right: w(right, limited, false),
            },
            G::LeftJoin {
                left,
                right,
                expression,
            } => G::LeftJoin {
                left: w(left, limited, alone),
                right: w(right, limited, false),
                expression,
            },
            G::Minus { left, right } => G::Minus {
                left: w(left, limited, alone),
                right: w(right, limited, false),
            },
            G::Union { left, right } => G::Union {
                left: w(left, limited, alone),
                right: w(right, limited, alone),
            },
            // The right side is evaluated once per left row, with its variables bound.
            G::Lateral { left, right } => G::Lateral {
                left: w(left, limited, alone),
                right,
            },
            G::Filter { expr, inner } => G::Filter {
                expr,
                inner: w(inner, limited, alone),
            },
            G::Extend {
                inner,
                variable,
                expression,
            } => G::Extend {
                inner: w(inner, limited, alone),
                variable,
                expression,
            },
            G::Project { inner, variables } => G::Project {
                inner: w(inner, limited, alone),
                variables,
            },
            G::Distinct { inner } => G::Distinct {
                inner: w(inner, limited, alone),
            },
            G::Reduced { inner } => G::Reduced {
                inner: w(inner, limited, alone),
            },
            // These read all the rows of their input.
            G::OrderBy { inner, expression } => G::OrderBy {
                inner: w(inner, false, alone),
                expression,
            },
            G::Group {
                inner,
                variables,
                aggregates,
            } => G::Group {
                inner: w(inner, false, alone),
                variables,
                aggregates,
            },
            G::Slice {
                inner,
                start,
                length,
            } => G::Slice {
                inner: w(inner, limited || length.is_some(), alone),
                start,
                length,
            },
            // Named graphs, remote services, paths and VALUES stay as written.
            other => other,
        }
    }
}

impl Pass<'_> {
    /// `ORDER BY ?a ?b ... LIMIT n` (with `OFFSET`, `DISTINCT` or `REDUCED`) over a covered
    /// pattern: its rows are found on ids, ordered on ids (the string ranks for IRIs and
    /// strings), and only the first `OFFSET + LIMIT` rows (distinct ones under `DISTINCT`)
    /// are read back, ordered again by the evaluator. `None` when `p` isn't that shape or
    /// a value can't be decoded.
    fn top(&mut self, p: &GraphPattern) -> Option<GraphPattern> {
        use GraphPattern as G;
        let (keep, distinct, by, body) = shape(p)?;
        if !covered(body) {
            return None;
        }
        let t = self.eval.eval(body)?;
        let rows = first_rows(&t, by, distinct, keep, self.eval.snapshot)?;
        let w = t.vars.len();
        let mut ids = Vec::with_capacity(rows.len() * w);
        for &r in &rows {
            ids.extend_from_slice(t.row(r));
        }
        let first = Table {
            vars: t.vars,
            ids,
            maybe: t.maybe,
            ones: rows.len(),
        };
        let read = read_back(first)?;
        // The same operators over the rows read back.
        fn put(p: &G, read: G) -> G {
            match p {
                G::Slice {
                    inner,
                    start,
                    length,
                } => G::Slice {
                    inner: Box::new(put(inner, read)),
                    start: *start,
                    length: *length,
                },
                G::Distinct { inner } => G::Distinct {
                    inner: Box::new(put(inner, read)),
                },
                G::Reduced { inner } => G::Reduced {
                    inner: Box::new(put(inner, read)),
                },
                G::Project { inner, variables } => G::Project {
                    inner: Box::new(put(inner, read)),
                    variables: variables.clone(),
                },
                G::OrderBy { expression, .. } => G::OrderBy {
                    inner: Box::new(read),
                    expression: expression.clone(),
                },
                other => other.clone(),
            }
        }
        Some(put(p, read))
    }
}

/// The rows a `SLICE` over `ORDER BY` needs: how many (`OFFSET + LIMIT`), the projected
/// variables when they must be distinct, the order (variable, descending) and the pattern
/// ordered.
type Shape<'a> = (
    usize,
    Option<&'a [Variable]>,
    Vec<(&'a Variable, bool)>,
    &'a GraphPattern,
);

fn shape(p: &GraphPattern) -> Option<Shape<'_>> {
    use GraphPattern as G;
    use spargebra::algebra::OrderExpression;
    let G::Slice {
        inner,
        start,
        length: Some(length),
    } = p
    else {
        return None;
    };
    let keep = start.checked_add(*length)?;
    let (distinct, inner) = match &**inner {
        // REDUCED drops consecutive duplicates in the evaluator: its rows are kept distinct
        // too, so that enough are left.
        G::Distinct { inner } | G::Reduced { inner } => (true, &**inner),
        other => (false, other),
    };
    let (variables, inner) = match inner {
        G::Project { inner, variables } => (Some(variables.as_slice()), &**inner),
        other => (None, other),
    };
    let G::OrderBy { inner, expression } = inner else {
        return None;
    };
    let by = expression
        .iter()
        .map(|e| match e {
            OrderExpression::Asc(Expression::Variable(v)) => Some((v, false)),
            OrderExpression::Desc(Expression::Variable(v)) => Some((v, true)),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    // DISTINCT without a projection (`SELECT DISTINCT *`) compares every variable.
    let distinct = match (distinct, variables) {
        (false, _) => None,
        (true, Some(v)) => Some(v),
        (true, None) => return None,
    };
    Some((keep, distinct, by, inner))
}

/// The numbers of the first `keep` rows of `t` in the order `by` (the first row of each
/// combination of the `distinct` variables only, when given), in that order. A column whose
/// values are of one class is ordered on ids; any other column (classes mixed, blank nodes,
/// literals of other datatypes) is ordered as the evaluator orders terms (`cmp_terms`), its
/// distinct values decoded and ranked once (#11). `None` when a value can't be decoded.
pub(crate) fn first_rows(
    t: &Table,
    by: Vec<(&Variable, bool)>,
    distinct: Option<&[Variable]>,
    keep: usize,
    snapshot: &Snapshot,
) -> Option<Vec<usize>> {
    use crate::idtree::Pick;
    use std::cmp::Ordering;
    let mut pick = Pick::new(snapshot);
    let mut columns: Vec<(Option<usize>, bool, Option<Ranks>)> = Vec::with_capacity(by.len());
    for (v, descending) in &by {
        let c = t.col(v);
        let terms = match c {
            Some(c) if !one_class(t, c, &pick) => Some(ranked(t, c, snapshot)?),
            _ => None,
        };
        columns.push((c, *descending, terms));
    }
    let mut failed = false;
    let mut cmp = |a: &usize, b: &usize| -> Ordering {
        for (c, descending, terms) in &columns {
            let Some(c) = *c else { continue };
            let (x, y) = (t.row(*a)[c], t.row(*b)[c]);
            let order = match terms {
                Some(ranks) => {
                    let rank = |id: TermId| ranks.get(&id).copied().unwrap_or(0);
                    Some(rank(x).cmp(&rank(y)))
                }
                None => sort_order(&mut pick, x, y),
            };
            match order {
                Some(Ordering::Equal) => {}
                Some(o) => return if *descending { o.reverse() } else { o },
                None => {
                    failed = true;
                    return Ordering::Equal;
                }
            }
        }
        // Ties in the order of the rows, as the evaluator's stable sort leaves them.
        a.cmp(b)
    };
    let mut rows: Vec<usize> = (0..t.len()).collect();
    let rows = match distinct {
        None => {
            if keep < rows.len() {
                rows.select_nth_unstable_by(keep, &mut cmp);
                rows.truncate(keep);
            }
            rows.sort_unstable_by(&mut cmp);
            rows
        }
        // Rows with the same values of the distinct variables order the same when those
        // include the ordered ones: keep one row of each combination, then pick as above.
        Some(variables) if by.iter().all(|(v, _)| variables.contains(v)) => {
            let columns: Vec<usize> = variables.iter().filter_map(|v| t.col(v)).collect();
            let columns = &columns;
            let key = |r: usize| columns.iter().map(move |&c| t.row(r)[c].0);
            rows.sort_unstable_by(|a, b| key(*a).cmp(key(*b)));
            rows.dedup_by(|a, b| key(*a).eq(key(*b)));
            if keep < rows.len() {
                rows.select_nth_unstable_by(keep, &mut cmp);
                rows.truncate(keep);
            }
            rows.sort_unstable_by(&mut cmp);
            rows
        }
        Some(variables) => {
            // In order, the first row of each combination, until there are enough.
            rows.sort_by(&mut cmp);
            let columns: Vec<Option<usize>> = variables.iter().map(|v| t.col(v)).collect();
            let mut seen = std::collections::HashSet::new();
            let mut out = Vec::new();
            for r in rows {
                if out.len() == keep {
                    break;
                }
                let key: Vec<_> = columns.iter().map(|c| c.map(|c| t.row(r)[c])).collect();
                if seen.insert(key) {
                    out.push(r);
                }
            }
            out
        }
    };
    (!failed).then_some(rows)
}

/// The rank of each id of a column in the evaluator's order of terms (unbound is 0, before
/// all).
type Ranks = HashMap<TermId, u32>;

/// Whether the values of column `c` of `t` are of one [`sort_class`] (IRIs before literals,
/// so IRIs mix with one class of literals): then they are ordered on ids.
fn one_class(t: &Table, c: usize, pick: &crate::idtree::Pick) -> bool {
    let mut literal = None;
    for r in 0..t.len() {
        let id = t.row(r)[c];
        if id == TermId(0) {
            continue;
        }
        match sort_class(pick, id) {
            Some(0) => {}
            Some(k) if literal.is_none_or(|l| l == k) => literal = Some(k),
            _ => return false,
        }
    }
    true
}

/// The distinct values of column `c` of `t` ranked in the evaluator's order of terms
/// (`cmp_terms`), each decoded once. `None` when one can't be decoded, or two are different
/// terms the evaluator orders as equal.
fn ranked(t: &Table, c: usize, snapshot: &Snapshot) -> Option<Ranks> {
    let mut ids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for r in 0..t.len() {
        let id = t.row(r)[c];
        if id != TermId(0) && seen.insert(id) {
            ids.push(id);
        }
    }
    drop(seen);
    let mut terms = Vec::with_capacity(ids.len());
    for id in ids {
        terms.push((id, spareval::ExpressionTerm::from(snapshot.term(id).ok()?)));
    }
    // All doubles but NaN (sums and averages of doubles made up for the query): the
    // evaluator orders them by value, compared here as numbers.
    let doubles: Option<Vec<f64>> = terms
        .iter()
        .map(|(_, t)| match t {
            spareval::ExpressionTerm::DoubleLiteral(d) => {
                Some(f64::from(*d)).filter(|d| !d.is_nan())
            }
            _ => None,
        })
        .collect();
    if let Some(values) = doubles {
        let mut order: Vec<usize> = (0..terms.len()).collect();
        order.sort_unstable_by(|&a, &b| values[a].total_cmp(&values[b]));
        // Equal values (`0` and `-0`, the same value written differently): the evaluator's.
        if order.windows(2).any(|w| values[w[0]] == values[w[1]]) {
            return None;
        }
        return Some(
            order
                .iter()
                .zip(1..)
                .map(|(&i, rank)| (terms[i].0, rank))
                .collect(),
        );
    }
    terms.sort_by(|(_, a), (_, b)| spareval::cmp_terms(Some(a), Some(b)));
    // Two different terms the evaluator orders as equal (`"…Z"` and `"…+00:00"`) come in the
    // order its sort leaves them, which the ids can't tell: those are its to order.
    if terms
        .windows(2)
        .any(|w| spareval::cmp_terms(Some(&w[0].1), Some(&w[1].1)).is_eq())
    {
        return None;
    }
    Some(
        terms
            .iter()
            .zip(1..)
            .map(|((id, _), rank)| (*id, rank))
            .collect(),
    )
}

/// The class of values `id` is sorted with: [`crate::idtree::Pick::order_class`], and the
/// inline numbers and dates (#64): doubles, decimals, and dates and date-times with or without
/// a time zone, each a class of its own. Within one, the evaluator orders by value, which
/// [`TermId::order_key`] does (ties, equal values written differently, in any order).
/// Different classes are compared by value or by lexical form there: not on ids.
fn sort_class(pick: &crate::idtree::Pick, id: TermId) -> Option<u8> {
    use tripletrail_store::Kind;
    match id.kind() {
        Kind::Double => Some(10),
        Kind::Decimal => Some(11),
        Kind::DateTime => {
            let v = id.date_time_value()?;
            Some(12 + 2 * u8::from(v.date) + u8::from(v.tz.is_some()))
        }
        _ => pick.order_class(id),
    }
}

/// The order key of an inline number or date without what tells equal values apart (the
/// number of fraction digits, the time zone): equal values are equal to the evaluator.
fn value_key(id: TermId) -> u64 {
    use tripletrail_store::{Kind, Numeric};
    if id.kind() == Kind::DateTime {
        return id.order_key() >> 11;
    }
    // `-0` and `0` are one value; the sign bit would put every `-0` first.
    if matches!(
        id.numeric(),
        Some(Numeric::Decimal { digits: 0, .. } | Numeric::Double(0.0))
    ) {
        return 1 << 59;
    }
    id.order_key() >> 4
}

/// `a` and `b` in the evaluator's order of terms, when they are of the same
/// [`sort_class`] (or one is an IRI, which comes first).
fn sort_order(pick: &mut crate::idtree::Pick, a: TermId, b: TermId) -> Option<std::cmp::Ordering> {
    if a != TermId(0) && b != TermId(0) && a != b {
        let (x, y) = (sort_class(pick, a)?, sort_class(pick, b)?);
        if x >= 10 || y >= 10 {
            return match (x, y) {
                (0, _) | (_, 0) => Some(x.cmp(&y)),
                _ if x == y => Some(value_key(a).cmp(&value_key(b))),
                _ => None,
            };
        }
    }
    pick.order(a, b)
}

/// Whether [`Eval::eval`] handles every operator of `p` (it may still give up on the data).
pub(crate) fn covered(p: &GraphPattern) -> bool {
    use GraphPattern as G;
    match p {
        G::Bgp { patterns } => patterns.iter().all(|t| {
            !matches!(t.subject, TermPattern::Triple(_))
                && !matches!(t.object, TermPattern::Triple(_))
        }),
        G::Join { left, right }
        | G::Minus { left, right }
        | G::Union { left, right }
        | G::LeftJoin {
            left,
            right,
            expression: None,
        } => covered(left) && covered(right),
        G::Filter { expr, inner } => covered_filter(expr) && covered(inner),
        G::Path { path, .. } => {
            crate::idtree::closure_path(path).is_some()
                || crate::idtree::optional_path(path).is_some()
                || crate::idtree::alternative_path(path).is_some()
                || crate::idtree::negated_path(path).is_some()
        }
        _ => false,
    }
}

fn covered_filter(e: &Expression) -> bool {
    crate::idtree::filter_covered(e)
}

/// Whether `p` filters rows out (`MINUS` or `FILTER`): the evaluator's own joins and
/// `OPTIONAL`s are about as fast as evaluating them on ids and reading the rows back.
fn worth(p: &GraphPattern) -> bool {
    use GraphPattern as G;
    match p {
        // A closure the evaluator would find one start at a time.
        G::Minus { .. } | G::Filter { .. } => true,
        // An alternative of predicates is the evaluator's union of scans: not worth reading
        // back.
        G::Path { path, .. } => crate::idtree::closure_path(path).is_some(),
        G::Join { left, right } | G::LeftJoin { left, right, .. } | G::Union { left, right } => {
            worth(left) || worth(right)
        }
        _ => false,
    }
}

/// A pattern that reads the rows of `t` from a registered result table, with the variables of
/// `t` only. `None` when it can't be read back (no column bound in every row, or the table is
/// too large to register).
pub(crate) fn read_back(t: Table) -> Option<GraphPattern> {
    if t.len() == 0 {
        return Some(GraphPattern::Values {
            variables: t.vars,
            bindings: Vec::new(),
        });
    }
    // Rows are found through a column bound in every row.
    let first = t.maybe.iter().position(|m| !m)?;
    let width = t.vars.len();
    let predicates =
        tripletrail_store::register_table(tripletrail_store::IdRows { width, ids: t.ids })?;
    // One row variable per table: the table's number.
    let n = predicates[0]
        .as_str()
        .rsplit(':')
        .nth(1)
        .unwrap_or_default();
    let row = Variable::new_unchecked(format!("__tripletrail_row_{n}"));
    let triple = |c: usize| TriplePattern {
        subject: row.clone().into(),
        predicate: predicates[c].clone().into(),
        object: t.vars[c].clone().into(),
    };
    let mut certain: Vec<TriplePattern> = vec![triple(first)];
    certain.extend(
        (0..width)
            .filter(|&c| c != first && !t.maybe[c])
            .map(triple),
    );
    let mut pattern = GraphPattern::Bgp { patterns: certain };
    for c in (0..width).filter(|&c| t.maybe[c]) {
        pattern = GraphPattern::LeftJoin {
            left: Box::new(pattern),
            right: Box::new(GraphPattern::Bgp {
                patterns: vec![triple(c)],
            }),
            expression: None,
        };
    }
    Some(GraphPattern::Project {
        inner: Box::new(pattern),
        variables: t.vars,
    })
}
