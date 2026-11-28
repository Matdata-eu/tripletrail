//! `ORDER BY ?v LIMIT k` over a pattern whose `?v` is the object of one triple pattern with a
//! constant predicate (#297), such as
//!
//! ```sparql
//! SELECT ?t ?v ?label WHERE {
//!   ?t a era:RunningTrack ; era:maximumPermittedSpeed ?v ; rdfs:label ?label
//! } ORDER BY DESC(?v) LIMIT 20
//! ```
//!
//! The whole pattern is joined and then all but the first rows dropped. Here the values of
//! that predicate are read from its value-ordered column ([`crate::range`]: the distinct
//! numbers it has, with how many triples each), the best of them picked (enough for a few
//! times `k` triples, with every value equal to the last), and the whole pattern joined from
//! those values only: its triples are looked up for them instead of read whole. When that
//! gives at least `k` rows (`k` distinct ones under `DISTINCT`), they are the first rows of
//! the whole join as well: every other row it has orders after them, as its value is worse
//! than every value picked. Otherwise the next values are joined too, and past a share of the
//! predicate's triples the pattern is joined whole, as before.
//!
//! The rows picked are then ordered as before ([`crate::rows::first_rows`]), so the same rows
//! come first; rows that tie on every key of the order come in the order this join gives
//! them (by value, then subject), which need not be the whole join's.
//!
//! The pattern may be that one triple pattern alone, its triples then looked up for the best
//! values only, and the top-k a sub-select, counted or joined with the rest (#360).
//!
//! Only for predicates whose objects are all numbers (others order apart from them), and only
//! on the default graph (the columns cover it).

use crate::idtree::{Eval, Table};
use oxigraph::sparql::Variable;
use spargebra::algebra::GraphPattern;
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};
use tripletrail_store::TermId;

/// The values first picked hold this many times `k` triples.
const FIRST: u64 = 4;
/// The triples picked grow this many times each round.
const GROWTH: u64 = 8;
/// The predicate's triples must be at least this many times `k`: with fewer, joining the
/// whole pattern costs little more.
const MIN_TRIPLES: u64 = 16;
/// Most triples of a predicate whose column is built for this.
const MAX_TRIPLES: u64 = 1 << 22;
/// Past this share of the predicate's triples (one in so many), the pattern is joined whole.
const MOST: u64 = 8;

/// The rows of `body` (a basic graph pattern, or a `FILTER` over one) among which the first
/// `keep` rows in the order `by` are, the first `keep` distinct ones of the `distinct`
/// variables when given. `None` when `body` is not that shape, the order's first key is not
/// the object of a predicate with a value column, its triples are few, or too many values
/// would have to be picked: then `body` is joined whole, as before.
pub(crate) fn table(
    body: &GraphPattern,
    by: &[(&Variable, bool)],
    keep: usize,
    distinct: Option<&[Variable]>,
    eval: &Eval,
) -> Option<Table> {
    let columns = eval.columns?;
    let (filter, patterns) = match body {
        GraphPattern::Bgp { patterns } => (None, patterns),
        GraphPattern::Filter { expr, inner } => match &**inner {
            GraphPattern::Bgp { patterns } => (Some(expr), patterns),
            _ => return None,
        },
        _ => return None,
    };
    let &(v, descending) = by.first()?;
    if keep == 0 || patterns.is_empty() {
        return None;
    }
    let predicate = key_predicate(patterns, v)?;
    // The column is built on first use: not for a few triples, nor for very many.
    let keep64 = keep as u64;
    let p = eval.snapshot.term_id(predicate.as_ref().into())?;
    let estimate = eval.snapshot.estimate_ids(None, Some(p), None, eval.graph) as u64;
    if estimate < keep64.saturating_mul(MIN_TRIPLES) || estimate > MAX_TRIPLES {
        return None;
    }
    let column = columns(predicate)?;
    let whole = GraphPattern::Bgp {
        patterns: patterns.clone(),
    };
    let mut joined: Option<Table> = None;
    // The positions of the values joined so far.
    let mut done = 0..0;
    let mut triples = keep64.saturating_mul(FIRST);
    loop {
        if (eval.cancelled)() {
            return None;
        }
        let range = column.first(triples, descending)?;
        let all = range.len() == column.len();
        if !all && column.triples(&range) > column.total() / MOST {
            return None;
        }
        // The values not joined yet.
        let new: Vec<usize> = range.clone().filter(|i| !done.contains(i)).collect();
        done = range;
        let mut ids: Vec<TermId> = Vec::with_capacity(new.len());
        for i in new {
            // A value the snapshot doesn't have (written since): no triple of it to join.
            if let Some(id) = eval.snapshot.term_id(column.literal(i).as_ref().into()) {
                ids.push(id);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        let seed = Table {
            vars: vec![v.clone()],
            ones: ids.len(),
            ids,
            maybe: vec![false],
        };
        let mut part = eval.seeded(&seed, &whole)?;
        if let Some(e) = filter {
            part = eval.filtered(e, part)?;
        }
        let t = match joined.take() {
            None => part,
            Some(mut t) => {
                // The same patterns joined from the same variable: the same columns.
                if t.vars != part.vars {
                    return None;
                }
                t.ids.extend_from_slice(&part.ids);
                t.ones += part.ones;
                t
            }
        };
        if all || enough(&t, keep, distinct) {
            return Some(t);
        }
        // The triples that would give `keep` rows at the rate these gave theirs (one row for
        // none): past the share, the whole join is cheaper.
        let picked = column.triples(&done);
        let need = keep64.saturating_mul(picked) / (t.len() as u64).max(1);
        if need > column.total() / MOST {
            return None;
        }
        joined = Some(t);
        triples = triples.saturating_mul(GROWTH).max(need.saturating_mul(2));
    }
}

/// The predicate of the triple pattern of `patterns` whose object is `v`, when it is constant,
/// the subject is another variable and `v` is in no other triple pattern.
fn key_predicate<'p>(
    patterns: &'p [TriplePattern],
    v: &Variable,
) -> Option<&'p oxigraph::model::NamedNode> {
    let has = |t: &TermPattern| matches!(t, TermPattern::Variable(x) if x == v);
    let mut found = None;
    for t in patterns {
        let in_predicate = matches!(&t.predicate, NamedNodePattern::Variable(x) if x == v);
        if !(has(&t.subject) || in_predicate || has(&t.object)) {
            continue;
        }
        if found.is_some() {
            return None;
        }
        match (&t.subject, &t.predicate, &t.object) {
            (
                TermPattern::Variable(s),
                NamedNodePattern::NamedNode(p),
                TermPattern::Variable(o),
            ) if o == v && s != v => {
                found = Some(p);
            }
            _ => return None,
        }
    }
    found
}

/// Whether `t` has `keep` rows, or `keep` distinct combinations of the `distinct` variables.
fn enough(t: &Table, keep: usize, distinct: Option<&[Variable]>) -> bool {
    let Some(variables) = distinct else {
        return t.len() >= keep;
    };
    let columns: Vec<Option<usize>> = variables.iter().map(|v| t.col(v)).collect();
    let mut seen = std::collections::HashSet::new();
    for r in 0..t.len() {
        let row = t.row(r);
        let key: Vec<Option<TermId>> = columns.iter().map(|c| c.map(|c| row[c])).collect();
        if seen.insert(key) && seen.len() >= keep {
            return true;
        }
    }
    false
}
