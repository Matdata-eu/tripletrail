//! Aggregates of geometry functions computed on every core.
//!
//! `SELECT ?r (SUM(geof:metricLength(geof:intersection(?w1, ?w2))) AS ?m) WHERE { ... }
//! GROUP BY ?r` spends most of its time in the functions, one row at a time in Oxigraph's
//! evaluator. When every aggregate of a `GROUP` is a `SUM`, `AVG`, `MIN`, `MAX` or `COUNT` of
//! an expression built from tripletrail's functions, variables and constants, the engine evaluates
//! the grouped pattern on its own (with the spatial rewrite, as any query), computes the
//! expressions in parallel, aggregates them, and puts the results in place of the `GROUP` as a
//! `VALUES` block: what is around it (`BIND`, `HAVING`, `ORDER BY`, the projection) is then
//! evaluated as usual, on one row per group.
//!
//! Only results that are all `xsd:double` (or errors) are aggregated here, which is what the
//! measures (`geof:metricLength`, `geof:area`, `geof:distance`...) give; anything else makes
//! the query run the usual way.

use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, Term};
use oxigraph::sparql::Variable;
use spargebra::Query;
use spargebra::algebra::{
    AggregateExpression, AggregateFunction, Expression, Function, GraphPattern,
};
use spargebra::term::GroundTerm;
use std::collections::HashMap;
use std::sync::Arc;
use tripletrail_store::DatasetError;

type Func = Arc<dyn Fn(&[Term]) -> Option<Term> + Send + Sync>;

/// An expression of tripletrail functions over a row's values.
enum Expr {
    Var(usize),
    Const(Term),
    Call(Func, Vec<Expr>),
}

impl Expr {
    fn compile(e: &Expression, vars: &mut Vec<Variable>) -> Option<Self> {
        Some(match e {
            Expression::Variable(v) => Self::Var(match vars.iter().position(|x| x == v) {
                Some(i) => i,
                None => {
                    vars.push(v.clone());
                    vars.len() - 1
                }
            }),
            Expression::Literal(l) => Self::Const(Term::Literal(l.clone())),
            Expression::NamedNode(n) => Self::Const(Term::NamedNode(n.clone())),
            Expression::FunctionCall(Function::Custom(iri), args) => Self::Call(
                tripletrail_geo::function(iri.as_str())?,
                args.iter()
                    .map(|a| Self::compile(a, vars))
                    .collect::<Option<_>>()?,
            ),
            _ => return None,
        })
    }

    fn has_call(&self) -> bool {
        matches!(self, Self::Call(..))
    }

    fn eval(&self, row: &[Option<Term>]) -> Option<Term> {
        match self {
            Self::Var(i) => row[*i].clone(),
            Self::Const(t) => Some(t.clone()),
            Self::Call(f, args) => {
                let args = args
                    .iter()
                    .map(|a| a.eval(row))
                    .collect::<Option<Vec<_>>>()?;
                f(&args)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Agg {
    Sum,
    Avg,
    Min,
    Max,
    Count,
}

/// `query` with its `GROUP` replaced by the aggregated rows, or `None` if it does not have the
/// shape this handles (then it runs as it is). `run` evaluates a query the usual way.
pub(crate) fn parallel(
    query: &Query,
    run: &dyn Fn(Query) -> Result<Vec<Vec<Option<Term>>>, crate::Error>,
    cancelled: &(dyn Fn() -> bool + Sync),
    source: Option<Arc<tripletrail_geo::GeometrySource>>,
    snapshot: Option<&tripletrail_store::Snapshot>,
) -> Result<Option<Query>, crate::Error> {
    let Query::Select {
        dataset: None,
        pattern,
        base_iri,
    } = query
    else {
        return Ok(None);
    };
    let Some(group) = find_group(pattern) else {
        return Ok(None);
    };
    let GraphPattern::Group {
        inner,
        variables: keys,
        aggregates,
    } = group
    else {
        unreachable!()
    };
    // The row: the group keys, then the variables the expressions read.
    let mut vars: Vec<Variable> = keys.clone();
    let mut plans = Vec::new();
    for (_, a) in aggregates {
        let AggregateExpression::FunctionCall {
            name,
            expr,
            distinct: false,
        } = a
        else {
            return Ok(None);
        };
        let agg = match name {
            AggregateFunction::Sum => Agg::Sum,
            AggregateFunction::Avg => Agg::Avg,
            AggregateFunction::Min => Agg::Min,
            AggregateFunction::Max => Agg::Max,
            AggregateFunction::Count => Agg::Count,
            _ => return Ok(None),
        };
        let Some(e) = Expr::compile(expr, &mut vars) else {
            return Ok(None);
        };
        plans.push((agg, e));
    }
    if plans.is_empty() || !plans.iter().any(|(_, e)| e.has_call()) {
        return Ok(None);
    }
    // A plain basic graph pattern on the tripletrail store is joined on term ids, with its
    // geometries left as ids the functions read parsed (see `by_ids`).
    let by_ids = match snapshot {
        Some(snapshot) => by_ids(snapshot, inner, &vars, keys.len(), cancelled)?,
        None => None,
    };
    let rows = match by_ids {
        Some(rows) => rows,
        None => run(Query::Select {
            dataset: None,
            pattern: GraphPattern::Project {
                inner: inner.clone(),
                variables: vars.clone(),
            },
            base_iri: base_iri.clone(),
        })?,
    };
    // Geometries given by id resolve to the store's parsed geometry, others as usual.
    let owned = snapshot.cloned();
    let resolve = move |l: &Literal| -> Option<Arc<tripletrail_geo::Geom>> {
        if let (Some(snapshot), Some(id)) = (&owned, l.value().strip_prefix(GEOMETRY_ID)) {
            return snapshot.geometry(tripletrail_store::TermId(id.parse().ok()?));
        }
        source.as_ref().and_then(|s| s(l))
    };
    let resolve: Arc<tripletrail_geo::GeometrySource> = Arc::new(resolve);
    let source: Option<&tripletrail_geo::GeometrySource> = Some(&*resolve);
    // The expressions of every row, on every core.
    enum Stop {
        Cancelled,
        NotDouble,
    }
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let chunk = rows.len().div_ceil(threads).max(256);
    let computed: Result<Vec<Vec<Option<f64>>>, Stop> = std::thread::scope(|scope| {
        let workers: Vec<_> = rows
            .chunks(chunk)
            .map(|part| {
                let plans = &plans;
                scope.spawn(move || {
                    let work = || {
                        let mut out = Vec::with_capacity(part.len());
                        for (i, row) in part.iter().enumerate() {
                            if i % 1024 == 0 && cancelled() {
                                return Err(Stop::Cancelled);
                            }
                            let mut v = Vec::with_capacity(plans.len());
                            for (_, e) in plans {
                                v.push(match e.eval(row) {
                                    None => None,
                                    Some(t) => Some(double(&t).ok_or(Stop::NotDouble)?),
                                });
                            }
                            out.push(v);
                        }
                        Ok(out)
                    };
                    // The store's parsed geometries, as in the evaluator.
                    match source {
                        Some(source) => tripletrail_geo::with_geometry_source(source, work),
                        None => work(),
                    }
                })
            })
            .collect();
        let mut all = Vec::with_capacity(rows.len());
        for w in workers {
            all.extend(w.join().unwrap()?);
        }
        Ok(all)
    });
    let values = match computed {
        Ok(v) => v,
        Err(Stop::Cancelled) => {
            return Err(oxigraph::sparql::QueryEvaluationError::Cancelled.into());
        }
        // A value that is not an xsd:double: aggregated the usual way.
        Err(Stop::NotDouble) => return Ok(None),
    };
    // Rows by group, in order of first appearance.
    let mut groups: Vec<(Vec<Option<Term>>, Vec<usize>)> = Vec::new();
    let mut index: HashMap<Vec<Option<Term>>, usize> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        let key = row[..keys.len()].to_vec();
        let g = *index.entry(key.clone()).or_insert_with(|| {
            groups.push((key, Vec::new()));
            groups.len() - 1
        });
        groups[g].1.push(i);
    }
    // Without GROUP BY there is one group, even with no rows.
    if keys.is_empty() && groups.is_empty() {
        groups.push((Vec::new(), Vec::new()));
    }
    let mut bindings = Vec::with_capacity(groups.len());
    for (key, members) in &groups {
        let mut out: Vec<Option<GroundTerm>> = Vec::with_capacity(key.len() + plans.len());
        for t in key {
            match t {
                None => out.push(None),
                // A blank node cannot be put in a VALUES block.
                Some(t) => match ground(t.clone()) {
                    Some(g) => out.push(Some(g)),
                    None => return Ok(None),
                },
            }
        }
        for (k, (agg, _)) in plans.iter().enumerate() {
            let column = members.iter().map(|&i| values[i][k]);
            let Some(v) = aggregate(*agg, column) else {
                return Ok(None);
            };
            out.push(v);
        }
        bindings.push(out);
    }
    let mut variables = keys.clone();
    variables.extend(aggregates.iter().map(|(v, _)| v.clone()));
    let block = GraphPattern::Values {
        variables,
        bindings,
    };
    Ok(Some(Query::Select {
        dataset: None,
        pattern: replace_group(pattern.clone(), &block),
        base_iri: base_iri.clone(),
    }))
}

/// Marks a stand-in literal for a geometry of the store given by its term id: the functions
/// read it through the geometry source, which resolves it to the parsed geometry, so the
/// geometry is neither decoded to text nor looked up by it. A stand-in never leaves the
/// aggregate: results that are not numbers make the query run the usual way.
const GEOMETRY_ID: &str = "\u{1}tripletrail-geometry-id:";

/// The rows of `vars` (the group keys first, `keys` of them) when `inner` is a basic graph
/// pattern of the default graph, joined on term ids in the store; `None` for any other
/// pattern (then it is evaluated the usual way). Geometries outside the keys are stand-ins
/// (see [`GEOMETRY_ID`]).
#[allow(clippy::type_complexity)]
fn by_ids(
    snapshot: &tripletrail_store::Snapshot,
    inner: &GraphPattern,
    vars: &[Variable],
    keys: usize,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<Option<Vec<Vec<Option<Term>>>>, crate::Error> {
    use tripletrail_store::{GraphSelector, IdSlot, Kind};
    let GraphPattern::Bgp { patterns } = inner else {
        return Ok(None);
    };
    if patterns.is_empty() {
        return Ok(None);
    }
    let Some(compiled) = snapshot.compile_bgp(patterns, vars) else {
        return Ok(None);
    };
    if compiled.empty {
        return Ok(Some(Vec::new()));
    }
    let triples = compiled.triples;
    let Some(rows) = snapshot.join_ids(
        &triples,
        compiled.columns.len(),
        GraphSelector::Default,
        cancelled,
    ) else {
        return Err(oxigraph::sparql::QueryEvaluationError::Cancelled.into());
    };
    // Columns no triple binds stay unbound.
    let bound: Vec<bool> = (0..vars.len())
        .map(|c| {
            triples
                .iter()
                .any(|t| [t.s, t.p, t.o].contains(&IdSlot::Var(c)))
        })
        .collect();
    // Decoded on every core, each term once per part.
    let width = rows.width;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let per = rows.len().div_ceil(threads).max(4096) * width;
    let parts: Vec<Result<Vec<Vec<Option<Term>>>, crate::Error>> = std::thread::scope(|scope| {
        let workers: Vec<_> = rows
            .ids
            .chunks(per.max(1))
            .map(|part| {
                let bound = &bound;
                scope.spawn(move || {
                    let reader = snapshot.reader();
                    let mut decoded: HashMap<tripletrail_store::TermId, Term> = HashMap::new();
                    let mut out = Vec::with_capacity(part.len() / width.max(1));
                    for (k, row) in part.chunks_exact(width).enumerate() {
                        if k % 4096 == 0 && cancelled() {
                            return Err(oxigraph::sparql::QueryEvaluationError::Cancelled.into());
                        }
                        let mut values = Vec::with_capacity(vars.len());
                        for (c, &id) in row[..vars.len()].iter().enumerate() {
                            values.push(if !bound[c] {
                                None
                            } else if c >= keys && id.kind() == Kind::Geometry {
                                Some(Term::Literal(Literal::new_typed_literal(
                                    format!("{GEOMETRY_ID}{}", id.0),
                                    tripletrail_geo::WKT_LITERAL,
                                )))
                            } else {
                                Some(match decoded.get(&id) {
                                    Some(t) => t.clone(),
                                    None => {
                                        let t = reader.term(id).map_err(DatasetError::from)?;
                                        decoded.insert(id, t.clone());
                                        t
                                    }
                                })
                            });
                        }
                        out.push(values);
                    }
                    Ok(out)
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    let mut out = Vec::with_capacity(rows.len());
    for part in parts {
        out.extend(part?);
    }
    Ok(Some(out))
}

/// The `GROUP` under the projection, modifiers, `BIND`s and `HAVING` of a query.
fn find_group(p: &GraphPattern) -> Option<&GraphPattern> {
    match p {
        GraphPattern::Group { .. } => Some(p),
        GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Extend { inner, .. }
        | GraphPattern::Filter { inner, .. } => find_group(inner),
        _ => None,
    }
}

fn replace_group(p: GraphPattern, block: &GraphPattern) -> GraphPattern {
    use GraphPattern as G;
    let r = |inner: Box<G>| Box::new(replace_group(*inner, block));
    match p {
        G::Group { .. } => block.clone(),
        G::Project { inner, variables } => G::Project {
            inner: r(inner),
            variables,
        },
        G::Distinct { inner } => G::Distinct { inner: r(inner) },
        G::Reduced { inner } => G::Reduced { inner: r(inner) },
        G::Slice {
            inner,
            start,
            length,
        } => G::Slice {
            inner: r(inner),
            start,
            length,
        },
        G::OrderBy { inner, expression } => G::OrderBy {
            inner: r(inner),
            expression,
        },
        G::Extend {
            inner,
            variable,
            expression,
        } => G::Extend {
            inner: r(inner),
            variable,
            expression,
        },
        G::Filter { expr, inner } => G::Filter {
            expr,
            inner: r(inner),
        },
        other => other,
    }
}

/// The value of an `xsd:double` literal, or `None` for any other term.
fn double(t: &Term) -> Option<f64> {
    match t {
        Term::Literal(l) if l.datatype() == xsd::DOUBLE => match l.value() {
            "INF" => Some(f64::INFINITY),
            "-INF" => Some(f64::NEG_INFINITY),
            "NaN" => Some(f64::NAN),
            v => v.parse().ok(),
        },
        _ => None,
    }
}

fn ground(t: Term) -> Option<GroundTerm> {
    match t {
        Term::NamedNode(n) => Some(GroundTerm::NamedNode(n)),
        Term::Literal(l) => Some(GroundTerm::Literal(l)),
        _ => None,
    }
}

/// One aggregate of a group's values (`None` for an error), as SPARQL defines it: `Some(None)`
/// is an unbound result, `None` a case left to Oxigraph's evaluator.
fn aggregate(agg: Agg, values: impl Iterator<Item = Option<f64>>) -> Option<Option<GroundTerm>> {
    let lit = |v: f64| Some(GroundTerm::Literal(Literal::from(v)));
    let int = |v: usize| Some(GroundTerm::Literal(Literal::from(v as i64)));
    let mut n = 0usize;
    let mut sum = 0.0;
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut error = false;
    for v in values {
        match v {
            Some(v) => {
                n += 1;
                sum += v;
                if v.is_nan() {
                    // Ordering with NaN is left to the evaluator.
                    if matches!(agg, Agg::Min | Agg::Max) {
                        return None;
                    }
                } else {
                    min = min.min(v);
                    max = max.max(v);
                }
            }
            None => error = true,
        }
    }
    Some(match agg {
        // COUNT(expr) counts the rows where the expression has a value.
        Agg::Count => int(n),
        // An error in a SUM or AVG makes the result an error: unbound.
        Agg::Sum | Agg::Avg if error => None,
        Agg::Sum | Agg::Avg if n == 0 => int(0),
        Agg::Sum => lit(sum),
        Agg::Avg => lit(sum / n as f64),
        // MIN and MAX with errors (which order before every value) are left to the evaluator.
        Agg::Min | Agg::Max if error => return None,
        Agg::Min | Agg::Max if n == 0 => None,
        Agg::Min => lit(min),
        Agg::Max => lit(max),
    })
}
