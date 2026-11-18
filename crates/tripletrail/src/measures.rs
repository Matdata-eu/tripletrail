//! Measures of the store's geometries computed ahead on every core (#703).
//!
//! `SELECT (geof:metricArea(?g) AS ?a) WHERE { ?f geo:asWKT ?g }` spends nearly all its time
//! in the geodesic area, one row at a time on the evaluator's thread: 44,834 CORINE polygons
//! take 4 s. The store keeps its geometries parsed, and a geometry keeps its area once
//! computed ([`tripletrail_geo::Geom::metric_area`]). So before the query runs, this pass finds
//! the areas a `BIND` or projection will ask for, computes them for the geometries the pattern
//! under it can bind, in parallel, and leaves the query as it is: the evaluator then finds each
//! area computed. Answers can't change, since the evaluator computes every value as before.
//!
//! The geometries are found on ids, without decoding their text: those of a variable that is
//! the object of the only triple pattern under the `BIND` (in the default graph or in a named
//! `GRAPH`). Anything else is left alone, and so is a `BIND` under a `LIMIT` without an
//! `ORDER BY`, where the evaluator may need only a few areas.

use spargebra::Query;
use spargebra::algebra::{Expression, Function, GraphPattern};
use spargebra::term::{NamedNodePattern, TermPattern, Variable};
use tripletrail_store::{GraphSelector, Kind, Snapshot, TermId};

const GEOF: &str = "http://www.opengis.net/def/function/geosparql/";
/// The functions whose value is the area a geometry keeps.
const AREA: [&str; 4] = ["metricArea", "area", "metricSize", "size"];
/// Fewer geometries are left to the evaluator: the threads cost more than they save.
const MIN_GEOMETRIES: usize = 1024;
/// Geometries per chunk at least.
const CHUNK: usize = 64;

static WARMED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many geometries' areas this pass has computed ahead, in this process so far.
pub fn warmed() -> u64 {
    WARMED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Computes ahead, on every core, the areas `query` will ask for of the store's geometries.
/// Says how many geometries it computed.
pub(crate) fn warm(
    query: &Query,
    snapshot: &Snapshot,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> usize {
    let pattern = match query {
        Query::Select {
            dataset: None,
            pattern,
            ..
        }
        | Query::Construct {
            dataset: None,
            pattern,
            ..
        } => pattern,
        _ => return 0,
    };
    let mut done = 0;
    walk(pattern, false, &mut |inner, v| {
        if cancelled() {
            return;
        }
        let Some(ids) = geometries(inner, v, snapshot) else {
            return;
        };
        if ids.len() < MIN_GEOMETRIES {
            return;
        }
        tripletrail_store::par::chunks_sized(ids.len(), MIN_GEOMETRIES, CHUNK, |range| {
            for (k, &id) in ids[range].iter().enumerate() {
                if k % 64 == 0 && cancelled() {
                    return;
                }
                if let Some(g) = snapshot.geometry(id) {
                    g.metric_area();
                }
            }
        });
        done += ids.len();
        WARMED.fetch_add(ids.len() as u64, std::sync::atomic::Ordering::Relaxed);
    });
    done
}

/// Calls `f` with the pattern under each `BIND` (or projected expression) that asks for the
/// area of a variable, and that variable. Not under a `LIMIT`/`OFFSET` (`limited`), where the
/// evaluator may stop after a few rows, unless an `ORDER BY` or `GROUP BY` between them makes
/// it compute every row anyway.
fn walk(p: &GraphPattern, limited: bool, f: &mut dyn FnMut(&GraphPattern, &Variable)) {
    use GraphPattern as G;
    match p {
        G::Extend {
            inner, expression, ..
        } => {
            if !limited {
                let mut vars = Vec::new();
                areas(expression, &mut vars);
                for v in &vars {
                    f(inner, v);
                }
            }
            walk(inner, limited, f);
        }
        G::Join { left, right }
        | G::LeftJoin { left, right, .. }
        | G::Lateral { left, right }
        | G::Union { left, right }
        | G::Minus { left, right } => {
            walk(left, limited, f);
            walk(right, limited, f);
        }
        G::Slice { inner, .. } => walk(inner, true, f),
        G::OrderBy { inner, .. } | G::Group { inner, .. } => walk(inner, false, f),
        G::Filter { inner, .. }
        | G::Graph { inner, .. }
        | G::Project { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::Service { inner, .. } => walk(inner, limited, f),
        G::Bgp { .. } | G::Path { .. } | G::Values { .. } => {}
    }
}

/// The variables `e` asks the area of.
fn areas(e: &Expression, out: &mut Vec<Variable>) {
    use Expression as E;
    match e {
        E::FunctionCall(Function::Custom(n), args) => {
            if let (Some(local), [E::Variable(v)]) = (n.as_str().strip_prefix(GEOF), &args[..])
                && AREA.contains(&local)
                && !out.contains(v)
            {
                out.push(v.clone());
            }
            args.iter().for_each(|a| areas(a, out));
        }
        E::FunctionCall(_, args) | E::Coalesce(args) => args.iter().for_each(|a| areas(a, out)),
        E::In(a, list) => {
            areas(a, out);
            list.iter().for_each(|b| areas(b, out));
        }
        E::Or(a, b)
        | E::And(a, b)
        | E::Equal(a, b)
        | E::SameTerm(a, b)
        | E::Greater(a, b)
        | E::GreaterOrEqual(a, b)
        | E::Less(a, b)
        | E::LessOrEqual(a, b)
        | E::Add(a, b)
        | E::Subtract(a, b)
        | E::Multiply(a, b)
        | E::Divide(a, b) => {
            areas(a, out);
            areas(b, out);
        }
        E::UnaryPlus(a) | E::UnaryMinus(a) | E::Not(a) => areas(a, out),
        E::If(a, b, c) => {
            areas(a, out);
            areas(b, out);
            areas(c, out);
        }
        _ => {}
    }
}

/// The distinct geometry ids `v` can take in the solutions of `p`, when `p` is one triple
/// pattern, read from one index run. Other patterns would have to be evaluated a second time
/// before the query, which can cost more than the areas save.
fn geometries(p: &GraphPattern, v: &Variable, snapshot: &Snapshot) -> Option<Vec<TermId>> {
    let mut ids: Vec<TermId> = match p {
        // One triple pattern, in a named graph or the default graph: the objects of its
        // predicate.
        GraphPattern::Graph {
            name: NamedNodePattern::NamedNode(g),
            inner,
        } => objects(inner, v, snapshot, || {
            snapshot
                .term_id(g.as_ref().into())
                .map(GraphSelector::Graph)
        })?,
        GraphPattern::Bgp { patterns } if patterns.len() == 1 => {
            objects(p, v, snapshot, || Some(GraphSelector::Default))?
        }
        _ => return None,
    };
    ids.retain(|id| id.kind() == Kind::Geometry);
    ids.sort_unstable_by_key(|id| id.0);
    ids.dedup();
    Some(ids)
}

/// The objects of the one triple pattern `p` holds, when `v` is its object and its
/// predicate is constant (subject a variable or blank node), in the graph `graph` gives
/// (`None`: a graph the store doesn't hold, so no objects).
fn objects(
    p: &GraphPattern,
    v: &Variable,
    snapshot: &Snapshot,
    graph: impl FnOnce() -> Option<GraphSelector>,
) -> Option<Vec<TermId>> {
    let GraphPattern::Bgp { patterns } = p else {
        return None;
    };
    let [t] = patterns.as_slice() else {
        return None;
    };
    if !matches!(&t.object, TermPattern::Variable(o) if o == v)
        || !matches!(
            t.subject,
            TermPattern::Variable(_) | TermPattern::BlankNode(_)
        )
    {
        return None;
    }
    let NamedNodePattern::NamedNode(predicate) = &t.predicate else {
        return None;
    };
    let Some(predicate) = snapshot.term_id(predicate.as_ref().into()) else {
        return Some(Vec::new());
    };
    let Some(graph) = graph() else {
        return Some(Vec::new());
    };
    Some(
        snapshot
            .quad_ids_for_pattern(None, Some(predicate), None, graph)
            .map(|q| q[2])
            .collect(),
    )
}
