//! [`Dataset`]: either store behind one interface, so the engine, routing, spatial index,
//! server and Python binding work on both while the tripletrail store replaces Oxigraph's.

use crate::{BlankNodes, Changes, Store, StoreError, UpdateError};
use oxigraph::io::RdfParser;
use oxigraph::model::{
    GraphNameRef, NamedNodeRef, NamedOrBlankNode, NamedOrBlankNodeRef, Quad, QuadRef, TermRef,
};
use oxigraph::sparql::{
    PreparedSparqlQuery, QueryEvaluationError, QueryExplanation, QueryResults, SparqlEvaluator,
    UpdateEvaluationError,
};
use oxigraph::store::{LoaderError, StorageError};
use std::io::Read;

/// What a caller under row rules sees of the tripletrail store (#202): the rows `rows` leaves
/// of `base`, the snapshot they were judged on. The built-in services of such a caller's query
/// read through it, as the query does.
#[derive(Clone)]
pub struct RowView {
    pub rows: std::sync::Arc<crate::RowFilter>,
    pub base: crate::Snapshot,
}

impl RowView {
    /// A key for what the view hides, for caches of what is read through it.
    pub fn fingerprint(&self) -> u64 {
        self.rows.fingerprint()
    }
}

/// The store a dataset lives in. Cheap to clone; clones share the data.
#[derive(Clone)]
pub enum Dataset {
    /// Oxigraph's store (in memory, or RocksDB on disk).
    Oxigraph(oxigraph::store::Store),
    /// The tripletrail store.
    TripleTrail(Store),
}

#[derive(Debug, thiserror::Error)]
pub enum DatasetError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Loader(#[from] LoaderError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Update(#[from] UpdateEvaluationError),
    #[error(transparent)]
    StoreUpdate(#[from] UpdateError),
}

pub type QuadIter = Box<dyn Iterator<Item = Result<Quad, DatasetError>> + Send>;

impl From<oxigraph::store::Store> for Dataset {
    fn from(s: oxigraph::store::Store) -> Self {
        Self::Oxigraph(s)
    }
}

impl From<Store> for Dataset {
    fn from(s: Store) -> Self {
        Self::TripleTrail(s)
    }
}

impl Dataset {
    /// The name of the backend, for logs and statistics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Oxigraph(_) => "oxigraph",
            Self::TripleTrail(_) => "tripletrail",
        }
    }

    /// Where the check of the store's files stands ([`Store::verification`]); `None` for
    /// Oxigraph's store, which does not check its files this way.
    pub fn verification(&self) -> Option<crate::Verification> {
        match self {
            Self::Oxigraph(_) => None,
            Self::TripleTrail(s) => Some(s.verification()),
        }
    }

    pub fn execute(
        &self,
        mut query: PreparedSparqlQuery,
    ) -> Result<QueryResults<'static>, QueryEvaluationError> {
        match self {
            Self::Oxigraph(s) => query.on_store(s).execute(),
            Self::TripleTrail(s) => {
                let snapshot = snapshot(s, &mut query);
                with_estimates(&snapshot, || {
                    query.on_queryable_dataset(snapshot.clone()).execute()
                })
            }
        }
    }

    /// [`Dataset::execute`] on `base`, the view the rewrites read: the terms they made up for
    /// it (ephemeral ids in the tables they registered, #11) are known only to it and its
    /// clones, not to a later view of the store.
    pub fn execute_on(
        &self,
        mut query: PreparedSparqlQuery,
        base: Option<crate::Snapshot>,
    ) -> Result<QueryResults<'static>, QueryEvaluationError> {
        match (self, base) {
            (Self::TripleTrail(_), Some(base)) => {
                let snapshot = merged_view(base.with_pending_tables(), query.dataset_mut());
                with_estimates(&snapshot, || {
                    query.on_queryable_dataset(snapshot.clone()).execute()
                })
            }
            _ => self.execute(query),
        }
    }

    /// [`Dataset::execute`] on `base` without the quads `rows` hides (#202). Only the tripletrail
    /// store has row filters: on Oxigraph's store a filter is an error, never ignored.
    pub fn execute_with_rows(
        &self,
        mut query: PreparedSparqlQuery,
        rows: std::sync::Arc<crate::RowFilter>,
        base: crate::Snapshot,
    ) -> Result<QueryResults<'static>, QueryEvaluationError> {
        match self {
            Self::Oxigraph(_) => Err(QueryEvaluationError::Unexpected(
                "Row-level access rules need the tripletrail store".into(),
            )),
            Self::TripleTrail(_) => {
                // The rows were judged on `base`, taken before the rewrites registered their
                // tables (the matches of `tt:search`, #203).
                let snapshot = merged_view(base.with_pending_tables(), query.dataset_mut())
                    .with_rows(Some(rows));
                with_estimates(&snapshot, || {
                    query.on_queryable_dataset(snapshot.clone()).execute()
                })
            }
        }
    }

    /// Evaluates with statistics and returns the explanation too.
    /// On `base` when given, as [`Dataset::execute_on`].
    pub fn explain(
        &self,
        mut query: PreparedSparqlQuery,
        base: Option<crate::Snapshot>,
    ) -> (
        Result<QueryResults<'static>, QueryEvaluationError>,
        QueryExplanation,
    ) {
        match self {
            Self::Oxigraph(s) => query.on_store(s).compute_statistics().explain(),
            Self::TripleTrail(s) => {
                let snapshot = match base {
                    Some(base) => merged_view(base.with_pending_tables(), query.dataset_mut()),
                    None => snapshot(s, &mut query),
                };
                with_estimates(&snapshot, || {
                    query
                        .on_queryable_dataset(snapshot.clone())
                        .compute_statistics()
                        .explain()
                })
            }
        }
    }

    /// The geometry of a geometry literal, if the store keeps it parsed (the tripletrail store).
    pub fn geometry(&self, term: TermRef<'_>) -> Option<std::sync::Arc<tripletrail_geo::Geom>> {
        match self {
            Self::Oxigraph(_) => None,
            Self::TripleTrail(s) => s.geometry(term),
        }
    }

    /// The distinct geometry literals (`geo:wktLiteral`, `geo:geoJSONLiteral`, GML, KML, H3)
    /// that are the object of a quad whose predicate `wanted` accepts, in any graph. The
    /// tripletrail store keeps geometry literals as their own kind of term, so it reads ids and
    /// decodes only the geometries; Oxigraph's store is scanned. With each, the geometry the
    /// store keeps parsed (the tripletrail store), found by its id.
    #[allow(clippy::type_complexity)]
    pub fn geometry_literals(
        &self,
        wanted: &dyn Fn(&str) -> bool,
    ) -> Result<
        Vec<(
            oxigraph::model::Literal,
            Option<std::sync::Arc<tripletrail_geo::Geom>>,
        )>,
        DatasetError,
    > {
        use oxigraph::model::Term;
        use std::collections::{HashMap, HashSet};
        let mut out = Vec::new();
        match self {
            Self::Oxigraph(s) => {
                let mut seen = HashSet::new();
                for quad in s.quads_for_pattern(None, None, None, None) {
                    let quad = quad?;
                    if let Term::Literal(l) = quad.object
                        && crate::dict::is_geometry_datatype(l.datatype().as_str())
                        && wanted(quad.predicate.as_str())
                        && seen.insert(l.clone())
                    {
                        out.push((l, None));
                    }
                }
            }
            Self::TripleTrail(s) => {
                let snapshot = s.snapshot();
                let mut predicates: HashMap<crate::TermId, bool> = HashMap::new();
                let mut seen = HashSet::new();
                for [_, p, o, _] in
                    snapshot.quad_ids_for_pattern(None, None, None, crate::GraphSelector::All)
                {
                    if o.kind() != crate::Kind::Geometry || seen.contains(&o) {
                        continue;
                    }
                    let keep = match predicates.get(&p) {
                        Some(k) => *k,
                        None => {
                            let k = match snapshot.term(p)? {
                                Term::NamedNode(n) => wanted(n.as_str()),
                                _ => false,
                            };
                            predicates.insert(p, k);
                            k
                        }
                    };
                    if keep {
                        seen.insert(o);
                        if let Term::Literal(l) = snapshot.term(o)? {
                            out.push((l, snapshot.geometry(o)));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// The ids of the distinct geometry literals that are the object of a quad whose predicate
    /// `wanted` accepts, on the tripletrail store (`None` on Oxigraph's, which has no ids).
    pub fn geometry_ids(
        &self,
        wanted: &dyn Fn(&str) -> bool,
    ) -> Result<Option<Vec<crate::TermId>>, DatasetError> {
        use oxigraph::model::Term;
        use std::collections::{HashMap, HashSet};
        let Self::TripleTrail(s) = self else {
            return Ok(None);
        };
        let snapshot = s.snapshot();
        let mut predicates: HashMap<crate::TermId, bool> = HashMap::new();
        let mut seen = HashSet::new();
        for [_, p, o, _] in
            snapshot.quad_ids_for_pattern(None, None, None, crate::GraphSelector::All)
        {
            if o.kind() != crate::Kind::Geometry || seen.contains(&o) {
                continue;
            }
            let keep = match predicates.get(&p) {
                Some(k) => *k,
                None => {
                    let k = matches!(snapshot.term(p)?, Term::NamedNode(n) if wanted(n.as_str()));
                    predicates.insert(p, k);
                    k
                }
            };
            if keep {
                seen.insert(o);
            }
        }
        let mut ids: Vec<_> = seen.into_iter().collect();
        ids.sort_unstable();
        Ok(Some(ids))
    }

    /// How many objects of quads with `predicate` (in any graph) are not geometry literals.
    /// The tripletrail store counts them from the ids, without decoding any term.
    pub fn non_geometry_objects(&self, predicate: NamedNodeRef<'_>) -> Result<usize, DatasetError> {
        match self {
            Self::Oxigraph(s) => {
                let mut n = 0;
                for quad in s.quads_for_pattern(None, Some(predicate), None, None) {
                    if !matches!(&quad?.object, oxigraph::model::Term::Literal(l)
                        if crate::dict::is_geometry_datatype(l.datatype().as_str()))
                    {
                        n += 1;
                    }
                }
                Ok(n)
            }
            Self::TripleTrail(s) => {
                let snapshot = s.snapshot();
                let Some(p) = snapshot.term_id(predicate.into()) else {
                    return Ok(0);
                };
                Ok(snapshot
                    .quad_ids_for_pattern(None, Some(p), None, crate::GraphSelector::All)
                    .filter(|[_, _, o, _]| o.kind() != crate::Kind::Geometry)
                    .count())
            }
        }
    }

    /// Runs a SPARQL update; `evaluator` makes the evaluator (with custom functions). Returns
    /// what the update changed when the store reports it (the tripletrail store does).
    pub fn update(
        &self,
        update: spargebra::Update,
        evaluator: impl Fn() -> SparqlEvaluator + Sync,
    ) -> Result<Option<Changes>, DatasetError> {
        self.update_with(update, evaluator, false, None)
    }

    /// As [`Dataset::update`]; with `union_default_graph`, the `WHERE` patterns of operations
    /// without `USING` read the union of all graphs as their default graph.
    pub fn update_with(
        &self,
        update: spargebra::Update,
        evaluator: impl Fn() -> SparqlEvaluator + Sync,
        union_default_graph: bool,
        prepare: Option<&crate::PrepareWhere<'_>>,
    ) -> Result<Option<Changes>, DatasetError> {
        match self {
            Self::Oxigraph(s) => {
                let mut prepared = evaluator().for_update(update);
                for d in prepared.using_datasets_mut() {
                    if union_default_graph && d.is_default_dataset() {
                        d.set_default_graph_as_union();
                    }
                    crate::read_stored_default_graph(d);
                }
                prepared.on_store(s).execute()?;
                Ok(None)
            }
            Self::TripleTrail(s) => Ok(Some(s.update_prepared(
                &update,
                evaluator,
                union_default_graph,
                prepare,
            )?)),
        }
    }

    /// [`Dataset::execute`], or under `view` when given: the query then sees only the rows
    /// the view's filter leaves (the built-in services of a query under row rules, #202).
    pub fn execute_in(
        &self,
        query: PreparedSparqlQuery,
        view: Option<&RowView>,
    ) -> Result<QueryResults<'static>, QueryEvaluationError> {
        match view {
            Some(v) => {
                self.execute_with_rows(query, std::sync::Arc::clone(&v.rows), v.base.clone())
            }
            None => self.execute(query),
        }
    }

    /// [`Dataset::quads_for_pattern`], or on `view` when given: only the quads its filter
    /// leaves.
    pub fn quads_in(
        &self,
        subject: Option<NamedOrBlankNodeRef<'_>>,
        predicate: Option<NamedNodeRef<'_>>,
        object: Option<TermRef<'_>>,
        graph_name: Option<GraphNameRef<'_>>,
        view: Option<&RowView>,
    ) -> QuadIter {
        match view {
            Some(v) => Box::new(
                v.base
                    .clone()
                    .with_rows(Some(std::sync::Arc::clone(&v.rows)))
                    .quads_for_pattern(subject, predicate, object, graph_name)
                    .map(Ok),
            ),
            None => self.quads_for_pattern(subject, predicate, object, graph_name),
        }
    }

    /// Quads matching a pattern; `graph_name: None` is every graph.
    pub fn quads_for_pattern(
        &self,
        subject: Option<NamedOrBlankNodeRef<'_>>,
        predicate: Option<NamedNodeRef<'_>>,
        object: Option<TermRef<'_>>,
        graph_name: Option<GraphNameRef<'_>>,
    ) -> QuadIter {
        match self {
            Self::Oxigraph(s) => Box::new(
                s.quads_for_pattern(subject, predicate, object, graph_name)
                    .map(|q| q.map_err(Into::into)),
            ),
            Self::TripleTrail(s) => Box::new(
                s.quads_for_pattern(subject, predicate, object, graph_name)
                    .map(Ok),
            ),
        }
    }

    /// How many quads match a pattern (`graph_name: None` is every graph), for planning. The
    /// tripletrail store answers from its block index without reading the quads; on Oxigraph's store
    /// the quads are counted, up to `cap`, so the answer is at most `cap` there.
    pub fn estimate_len(
        &self,
        subject: Option<NamedOrBlankNodeRef<'_>>,
        predicate: Option<NamedNodeRef<'_>>,
        object: Option<TermRef<'_>>,
        graph_name: Option<GraphNameRef<'_>>,
        cap: usize,
    ) -> Result<usize, DatasetError> {
        match self {
            Self::Oxigraph(s) => {
                let mut n = 0;
                for q in s
                    .quads_for_pattern(subject, predicate, object, graph_name)
                    .take(cap)
                {
                    q?;
                    n += 1;
                }
                Ok(n)
            }
            Self::TripleTrail(s) => Ok(s.estimate_len(subject, predicate, object, graph_name)),
        }
    }

    pub fn iter(&self) -> QuadIter {
        self.quads_for_pattern(None, None, None, None)
    }

    pub fn len(&self) -> Result<usize, DatasetError> {
        match self {
            Self::Oxigraph(s) => Ok(s.len()?),
            Self::TripleTrail(s) => Ok(s.len()),
        }
    }

    pub fn is_empty(&self) -> Result<bool, DatasetError> {
        match self {
            Self::Oxigraph(s) => Ok(s.is_empty()?),
            Self::TripleTrail(s) => Ok(s.is_empty()),
        }
    }

    pub fn insert<'a>(&self, quad: impl Into<QuadRef<'a>>) -> Result<(), DatasetError> {
        match self {
            Self::Oxigraph(s) => Ok(s.insert(quad)?),
            Self::TripleTrail(s) => {
                s.insert(quad)?;
                Ok(())
            }
        }
    }

    /// As [`Dataset::insert`], also saying whether the quad was new: `None` on Oxigraph's
    /// store, which doesn't say.
    pub fn insert_reporting<'a>(
        &self,
        quad: impl Into<QuadRef<'a>>,
    ) -> Result<Option<bool>, DatasetError> {
        match self {
            Self::Oxigraph(s) => {
                s.insert(quad)?;
                Ok(None)
            }
            Self::TripleTrail(s) => Ok(Some(s.insert(quad)?)),
        }
    }

    /// As [`Dataset::remove`], also saying whether the quad was there: `None` on Oxigraph's
    /// store, which doesn't say.
    pub fn remove_reporting<'a>(
        &self,
        quad: impl Into<QuadRef<'a>>,
    ) -> Result<Option<bool>, DatasetError> {
        match self {
            Self::Oxigraph(s) => {
                s.remove(quad)?;
                Ok(None)
            }
            Self::TripleTrail(s) => Ok(Some(s.remove(quad)?)),
        }
    }

    pub fn remove<'a>(&self, quad: impl Into<QuadRef<'a>>) -> Result<(), DatasetError> {
        match self {
            Self::Oxigraph(s) => Ok(s.remove(quad)?),
            Self::TripleTrail(s) => {
                s.remove(quad)?;
                Ok(())
            }
        }
    }

    /// Whether a named graph exists. In the tripletrail store a graph exists while it has quads.
    pub fn contains_named_graph<'a>(
        &self,
        graph: impl Into<NamedOrBlankNodeRef<'a>>,
    ) -> Result<bool, DatasetError> {
        let graph = graph.into();
        match self {
            Self::Oxigraph(s) => Ok(s.contains_named_graph(graph)?),
            Self::TripleTrail(s) => {
                let g = match graph {
                    NamedOrBlankNodeRef::NamedNode(n) => GraphNameRef::NamedNode(n),
                    NamedOrBlankNodeRef::BlankNode(b) => GraphNameRef::BlankNode(b),
                };
                Ok(s.quads_for_pattern(None, None, None, Some(g))
                    .next()
                    .is_some())
            }
        }
    }

    pub fn named_graphs(&self) -> Result<Vec<NamedOrBlankNode>, DatasetError> {
        match self {
            Self::Oxigraph(s) => Ok(s.named_graphs().collect::<Result<_, _>>()?),
            Self::TripleTrail(s) => Ok(s.named_graphs()),
        }
    }

    pub fn clear(&self) -> Result<(), DatasetError> {
        match self {
            Self::Oxigraph(s) => Ok(s.clear()?),
            Self::TripleTrail(s) => Ok(s.clear()?),
        }
    }

    pub fn clear_graph<'a>(&self, graph: impl Into<GraphNameRef<'a>>) -> Result<(), DatasetError> {
        match self {
            Self::Oxigraph(s) => Ok(s.clear_graph(graph)?),
            Self::TripleTrail(s) => Ok(s.clear_graph(graph)?),
        }
    }

    /// Declares an empty named graph. In the tripletrail store a graph exists while it has quads,
    /// so this does nothing there.
    pub fn insert_named_graph<'a>(
        &self,
        graph: impl Into<NamedOrBlankNodeRef<'a>>,
    ) -> Result<(), DatasetError> {
        match self {
            Self::Oxigraph(s) => {
                s.insert_named_graph(graph)?;
                Ok(())
            }
            Self::TripleTrail(_) => Ok(()),
        }
    }

    /// Removes a named graph and its quads.
    pub fn remove_named_graph<'a>(
        &self,
        graph: impl Into<NamedOrBlankNodeRef<'a>>,
    ) -> Result<(), DatasetError> {
        let graph = graph.into();
        match self {
            Self::Oxigraph(s) => {
                s.remove_named_graph(graph)?;
                Ok(())
            }
            Self::TripleTrail(s) => Ok(s.clear_graph(match graph {
                NamedOrBlankNodeRef::NamedNode(n) => GraphNameRef::NamedNode(n),
                NamedOrBlankNodeRef::BlankNode(b) => GraphNameRef::BlankNode(b),
            })?),
        }
    }

    /// Bulk-loads a document. Its blank nodes are its own (#840): `_:b1` in two loads is two
    /// blank nodes, as with Jena's loaders.
    pub fn load(
        &self,
        parser: impl Into<RdfParser>,
        reader: impl Read,
    ) -> Result<(), DatasetError> {
        self.load_with_blank_nodes(parser, reader, BlankNodes::PerLoad)
    }

    /// [`Dataset::load`], keeping the document's blank node labels or not.
    pub fn load_with_blank_nodes(
        &self,
        parser: impl Into<RdfParser>,
        reader: impl Read,
        blank_nodes: BlankNodes,
    ) -> Result<(), DatasetError> {
        match self {
            Self::Oxigraph(s) => {
                let mut parser = parser.into();
                if blank_nodes == BlankNodes::PerLoad {
                    // Oxigraph's loader parses a reader in one piece, so one rename map.
                    parser = parser.rename_blank_nodes();
                }
                let mut loader = s.bulk_loader();
                let result = loader.load_from_reader(parser, reader);
                loader.commit()?;
                Ok(result?)
            }
            Self::TripleTrail(s) => {
                s.bulk_load_with_blank_nodes(parser.into(), reader, blank_nodes)?;
                Ok(())
            }
        }
    }

    /// Bulk-loads quads.
    pub fn load_quads(&self, quads: impl IntoIterator<Item = Quad>) -> Result<(), DatasetError> {
        match self {
            Self::Oxigraph(s) => {
                let mut loader = s.bulk_loader();
                loader.load_quads(quads)?;
                Ok(loader.commit()?)
            }
            Self::TripleTrail(s) => Ok(s.bulk_load_quads(quads)?),
        }
    }

    /// Bulk-loads quads given a batch at a time (the tripletrail store interns each batch's terms
    /// in parallel).
    pub fn load_quad_batches(
        &self,
        batches: impl IntoIterator<Item = Vec<Quad>>,
    ) -> Result<(), DatasetError> {
        match self {
            Self::Oxigraph(_) => self.load_quads(batches.into_iter().flatten()),
            Self::TripleTrail(s) => {
                s.bulk_load_quad_batches(batches)?;
                Ok(())
            }
        }
    }
}

/// Runs `f` (which plans a query on `snapshot`) with join-order estimates from the data.
pub(crate) fn with_estimates<R>(snapshot: &crate::Snapshot, f: impl FnOnce() -> R) -> R {
    sparopt::estimate::with_estimator(
        std::rc::Rc::new(crate::estimate::DataEstimator::new(snapshot.clone())),
        f,
    )
}

/// The snapshot a query reads, for its dataset (see [`merged_view`]).
fn snapshot(store: &Store, query: &mut PreparedSparqlQuery) -> crate::Snapshot {
    merged_view(store.snapshot(), query.dataset_mut())
}

/// `view` for a query on `dataset`. A default graph that merges several graphs (the union of
/// all graphs, or `FROM` with several graphs) is read by the store, each triple once (issue
/// #376): the evaluator would ask each graph and see a triple once per graph that holds it.
/// `dataset` then names the store's default graph, which the view reads as that merge.
pub(crate) fn merged_view(
    view: crate::Snapshot,
    dataset: &mut oxigraph::sparql::QueryDataset,
) -> crate::Snapshot {
    use crate::MergedDefault;
    use oxigraph::model::GraphName;
    let merged = match dataset.default_graph_graphs() {
        None => MergedDefault::All,
        Some(graphs) if graphs.len() > 1 => {
            let mut ids: Vec<crate::TermId> = graphs
                .iter()
                .filter_map(|g| match g {
                    GraphName::DefaultGraph => Some(crate::TermId::DEFAULT_GRAPH),
                    GraphName::NamedNode(n) => view.term_id(n.as_ref().into()),
                    GraphName::BlankNode(b) => view.term_id(b.as_ref().into()),
                })
                .collect();
            ids.sort_unstable();
            ids.dedup();
            // Graphs that hold every quad of the store (a graph grant that covers it all, issue
            // #204) merge to the union: one scan instead of one per graph.
            if view.graphs_within(&ids) {
                MergedDefault::All
            } else {
                MergedDefault::Graphs(ids.into())
            }
        }
        Some(_) => return view,
    };
    dataset.set_default_graph(vec![GraphName::DefaultGraph]);
    // "Any graph" still reads the default graph too, as the union did (`GRAPH ?g` drops it).
    let view = if dataset.available_named_graphs().is_none() && matches!(merged, MergedDefault::All)
    {
        view.with_all_graphs()
    } else {
        view
    };
    view.with_merged_default(merged)
}
