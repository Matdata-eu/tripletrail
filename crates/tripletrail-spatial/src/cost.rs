//! Cardinality estimates for the spatial rewrite's choices.
//!
//! The estimates are deliberately cheap: per triple pattern, a count the store answers without
//! evaluating anything ([`SpatialContext::estimate`]), and per envelope, the index's histogram
//! estimate of its candidates ([`crate::SpatialIndex::estimate_candidates`]). A join of
//! connected operands is estimated by its smallest operand (joins through subjects and
//! geometries are mostly one-to-one in geospatial data), unconnected operands multiply, and a
//! union adds up.

use crate::rewrite::SpatialContext;
use oxigraph::model::vocab::rdf;
use oxigraph::sparql::Variable;
use spargebra::algebra::{GraphPattern, PropertyPathExpression};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern, TriplePattern};
use std::collections::{HashMap, HashSet};

/// Inject a constant test's candidates only when the index would return at most this share of
/// the rows the rest of the pattern produces for the variable. Each candidate costs an exact
/// test while planning, then a lookup and the FILTER again in the evaluator; leaving the FILTER
/// alone costs one test per row.
pub(crate) const INJECT_RATIO: f64 = 0.5;
/// Below this many candidates the block is cheap whatever the estimates say: always inject.
pub(crate) const ALWAYS_INJECT: usize = 32;
/// A join's probe side is taken from estimates, without evaluating both sides, when one side
/// is estimated at least this many times smaller than the other.
pub(crate) const CLEAR_RATIO: f64 = 4.0;

/// A join whose smaller side has fewer geometries than this probes the R-tree once per
/// geometry of that side (index nested loop); from here on both sides are swept
/// ([`crate::sweep`]), which sorts both and runs the exact tests on every core.
pub(crate) const SWEEP_MIN: usize = 2_000;
/// ... unless the other side has more than this many times as many geometries: the sweep sorts
/// all of them, where the nested loop only visits the R-tree nodes near the small side.
pub(crate) const SWEEP_RATIO: usize = 32;

/// Whether to answer a join of `probe` geometries against `other` geometries with the
/// sweep-line join rather than the index nested loop.
pub(crate) fn sweep(probe: usize, other: usize) -> bool {
    let (small, large) = (probe.min(other), probe.max(other));
    small >= SWEEP_MIN && large <= small.saturating_mul(SWEEP_RATIO)
}

/// A `VALUES` block of one variable with at most this many values estimates the triples it
/// binds with each value in place of the variable (#463); a larger one counts as its size.
pub(crate) const VALUES_EXPANDED: usize = 256;

/// A variable or blank node; blank nodes act as variables inside a basic graph pattern.
pub(crate) type Name = String;

pub(crate) fn var_name(v: &Variable) -> Name {
    format!("?{}", v.as_str())
}

fn term_name(t: &TermPattern) -> Option<Name> {
    match t {
        TermPattern::Variable(v) => Some(var_name(v)),
        TermPattern::BlankNode(b) => Some(format!("_:{}", b.as_str())),
        _ => None,
    }
}

pub(crate) fn triple_names(t: &TriplePattern) -> HashSet<Name> {
    let mut out: HashSet<Name> = [term_name(&t.subject), term_name(&t.object)]
        .into_iter()
        .flatten()
        .collect();
    if let NamedNodePattern::Variable(v) = &t.predicate {
        out.insert(var_name(v));
    }
    out
}

/// The names a join operand shares with its siblings: the variables and blank nodes of a
/// basic graph pattern, the in-scope variables of anything else.
pub(crate) fn names(p: &GraphPattern) -> HashSet<Name> {
    match p {
        GraphPattern::Bgp { patterns } => patterns.iter().flat_map(triple_names).collect(),
        other => {
            let mut out = HashSet::new();
            other.on_in_scope_variable(|v| {
                out.insert(var_name(v));
            });
            out
        }
    }
}

/// Splits a pattern into its join operands, with BGPs split into single triples.
pub(crate) fn join_parts(p: &GraphPattern, out: &mut Vec<GraphPattern>) {
    match p {
        GraphPattern::Join { left, right } => {
            join_parts(left, out);
            join_parts(right, out);
        }
        GraphPattern::Bgp { patterns } if patterns.len() > 1 => {
            out.extend(patterns.iter().map(|t| GraphPattern::Bgp {
                patterns: vec![t.clone()],
            }));
        }
        other => out.push(other.clone()),
    }
}

/// For each of `parts`, whether it is connected to one of `start` through shared names; and
/// every name reached.
pub(crate) fn connected(
    parts: &[GraphPattern],
    start: &HashSet<Name>,
) -> (Vec<bool>, HashSet<Name>) {
    let vars: Vec<HashSet<Name>> = parts.iter().map(names).collect();
    let mut reached = start.clone();
    let mut taken = vec![false; parts.len()];
    loop {
        let mut grew = false;
        for (i, vs) in vars.iter().enumerate() {
            if !taken[i] && vs.iter().any(|v| reached.contains(v)) {
                taken[i] = true;
                reached.extend(vs.iter().cloned());
                grew = true;
            }
        }
        if !grew {
            return (taken, reached);
        }
    }
}

/// Joins operands back together, the triples in one BGP (so blank nodes keep joining them).
pub(crate) fn rejoin(parts: Vec<GraphPattern>) -> Option<GraphPattern> {
    let mut triples = Vec::new();
    let mut others = Vec::new();
    for p in parts {
        match p {
            GraphPattern::Bgp { patterns } => triples.extend(patterns),
            other => others.push(other),
        }
    }
    let first = (!triples.is_empty()).then_some(GraphPattern::Bgp { patterns: triples });
    first
        .into_iter()
        .chain(others)
        .reduce(|left, right| GraphPattern::Join {
            left: Box::new(left),
            right: Box::new(right),
        })
}

/// What sparopt (Oxigraph's optimizer) guesses for a triple pattern when it picks the operand
/// a join starts from: it only looks at which positions are constants.
pub(crate) fn optimizer_guess(t: &TriplePattern) -> usize {
    let s = !matches!(
        t.subject,
        TermPattern::Variable(_) | TermPattern::BlankNode(_)
    );
    let o = !matches!(
        t.object,
        TermPattern::Variable(_) | TermPattern::BlankNode(_)
    );
    let (p, typed) = match &t.predicate {
        NamedNodePattern::NamedNode(n) => (true, n.as_ref() == rdf::TYPE),
        NamedNodePattern::Variable(_) => (false, false),
    };
    let size = match (s, p, o) {
        (true, true, true) => 1,
        (true, true, false) => 10,
        (true, false, true) => 2,
        (false, true, true) => 1_000,
        (true, false, false) => 100,
        (false, false, false) => 1_000_000_000,
        (false, true, false) => 1_000_000,
        (false, false, true) => 10_000,
    };
    size + usize::from(typed)
}

/// Row estimates of patterns, from triple-pattern counts.
pub(crate) struct Estimator<'a> {
    ctx: &'a dyn SpatialContext,
    cache: HashMap<(TriplePattern, bool), Option<f64>>,
}

impl<'a> Estimator<'a> {
    pub(crate) fn new(ctx: &'a dyn SpatialContext) -> Self {
        Self {
            ctx,
            cache: HashMap::new(),
        }
    }

    /// Estimated matches of one triple pattern, in the default graph or (`all_graphs`) in any.
    pub(crate) fn triple(&mut self, t: &TriplePattern, all_graphs: bool) -> Option<f64> {
        let ctx = self.ctx;
        *self
            .cache
            .entry((t.clone(), all_graphs))
            .or_insert_with(|| ctx.estimate(t, all_graphs))
    }

    /// Estimated rows of a pattern, or `None` if some part cannot be estimated.
    pub(crate) fn rows(&mut self, p: &GraphPattern, all_graphs: bool) -> Option<f64> {
        use GraphPattern as G;
        match p {
            G::Bgp { patterns } if patterns.is_empty() => Some(1.0),
            G::Bgp { patterns } if patterns.len() == 1 => self.triple(&patterns[0], all_graphs),
            G::Bgp { .. } | G::Join { .. } => {
                let mut parts = Vec::new();
                join_parts(p, &mut parts);
                self.join(&parts, all_graphs)
            }
            G::Lateral { left, right } => {
                self.join(&[(**left).clone(), (**right).clone()], all_graphs)
            }
            G::Union { left, right } => {
                Some(self.rows(left, all_graphs)? + self.rows(right, all_graphs)?)
            }
            G::Minus { left, .. } | G::LeftJoin { left, .. } => self.rows(left, all_graphs),
            G::Filter { inner, .. }
            | G::Extend { inner, .. }
            | G::OrderBy { inner, .. }
            | G::Project { inner, .. }
            | G::Distinct { inner }
            | G::Reduced { inner }
            | G::Group { inner, .. } => self.rows(inner, all_graphs),
            G::Graph { inner, .. } => self.rows(inner, true),
            G::Slice { inner, length, .. } => {
                let rows = self.rows(inner, all_graphs)?;
                Some(length.map_or(rows, |l| rows.min(l as f64)))
            }
            G::Values { bindings, .. } => Some(bindings.len() as f64),
            G::Path {
                subject,
                path: PropertyPathExpression::NamedNode(n),
                object,
            } => self.triple(
                &TriplePattern {
                    subject: subject.clone(),
                    predicate: n.clone().into(),
                    object: object.clone(),
                },
                all_graphs,
            ),
            _ => None,
        }
    }

    /// Connected operands: the smallest; unconnected groups multiply. A small `VALUES` block
    /// of one variable is no operand of its own where a triple of the group has that
    /// variable: the triple is estimated with each value in its place and those estimates
    /// added (`VALUES ?c { … } ?f a ?c` has the instances of the listed classes, not one row
    /// per class, #463).
    fn join(&mut self, parts: &[GraphPattern], all_graphs: bool) -> Option<f64> {
        let mut done = vec![false; parts.len()];
        let mut total = 1.0;
        while let Some(first) = done.iter().position(|d| !d) {
            let (group, _) = connected(parts, &names(&parts[first]));
            let members: Vec<usize> = (0..parts.len())
                .filter(|&i| group[i] || i == first)
                .collect();
            let blocks: Vec<(usize, &Variable, Vec<&GroundTerm>)> = members
                .iter()
                .filter_map(|&i| match &parts[i] {
                    GraphPattern::Values {
                        variables,
                        bindings,
                    } if variables.len() == 1 && bindings.len() <= VALUES_EXPANDED => {
                        let values = bindings
                            .iter()
                            .map(|row| row[0].as_ref())
                            .collect::<Option<Vec<_>>>()?;
                        Some((i, &variables[0], values))
                    }
                    _ => None,
                })
                .collect();
            let mut absorbed = vec![false; parts.len()];
            let mut rows = Vec::with_capacity(members.len());
            for &i in &members {
                done[i] = true;
                let block = match &parts[i] {
                    GraphPattern::Bgp { patterns } if patterns.len() == 1 => blocks
                        .iter()
                        .find(|(_, v, _)| mentions(&patterns[0], v))
                        .map(|block| (&patterns[0], block)),
                    _ => None,
                };
                let n = match block {
                    Some((t, (b, v, values))) => {
                        absorbed[*b] = true;
                        let mut sum = 0.0;
                        for value in values {
                            if let Some(t) = substitute(t, v, value) {
                                sum += self.triple(&t, all_graphs)?;
                            }
                        }
                        sum
                    }
                    None => self.rows(&parts[i], all_graphs)?,
                };
                rows.push((i, n));
            }
            let smallest = rows
                .iter()
                .filter(|(i, _)| !absorbed[*i])
                .fold(f64::INFINITY, |m, (_, n)| m.min(*n));
            total *= smallest;
        }
        Some(total)
    }
}

/// Whether `v` is the subject, predicate or object of `t`.
fn mentions(t: &TriplePattern, v: &Variable) -> bool {
    matches!(&t.subject, TermPattern::Variable(x) if x == v)
        || matches!(&t.predicate, NamedNodePattern::Variable(x) if x == v)
        || matches!(&t.object, TermPattern::Variable(x) if x == v)
}

/// `t` with `value` in place of `v`, or `None` where the value cannot stand (a literal as
/// subject or predicate: the triple then matches nothing).
fn substitute(t: &TriplePattern, v: &Variable, value: &GroundTerm) -> Option<TriplePattern> {
    let term = |p: &TermPattern, subject: bool| -> Option<TermPattern> {
        Some(match p {
            TermPattern::Variable(x) if x == v => match value {
                GroundTerm::NamedNode(n) => n.clone().into(),
                GroundTerm::Literal(l) if !subject => l.clone().into(),
                _ => return None,
            },
            other => other.clone(),
        })
    };
    let predicate = match &t.predicate {
        NamedNodePattern::Variable(x) if x == v => match value {
            GroundTerm::NamedNode(n) => n.clone().into(),
            _ => return None,
        },
        other => other.clone(),
    };
    Some(TriplePattern {
        subject: term(&t.subject, true)?,
        predicate,
        object: term(&t.object, false)?,
    })
}
