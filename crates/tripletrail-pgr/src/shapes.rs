//! `pgr:shapes` and `pgr:vehicle`: SHACL shapes as an edge filter.
//!
//! Each edge's focus node (`?id`, or the column `pgr:focus` names) is validated against the
//! shapes of a named graph, or against the shapes a vehicle's compatibility rules compile to,
//! reading the store's data; edges whose focus node does not conform are left out of the
//! search. When the shapes graph has SHACL-AF rules, they run first, in memory, and validation
//! sees the store's data plus what they infer. Nothing here knows a data model: the shapes and
//! rules do.

use crate::network::{FilterReads, Network};
use crate::request::Request;
use crate::service::PgrError;
use crate::tables::Row;
use oxigraph::model::{GraphName, GraphNameRef, NamedNode, Term};
use oxigraph::sparql::SparqlEvaluator;
use std::collections::{BTreeSet, HashMap};
use tripletrail_shacl::{
    DataGraph, Inference, Inferred, ShapeId, Shapes, StoreGraph, Union, Validator, compat,
};
use tripletrail_store::Dataset;

/// From this many focus nodes on, validation runs on several threads.
const PARALLEL_FROM: usize = 2048;
/// Focus nodes validated per lock of the store's dictionary.
const BATCH: usize = 4096;

/// Makes the evaluators SHACL-SPARQL queries run on (with the engine's functions).
pub(crate) type Evaluator = dyn Fn() -> SparqlEvaluator + Send + Sync;

fn shacl(e: tripletrail_shacl::ShaclError) -> PgrError {
    PgrError::Request(e.to_string())
}

/// Shapes to check every focus node against: those named (all of them), or else those whose
/// targets include it.
struct Check {
    shapes: Shapes,
    named: Vec<ShapeId>,
    /// The shapes graph (`$shapesGraph` in SHACL-SPARQL), for shapes read from the store.
    graph: Option<(NamedNode, StoreGraph)>,
}

impl Check {
    fn validator<'a>(&'a self, data: &'a dyn DataGraph, ev: &'a Ev<'a>) -> Validator<'a> {
        let v = Validator::new(&self.shapes, data).with_evaluator(ev);
        match &self.graph {
            Some((name, g)) => v.with_shapes_graph(name, g),
            None => v,
        }
    }

    fn prefetch(&self, v: &Validator<'_>, foci: &[&Term]) {
        if self.named.is_empty() {
            v.prefetch(foci);
        } else {
            for &s in &self.named {
                v.prefetch_for(s, foci);
            }
        }
    }

    fn conforms(&self, v: &Validator<'_>, focus: &Term) -> bool {
        if self.named.is_empty() {
            v.conforms(focus)
        } else {
            self.named.iter().all(|&s| v.conforms_to(focus, s))
        }
    }
}

/// An [`tripletrail_shacl::Evaluator`] from the service's evaluator factory.
struct Ev<'a>(&'a Evaluator);

impl tripletrail_shacl::Evaluator for Ev<'_> {
    fn evaluator(&self) -> SparqlEvaluator {
        (self.0)()
    }
}

/// The key of the filter a request asks for, if any: the network caches filtered graphs by it.
pub(crate) fn filter_key(request: &Request) -> Option<String> {
    if request.constraint.is_none() && request.shapes.is_none() && request.vehicle.is_none() {
        return None;
    }
    if request.shapes.is_none() && request.vehicle.is_none() {
        return request.constraint.clone();
    }
    Some(format!(
        "{:?}\u{0}{:?}\u{0}{:?}\u{0}{:?}\u{0}{:?}\u{0}{:?}\u{0}{:?}",
        request.constraint,
        request.shapes.as_ref().map(NamedNode::as_str),
        request
            .shape_nodes
            .iter()
            .map(NamedNode::as_str)
            .collect::<Vec<_>>(),
        request.focus,
        request.vehicle.as_ref().map(Term::to_string),
        request.rules.as_ref().map(NamedNode::as_str),
        request.data_graph.as_ref().map(NamedNode::as_str),
    ))
}

/// The triples the SHACL-AF rules of `shapes` (read from graph `name`) infer from `data`.
fn infer(
    shapes: &Shapes,
    name: &NamedNode,
    shapes_graph: &StoreGraph,
    data: &StoreGraph,
    evaluator: &Evaluator,
) -> Result<Inferred, PgrError> {
    let ev = Ev(evaluator);
    data.read(|g| {
        Inference::new(shapes, g)
            .with_evaluator(&ev)
            .with_shapes_graph(name, shapes_graph)
            .run()
    })
    .map_err(shacl)
}

/// Which edges (by edge id) the request's filter rejects, and what the filter reads.
pub(crate) fn rejects(
    store: &Dataset,
    evaluator: &Evaluator,
    network: &Network,
    request: &Request,
) -> Result<(Vec<bool>, FilterReads), PgrError> {
    let mut disabled = match &request.constraint {
        Some(c) => network.constraint_rejects(c)?,
        None => vec![false; network.graph.edges.len()],
    };
    if request.shapes.is_none() && request.vehicle.is_none() {
        return Ok((disabled, FilterReads::Rows));
    }
    let data_name: GraphName = request
        .data_graph
        .clone()
        .map_or(GraphName::DefaultGraph, Into::into);
    let data = StoreGraph::new(store, data_name.as_ref());
    let mut checks = Vec::new();
    let mut reads = BTreeSet::new();
    let mut any = false;
    let mut add_reads = |shapes: &Shapes, reads: &mut BTreeSet<String>| match shapes.reads() {
        Some(r) => reads.extend(r),
        None => any = true,
    };
    // What the shapes graph's rules infer, kept in memory.
    let mut inferred = Inferred::new();
    if let Some(g) = &request.shapes {
        let graph = StoreGraph::new(store, g.as_ref().into());
        let roots: Vec<Term> = request
            .shape_nodes
            .iter()
            .map(|n| n.clone().into())
            .collect();
        let shapes = Shapes::compile_with(&graph, &roots).map_err(shacl)?;
        if shapes.is_empty() {
            return Err(PgrError::Request(format!("no SHACL shapes in graph {g}")));
        }
        if shapes.has_rules() {
            inferred = infer(&shapes, g, &graph, &data, evaluator)?;
        }
        add_reads(&shapes, &mut reads);
        let mut predicates = Vec::new();
        graph.predicates(&mut predicates);
        reads.extend(predicates.iter().map(|p| p.as_str().to_owned()));
        let named = roots.iter().filter_map(|r| shapes.shape(r)).collect();
        checks.push(Check {
            shapes,
            named,
            graph: Some((g.clone(), graph)),
        });
    }
    if let (Some(vehicle), Some(rules)) = (&request.vehicle, &request.rules) {
        let rules_graph = StoreGraph::new(store, rules.as_ref().into());
        let vehicles = Union(vec![&data, &inferred, &rules_graph]);
        let compiled = compat::compile(&rules_graph, &vehicles, vehicle, None).map_err(shacl)?;
        if compiled.checked + compiled.skipped == 0 {
            return Err(PgrError::Request(format!(
                "no compatibility rules in graph {rules}"
            )));
        }
        let shapes = Shapes::compile_with(&compiled.graph, std::slice::from_ref(&compiled.shape))
            .map_err(shacl)?;
        add_reads(&shapes, &mut reads);
        reads.extend(compiled.reads);
        let named = vec![shapes.shape(&compiled.shape).expect("compiled")];
        checks.push(Check {
            shapes,
            named,
            graph: None,
        });
    }

    // The focus node of every row, validated once each.
    let column = match &request.focus {
        Some(name) => Some(
            network
                .extras
                .as_ref()
                .and_then(|x| x.names.iter().position(|n| n == name))
                .ok_or_else(|| {
                    PgrError::Request(format!("pgr:focus: the edge set does not project ?{name}"))
                })?,
        ),
        None => None,
    };
    let mut index: HashMap<&Term, u32> = HashMap::new();
    let mut foci: Vec<&Term> = Vec::new();
    let mut row_focus = Vec::with_capacity(network.graph.rows.len());
    for (i, r) in network.graph.rows.iter().enumerate() {
        let focus = match column {
            Some(c) => network.extras.as_ref().and_then(|x| x.row(i)[c].as_ref()),
            None => Some(&network.graph.edges[r.id as usize]),
        };
        row_focus.push(focus.map(|f| {
            *index.entry(f).or_insert_with(|| {
                foci.push(f);
                (foci.len() - 1) as u32
            })
        }));
    }
    let ok = validate(&checks, &data, &inferred, evaluator, &foci)?;
    for (r, focus) in network.graph.rows.iter().zip(row_focus) {
        // A row without a focus node has nothing to check.
        if focus.is_some_and(|f| !ok[f as usize]) {
            disabled[r.id as usize] = true;
        }
    }
    let reads = if any {
        FilterReads::Any
    } else {
        FilterReads::Predicates(reads)
    };
    Ok((disabled, reads))
}

/// Whether each focus node passes every check, on several threads for many nodes. The data
/// is the store's graph plus `inferred`.
fn validate(
    checks: &[Check],
    data: &StoreGraph,
    inferred: &Inferred,
    evaluator: &Evaluator,
    foci: &[&Term],
) -> Result<Vec<bool>, PgrError> {
    // In batches, each read with the store's dictionary locked once (writers wait for a
    // batch, not for the whole validation).
    let run = |part: &[&Term]| -> Result<Vec<bool>, PgrError> {
        let ev = Ev(evaluator);
        let mut out = Vec::with_capacity(part.len());
        for batch in part.chunks(BATCH) {
            data.read(|g| {
                let with_inferred = Union(vec![g, inferred]);
                let g: &dyn DataGraph = if inferred.is_empty() {
                    g
                } else {
                    &with_inferred
                };
                let validators: Vec<Validator<'_>> =
                    checks.iter().map(|c| c.validator(g, &ev)).collect();
                for (c, v) in checks.iter().zip(&validators) {
                    c.prefetch(v, batch);
                }
                out.extend(batch.iter().map(|f| {
                    checks
                        .iter()
                        .zip(&validators)
                        .all(|(c, v)| c.conforms(v, f))
                }));
                validators
                    .iter()
                    .find_map(Validator::take_error)
                    .map_or(Ok(()), |e| Err(shacl(e)))
            })?;
        }
        Ok(out)
    };
    let threads = tripletrail_store::par::cores().min(16);
    if foci.len() < PARALLEL_FROM || threads < 2 {
        return run(foci);
    }
    let chunk = foci.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = foci
            .chunks(chunk)
            .map(|part| scope.spawn(move || run(part)))
            .collect();
        let mut out = Vec::with_capacity(foci.len());
        for h in handles {
            out.extend(h.join().expect("validation thread")?);
        }
        Ok(out)
    })
}

/// `rail:compatibilityShapes`: the shapes a vehicle's rules compile to, one row per triple
/// (`pgr:subject`, `pgr:predicate`, `pgr:object`).
pub(crate) fn compatibility_table(
    store: &Dataset,
    request: &Request,
) -> Result<Vec<Row>, PgrError> {
    let (Some(vehicle), Some(rules)) = (&request.vehicle, &request.rules) else {
        unreachable!("checked by Request::parse")
    };
    let data_name: GraphName = request
        .data_graph
        .clone()
        .map_or(GraphName::DefaultGraph, Into::into);
    let data = StoreGraph::new(store, data_name.as_ref());
    let rules_graph = StoreGraph::new(store, GraphNameRef::NamedNode(rules.as_ref()));
    let vehicles = Union(vec![&data, &rules_graph]);
    let compiled = compat::compile(
        &rules_graph,
        &vehicles,
        vehicle,
        request.shape_nodes.first().cloned(),
    )
    .map_err(shacl)?;
    Ok(triple_rows(compiled.graph.iter()))
}

/// `rail:shaclRules`: the triples the SHACL-AF rules of the `pgr:shapes` graph infer from the
/// default graph (or `pgr:data_graph`), one row per triple (`pgr:subject`, `pgr:predicate`,
/// `pgr:object`). Nothing is written: `INSERT ... WHERE { SERVICE ... }` stores them.
pub(crate) fn rules_table(
    store: &Dataset,
    evaluator: &Evaluator,
    request: &Request,
) -> Result<Vec<Row>, PgrError> {
    let Some(g) = &request.shapes else {
        unreachable!("checked by Request::parse")
    };
    let data_name: GraphName = request
        .data_graph
        .clone()
        .map_or(GraphName::DefaultGraph, Into::into);
    let data = StoreGraph::new(store, data_name.as_ref());
    let graph = StoreGraph::new(store, g.as_ref().into());
    let shapes = Shapes::compile(&graph).map_err(shacl)?;
    if !shapes.has_rules() {
        return Err(PgrError::Request(format!("no SHACL rules in graph {g}")));
    }
    Ok(triple_rows(
        infer(&shapes, g, &graph, &data, evaluator)?
            .iter()
            .map(Into::into),
    ))
}

fn triple_rows<'a>(triples: impl Iterator<Item = oxigraph::model::TripleRef<'a>>) -> Vec<Row> {
    triples
        .map(|t| {
            vec![
                Some(t.subject.into_owned().into()),
                Some(t.predicate.into_owned().into()),
                Some(t.object.into_owned()),
            ]
        })
        .collect()
}
