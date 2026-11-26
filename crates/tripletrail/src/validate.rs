//! SHACL validation of a graph of the store against the shapes of another, with a W3C
//! validation report.

use crate::{Engine, Error};
use oxigraph::model::{GraphNameRef, NamedNode, Triple};
use oxigraph::sparql::SparqlEvaluator;
use tripletrail_shacl::{
    DataGraph, FailingFocusNodes, Inference, Inferred, ReportWriter, ShapeCount, Shapes,
    StoreGraph, Union, ValidationResult, Validator,
};

/// An error of the shapes graph or of a SHACL-SPARQL query.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct ShaclError(#[from] tripletrail_shacl::ShaclError);

struct Ev<'a> {
    engine: &'a Engine,
    /// With [`InferOptions::in_store`]: how many inferred triples are in the default graph.
    in_store: Option<std::sync::Mutex<usize>>,
}

impl<'a> Ev<'a> {
    fn new(engine: &'a Engine) -> Self {
        Self {
            engine,
            in_store: None,
        }
    }
}

impl tripletrail_shacl::Evaluator for Ev<'_> {
    fn evaluator(&self) -> SparqlEvaluator {
        self.engine.evaluator()
    }

    fn prepare(&self, mut query: spargebra::Query) -> spargebra::Query {
        self.engine.inline_functions(&mut query);
        crate::plan::plan_query(query)
    }

    fn construct_native(
        &self,
        query: &spargebra::Query,
        inferred: &[Triple],
    ) -> Option<Result<Vec<Triple>, String>> {
        Some(self.native(query, inferred)?.and_then(|r| {
            match r {
                oxigraph::sparql::QueryResults::Graph(triples) => triples
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| e.to_string()),
                _ => Err("expected a CONSTRUCT query".into()),
            }
        }))
    }

    fn select_native(
        &self,
        query: &spargebra::Query,
        inferred: &[Triple],
    ) -> Option<Result<Vec<oxigraph::sparql::QuerySolution>, String>> {
        Some(self.native(query, inferred)?.and_then(|r| {
            match r {
                oxigraph::sparql::QueryResults::Solutions(solutions) => solutions
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| e.to_string()),
                _ => Err("expected a SELECT query".into()),
            }
        }))
    }

    fn focus_nodes_native(
        &self,
        query: &spargebra::Query,
        inferred: &[Triple],
    ) -> Option<Result<Vec<oxigraph::model::Term>, String>> {
        Some(self.native(query, inferred)?.and_then(|r| match r {
            oxigraph::sparql::QueryResults::Solutions(solutions) => {
                let mut out = Vec::new();
                for s in solutions {
                    let s = s.map_err(|e| e.to_string())?;
                    if let Some(t) = s.get("this") {
                        out.push(t.clone());
                    }
                }
                Ok(out)
            }
            _ => Err("expected a SELECT query".into()),
        }))
    }
}

impl Ev<'_> {
    /// Evaluates a query on the store after adding the inferred triples it lacks to the
    /// default graph. `None` without [`InferOptions::in_store`].
    fn native(
        &self,
        query: &spargebra::Query,
        inferred: &[Triple],
    ) -> Option<Result<oxigraph::sparql::QueryResults<'static>, String>> {
        let mut written = self
            .in_store
            .as_ref()?
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *written < inferred.len() {
            let quads = inferred[*written..]
                .iter()
                .map(|t| t.clone().in_graph(oxigraph::model::GraphName::DefaultGraph));
            if let Err(e) = self.engine.load_quads(quads) {
                return Some(Err(e.to_string()));
            }
            *written = inferred.len();
        }
        // The rules of a pass all read the same inferred triples (#891): once they are in the
        // store, their queries run at the same time.
        drop(written);
        Some(
            self.engine
                .evaluate(query.clone(), crate::QueryOptions::default())
                .map_err(|e| e.to_string()),
        )
    }
}

/// Options of [`Engine::infer_with`].
#[derive(Debug, Clone)]
pub struct InferOptions {
    /// Passes at most over each group of rules that feed each other (64 by default).
    pub max_passes: usize,
    /// Also return (and write to `into`) the constructed triples that are already in the data
    /// graph, as TopBraid's `shaclinfer` does: every triple a rule constructs, once.
    pub include_existing: bool,
    /// Run later passes of SPARQL rules only on what the last pass inferred can add
    /// (semi-naive, the default); `false` runs every rule on all the data again.
    pub semi_naive: bool,
    /// Run the rules' SPARQL queries on the store itself (#260), adding each inferred triple
    /// to the data graph before the next query: several times faster than reading the store
    /// through the SHACL engine's own evaluator. The inferred triples are then in the data
    /// graph afterwards, so this is for a store made for the inference, as
    /// `tripletrail infer --data` makes. Only on the tripletrail store with the data in the default
    /// graph (and no union default graph); otherwise ignored.
    pub in_store: bool,
    /// Copy the data graph and the shapes graph into a new in-memory tripletrail store first,
    /// and run the rules there with [`InferOptions::in_store`]: as fast, and this store is
    /// not changed (only `into` is written). The data graph is then held in memory. For an
    /// on-disk store, as `tripletrail infer --location` opens. Always so for the default graph
    /// of an engine with a union default graph, which is all graphs only in a copy.
    pub scratch: bool,
    /// Run each rule once, in `sh:order` (then by the rule node), each seeing what the rules
    /// before it inferred: one pass, no stratification and no fixpoint (see
    /// [`Inference::with_sequential`]).
    pub sequential: bool,
    /// Where to put the [`tripletrail_shacl::Plan`] of the run: its groups of rules and why.
    pub plan: Option<std::sync::Arc<tripletrail_shacl::PlanSink>>,
}

impl Default for InferOptions {
    fn default() -> Self {
        Self {
            max_passes: 64,
            include_existing: false,
            semi_naive: true,
            in_store: false,
            scratch: false,
            sequential: false,
            plan: None,
        }
    }
}

impl Engine {
    /// Validates graph `data` of the store against the shapes of the named graph `shapes`,
    /// giving every [`ValidationResult`] as it is found (see [`Validator::report`]). When the
    /// shapes graph has SHACL-AF rules, they run first, in memory, and validation sees the data
    /// plus what they infer. SHACL-SPARQL queries run with the engine's functions. The
    /// validation reads one snapshot without holding the store's dictionary lock for the run:
    /// writes go on meanwhile, and it does not see them.
    pub fn validate_results(
        &self,
        shapes: &NamedNode,
        data: GraphNameRef<'_>,
        emit: &mut dyn FnMut(ValidationResult<'_>),
    ) -> Result<(), Error> {
        self.validate_counted(shapes, data, false, emit).map(|_| ())
    }

    /// As [`Engine::validate_results`]; with `counts`, also returns how many focus and value
    /// nodes each shape was checked against ([`Validator::shape_counts`]), else an empty list.
    pub fn validate_counted(
        &self,
        shapes: &NamedNode,
        data: GraphNameRef<'_>,
        counts: bool,
        emit: &mut dyn FnMut(ValidationResult<'_>),
    ) -> Result<Vec<ShapeCount>, Error> {
        self.validate_counted_with(shapes, data, &[], counts, emit)
    }

    /// As [`Engine::validate_counted`], with lookup graphs (#91): the named graphs `lookups`
    /// are read with `data` (by `sh:class`, paths and SPARQL constraints, as one data graph)
    /// but give no focus nodes, so vocabularies and code lists the data refers to are not
    /// validated themselves (see [`Validator::with_focus_graph`]).
    pub fn validate_counted_with(
        &self,
        shapes: &NamedNode,
        data: GraphNameRef<'_>,
        lookups: &[NamedNode],
        counts: bool,
        emit: &mut dyn FnMut(ValidationResult<'_>),
    ) -> Result<Vec<ShapeCount>, Error> {
        // The shapes read the store through its graphs, not through queries: the union default
        // graph is all graphs only in a copy.
        if data.is_default_graph() && self.union_default_graph() {
            if !lookups.is_empty() {
                return Err(Error::Request(
                    "lookup graphs need a named data graph when the default graph is the \
                     union of the graphs"
                        .into(),
                ));
            }
            return self
                .scratch_copy(shapes, data)?
                .validate_counted(shapes, data, counts, emit);
        }
        let shapes_graph = StoreGraph::new(self.store(), shapes.as_ref().into());
        let engine = self.with_shape_functions(shapes)?;
        engine.validate_on(
            shapes,
            &shapes_graph,
            self,
            data,
            lookups,
            counts,
            None,
            emit,
        )
    }

    /// Validates graph `data` of `store` (this engine, with the shapes' functions) against
    /// `shapes_graph`, named `shapes`; with `focus`, only that node, against the shapes that
    /// target it.
    #[allow(clippy::too_many_arguments)]
    fn validate_on(
        &self,
        shapes: &NamedNode,
        shapes_graph: &dyn DataGraph,
        store: &Engine,
        data: GraphNameRef<'_>,
        lookups: &[NamedNode],
        counts: bool,
        focus: Option<&oxigraph::model::Term>,
        emit: &mut dyn FnMut(ValidationResult<'_>),
    ) -> Result<Vec<ShapeCount>, Error> {
        let compiled = Shapes::compile(shapes_graph).map_err(ShaclError::from)?;
        // The snapshot is read directly, not through `StoreGraph::read`, so the store's
        // dictionary lock is taken at each lookup rather than for the whole run: writes with
        // new terms go on meanwhile (#942), and the validation does not see them. On a large
        // production knowledge graph this costs about 5% of the run.
        let data = StoreGraph::new(store.store(), data);
        let lookups: Vec<StoreGraph> = lookups
            .iter()
            .map(|l| StoreGraph::new(store.store(), l.as_ref().into()))
            .collect();
        let with_lookups = Union(
            std::iter::once(&data as &dyn DataGraph)
                .chain(lookups.iter().map(|l| l as &dyn DataGraph))
                .collect(),
        );
        let g: &dyn DataGraph = if lookups.is_empty() {
            &data
        } else {
            &with_lookups
        };
        let ev = Ev::new(self);
        let inferred = if compiled.has_rules() {
            Inference::new(&compiled, g)
                .with_evaluator(&ev)
                .with_shapes_graph(shapes, shapes_graph)
                .run()
                .map_err(ShaclError::from)?
        } else {
            Inferred::new()
        };
        let with_inferred = Union(vec![g, &inferred]);
        let g: &dyn DataGraph = if inferred.is_empty() {
            g
        } else {
            &with_inferred
        };
        let v = Validator::new(&compiled, g)
            .with_evaluator(&ev)
            .with_shapes_graph(shapes, shapes_graph);
        // With lookups, focus nodes come from the data graph (and what rules infer).
        let focus_graph = Union(vec![&data, &inferred]);
        let v = match (lookups.is_empty(), inferred.is_empty()) {
            (true, _) => v,
            (false, true) => v.with_focus_graph(&data),
            (false, false) => v.with_focus_graph(&focus_graph),
        };
        let mut failing = FailingFocusNodes::default();
        let mut tally = |r: ValidationResult<'_>| {
            if counts {
                failing.add(&r);
            }
            emit(r)
        };
        match focus {
            None => v.report(&mut tally),
            Some(node) => {
                for &s in compiled.targeted() {
                    if v.is_target(node, s) {
                        v.report_focus(node, s, &mut tally);
                    }
                }
            }
        }
        let counts = if counts {
            let mut c = v.shape_counts();
            failing.fill(&mut c);
            c
        } else {
            Vec::new()
        };
        match v.take_error() {
            Some(e) => Err(ShaclError::from(e).into()),
            None => Ok(counts),
        }
    }

    /// Validates graph `data` of the store (with `union`, its named graphs as one, as Jena's
    /// union graph: not the default graph) against
    /// `shapes`, a graph given with the request rather than one of the store (Fuseki's SHACL
    /// service), as a W3C validation report given to `sink`; with `focus`, only that node.
    /// Returns whether the data conforms.
    pub fn validate_graph_report(
        &self,
        shapes: &oxigraph::model::Graph,
        data: GraphNameRef<'_>,
        union: bool,
        focus: Option<&oxigraph::model::Term>,
        sink: &mut dyn FnMut(Triple),
    ) -> Result<bool, Error> {
        let name = NamedNode::new_unchecked("urn:tripletrail:shacl:request");
        // A union of graphs is a default graph only in a copy, as for `validate_counted`.
        if union || (data.is_default_graph() && self.union_default_graph()) {
            let scope = if union {
                CopyScope::Named
            } else {
                CopyScope::All
            };
            return self
                .scratch_copy_as(&name, GraphNameRef::DefaultGraph, scope)?
                .validate_graph_report(shapes, GraphNameRef::DefaultGraph, false, focus, sink);
        }
        let engine = self
            .with_functions_from(shapes.iter().map(|t| t.into_owned()))
            .map_err(|e| Error::Request(e.to_string()))?;
        let mut writer = ReportWriter::new();
        engine.validate_on(&name, shapes, self, data, &[], false, focus, &mut |r| {
            writer.result(&r, sink)
        })?;
        Ok(writer.finish(sink))
    }

    /// Validates the default graph of the store against each of `shape_sets`, graphs given
    /// with the request as for [`Engine::validate_graph_report`], on one snapshot of the store,
    /// so that every set sees the same data, without holding the store's dictionary lock for
    /// the run: writes go on meanwhile, and the validation does not see them. Gives every result as it is found, with the index of
    /// its set; the sets go in order. With a union default graph, the data is copied once
    /// for all of them.
    pub fn validate_graphs_results(
        &self,
        shape_sets: &[&oxigraph::model::Graph],
        emit: &mut dyn FnMut(usize, ValidationResult<'_>),
    ) -> Result<(), Error> {
        let name = NamedNode::new_unchecked("urn:tripletrail:shacl:request");
        if self.union_default_graph() {
            return self
                .scratch_copy_as(&name, GraphNameRef::DefaultGraph, CopyScope::All)?
                .validate_graphs_results(shape_sets, emit);
        }
        let mut sets = Vec::with_capacity(shape_sets.len());
        for shapes in shape_sets {
            let engine = self
                .with_functions_from(shapes.iter().map(|t| t.into_owned()))
                .map_err(|e| Error::Request(e.to_string()))?;
            let compiled = Shapes::compile(*shapes as &dyn DataGraph).map_err(ShaclError::from)?;
            sets.push((*shapes, engine, compiled));
        }
        // One snapshot for every set, read without holding the store's dictionary lock for
        // the run (each lookup takes it on its own): a write with new terms goes on meanwhile.
        let data = StoreGraph::new(self.store(), GraphNameRef::DefaultGraph);
        let g: &dyn DataGraph = &data;
        for (i, (shapes, engine, compiled)) in sets.iter().enumerate() {
            let ev = Ev::new(engine);
            let inferred = if compiled.has_rules() {
                Inference::new(compiled, g)
                    .with_evaluator(&ev)
                    .with_shapes_graph(&name, *shapes as &dyn DataGraph)
                    .run()
                    .map_err(ShaclError::from)?
            } else {
                Inferred::new()
            };
            let with_inferred = Union(vec![g, &inferred]);
            let g: &dyn DataGraph = if inferred.is_empty() {
                g
            } else {
                &with_inferred
            };
            let v = Validator::new(compiled, g)
                .with_evaluator(&ev)
                .with_shapes_graph(&name, *shapes as &dyn DataGraph);
            v.report(&mut |r| emit(i, r));
            if let Some(e) = v.take_error() {
                return Err(ShaclError::from(e).into());
            }
        }
        Ok(())
    }

    /// Runs the SHACL-AF rules (`sh:TripleRule`, `sh:SPARQLRule`) of the shapes in the named
    /// graph `shapes` on graph `data` of the store, in `sh:order`, pass after pass until they
    /// infer nothing new (at most `max_passes`). Returns the inferred triples that are not in
    /// the data graph; with `into`, also writes them to that graph of the store.
    pub fn infer(
        &self,
        shapes: &NamedNode,
        data: GraphNameRef<'_>,
        into: Option<GraphNameRef<'_>>,
        max_passes: usize,
    ) -> Result<Inferred, Error> {
        self.infer_observed(shapes, data, into, max_passes, &|_| {})
    }

    /// As [`Engine::infer`], calling `observer` after each rule of each pass with what it did
    /// (focus nodes, triples, time), to report progress or find slow rules.
    pub fn infer_observed(
        &self,
        shapes: &NamedNode,
        data: GraphNameRef<'_>,
        into: Option<GraphNameRef<'_>>,
        max_passes: usize,
        observer: &dyn Fn(&tripletrail_shacl::RuleRun<'_>),
    ) -> Result<Inferred, Error> {
        let options = InferOptions {
            max_passes,
            ..InferOptions::default()
        };
        self.infer_with(shapes, data, into, &options, observer)
    }

    /// As [`Engine::infer_observed`], with [`InferOptions`].
    pub fn infer_with(
        &self,
        shapes: &NamedNode,
        data: GraphNameRef<'_>,
        into: Option<GraphNameRef<'_>>,
        options: &InferOptions,
        observer: &dyn Fn(&tripletrail_shacl::RuleRun<'_>),
    ) -> Result<Inferred, Error> {
        // The rules read the store through its graphs, not through queries: the union default
        // graph is all graphs only in a copy.
        if options.scratch || (data.is_default_graph() && self.union_default_graph()) {
            return self.infer_in_scratch(shapes, data, into, options, observer);
        }
        let shapes_graph = StoreGraph::new(self.store(), shapes.as_ref().into());
        let compiled = Shapes::compile(&shapes_graph).map_err(ShaclError::from)?;
        let in_store = options.in_store
            && data.is_default_graph()
            && matches!(self.store(), crate::Dataset::TripleTrail(_))
            && !self.union_default_graph();
        let engine = self.with_shape_functions(shapes)?;
        let mut ev = Ev::new(&engine);
        if in_store {
            ev.in_store = Some(std::sync::Mutex::new(0));
        }
        let run = |g: &dyn DataGraph| {
            Inference::new(&compiled, g)
                .with_evaluator(&ev)
                .with_shapes_graph(shapes, &shapes_graph)
                .with_max_passes(options.max_passes)
                .with_existing(options.include_existing)
                .with_semi_naive(options.semi_naive)
                .with_sequential(options.sequential)
                .with_observer(observer)
                .with_plan_from(options.plan.as_deref())
                .run()
        };
        let graph = StoreGraph::new(self.store(), data);
        // The inferred triples are written to the store as the rules run: the snapshot's
        // dictionary cannot stay locked meanwhile.
        let inferred = if in_store {
            run(&graph)
        } else {
            graph.read(run)
        }
        .map_err(ShaclError::from)?;
        if let Some(graph) = into {
            let graph = graph.into_owned();
            self.load_quads(inferred.iter().map(|t| t.clone().in_graph(graph.clone())))?;
        }
        Ok(inferred)
    }

    /// As [`Engine::infer_with`] without `into`, with where each inferred triple came from
    /// (see [`tripletrail_shacl::Inference::with_provenance`]), in the same order. With
    /// [`InferOptions::in_store`] or [`InferOptions::scratch`], the rules' queries run on the
    /// store (or a copy) as without provenance (#455).
    pub fn infer_traced(
        &self,
        shapes: &NamedNode,
        data: GraphNameRef<'_>,
        options: &InferOptions,
        observer: &dyn Fn(&tripletrail_shacl::RuleRun<'_>),
    ) -> Result<(Inferred, Vec<tripletrail_shacl::Origin>), Error> {
        if options.scratch || (data.is_default_graph() && self.union_default_graph()) {
            let scratch = self
                .scratch_copy(shapes, data)?
                .with_functions(self.functions().clone());
            let options = InferOptions {
                in_store: true,
                scratch: false,
                ..options.clone()
            };
            return scratch.infer_traced(shapes, GraphNameRef::DefaultGraph, &options, observer);
        }
        let shapes_graph = StoreGraph::new(self.store(), shapes.as_ref().into());
        let compiled = Shapes::compile(&shapes_graph).map_err(ShaclError::from)?;
        let in_store = options.in_store
            && data.is_default_graph()
            && matches!(self.store(), crate::Dataset::TripleTrail(_))
            && !self.union_default_graph();
        let engine = self.with_shape_functions(shapes)?;
        let mut ev = Ev::new(&engine);
        if in_store {
            ev.in_store = Some(std::sync::Mutex::new(0));
        }
        let run = |g: &dyn DataGraph| {
            Inference::new(&compiled, g)
                .with_evaluator(&ev)
                .with_shapes_graph(shapes, &shapes_graph)
                .with_max_passes(options.max_passes)
                .with_existing(options.include_existing)
                .with_semi_naive(options.semi_naive)
                .with_provenance(true)
                .with_sequential(options.sequential)
                .with_observer(observer)
                .with_plan_from(options.plan.as_deref())
                .run_traced()
        };
        let graph = StoreGraph::new(self.store(), data);
        // As in `infer_with`: the store is written while the rules run.
        Ok(if in_store {
            run(&graph)
        } else {
            graph.read(run)
        }
        .map_err(ShaclError::from)?)
    }

    /// [`InferOptions::scratch`]: the rules run on a copy of the data and shapes graphs.
    fn infer_in_scratch(
        &self,
        shapes: &NamedNode,
        data: GraphNameRef<'_>,
        into: Option<GraphNameRef<'_>>,
        options: &InferOptions,
        observer: &dyn Fn(&tripletrail_shacl::RuleRun<'_>),
    ) -> Result<Inferred, Error> {
        let scratch = self
            .scratch_copy(shapes, data)?
            .with_functions(self.functions().clone());
        let options = InferOptions {
            in_store: true,
            scratch: false,
            ..options.clone()
        };
        let inferred =
            scratch.infer_with(shapes, GraphNameRef::DefaultGraph, None, &options, observer)?;
        if let Some(graph) = into {
            let graph = graph.into_owned();
            self.load_quads(inferred.iter().map(|t| t.clone().in_graph(graph.clone())))?;
        }
        Ok(inferred)
    }

    /// This engine with the `sh:SPARQLFunction`s of the shapes graph `shapes` too.
    fn with_shape_functions(&self, shapes: &NamedNode) -> Result<Engine, Error> {
        let graph = GraphNameRef::NamedNode(shapes.as_ref());
        let mut triples = Vec::new();
        for quad in self
            .store()
            .quads_for_pattern(None, None, None, Some(graph))
        {
            triples.push(Triple::from(quad?));
        }
        self.with_functions_from(triples)
            .map_err(|e| Error::Request(e.to_string()))
    }

    /// A new in-memory tripletrail engine holding graph `data` of this one as its default graph
    /// (all graphs but the shapes graph, for the default graph of an engine with a union
    /// default graph), its named graphs as they are, so rules and shapes that read a `GRAPH`
    /// still find it, and the shapes graph `shapes`. It stops with this engine's cancellation.
    fn scratch_copy(&self, shapes: &NamedNode, data: GraphNameRef<'_>) -> Result<Engine, Error> {
        let scope = if data.is_default_graph() && self.union_default_graph() {
            CopyScope::All
        } else {
            CopyScope::None
        };
        self.scratch_copy_as(shapes, data, scope)
    }

    /// As [`Engine::scratch_copy`]; with a `union`, the copy's default graph is all graphs, or
    /// the named graphs only (but the shapes graph in either case), whether or not this engine
    /// reads its default graph so.
    fn scratch_copy_as(
        &self,
        shapes: &NamedNode,
        data: GraphNameRef<'_>,
        scope: CopyScope,
    ) -> Result<Engine, Error> {
        let union = scope != CopyScope::None;
        let named_only = scope == CopyScope::Named;
        use oxigraph::model::{GraphName, Quad};
        // The copy stops with this engine (a deadline on the step or request).
        let scratch = match &self.cancellation {
            Some(token) => Engine::new_tripletrail().with_cancellation_token(token.clone()),
            None => Engine::new_tripletrail(),
        };
        // On the tripletrail store, the default graph to run on is made by id in a fork, which
        // shares the store's quads: no quad is decoded or copied (#520). The named graphs are
        // the store's own, the shapes graph among them.
        if (data.is_default_graph() || union)
            && let crate::Dataset::TripleTrail(store) = self.store()
        {
            let fork = store.fork();
            if named_only {
                fork.clear_graph(GraphNameRef::DefaultGraph)
                    .map_err(crate::DatasetError::from)?;
            }
            if union {
                fork.union_into_default_graph(store.term_id(shapes.as_ref().into()))
                    .map_err(crate::DatasetError::from)?;
            }
            let scratch = Engine::from_store(fork);
            return Ok(match &self.cancellation {
                Some(token) => scratch.with_cancellation_token(token.clone()),
                None => scratch,
            });
        }
        let shapes_name = GraphName::NamedNode(shapes.clone());
        // (graph read, whether to keep its graph name instead of the default graph)
        let mut passes = vec![(if union { None } else { Some(data) }, false)];
        passes.push((None, true));
        for (from, keep) in passes {
            let mut error = None;
            let quads = self
                .store()
                .quads_for_pattern(None, None, None, from)
                .filter(|q| {
                    q.as_ref().map_or(true, |q| {
                        // The shapes are not data; `None` reads every graph, the default
                        // graph included, which the copy of named graphs leaves out.
                        q.graph_name != shapes_name
                            && !((keep || named_only) && q.graph_name.is_default_graph())
                    })
                })
                .map_while(|q| match q {
                    Ok(q) => {
                        let to = if keep {
                            q.graph_name
                        } else {
                            GraphName::DefaultGraph
                        };
                        Some(Quad::new(q.subject, q.predicate, q.object, to))
                    }
                    Err(e) => {
                        error = Some(e);
                        None
                    }
                });
            scratch.load_quads(quads)?;
            if let Some(e) = error {
                return Err(e.into());
            }
        }
        let mut error = None;
        let quads = self
            .store()
            .quads_for_pattern(None, None, None, Some(shapes.as_ref().into()))
            .map_while(|q| match q {
                Ok(q) => Some(q),
                Err(e) => {
                    error = Some(e);
                    None
                }
            });
        scratch.load_quads(quads)?;
        if let Some(e) = error {
            return Err(e.into());
        }
        Ok(scratch)
    }

    /// As [`Engine::validate_results`], written as a W3C validation report (triples given to
    /// `sink` as they are made). Returns whether the data conforms.
    pub fn validate(
        &self,
        shapes: &NamedNode,
        data: GraphNameRef<'_>,
        sink: &mut dyn FnMut(Triple),
    ) -> Result<bool, Error> {
        self.validate_report(shapes, data, false, sink)
    }

    /// As [`Engine::validate`]; with `statistics`, the report also says how many focus and
    /// value nodes each shape was checked against (see [`ReportWriter::statistics`]).
    pub fn validate_report(
        &self,
        shapes: &NamedNode,
        data: GraphNameRef<'_>,
        statistics: bool,
        sink: &mut dyn FnMut(Triple),
    ) -> Result<bool, Error> {
        self.validate_report_with(shapes, data, &[], statistics, sink)
    }

    /// As [`Engine::validate_report`], with lookup graphs (see
    /// [`Engine::validate_counted_with`]).
    pub fn validate_report_with(
        &self,
        shapes: &NamedNode,
        data: GraphNameRef<'_>,
        lookups: &[NamedNode],
        statistics: bool,
        sink: &mut dyn FnMut(Triple),
    ) -> Result<bool, Error> {
        let mut writer = ReportWriter::new();
        let counts = self.validate_counted_with(shapes, data, lookups, statistics, &mut |r| {
            writer.result(&r, sink)
        })?;
        if statistics {
            writer.statistics(&counts, sink);
        }
        Ok(writer.finish(sink))
    }
}

/// Which graphs a scratch copy's default graph is made of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyScope {
    /// The graph asked for only.
    None,
    /// Every graph, the default graph included (an engine's union default graph).
    All,
    /// The named graphs only (Jena's union graph, `urn:x-arq:UnionGraph`).
    Named,
}
