//! An RDF store wired with GeoSPARQL/PostGIS functions and pgRouting-style routing.
//!
//! Storage is the tripletrail store by default, or Oxigraph's store; queries run on Oxigraph's
//! evaluator, and spatial and routing features plug in through its custom function and SERVICE
//! hooks. Before evaluation, spatial FILTERs against a
//! constant geometry are rewritten to use a spatial index.

mod aggregate;
mod bind;
pub use bind::{ParallelBindStats, parallel_bind_stats};
mod cache;
pub mod canon;
pub mod complexity;
mod cosine_order;
mod direct;
pub use cache::{CacheStatus, ResultCacheStats};
pub use direct::IdSolutions;
#[doc(hidden)]
pub use direct::with_answer_cells;
pub use hints::{Hints, without_defines};
pub mod bind_join;
mod count;
mod equality;
#[cfg(feature = "geoparquet")]
pub mod geoparquet;
pub mod hints;
mod hnsw;
mod idstring;
mod idtree;
mod idvalue;
mod measures;
pub use measures::warmed as measures_warmed;
pub mod path;
mod plan;
mod profile;
pub mod propfn;
mod range;
#[cfg(feature = "remote-service")]
pub mod remote;
mod row_rules;
mod rows;
pub mod shapes;
mod st_service;
pub mod statagg;
mod statistics;
pub mod temporal;
pub mod text;
mod topk;
pub mod ttfn;
pub mod udf;
mod validate;
pub mod vector;
pub mod window;
pub mod xfn;
mod zero_length;
pub use profile::{
    ClassProfile, DatasetProfile, GeometryPath, MAX_LINKS_PER_NODE, ProfileOptions, PropertyProfile,
};
pub use statistics::{DatasetStatistics, GraphStatistics, PropertyStatistics};
pub use validate::InferOptions;

pub use path::{PATH_SERVICE, PathError, PathService};
pub use st_service::{ST_SERVICE, StError, StService};
pub use window::{WINDOW_SERVICE, WindowError, WindowService};

use oxigraph::io::RdfParser;
use oxigraph::model::{
    GraphName, GraphNameRef, NamedNode, NamedOrBlankNode, NamedOrBlankNodeRef, Quad,
};
use oxigraph::model::{NamedNodeRef, Term};
use oxigraph::sparql::{
    CancellationToken, QueryDataset, QueryResults, QuerySolutionIter, SparqlEvaluator, Variable,
};
use oxigraph::store::Store;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use tripletrail_geo::sync::LockUnpoisoned;
use tripletrail_pgr::{NetworkCache, PGR_SERVICE, PgrService};
use tripletrail_spatial::{
    IndexedPredicates, RewriteStats, SpatialContext, SpatialIndex, rewrite_query,
};
use tripletrail_store::{Dataset, DatasetError, GraphSelector, Snapshot};

const AS_WKT: NamedNodeRef<'static> =
    NamedNodeRef::new_unchecked("http://www.opengis.net/ont/geosparql#asWKT");
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub use tripletrail_fx as fx;
use tripletrail_fx::{FxConfig, FxService};
pub use tripletrail_geo as geo;
pub use tripletrail_pgr as pgr;
pub use tripletrail_shacl as shacl;
pub use tripletrail_spatial as spatial;
pub use tripletrail_store as store;
pub use tripletrail_store::GraphChanges;

/// Sets, for the whole process, the memory in bytes that one query's intermediate rows may
/// take (#479, #476): the joins on term ids ([`store::set_join_memory`]; past it the query is
/// evaluated row by row) and the rows each hash join of the evaluator holds (past it the
/// query fails). Without a call, or with 0, id joins take up to 1 GiB and the evaluator has
/// no limit.
pub fn set_query_memory(bytes: usize) {
    // 0: back to no limit, the id joins' default.
    store::set_join_memory(bytes);
    spareval::set_build_memory(bytes);
}

/// What [`Engine::write_quads`] empties before it writes.
#[derive(Clone, Copy, Debug)]
pub enum Replace<'a> {
    Nothing,
    /// One graph.
    Graph(GraphNameRef<'a>),
    /// Every graph.
    All,
}

/// Sets, for the whole process, whether a hash join without keys, or whose keys take few
/// distinct values, looks its basic graph pattern side up once per row of the other side
/// when every such row binds a variable of it, such as an IRI made by `BIND` (#494): a
/// safety net for plans that would otherwise build a product. Off by default.
pub fn set_lookup_joins(on: bool) {
    spareval::set_lookup_joins(on);
}
pub use oxigraph;
pub use row_rules::{
    ANONYMOUS, NEEDS_ENTERPRISE, REQUESTING_AGENT, ResourceExplained, RowCompiler, RowRule,
    RowRules, RowShapes, check_row_shapes, depends_on_agent, install_row_compiler,
    row_policies_available,
};
pub use tripletrail_store::{BlankNodes, RowFilter};

/// A query parsed once by [`Engine::prepare_query`], run by [`Engine::query_prepared`].
#[derive(Clone, Debug)]
pub struct PreparedQuery {
    query: spargebra::Query,
    hints: Hints,
}

impl PreparedQuery {
    /// The variables the query selects: those [`Engine::query_prepared`] can bind.
    pub fn variables(&self) -> Vec<Variable> {
        use spargebra::algebra::GraphPattern as P;
        let spargebra::Query::Select { pattern, .. } = &self.query else {
            return Vec::new();
        };
        let mut pattern = pattern;
        loop {
            match pattern {
                P::Slice { inner, .. } | P::Distinct { inner } | P::Reduced { inner } => {
                    pattern = inner
                }
                P::Project { variables, .. } => return variables.clone(),
                _ => return Vec::new(),
            }
        }
    }
}

/// Options of [`Engine::query_with_options`].
#[derive(Clone, Debug, Default)]
pub struct QueryOptions {
    /// The dataset to evaluate on, replacing the query's own `FROM` and `FROM NAMED`.
    pub dataset: Option<QueryDataset>,
    /// Variables bound to these values before evaluation.
    pub substitutions: Vec<(Variable, Term)>,
    /// The base IRI relative IRIs in the query resolve against (the query's own `BASE` still
    /// wins).
    pub base_iri: Option<String>,
    /// Only these graphs may be read (graph access, #201); `None` reads every graph. See
    /// [`ReadableGraphs`].
    pub readable: Option<ReadableGraphs>,
    /// What the query may not see of the data (row-level access rules, #202): every quad
    /// scan leaves the quads it hides out. The query is then evaluated without the rewrites
    /// and shortcuts that read the store's indexes or counts directly (the spatial rewrite
    /// reads a view of its index and evaluates its own queries on the filtered view), and
    /// without the result cache; a `SERVICE` is refused. Only on the tripletrail store: on
    /// Oxigraph's store the query is refused, never run without the filter.
    pub rows: Option<Arc<RowFilter>>,
    /// Row rules to compile and apply (#202): as `rows`, with the filter compiled from the
    /// same snapshot of the store the query reads, so a write in between is never shown
    /// unjudged.
    pub row_rules: Option<RowRules>,
}

/// The graphs a caller may read (graph access, #201). A query then sees, without `FROM`, the
/// union of these graphs as its default graph and the named ones as its named graphs; `FROM`,
/// `FROM NAMED` and [`QueryOptions::dataset`] only narrow that. Other graphs look empty. A
/// `SERVICE` is refused, since a service reads past the dataset. The spatial rewrite reads a
/// view of the spatial index holding only the geometries of these graphs (#203), made on
/// first use and kept until the next write; the rewrites that read the whole store (counts
/// from statistics, the range index, nearest neighbours) are left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadableGraphs {
    /// Whether the store's default graph is readable.
    pub default_graph: bool,
    /// The readable named graphs.
    pub named: Vec<NamedNode>,
}

impl ReadableGraphs {
    /// Whether `graph` may be read.
    pub fn contains(&self, graph: GraphNameRef<'_>) -> bool {
        match graph {
            GraphNameRef::DefaultGraph => self.default_graph,
            GraphNameRef::NamedNode(n) => self.named.iter().any(|m| m.as_ref() == n),
            GraphNameRef::BlankNode(_) => false,
        }
    }

    /// A key for what these graphs let a caller see. Anything cached from a read under
    /// access limits (a document, a tile, statistics) takes it as part of its key (#203), so
    /// callers with other grants never share an entry. The same graphs in another order give
    /// the same fingerprint.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut names: Vec<&str> = self.named.iter().map(NamedNode::as_str).collect();
        names.sort_unstable();
        names.dedup();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.default_graph.hash(&mut hasher);
        names.hash(&mut hasher);
        hasher.finish()
    }

    /// The dataset a query is evaluated on: `dataset` (the protocol's, else the query's own
    /// `FROM` and `FROM NAMED`) narrowed to these graphs.
    fn narrow(
        &self,
        query: &spargebra::Query,
        dataset: Option<&QueryDataset>,
    ) -> Result<QueryDataset, Error> {
        let pattern = match query {
            spargebra::Query::Select { pattern, .. }
            | spargebra::Query::Construct { pattern, .. }
            | spargebra::Query::Describe { pattern, .. }
            | spargebra::Query::Ask { pattern, .. } => pattern,
        };
        scoped_services_only(pattern, &query.to_string(), SOME_GRAPHS)?;
        let own = match query {
            spargebra::Query::Select { dataset, .. }
            | spargebra::Query::Construct { dataset, .. }
            | spargebra::Query::Describe { dataset, .. }
            | spargebra::Query::Ask { dataset, .. } => dataset.as_ref(),
        };
        let (default, named): (Option<Vec<GraphName>>, Option<Vec<NamedOrBlankNode>>) =
            match (dataset, own) {
                (Some(d), _) => (
                    d.default_graph_graphs().map(<[_]>::to_vec),
                    d.available_named_graphs().map(<[_]>::to_vec),
                ),
                (None, Some(own)) => (
                    Some(own.default.iter().cloned().map(GraphName::from).collect()),
                    // FROM without FROM NAMED: no named graphs.
                    Some(
                        own.named
                            .iter()
                            .flatten()
                            .cloned()
                            .map(NamedOrBlankNode::from)
                            .collect(),
                    ),
                ),
                (None, None) => (None, None),
            };
        let all = || {
            self.default_graph
                .then_some(GraphName::DefaultGraph)
                .into_iter()
                .chain(self.named.iter().cloned().map(GraphName::from))
                .collect::<Vec<_>>()
        };
        let default = match default {
            Some(graphs) => graphs
                .into_iter()
                .filter(|g| self.contains(g.as_ref()))
                .collect(),
            None => all(),
        };
        let named = match named {
            Some(graphs) => graphs
                .into_iter()
                .filter(|g| match g {
                    NamedOrBlankNode::NamedNode(n) => self.contains(n.as_ref().into()),
                    NamedOrBlankNode::BlankNode(_) => false,
                })
                .collect(),
            None => self.named.iter().cloned().map(Into::into).collect(),
        };
        let mut narrowed = QueryDataset::new();
        narrowed.set_default_graph(default);
        narrowed.set_available_named_graphs(named);
        Ok(narrowed)
    }
}

/// The services a caller limited to some graphs may call: their sub-selects, and the edges
/// path search reads, then come from the caller's dataset (#203).
const SCOPED_SERVICES: [&str; 4] = [ST_SERVICE, WINDOW_SERVICE, PATH_SERVICE, PGR_SERVICE];

/// How [`scoped_services_only`] names the limit of a caller who reads only some graphs.
const SOME_GRAPHS: &str = "with access to only some graphs";

/// Refuses a `SERVICE` other than [`SCOPED_SERVICES`], and one of them called inside another
/// (the inner one would read the store). `text` is the query's text: every `SERVICE` in it
/// must be one found here, so one inside an `EXISTS` (not walked) is refused too.
fn scoped_services_only(
    pattern: &spargebra::algebra::GraphPattern,
    text: &str,
    limit: &str,
) -> Result<(), Error> {
    use spargebra::algebra::GraphPattern as G;
    fn walk(p: &G, found: &mut usize) -> Result<(), ()> {
        match p {
            G::Service { name, inner, .. } => {
                let scoped = matches!(name, spargebra::term::NamedNodePattern::NamedNode(n)
                    if SCOPED_SERVICES.contains(&n.as_str()));
                if !scoped || inner.to_string().contains("SERVICE ") {
                    return Err(());
                }
                *found += 1;
                Ok(())
            }
            G::Bgp { .. } | G::Path { .. } | G::Values { .. } => Ok(()),
            G::Join { left, right }
            | G::LeftJoin { left, right, .. }
            | G::Union { left, right }
            | G::Minus { left, right }
            | G::Lateral { left, right } => {
                walk(left, found)?;
                walk(right, found)
            }
            G::Filter { inner, .. }
            | G::Graph { inner, .. }
            | G::Extend { inner, .. }
            | G::OrderBy { inner, .. }
            | G::Project { inner, .. }
            | G::Distinct { inner }
            | G::Reduced { inner }
            | G::Slice { inner, .. }
            | G::Group { inner, .. } => walk(inner, found),
        }
    }
    let mut found = 0;
    let refused = || {
        Error::Request(format!(
            "SERVICE is not available {limit}, except st:, window:, path: and pgr:, not nested"
        ))
    };
    walk(pattern, &mut found).map_err(|()| refused())?;
    if text.matches("SERVICE ").count() != found {
        return Err(refused());
    }
    Ok(())
}

/// Options of [`Engine::update_with_options`]: what the SPARQL Protocol adds to an update.
#[derive(Clone, Debug, Default)]
pub struct UpdateOptions {
    /// The base IRI relative IRIs in the update resolve against (the update's own `BASE`
    /// still wins).
    pub base_iri: Option<String>,
    /// The protocol's `using-graph-uri` and `using-named-graph-uri`: the dataset of every
    /// `DELETE`/`INSERT … WHERE`. `None` keeps the update's own. An update with `USING` or
    /// `WITH` of its own and these is an error, as the protocol requires.
    pub using: Option<(Vec<NamedNode>, Vec<NamedNode>)>,
    /// Only these graphs may be read (graph access, #201): the `WHERE` of a
    /// `DELETE`/`INSERT` reads them only, its `USING` and `WITH` narrowed to them (without
    /// either, it reads the union of the readable named graphs). The spatial rewrite is left
    /// out and a `SERVICE` refused, as for queries.
    pub readable: Option<ReadableGraphs>,
    /// Only these graphs may be written (#201): an update that may change another graph is
    /// refused as a whole before it runs. A template with `GRAPH ?g`, `CLEAR`/`DROP` of
    /// `NAMED` or `ALL`, are refused too, since what they change is not known beforehand.
    pub writable: Option<ReadableGraphs>,
    /// Row rules (#202): the update reads and writes only what they let the caller see (see
    /// [`tripletrail_store::RowGuard`]). Only `INSERT DATA`, `DELETE DATA` and `DELETE`/`INSERT …
    /// WHERE` are taken; only on the tripletrail store.
    pub row_rules: Option<RowRules>,
}

/// Why a caller with access to only some graphs may not call a user-defined function whose
/// body reads the store.
const FUNCTIONS_REFUSED: &str = "User-defined functions that read the data are not available \
     with access to only some graphs or rows (functions whose body is one expression are)";

/// Refuses an update that may change a graph outside `writable`, or whose `WHERE` calls a
/// `SERVICE` other than [`SCOPED_SERVICES`], and narrows the dataset of its `WHERE` to
/// `readable`. Gives the dataset the scoped services read (#203): that of the `WHERE`s that
/// call one, which must all have the same.
fn limit_update(
    update: &mut spargebra::Update,
    readable: Option<&ReadableGraphs>,
    writable: Option<&ReadableGraphs>,
) -> Result<Option<QueryDataset>, Error> {
    use spargebra::GraphUpdateOperation as Op;
    use spargebra::algebra::GraphTarget;
    use spargebra::term::{GraphName as G, GraphNamePattern as P};
    let refused = |what: String| {
        Error::Forbidden(format!(
            "The update may change {what}, which you may not write"
        ))
    };
    let named = |n: &NamedNode| writable.is_none_or(|w| w.contains(n.as_ref().into()));
    let default = || writable.is_none_or(|w| w.default_graph);
    let graph = |g: &G| -> Result<(), Error> {
        match g {
            G::NamedNode(n) if !named(n) => Err(refused(format!("the graph {n}"))),
            G::DefaultGraph if !default() => Err(refused("the default graph".to_owned())),
            _ => Ok(()),
        }
    };
    let pattern = |g: &P| -> Result<(), Error> {
        match g {
            P::NamedNode(n) if !named(n) => Err(refused(format!("the graph {n}"))),
            P::DefaultGraph if !default() => Err(refused("the default graph".to_owned())),
            P::Variable(v) if writable.is_some() => {
                Err(refused(format!("the graphs of ?{}", v.as_str())))
            }
            _ => Ok(()),
        }
    };
    let target = |t: &GraphTarget| -> Result<(), Error> {
        match t {
            GraphTarget::NamedNode(n) if !named(n) => Err(refused(format!("the graph {n}"))),
            GraphTarget::DefaultGraph if !default() => Err(refused("the default graph".to_owned())),
            GraphTarget::NamedGraphs | GraphTarget::AllGraphs if writable.is_some() => {
                Err(refused("every graph".to_owned()))
            }
            _ => Ok(()),
        }
    };
    let mut services = None;
    for operation in &mut update.operations {
        match operation {
            Op::InsertData { data } => data.iter().try_for_each(|q| graph(&q.graph_name))?,
            Op::DeleteData { data } => data.iter().try_for_each(|q| graph(&q.graph_name))?,
            Op::DeleteInsert {
                delete,
                insert,
                using,
                pattern: where_,
            } => {
                delete.iter().try_for_each(|q| pattern(&q.graph_name))?;
                insert.iter().try_for_each(|q| pattern(&q.graph_name))?;
                if let Some(r) = readable {
                    let text = where_.to_string();
                    let calls_service = text.contains("SERVICE ");
                    if calls_service {
                        scoped_services_only(where_, &text, SOME_GRAPHS)?;
                    }
                    let keep = |n: &NamedNode| r.contains(n.as_ref().into());
                    *using = Some(match using.take() {
                        Some(d) => spargebra::algebra::QueryDataset {
                            default: d.default.into_iter().filter(keep).collect(),
                            named: Some(match d.named {
                                Some(n) => n.into_iter().filter(keep).collect(),
                                None => r.named.clone(),
                            }),
                        },
                        None => spargebra::algebra::QueryDataset {
                            default: r.named.clone(),
                            named: Some(r.named.clone()),
                        },
                    });
                    if calls_service && let Some(d) = using {
                        let mut dataset = QueryDataset::new();
                        dataset.set_default_graph(
                            d.default.iter().cloned().map(GraphName::from).collect(),
                        );
                        dataset.set_available_named_graphs(
                            d.named
                                .iter()
                                .flatten()
                                .cloned()
                                .map(NamedOrBlankNode::from)
                                .collect(),
                        );
                        match &services {
                            Some(other) if *other != dataset => {
                                return Err(Error::Request(
                                    "With access to only some graphs, the operations of an \
                                     update that call a SERVICE must read the same graphs"
                                        .to_owned(),
                                ));
                            }
                            _ => services = Some(dataset),
                        }
                    }
                }
            }
            Op::Load { destination, .. } => graph(destination)?,
            Op::Clear { graph: t, .. } | Op::Drop { graph: t, .. } => target(t)?,
            Op::Create { graph: n, .. } => {
                if !named(n) {
                    return Err(refused(format!("the graph {n}")));
                }
            }
        }
    }
    Ok(services)
}

/// The view of `snapshot` that the id rewrites read the union default graph from, with the
/// graphs they read: every graph, without the copies of a triple held by several (#808).
/// `None` when the query has nothing those rewrites take (groups, `MINUS`, `FILTER`, `ORDER
/// BY`), so a lookup never waits for the copies, or when they are not known: they are found
/// by a scan of the whole store only for a query whose triple patterns match at least an
/// eighth as many quads, the work that scan stands to save, and kept up to date after writes.
fn union_ids(
    query: &spargebra::Query,
    snapshot: tripletrail_store::Snapshot,
    cancelled: &dyn Fn() -> bool,
) -> Option<(tripletrail_store::Snapshot, GraphSelector)> {
    let scan = union_scan(query, &snapshot)?;
    let copies = snapshot.union_copies(scan, cancelled)?;
    Some((snapshot.with_rows(copies), GraphSelector::All))
}

/// Whether the id rewrites take a part of `query` (`None` if not), and if so whether its
/// triple patterns match enough of the store that finding the copies of triples in several
/// graphs is one scan of all quads rather than a look at the subjects written since.
fn union_scan(query: &spargebra::Query, snapshot: &tripletrail_store::Snapshot) -> Option<bool> {
    use spargebra::algebra::GraphPattern as P;
    let pattern = match query {
        spargebra::Query::Select { pattern, .. }
        | spargebra::Query::Ask { pattern, .. }
        | spargebra::Query::Construct { pattern, .. } => pattern,
        spargebra::Query::Describe { .. } => return None,
    };
    // Whether a part the rewrites take is there, and the quads the triple patterns match.
    fn walk(p: &P, snapshot: &tripletrail_store::Snapshot, taken: &mut bool, matches: &mut usize) {
        let id = |t: &spargebra::term::TermPattern| match t {
            spargebra::term::TermPattern::NamedNode(n) => Some(snapshot.term_id(n.into())),
            spargebra::term::TermPattern::Literal(l) => Some(snapshot.term_id(l.into())),
            _ => None,
        };
        match p {
            P::Bgp { patterns } => {
                for t in patterns {
                    let (s, o) = (id(&t.subject), id(&t.object));
                    let pr = match &t.predicate {
                        spargebra::term::NamedNodePattern::NamedNode(n) => {
                            Some(snapshot.term_id(n.into()))
                        }
                        _ => None,
                    };
                    // A term the store doesn't hold matches nothing.
                    if [s, pr, o].contains(&Some(None)) {
                        continue;
                    }
                    *matches = matches.saturating_add(snapshot.estimate_ids(
                        s.flatten(),
                        pr.flatten(),
                        o.flatten(),
                        GraphSelector::All,
                    ));
                }
            }
            P::Path { .. } | P::Values { .. } | P::Service { .. } => {}
            P::Group { inner, .. } | P::OrderBy { inner, .. } => {
                *taken = true;
                walk(inner, snapshot, taken, matches);
            }
            P::Minus { left, right } => {
                *taken = true;
                walk(left, snapshot, taken, matches);
                walk(right, snapshot, taken, matches);
            }
            P::Filter { inner, .. } => {
                *taken = true;
                walk(inner, snapshot, taken, matches);
            }
            P::Join { left, right }
            | P::LeftJoin { left, right, .. }
            | P::Lateral { left, right }
            | P::Union { left, right } => {
                walk(left, snapshot, taken, matches);
                walk(right, snapshot, taken, matches);
            }
            P::Graph { inner, .. }
            | P::Extend { inner, .. }
            | P::Distinct { inner }
            | P::Reduced { inner }
            | P::Slice { inner, .. }
            | P::Project { inner, .. } => walk(inner, snapshot, taken, matches),
        }
    }
    let (mut taken, mut matches) = (false, 0);
    walk(pattern, snapshot, &mut taken, &mut matches);
    if !taken {
        return None;
    }
    Some(matches >= snapshot.len() / 8)
}

/// The query's own dataset (`FROM`, `FROM NAMED`), taken out of it.
fn take_dataset(query: &mut spargebra::Query) -> Option<spargebra::algebra::QueryDataset> {
    match query {
        spargebra::Query::Select { dataset, .. }
        | spargebra::Query::Construct { dataset, .. }
        | spargebra::Query::Describe { dataset, .. }
        | spargebra::Query::Ask { dataset, .. } => dataset.take(),
    }
}

/// Puts back the dataset [`take_dataset`] took.
fn set_dataset(query: &mut spargebra::Query, own: Option<spargebra::algebra::QueryDataset>) {
    match query {
        spargebra::Query::Select { dataset, .. }
        | spargebra::Query::Construct { dataset, .. }
        | spargebra::Query::Describe { dataset, .. }
        | spargebra::Query::Ask { dataset, .. } => *dataset = own,
    }
}

/// The union of all graphs as a query's default graph.
fn union_dataset() -> QueryDataset {
    let mut d = QueryDataset::new();
    d.set_default_graph_as_union();
    d
}

/// `COPY`, `MOVE` and `ADD DEFAULT TO <g>` read the right default graph, as it was before the
/// request (#782). spargebra writes `COPY DEFAULT TO <g>` as `DROP SILENT GRAPH <g>` and then
/// an `INSERT` into `<g>` of every triple, with variables of its own
/// ([`spargebra::COPY_VARIABLES`]; `MOVE` adds a `DROP DEFAULT`, `ADD` has no drop).
/// - On a store that reads the union of all graphs as its default graph, that `INSERT` reads
///   the stored default graph, as Fuseki's does under `tdb2:unionDefaultGraph` (Jena takes
///   the dataset's own default graph for these operations): it gets
///   `USING <urn:x-arq:DefaultGraph>`.
/// - Where the default graph of the `INSERT` is a union that includes `<g>` (a caller that
///   reads only some graphs, `USING <g>`), the drop would empty part of the source before it
///   is read; since `<g>` ends up holding the source, which includes all of `<g>`, the drop
///   is left out.
fn copy_reads_before_drop(update: &mut spargebra::Update, union_default: bool) {
    use spargebra::GraphUpdateOperation as Op;
    use spargebra::algebra::{GraphPattern, GraphTarget, QueryDataset};
    use spargebra::term::{GraphNamePattern, NamedNodePattern, TermPattern, TriplePattern};
    // Only the operations spargebra writes for COPY, MOVE and ADD: not an update a user wrote
    // the same way, which keeps reading the union.
    let spo = spargebra::COPY_VARIABLES.map(Variable::new_unchecked);
    let all = GraphPattern::Bgp {
        patterns: vec![TriplePattern {
            subject: spo[0].clone().into(),
            predicate: spo[1].clone().into(),
            object: spo[2].clone().into(),
        }],
    };
    let mut drops = Vec::new();
    for i in 0..update.operations.len() {
        let Op::DeleteInsert {
            delete,
            insert,
            using,
            pattern,
        } = &mut update.operations[i]
        else {
            continue;
        };
        // The INSERT that spargebra writes for a copy of the default graph.
        let [quad] = insert.as_slice() else {
            continue;
        };
        let GraphNamePattern::NamedNode(to) = &quad.graph_name else {
            continue;
        };
        let copies = delete.is_empty()
            && **pattern == all
            && matches!(&quad.subject, TermPattern::Variable(v) if *v == spo[0])
            && matches!(&quad.predicate, NamedNodePattern::Variable(v) if *v == spo[1])
            && matches!(&quad.object, TermPattern::Variable(v) if *v == spo[2]);
        if !copies {
            continue;
        }
        match using {
            None if union_default => {
                *using = Some(QueryDataset {
                    default: vec![NamedNode::new_unchecked(
                        tripletrail_store::STORED_DEFAULT_GRAPH,
                    )],
                    named: None,
                });
            }
            Some(dataset) if dataset.default.contains(to) && i > 0 => {
                let to = to.clone();
                if matches!(
                    &update.operations[i - 1],
                    Op::Drop { silent: true, graph: GraphTarget::NamedNode(n) } if *n == to
                ) {
                    drops.push(i - 1);
                }
            }
            _ => {}
        }
    }
    for i in drops.into_iter().rev() {
        update.operations.remove(i);
    }
}

impl Error {
    /// Whether the error is the request's fault: a wrong parameter of one of the engine's own
    /// services (`path`, `pgr`, `window`, `st`), or a SERVICE no handler answers, anywhere in
    /// the chain of causes. A server answers it as a bad request.
    pub fn is_bad_service_request(&self) -> bool {
        let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(self);
        while let Some(e) = cause {
            if matches!(e.downcast_ref(), Some(path::PathError::Request(_)))
                || matches!(e.downcast_ref(), Some(window::WindowError::Request(_)))
                || matches!(e.downcast_ref(), Some(StError::Request(_)))
                || matches!(e.downcast_ref(), Some(tripletrail_pgr::PgrError::Request(_)))
                // A SERVICE in a service's own query (a stored pgr:edgeQuery, #861), where
                // no handler answers it.
                || matches!(
                    e.downcast_ref(),
                    Some(oxigraph::sparql::QueryEvaluationError::UnsupportedService(_))
                )
            {
                return true;
            }
            cause = e.source();
        }
        false
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A request the engine refuses before running it, such as protocol options that
    /// conflict with the update.
    #[error("{0}")]
    Request(String),
    /// A request that would read or write graphs outside the caller's access (#201).
    #[error("{0}")]
    Forbidden(String),
    #[error(transparent)]
    Syntax(#[from] oxigraph::sparql::SparqlSyntaxError),
    #[error(transparent)]
    Query(#[from] oxigraph::sparql::QueryEvaluationError),
    #[error(transparent)]
    Update(#[from] oxigraph::sparql::UpdateEvaluationError),
    #[error(transparent)]
    Storage(#[from] oxigraph::store::StorageError),
    #[error(transparent)]
    Loader(#[from] oxigraph::store::LoaderError),
    #[error(transparent)]
    Dataset(#[from] DatasetError),
    #[error("network directory: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Shacl(#[from] validate::ShaclError),
    /// A GeoParquet file that cannot be read or written (feature `geoparquet`).
    #[error("GeoParquet: {0}")]
    GeoParquet(String),
}

const MAKE_LINE: &str = "http://w3id.org/tripletrail/st#ST_MakeLine";
const MAKE_LINE_AGG: &str = "http://w3id.org/tripletrail/st#ST_MakeLineAgg";

/// The custom functions [`Engine::evaluator`] registers, by IRI: every function of
/// [`tripletrail_geo::function_table`]. The SPARQL Service Description lists these, so a new
/// function shows up there without anyone having to add it.
pub fn extension_functions() -> Vec<NamedNode> {
    tripletrail_geo::function_table()
        .into_iter()
        .map(|(name, _)| name)
        .chain(
            tripletrail_fx::functions()
                .into_iter()
                .map(|(name, _)| name),
        )
        .chain(xfn::functions().into_iter().map(|(name, _)| name))
        .chain(xfn::builtin_aliases())
        .chain(
            ttfn::functions()
                .into_iter()
                .map(|(name, _)| name)
                .filter(|n| {
                    !n.as_str().ends_with("#skolemKey")
                        && !n.as_str().ends_with("#iriString")
                        && !n.as_str().ends_with("#uriString")
                        && !n.as_str().ends_with("#splitParts")
                        && !n.as_str().ends_with("#splitPart")
                }),
        )
        .chain(ttfn::rewritten())
        .chain(temporal::names())
        .collect()
}

/// The custom aggregates [`Engine::evaluator`] registers, by IRI: the aggregates of
/// [`tripletrail_geo::aggregate_functions`] and `st:ST_MakeLine` with one argument.
pub fn extension_aggregates() -> Vec<NamedNode> {
    let mut names: Vec<NamedNode> = tripletrail_geo::aggregate_functions()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    names.push(NamedNode::new_unchecked(MAKE_LINE));
    names.extend(statagg::aggregates().into_iter().map(|(name, _)| name));
    names
}

/// The SERVICE handlers [`Engine::evaluator`] registers, by IRI (pgRouting, the
/// set-returning `ST_*` functions, window functions and path search).
pub fn extension_services() -> Vec<NamedNode> {
    [PGR_SERVICE, ST_SERVICE, WINDOW_SERVICE, PATH_SERVICE]
        .into_iter()
        .map(NamedNode::new_unchecked)
        .collect()
}

/// Runs Facade-X SERVICE patterns on a tripletrail store of their own: bulk loading the
/// generated triples there is several times faster than into an in-memory Oxigraph dataset,
/// and the pattern gets the engine's planner.
struct FxBackend;

impl tripletrail_fx::Backend for FxBackend {
    fn evaluate(
        &self,
        quads: Vec<oxigraph::model::Quad>,
        query: spargebra::Query,
        fx: &FxService,
    ) -> Result<QuerySolutionIter<'static>, tripletrail_fx::FxError> {
        let backend = |e: Error| tripletrail_fx::FxError::Backend(Box::new(e));
        let store = tripletrail_store::Store::new();
        store
            .bulk_load_quads(quads)
            .map_err(|e| backend(DatasetError::from(e).into()))?;
        let engine = Self::engine(store, fx);
        match engine.query(&query.to_string()).map_err(backend)? {
            QueryResults::Solutions(solutions) => Ok(solutions),
            _ => unreachable!("a SELECT gives solutions"),
        }
    }
}

impl FxBackend {
    fn engine(store: tripletrail_store::Store, fx: &FxService) -> Engine {
        Engine {
            fx: Some(fx.clone()),
            ..Engine::from_store(store)
        }
    }
}

/// An evaluator with the geo functions, made once (#186): the routing and `st:` SERVICEs
/// make one for each call.
fn geo_evaluator() -> SparqlEvaluator {
    static GEO: OnceLock<SparqlEvaluator> = OnceLock::new();
    GEO.get_or_init(|| tripletrail_geo::register(SparqlEvaluator::new()))
        .clone()
}

/// The default of [`Engine::set_default_executor_ids`], for the whole process.
static EXECUTOR_IDS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether the id executor runs this query: its `#pragma tt.executor`, else the default.
pub(crate) fn executor_ids() -> bool {
    Hints::current()
        .executor_ids
        .unwrap_or_else(|| EXECUTOR_IDS.load(Ordering::Relaxed))
}

/// A store plus the evaluator extensions. Cheap to clone.
#[derive(Clone)]
pub struct Engine {
    store: Dataset,
    networks: Arc<NetworkCache>,
    pgr: PgrService,
    st: StService,
    window: WindowService,
    spatial: Arc<SpatialState>,
    results: Arc<cache::ResultCache>,
    statistics: Arc<statistics::Cache>,
    profile: Arc<profile::Cache>,
    cancellation: Option<CancellationToken>,
    /// `SERVICE <x-sparql-anything:…>` calls, off unless [`Engine::with_fx`] enabled them.
    fx: Option<FxService>,
    /// Named virtual graphs: `GRAPH <iri>` read as `SERVICE <x-sparql-anything:…>` (#216).
    fx_graphs: Arc<std::collections::HashMap<NamedNode, NamedNode>>,
    /// `SERVICE <https://…>` calls to remote endpoints, off unless
    /// [`Engine::with_remote_services`] allowed some.
    #[cfg(feature = "remote-service")]
    remote: Option<remote::RemoteService>,
    /// The evaluator with every function registered, made once for this store: registering
    /// them (about a thousand geo functions) for each query took 0.3 ms, most of a small
    /// query's time (#186). Each query clones it and adds its services.
    functions: Arc<OnceLock<SparqlEvaluator>>,
    /// The full-text index of `tt:search` (#188), built on the first search after a change.
    text: Arc<Mutex<Option<Arc<text::TextIndex>>>>,
    /// The properties the full-text indexes cover: [`text::INDEXED`] and the dataset's own.
    text_properties: Arc<Mutex<Arc<[NamedNode]>>>,
    /// Full-text indexes of what callers limited to some graphs or rows see (#203), by
    /// [`Engine::text_view`]'s key, the latest last.
    text_views: Arc<Mutex<Vec<TextView>>>,
    /// The view indexes built and patched so far, for tests ([`Engine::text_view_work`]).
    text_view_work: Arc<[std::sync::atomic::AtomicUsize; 2]>,
    /// The vector indexes of `tt:nearest` (#236), by view (0 for the whole store) and
    /// property, the latest used last.
    vectors: Arc<Mutex<Vec<VectorSlot>>>,
    /// How many vectors of one length an index needs before it makes a graph of them
    /// ([`Engine::set_vector_graph_min`]).
    vector_graph_min: Arc<std::sync::atomic::AtomicUsize>,
    /// The offset, in minutes east of UTC, that the temporal functions read values without a
    /// timezone at ([`Engine::set_time_offset`]; UTC by default).
    time_offset: Arc<std::sync::atomic::AtomicI64>,
    /// How many queries ordered by `tt:cosine` the vector index answered.
    cosine_orders: Arc<AtomicU64>,
    /// How many parts of queries by callers limited to some graphs or rows the count and
    /// id-row shortcuts answered (#204).
    view_shortcuts: Arc<AtomicU64>,
    /// The filters of the views [`Engine::view_shortcuts`] read, by [`ViewKey`] and the row
    /// rules' filter (the same one, not only one with the same fingerprint), the latest used
    /// last: finding the copies within some graphs and merging them with row rules is
    /// done once per store version, not for each query.
    view_filters: Arc<Mutex<Vec<(ViewKey, ViewFilter, ViewFilter)>>>,
    /// User-defined functions (`sh:SPARQLFunction`, #235).
    udf: Arc<udf::Functions>,
    /// The dataset the sub-selects of the `st:` and window services read, for a caller who
    /// reads only some graphs (#203): the calling query's, narrowed to those graphs.
    service_dataset: Option<QueryDataset>,
    /// What the built-in services read for a caller under row rules (#202): the rows the
    /// calling query sees, so their sub-selects and searches see no more.
    service_rows: Option<tripletrail_store::RowView>,
}

/// What a view of [`Engine::view_shortcuts`] shows: the store version, the graphs (`None` for
/// all of them) and the fingerprint of the rows row rules hide (0 for none).
type ViewKey = (u64, Option<Vec<u64>>, u64);

/// The filter of such a view; `None` when it hides nothing.
type ViewFilter = Option<Arc<tripletrail_store::RowFilter>>;

/// How many view filters [`Engine::view_shortcuts`] keeps.
const VIEW_FILTERS: usize = 8;

/// The full-text index of what some callers see (#203), by [`Engine::text_view`]'s key: a
/// slot of its own, so the searches of one view wait for its one build or patch.
type TextView = (u64, Arc<Mutex<Option<Arc<text::TextIndex>>>>);

/// The key of what `source` shows: its graphs and the rows it hides.
fn view_key(source: &text::Source) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source
        .graphs
        .as_ref()
        .map(|g| g.iter().map(|t| t.0).collect::<Vec<_>>())
        .hash(&mut hasher);
    source
        .snapshot
        .rows()
        .map(|r| r.fingerprint())
        .hash(&mut hasher);
    hasher.finish()
}

/// The vector index of one property in what some callers see (#236): a slot of its own, as
/// for [`TextView`].
type VectorSlot = (u64, NamedNode, Arc<Mutex<Option<Arc<vector::VectorIndex>>>>);

/// Roughly the heap bytes an engine holds for its dataset, by part (see
/// [`Engine::memory_use`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryUse {
    pub dictionary: usize,
    pub quads: usize,
    /// The geometry literals parsed so far.
    pub geometries: usize,
    /// The spatial index and its views for callers who see some graphs only.
    pub spatial_index: usize,
    /// The full-text index and its views.
    pub text_index: usize,
    /// The vector indexes of `tt:nearest` (#236).
    pub vector_indexes: usize,
    /// The value-ordered indexes of numeric objects, for range FILTERs.
    pub range_indexes: usize,
    pub result_cache: usize,
    pub routing_networks: usize,
    /// The copies of triples in several graphs, for the union default graph (#808).
    pub union_copies: usize,
}

impl MemoryUse {
    pub fn total(&self) -> usize {
        self.dictionary
            + self.quads
            + self.geometries
            + self.spatial_index
            + self.text_index
            + self.vector_indexes
            + self.range_indexes
            + self.result_cache
            + self.routing_networks
            + self.union_copies
    }

    /// The parts by name, as `/$/stats` and `/$/metrics` show them.
    pub fn parts(&self) -> [(&'static str, usize); 10] {
        [
            ("dictionary", self.dictionary),
            ("quads", self.quads),
            ("geometries", self.geometries),
            ("spatialIndex", self.spatial_index),
            ("textIndex", self.text_index),
            ("vectorIndexes", self.vector_indexes),
            ("rangeIndexes", self.range_indexes),
            ("resultCache", self.result_cache),
            ("routingNetworks", self.routing_networks),
            ("unionCopies", self.union_copies),
        ]
    }
}

/// A view of the spatial index: key, store generation, index generation, the view.
type IndexView = (u64, u64, u64, Arc<SpatialIndex>);

#[derive(Default)]
struct SpatialState {
    enabled: AtomicBool,
    /// Whether the rewrite gets cardinality estimates (the cost model).
    costed: AtomicBool,
    /// Whether the cost model may answer a join of two large sides with the sweep-line join.
    sweep: AtomicBool,
    index: Mutex<Option<Arc<SpatialIndex>>>,
    /// Held while the index is built, so queries that need it wait for one build instead
    /// of each starting their own. Writes never take it.
    build: Mutex<()>,
    /// Advanced by every write that changes the index (or drops it): a build that saw it
    /// change while it read the store is out of date.
    generation: AtomicU64,
    /// The predicates whose geometry literals the index holds.
    predicates: Mutex<IndexedPredicates>,
    /// Whether the index is rebuilt in the background as soon as a write drops it
    /// (see [`Engine::warm_up`]).
    warm: AtomicBool,
    /// Whether a background build is under way or about to start.
    building: AtomicBool,
    last: Mutex<RewriteStats>,
    counts: Mutex<Counts>,
    /// Value-ordered numeric objects per predicate, for range FILTERs (see `range`).
    ranges: range::RangeIndexes,
    /// How many objects of a predicate are not geometry literals, by predicate (for the
    /// nearest-neighbour path), with a count of the writes that may change them: a count
    /// taken while a write went by is not kept.
    non_geometry: Mutex<HashMap<String, usize>>,
    writes: AtomicU64,
    /// The spatial index as callers with access to some graphs only see it (#203): by
    /// [`ReadableGraphs::fingerprint`] and the store generation it was made at, newest last.
    /// Views of the index (#203, #202): key, store generation, and the index's own
    /// generation they were made from (the index is patched after a write commits).
    views: Mutex<Vec<IndexView>>,
    /// Whether queries and updates read the union of all graphs as their default graph (see
    /// [`Engine::set_union_default_graph`]).
    union_default_graph: AtomicBool,
    /// Compiled row rules (#202), see [`Engine::compile_row_rules_cached`].
    row_filters: Mutex<row_rules::Cache>,
}

/// On Oxigraph's store a triple-pattern estimate is a count of the matches, stopped here: the
/// cost model only needs to know that a pattern is large.
const COUNT_CAP: usize = 250_000;
/// Cached estimates kept at most; the cache starts over beyond.
const COUNTS_KEPT: usize = 4096;

/// Triple-pattern estimates (subject, predicate, object as constants or `None`, every graph
/// or the default one), kept until a write touches their predicate.
#[derive(Default)]
struct Counts {
    entries: HashMap<CountKey, usize>,
}

type CountKey = (Option<Term>, Option<NamedNode>, Option<Term>, bool);

impl Counts {
    /// Drops the estimates a write of quads with these predicates may have changed.
    fn invalidate(&mut self, predicates: &HashSet<&str>) {
        self.entries
            .retain(|(_, p, _, _), _| p.as_ref().is_some_and(|p| !predicates.contains(p.as_str())));
    }
}

impl Engine {
    /// A new in-memory engine, on the tripletrail store, or on Oxigraph's store when the
    /// environment variable `TRIPLETRAIL_STORE` is `oxigraph`.
    pub fn new() -> Result<Self, Error> {
        if std::env::var("TRIPLETRAIL_STORE").as_deref() == Ok("oxigraph") {
            return Self::new_oxigraph();
        }
        Ok(Self::new_tripletrail())
    }

    /// A new in-memory engine on Oxigraph's store.
    pub fn new_oxigraph() -> Result<Self, Error> {
        Ok(Self::from_store(Store::new()?))
    }

    /// A new in-memory engine on the tripletrail store.
    pub fn new_tripletrail() -> Self {
        Self::from_store(tripletrail_store::Store::new())
    }

    /// An engine on the tripletrail store kept in directory `dir` (created if needed).
    pub fn open_tripletrail(dir: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let store = tripletrail_store::Store::open(dir).map_err(DatasetError::from)?;
        Ok(Self::from_store(store))
    }

    /// An engine on the tripletrail store in directory `dir`, opened for reading only: the
    /// checkpoint files are memory-mapped, nothing in `dir` changes, and updates and loads
    /// fail with [`tripletrail_store::StoreError::ReadOnly`]. For datasets that are published once
    /// and only queried, e.g. a directory written by [`Engine::save`].
    pub fn open_read_only(dir: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let store = tripletrail_store::Store::open_read_only(dir).map_err(DatasetError::from)?;
        Ok(Self::from_store(store))
    }

    /// Writes the data, as it is now, to a new on-disk dataset directory `dir` that
    /// [`Engine::open_tripletrail`], [`Engine::open_read_only`] and `tripletrail serve --location`
    /// open without a load step: a checkpoint of the tripletrail store with the parsed geometries in
    /// binary form. It is written under a temporary name and renamed at the end, so `dir`
    /// never holds a half-written dataset; `dir` must not exist or be empty. On Oxigraph's
    /// store the quads are copied into an tripletrail store first.
    pub fn save(&self, dir: impl AsRef<std::path::Path>) -> Result<(), Error> {
        let dir = dir.as_ref();
        match &self.store {
            Dataset::TripleTrail(store) => store.save_to(dir).map_err(DatasetError::from)?,
            Dataset::Oxigraph(store) => {
                let copy = tripletrail_store::Store::new();
                let mut quads = store.iter();
                let mut error = None;
                let batches = std::iter::from_fn(|| {
                    let mut batch = Vec::with_capacity(1 << 16);
                    for q in quads.by_ref() {
                        match q {
                            Ok(q) => batch.push(q),
                            Err(e) => {
                                error = Some(e);
                                break;
                            }
                        }
                        if batch.len() == 1 << 16 {
                            break;
                        }
                    }
                    (!batch.is_empty()).then_some(batch)
                });
                copy.bulk_load_quad_batches(batches)
                    .map_err(DatasetError::from)?;
                if let Some(e) = error {
                    return Err(e.into());
                }
                copy.save_to(dir).map_err(DatasetError::from)?;
            }
        }
        Ok(())
    }

    /// Whether writes are refused (see [`Engine::open_read_only`]).
    pub fn is_read_only(&self) -> bool {
        matches!(&self.store, Dataset::TripleTrail(s) if s.is_read_only())
    }

    /// An engine on an existing store: Oxigraph's [`Store`] or an [`tripletrail_store::Store`].
    pub fn from_store(store: impl Into<Dataset>) -> Self {
        Self::with_cache(store.into(), NetworkCache::new())
    }

    /// An engine that keeps built routing networks and contraction hierarchies in `dir`, so
    /// they survive a restart. Every write must go through this engine (`load`, `load_quads`,
    /// `update`): writes made directly on the store would leave saved networks stale.
    pub fn with_network_dir(
        store: impl Into<Dataset>,
        dir: impl Into<std::path::PathBuf>,
    ) -> Result<Self, Error> {
        Ok(Self::with_cache(
            store.into(),
            NetworkCache::persistent(dir)?,
        ))
    }

    fn with_cache(store: Dataset, networks: NetworkCache) -> Self {
        let networks = Arc::new(networks);
        let pgr = PgrService::new(store.clone(), Arc::clone(&networks), geo_evaluator);
        let st = StService::new(store.clone(), geo_evaluator);
        let window = WindowService::new(store.clone(), geo_evaluator);
        let spatial = Arc::new(SpatialState::default());
        spatial.enabled.store(true, Ordering::Relaxed);
        spatial.costed.store(true, Ordering::Relaxed);
        spatial.sweep.store(true, Ordering::Relaxed);
        Self {
            store,
            networks,
            pgr,
            st,
            window,
            spatial,
            results: Arc::default(),
            statistics: Arc::default(),
            profile: Arc::default(),
            cancellation: None,
            fx: None,
            fx_graphs: Arc::default(),
            #[cfg(feature = "remote-service")]
            remote: None,
            functions: Arc::default(),
            text: Arc::default(),
            text_properties: Arc::new(Mutex::new(text::default_properties())),
            text_views: Arc::default(),
            service_dataset: None,
            service_rows: None,
            text_view_work: Arc::default(),
            vectors: Arc::default(),
            vector_graph_min: Arc::new(std::sync::atomic::AtomicUsize::new(vector::GRAPH_MIN)),
            time_offset: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            cosine_orders: Arc::new(AtomicU64::new(0)),
            view_shortcuts: Arc::new(AtomicU64::new(0)),
            view_filters: Arc::new(Mutex::new(Vec::new())),
            udf: Arc::default(),
        }
    }

    /// The same engine, also answering `SERVICE <x-sparql-anything:…>` calls (the Facade-X
    /// view of CSV, JSON and other files, see [`tripletrail_fx`]) from the files `config`
    /// allows. Without it those calls fail. The `fx:` functions work either way.
    pub fn with_fx(&self, config: FxConfig) -> Self {
        Self {
            fx: Some(FxService::with_backend(config, FxBackend)),
            ..self.clone()
        }
    }

    /// The same engine, also reading `bytes` as `SERVICE <x-sparql-anything:location=upload:NAME>`
    /// (the Import page): a file held in memory, for this engine only. Without Facade-X files
    /// ([`Engine::with_fx`]), the service reads the upload and nothing else: no files, no URLs,
    /// limits that may only be lowered.
    pub fn with_upload(&self, name: &str, bytes: Arc<[u8]>) -> Result<Self, Error> {
        let fx = match &self.fx {
            Some(fx) => fx.clone(),
            None => FxService::with_backend(
                FxConfig {
                    allowed: Some(Vec::new()),
                    fixed_limits: true,
                    ..FxConfig::default()
                },
                FxBackend,
            ),
        };
        let fx = fx
            .with_upload(name, bytes)
            .map_err(|e| Error::Request(e.to_string()))?;
        Ok(Self {
            fx: Some(fx),
            ..self.clone()
        })
    }

    /// The same engine, reading `GRAPH <iri>` as `SERVICE <service>` for each `(iri, service)`
    /// of `graphs` (named virtual graphs, #216): a query names a Facade-X source as a graph,
    /// without writing the SERVICE. Not for callers with access to only some graphs, who may
    /// not call a SERVICE: for them `GRAPH <iri>` reads the store as usual.
    pub fn with_fx_graphs(&self, graphs: impl IntoIterator<Item = (NamedNode, NamedNode)>) -> Self {
        Self {
            fx_graphs: Arc::new(graphs.into_iter().collect()),
            ..self.clone()
        }
    }

    /// The same engine, also answering `SERVICE <https://…>` calls to the remote SPARQL
    /// endpoints `config` allows (#192). Without it, or for an endpoint not on its list, those
    /// calls fail (`SERVICE SILENT` then adds nothing).
    #[cfg(feature = "remote-service")]
    pub fn with_remote_services(&self, config: remote::RemoteConfig) -> Self {
        Self {
            remote: (!config.allow.is_empty()).then(|| remote::RemoteService::new(config)),
            ..self.clone()
        }
    }

    /// The same engine with the user-defined functions `functions` (`sh:SPARQLFunction`s,
    /// see [`udf`]) in place of its own: queries, updates, rules and constraints can call
    /// them by their IRIs.
    pub fn with_functions(&self, functions: udf::Functions) -> Self {
        Self {
            udf: Arc::new(functions),
            ..self.clone()
        }
    }

    /// The same engine with its user-defined functions and the `sh:SPARQLFunction`s of
    /// `triples` (those of `triples` win where both define one).
    pub fn with_functions_from(
        &self,
        triples: impl IntoIterator<Item = oxigraph::model::Triple>,
    ) -> Result<Self, udf::FunctionError> {
        let added = udf::Functions::from_triples(triples)?;
        if added.is_empty() {
            return Ok(self.clone());
        }
        Ok(self.with_functions(self.udf.merged(&added)?))
    }

    /// The user-defined functions (see [`Engine::with_functions`]).
    pub fn functions(&self) -> &udf::Functions {
        &self.udf
    }

    /// Replaces the calls of inlinable user-defined functions by their expressions; returns
    /// whether `query` still calls one.
    pub(crate) fn inline_functions(&self, query: &mut spargebra::Query) -> bool {
        self.udf.inline(query)
    }

    /// The same engine (same store, caches and settings) with queries and updates that stop
    /// with an error once `token` is cancelled, e.g. by a timeout.
    pub fn with_cancellation_token(&self, token: CancellationToken) -> Self {
        // A memory limit set before (`with_query_memory`) stays with the new token.
        let token = match &self.cancellation {
            Some(old) if token.memory_limit() == 0 && old.memory_limit() > 0 => {
                token.with_memory_limit(old.memory_limit())
            }
            _ => token,
        };
        Self {
            cancellation: Some(token),
            ..self.clone()
        }
    }

    /// The same engine with queries whose intermediate rows may take at most `bytes` (#189),
    /// below the process's [`set_query_memory`]: a hash join of the evaluator past it fails
    /// the query with [`QueryEvaluationError::MemoryLimit`](oxigraph::sparql::QueryEvaluationError),
    /// and a join on term ids past it is evaluated row by row instead. For a limit per dataset
    /// or per request; 0 keeps the process's limit alone.
    pub fn with_query_memory(&self, bytes: usize) -> Self {
        let token = self.cancellation.clone().unwrap_or_default();
        Self {
            cancellation: Some(token.with_memory_limit(bytes)),
            ..self.clone()
        }
    }

    /// Runs `f` with `hints` in force and the id joins limited to this engine's query memory
    /// ([`Engine::with_query_memory`]), on this thread.
    fn scope<R>(&self, hints: Hints, f: impl FnOnce() -> R) -> R {
        let memory = self
            .cancellation
            .as_ref()
            .map_or(0, CancellationToken::memory_limit);
        hints::scope(hints, || tripletrail_store::with_join_memory(memory, f))
    }

    /// The store the data lives in.
    pub fn store(&self) -> &Dataset {
        &self.store
    }

    pub fn networks(&self) -> &NetworkCache {
        &self.networks
    }

    /// Timings of the most recent routing call.
    pub fn last_routing_stats(&self) -> tripletrail_pgr::QueryStats {
        self.pgr.last_stats()
    }

    /// An evaluator with every geo function and the pgr SERVICE registered. Keep
    /// [`extension_functions`], [`extension_aggregates`] and [`extension_services`] in step
    /// with what this registers.
    pub fn evaluator(&self) -> SparqlEvaluator {
        let functions = self.functions.get_or_init(|| {
            let make_line = tripletrail_geo::aggregate_functions()
                .into_iter()
                .find(|(n, _)| n.as_str() == MAKE_LINE_AGG)
                .map(|(_, f)| f)
                .expect("ST_MakeLineAgg is registered");
            // On the tripletrail store, functions read the geometries it keeps parsed by term
            // id. They stop with the query that calls them, whichever it is.
            let cancel: tripletrail_geo::cancel::Check =
                Arc::new(spareval::custom_function_call_cancelled);
            let evaluator = tripletrail_geo::register_with(
                SparqlEvaluator::new(),
                self.geometry_source(),
                Some(cancel),
            )
            .with_custom_aggregate_function(NamedNode::new_unchecked(MAKE_LINE), make_line);
            let mut evaluator = tripletrail_fx::register_functions(evaluator);
            let temporal = temporal::functions(Arc::clone(&self.time_offset));
            for (name, f) in xfn::functions()
                .into_iter()
                .chain(ttfn::functions())
                .chain(temporal)
            {
                evaluator = evaluator.with_custom_function(name, move |args| f(args));
            }
            for (name, make) in statagg::aggregates() {
                evaluator = evaluator.with_custom_aggregate_function(name, make);
            }
            evaluator
                .with_custom_function(NamedNode::new_unchecked(statagg::ARGUMENTS), statagg::pack)
        });
        let services = Services {
            fx: self.fx.clone(),
            #[cfg(feature = "remote-service")]
            remote: self.remote.clone(),
        };
        let mut evaluator = functions.clone();
        for f in self.udf.iter() {
            let engine = self.clone();
            let f = Arc::clone(f);
            evaluator = evaluator
                .with_custom_function(f.iri.clone(), move |args| udf::call(&engine, &f, args));
        }
        let evaluator = evaluator
            .with_service_handler(NamedNode::new_unchecked(PGR_SERVICE), self.pgr_service())
            .with_service_handler(NamedNode::new_unchecked(ST_SERVICE), self.st_service())
            .with_service_handler(
                NamedNode::new_unchecked(WINDOW_SERVICE),
                self.window_service(),
            )
            .with_service_handler(NamedNode::new_unchecked(PATH_SERVICE), {
                let path = PathService::new(
                    self.store.clone(),
                    geo_evaluator,
                    self.union_default_graph(),
                    self.cancellation.clone(),
                );
                let path = match &self.service_dataset {
                    Some(d) => path.with_dataset(d.clone()),
                    None => path,
                };
                match &self.service_rows {
                    Some(r) => path.with_rows(r.clone()),
                    None => path,
                }
            })
            .with_default_service_handler(services);
        // `#pragma tt.optimizer off` (#238): the joins in the written order.
        let evaluator = if Hints::current().optimizer_off {
            evaluator.without_optimizations()
        } else {
            evaluator
        };
        match &self.cancellation {
            Some(token) => evaluator.with_cancellation_token(token.clone()),
            None => evaluator,
        }
    }

    /// Whether the spatial rewrite runs: switched on, and not turned off by
    /// `#pragma tt.spatial-rewrite off` (#238).
    fn spatial_rewrite_on(&self) -> bool {
        self.spatial.enabled.load(Ordering::Relaxed) && !Hints::current().spatial_rewrite_off
    }

    /// The graphs the built-in services read: the caller's ([`Engine::service_dataset`]),
    /// else the union of all graphs when that is the default graph (#861), else `None` for
    /// the default graph.
    fn services_dataset(&self) -> Option<QueryDataset> {
        self.service_dataset
            .clone()
            .or_else(|| self.union_dataset())
    }

    /// The `st:` SERVICE, its sub-selects stopping with this engine's cancellation token and
    /// reading [`Engine::service_dataset`].
    fn st_service(&self) -> StService {
        let st = match &self.cancellation {
            Some(token) => self.st.with_cancellation_token(token.clone()),
            None => self.st.clone(),
        };
        let st = match self.services_dataset() {
            Some(d) => st.with_dataset(d),
            None => st,
        };
        match &self.service_rows {
            Some(r) => st.with_rows(r.clone()),
            None => st,
        }
    }

    /// The window SERVICE, its sub-selects stopping with this engine's cancellation token and
    /// reading [`Engine::service_dataset`].
    fn window_service(&self) -> WindowService {
        let window = match &self.cancellation {
            Some(token) => self.window.with_cancellation_token(token.clone()),
            None => self.window.clone(),
        };
        let window = match self.services_dataset() {
            Some(d) => window.with_dataset(d),
            None => window,
        };
        match &self.service_rows {
            Some(r) => window.with_rows(r.clone()),
            None => window,
        }
    }

    /// The pgr SERVICE, stopping with this engine's cancellation token and reading
    /// [`Engine::service_dataset`].
    fn pgr_service(&self) -> PgrService {
        let pgr = match &self.cancellation {
            Some(token) => self.pgr.with_cancellation_token(token.clone()),
            None => self.pgr.clone(),
        };
        let pgr = match self.services_dataset() {
            Some(d) => pgr.with_dataset(d),
            None => pgr,
        };
        match &self.service_rows {
            Some(r) => pgr.with_rows(r.clone()),
            None => pgr,
        }
    }

    /// The store's parsed geometries, if it keeps them (the tripletrail store).
    fn geometry_source(&self) -> Option<Arc<tripletrail_geo::GeometrySource>> {
        if self.store.kind() != "tripletrail" {
            return None;
        }
        let store = self.store.clone();
        Some(Arc::new(move |l: &oxigraph::model::Literal| {
            store.geometry(l.as_ref().into())
        }))
    }

    /// A SPARQL parser that knows the custom aggregates; with `make_line`, `st:ST_MakeLine`
    /// with one argument is the aggregate (as in PostGIS) rather than the two-argument function.
    fn parser(make_line: bool) -> spargebra::SparqlParser {
        let mut parser = spargebra::SparqlParser::new().with_aggregate_arguments(
            NamedNode::new_unchecked(statagg::ARGUMENTS),
            statagg::taking_arguments(),
        );
        for (name, _) in tripletrail_geo::aggregate_functions() {
            parser = parser.with_custom_aggregate_function(name);
        }
        for (name, _) in statagg::aggregates() {
            parser = parser.with_custom_aggregate_function(name);
        }
        if make_line {
            parser = parser.with_custom_aggregate_function(NamedNode::new_unchecked(MAKE_LINE));
        }
        parser
    }

    /// Parses with `st:ST_MakeLine` as an aggregate, or as the function if that fails (a
    /// SPARQL name is either an aggregate or a function in one query).
    ///
    /// Calls of XPath, Jena and `tt:` functions that rewrite to built-ins, and property
    /// functions, are rewritten here (see [`xfn`]), before the other rewrites read the
    /// query's triple patterns. Planning rewrites them again for queries parsed elsewhere.
    fn parse_query(query: &str) -> Result<spargebra::Query, Error> {
        Self::parse_query_with_base(query, None)
    }

    fn parse_query_with_base(query: &str, base: Option<&str>) -> Result<spargebra::Query, Error> {
        let defined = hints::without_defines(query).map_err(Error::Request)?;
        let query = defined.as_ref().map_or(query, |(rest, _)| rest.as_str());
        let query_text = query;
        complexity::check_nesting(query)?;
        let query = match Self::parser_with_base(true, base)?.parse_query(query) {
            Ok(q) => q,
            Err(e) => Self::parser_with_base(false, base)?
                .parse_query(query)
                .map_err(|_| Error::from(e))?,
        };
        complexity::check_query(&query)?;
        let mut query = query;
        if query_text.contains(hints::BLAZEGRAPH) {
            hints::strip_query(&mut query);
        }
        Ok(xfn::alias_query(query))
    }

    fn parser_with_base(
        make_line: bool,
        base: Option<&str>,
    ) -> Result<spargebra::SparqlParser, Error> {
        let parser = Self::parser(make_line);
        match base {
            Some(base) => parser
                .with_base_iri(base)
                .map_err(|e| Error::Request(format!("base IRI <{base}>: {e}"))),
            None => Ok(parser),
        }
    }

    fn parse_update(update: &str) -> Result<spargebra::Update, Error> {
        Self::parse_update_with_base(update, None)
    }

    fn parse_update_with_base(
        update: &str,
        base: Option<&str>,
    ) -> Result<spargebra::Update, Error> {
        let defined = hints::without_defines(update).map_err(Error::Request)?;
        let update = defined.as_ref().map_or(update, |(rest, _)| rest.as_str());
        let update_text = update;
        complexity::check_nesting(update)?;
        let update = match Self::parser_with_base(true, base)?.parse_update(update) {
            Ok(u) => u,
            Err(e) => Self::parser_with_base(false, base)?
                .parse_update(update)
                .map_err(|_| Error::from(e))?,
        };
        complexity::check_update(&update)?;
        let mut update = update;
        if update_text.contains(hints::BLAZEGRAPH) {
            hints::strip_update(&mut update);
        }
        Ok(xfn::alias_update(update))
    }

    /// Parses a query the way [`Engine::query`] does, without evaluating it: `Ok` when it
    /// would be accepted. The conformance runner's syntax tests use it.
    pub fn check_query(query: &str) -> Result<(), Error> {
        Self::parse_query(query).map(|_| ())
    }

    /// Parses an update the way [`Engine::update`] does, without running it.
    pub fn check_update(update: &str) -> Result<(), Error> {
        Self::parse_update(update).map(|_| ())
    }

    /// Whether every operation of `update` only removes data: `DELETE DATA`, `DELETE WHERE`,
    /// `DELETE` without `INSERT`, `CLEAR` and `DROP`. False when it does not parse. A server
    /// lets such updates through when a dataset is over its storage quota.
    pub fn update_only_removes(update: &str) -> bool {
        use spargebra::GraphUpdateOperation as Op;
        Self::parse_update(update).is_ok_and(|u| {
            u.operations.iter().all(|op| match op {
                Op::DeleteData { .. } | Op::Clear { .. } | Op::Drop { .. } => true,
                Op::DeleteInsert { insert, .. } => insert.is_empty(),
                Op::InsertData { .. } | Op::Load { .. } | Op::Create { .. } => false,
            })
        })
    }

    pub fn query(&self, query: &str) -> Result<QueryResults<'static>, Error> {
        self.query_with_options(query, QueryOptions::default())
    }

    /// Evaluates a query with [`QueryOptions`]: another dataset than the store's default
    /// one, and variables bound to values before evaluation.
    pub fn query_with_options(
        &self,
        query: &str,
        options: QueryOptions,
    ) -> Result<QueryResults<'static>, Error> {
        self.query_with_cache_status(query, options).map(|(r, _)| r)
    }

    /// [`Engine::query_with_options`], and whether the result cache answered it (see
    /// [`Engine::set_result_cache`]).
    pub fn query_with_cache_status(
        &self,
        query: &str,
        options: QueryOptions,
    ) -> Result<(QueryResults<'static>, CacheStatus), Error> {
        let hints = Hints::parse(query).map_err(Error::Request)?;
        self.scope(hints, || self.query_hinted(query, options))
    }

    /// Parses `query` once, to run it many times with new values for its variables
    /// ([`Engine::query_prepared`]): the way to pass caller values into a query without writing them
    /// into its text. Hints (`#pragma tt.…`) and the base IRI are read here.
    pub fn prepare_query(
        &self,
        query: &str,
        base_iri: Option<&str>,
    ) -> Result<PreparedQuery, Error> {
        let hints = Hints::parse(query).map_err(Error::Request)?;
        let query = Self::parse_query_with_base(query, base_iri)?;
        Ok(PreparedQuery { query, hints })
    }

    /// Runs a [prepared](Engine::prepare_query) query, with `options.substitutions` bound before
    /// evaluation (each a variable the query selects). As [`Engine::query_with_options`]; the
    /// base IRI of `options` is ignored, the query having been parsed already.
    pub fn query_prepared(
        &self,
        prepared: &PreparedQuery,
        options: QueryOptions,
    ) -> Result<QueryResults<'static>, Error> {
        self.scope(prepared.hints, || {
            self.query_parsed(prepared.query.clone(), options)
                .map(|(r, _)| r)
        })
    }

    fn query_hinted(
        &self,
        query: &str,
        options: QueryOptions,
    ) -> Result<(QueryResults<'static>, CacheStatus), Error> {
        let query = Self::parse_query_with_base(query, options.base_iri.as_deref())?;
        self.query_parsed(query, options)
    }

    fn query_parsed(
        &self,
        mut query: spargebra::Query,
        mut options: QueryOptions,
    ) -> Result<(QueryResults<'static>, CacheStatus), Error> {
        // A user-defined function that is not inlined reads the store from its own query,
        // which the result cache cannot see: such queries are not cached.
        let calls_functions = self.inline_functions(&mut query);
        // Such a function reads the whole store: not for callers who may read only some of
        // it (as SERVICE).
        if calls_functions
            && (options.readable.is_some() || options.rows.is_some() || options.row_rules.is_some())
        {
            return Err(Error::Forbidden(FUNCTIONS_REFUSED.to_owned()));
        }
        // Callers with access to only some graphs, or under row rules, may not call a SERVICE:
        // for them `GRAPH <iri>` reads the store.
        if options.readable.is_none() && options.rows.is_none() && options.row_rules.is_none() {
            self.virtual_graphs(&mut query);
        }
        if let Some(readable) = &options.readable {
            let dataset = readable.narrow(&query, options.dataset.as_ref())?;
            if self.service_dataset.is_none() && query.to_string().contains("SERVICE ") {
                // The st: and window: services read the same graphs as the query.
                let scoped = Self {
                    service_dataset: Some(dataset.clone()),
                    ..self.clone()
                };
                options.dataset = Some(dataset);
                return scoped.query_narrowed(query, options, calls_functions);
            }
            options.dataset = Some(dataset);
        }
        self.query_narrowed(query, options, calls_functions)
    }

    /// [`Engine::query_hinted`] after the dataset was narrowed to the caller's graphs.
    fn query_narrowed(
        &self,
        query: spargebra::Query,
        mut options: QueryOptions,
        calls_functions: bool,
    ) -> Result<(QueryResults<'static>, CacheStatus), Error> {
        if let Some(rules) = options.row_rules.take() {
            let Dataset::TripleTrail(store) = &self.store else {
                return Err(Error::Request(
                    "Row-level access rules need the tripletrail store".to_owned(),
                ));
            };
            // Even when the rules hide nothing now, the query reads the snapshot they were
            // judged on.
            let snapshot = store.snapshot();
            let rows = self.row_filter_for(&snapshot, &rules)?;
            return Ok((
                self.run_query_rows(query, options, rows, Some(snapshot))?,
                CacheStatus::Bypass,
            ));
        }
        if let Some(rows) = options.rows.take().filter(|r| !r.is_empty()) {
            return Ok((
                self.run_query_rows(query, options, rows, None)?,
                CacheStatus::Bypass,
            ));
        }
        let Some((key, generation)) = self
            .cache_key(&query, &options)
            .filter(|_| !calls_functions)
        else {
            return Ok((self.evaluate(query, options)?, CacheStatus::Bypass));
        };
        if let Some(results) = self.results.get(&key, generation) {
            return Ok((results, CacheStatus::Hit));
        }
        let results = self.evaluate(query, options)?;
        Ok((
            self.results.store(key, generation, results),
            CacheStatus::Miss,
        ))
    }

    /// The result cache's key for a query and the store's write generation, or `None` if
    /// its results are not cached.
    fn cache_key(&self, query: &spargebra::Query, options: &QueryOptions) -> Option<(String, u64)> {
        if self.results.budget() == 0
            || Hints::current().skips_cache()
            || !matches!(
                query,
                spargebra::Query::Select { .. } | spargebra::Query::Ask { .. }
            )
        {
            return None;
        }
        let Dataset::TripleTrail(store) = &self.store else {
            return None;
        };
        // Read before evaluating: a write during the evaluation makes the entry stale.
        let generation = store.generation();
        let algebra = cache::normalize(&query.to_string());
        if !cache::cacheable(&algebra) {
            return None;
        }
        let flag = |b: &AtomicBool| if b.load(Ordering::Relaxed) { '1' } else { '0' };
        let mut key = format!(
            "{}{}{}{}\n{:?}\n{:?}\n",
            flag(&self.spatial.enabled),
            flag(&self.spatial.costed),
            flag(&self.spatial.sweep),
            flag(&self.spatial.union_default_graph),
            options.dataset,
            options.base_iri,
        );
        for (variable, value) in &options.substitutions {
            key.push_str(&format!("{variable}={value}\n"));
        }
        key.push_str(&algebra);
        Some((key, generation))
    }

    /// Keeps the results of repeated `SELECT` and `ASK` queries in memory, up to `bytes` (0,
    /// the default, turns the cache off and empties it). A result is kept once it has been
    /// read to the end, if it takes at most a quarter of the budget; the least recently used
    /// are dropped first. Every write to the store invalidates all of them. Queries calling
    /// `RAND`, `NOW`, `UUID`, `STRUUID` or `BNODE`, or a `SERVICE` other than the engine's own,
    /// are not cached. Only on the tripletrail store.
    pub fn set_result_cache(&self, bytes: usize) {
        self.results.set_budget(bytes);
        if bytes == 0 {
            self.results.clear();
        }
    }

    /// The result cache budget of the engines made from now on, in bytes (0 by default: off).
    /// `tripletrail serve` sets it from `--result-cache-mb`.
    pub fn set_default_result_cache(bytes: usize) {
        cache::DEFAULT_BUDGET.store(bytes, Ordering::Relaxed);
    }

    /// What the result cache holds, and its hits and misses so far.
    pub fn result_cache_stats(&self) -> ResultCacheStats {
        self.results.stats()
    }

    fn evaluate(
        &self,
        mut query: spargebra::Query,
        options: QueryOptions,
    ) -> Result<QueryResults<'static>, Error> {
        self.bind_join(&mut query, &options)?;
        let dataset = options.dataset.filter(|d| !d.is_default_dataset());
        if dataset.is_none()
            && !has_own_dataset(&query)
            && let Some(union) = self.union_dataset()
        {
            // The rewrite and the planner as on the default graph (their own queries read
            // the union too), without the shortcuts that read the default graph directly.
            return self.run_query_with(query, Some(&union), options.substitutions, true);
        }
        if dataset.is_some() || !options.substitutions.is_empty() {
            if dataset.is_some() {
                // Marks the query as scoped to a dataset, so the spatial rewrite, the
                // nearest-neighbour path and the parallel aggregates leave the default graph
                // alone; the evaluation dataset is set in `run_query_with`.
                let marker = Some(spargebra::algebra::QueryDataset {
                    default: Vec::new(),
                    named: Some(Vec::new()),
                });
                match &mut query {
                    spargebra::Query::Select { dataset, .. }
                    | spargebra::Query::Construct { dataset, .. }
                    | spargebra::Query::Describe { dataset, .. }
                    | spargebra::Query::Ask { dataset, .. } => *dataset = marker,
                }
            }
            // Queries limited to some graphs (#201) are rewritten from a view of the spatial
            // index holding only their geometries: the index holds every graph's.
            let rewrite = options.readable.is_none();
            if let (Some(readable), Some(dataset)) = (&options.readable, &dataset) {
                return tripletrail_store::table_scope(|| {
                    let query = if text::query_calls_search(&query) {
                        self.search_rewrite(query, Some(dataset))?
                    } else {
                        query
                    };
                    let mut query = self.scoped_spatial_rewrite(query, readable, dataset)?;
                    if options.substitutions.is_empty()
                        && let Dataset::TripleTrail(store) = &self.store
                    {
                        // The marker set above is the query's own dataset: the shortcuts see
                        // the query as one on the default graph, read as `dataset`.
                        let marker = take_dataset(&mut query);
                        let snapshot = store.snapshot();
                        // Zero-length paths from terms the caller's graphs don't hold first,
                        // as under row rules: the shortcuts follow paths from nodes only.
                        query = zero_length::rewrite(query, &snapshot, Some(dataset));
                        query = self.view_shortcuts(query, &snapshot, None, Some(dataset));
                        set_dataset(&mut query, marker);
                    }
                    self.run_query_scoped(query, Some(dataset), options.substitutions, false)
                });
            }
            return self.run_query_with(query, dataset.as_ref(), options.substitutions, rewrite);
        }
        let run = |q: spargebra::Query| -> Result<Vec<Vec<Option<Term>>>, Error> {
            let QueryResults::Solutions(solutions) = self.run_query(q)? else {
                unreachable!("a SELECT query")
            };
            let variables: Vec<Variable> = solutions.variables().to_vec();
            solutions
                .map(|s| {
                    let s = s?;
                    Ok(variables.iter().map(|v| s.get(v).cloned()).collect())
                })
                .collect()
        };
        let source = self.geometry_source();
        let snapshot = match &self.store {
            Dataset::TripleTrail(s) => Some(s.snapshot()),
            Dataset::Oxigraph(_) => None,
        };
        if let Some(q) = aggregate::parallel(
            &query,
            &run,
            &|| self.is_cancelled(),
            source,
            snapshot.as_ref(),
        )? {
            query = q;
        }
        self.run_query(query)
    }

    /// Rewrites `GRAPH <iri> { … }` of a named virtual graph to its SERVICE.
    fn virtual_graphs(&self, query: &mut spargebra::Query) {
        if self.fx_graphs.is_empty() {
            return;
        }
        let (spargebra::Query::Select { pattern, .. }
        | spargebra::Query::Construct { pattern, .. }
        | spargebra::Query::Describe { pattern, .. }
        | spargebra::Query::Ask { pattern, .. }) = query;
        bind_join::virtual_graphs(pattern, &self.fx_graphs);
    }

    /// Sends the values a remote SERVICE or a Facade-X database source joins on along with
    /// it (see [`bind_join::bind_join`]).
    fn bind_join(&self, query: &mut spargebra::Query, options: &QueryOptions) -> Result<(), Error> {
        let fx = self.fx.as_ref().filter(|fx| fx.has_databases());
        if let Some(fx) = fx {
            let takes =
                |iri: &str, inner: &spargebra::algebra::GraphPattern| fx.takes_limit(iri, inner);
            match query {
                spargebra::Query::Select { pattern, .. }
                | spargebra::Query::Construct { pattern, .. } => {
                    bind_join::push_limit(pattern, &takes);
                }
                _ => {}
            }
        }
        #[cfg(feature = "remote-service")]
        let remote = self.remote.as_ref();
        #[cfg(not(feature = "remote-service"))]
        let remote: Option<&()> = None;
        if fx.is_none() && remote.is_none() {
            return Ok(());
        }
        let binds = |iri: &str,
                     inner: &spargebra::algebra::GraphPattern,
                     shared: &[Variable]|
         -> Option<usize> {
            #[cfg(feature = "remote-service")]
            if remote.is_some_and(|r| r.allows(iri)) {
                return Some(bind_join::BIND_JOIN_BATCH);
            }
            fx?.bind_join_batch(iri, inner, shared)
        };
        let (dataset, pattern, base_iri) = match query {
            spargebra::Query::Select {
                dataset,
                pattern,
                base_iri,
            }
            | spargebra::Query::Construct {
                dataset,
                pattern,
                base_iri,
                ..
            }
            | spargebra::Query::Describe {
                dataset,
                pattern,
                base_iri,
            }
            | spargebra::Query::Ask {
                dataset,
                pattern,
                base_iri,
            } => (dataset.clone(), pattern, base_iri.clone()),
        };
        let mut run = |outer: spargebra::algebra::GraphPattern,
                       shared: &[Variable]|
         -> Result<bind_join::Rows, Error> {
            let query = spargebra::Query::Select {
                dataset: dataset.clone(),
                pattern: spargebra::algebra::GraphPattern::Distinct {
                    inner: Box::new(spargebra::algebra::GraphPattern::Project {
                        inner: Box::new(outer),
                        variables: shared.to_vec(),
                    }),
                },
                base_iri: base_iri.clone(),
            };
            let QueryResults::Solutions(solutions) = self.evaluate(query, options.clone())? else {
                unreachable!("a SELECT query")
            };
            let mut rows = Vec::new();
            for solution in solutions {
                if rows.len() == bind_join::BIND_JOIN_MAX_ROWS {
                    return Ok(None);
                }
                let solution = solution?;
                rows.push(shared.iter().map(|v| solution.get(v).cloned()).collect());
            }
            Ok(Some(rows))
        };
        bind_join::bind_join(pattern, &binds, &mut run)
    }

    /// The spatial rewrite of a query by a caller who may read only `readable` graphs (#203),
    /// evaluated on `dataset` (theirs, narrowed): it reads a view of the index holding only
    /// the geometries of those graphs, and evaluates its own queries on `dataset`. Its other
    /// rewrites (counts from statistics, the range index, nearest neighbours) are left out:
    /// they read the whole store. To be called inside a table scope, with the evaluation.
    fn scoped_spatial_rewrite(
        &self,
        mut query: spargebra::Query,
        readable: &ReadableGraphs,
        dataset: &QueryDataset,
    ) -> Result<spargebra::Query, Error> {
        if !self.spatial_rewrite_on() {
            return Ok(query);
        }
        let own = |q: &mut spargebra::Query| match q {
            spargebra::Query::Select { dataset, .. }
            | spargebra::Query::Construct { dataset, .. }
            | spargebra::Query::Describe { dataset, .. }
            | spargebra::Query::Ask { dataset, .. } => std::mem::take(dataset),
        };
        // The query's own dataset (the marker `evaluate` set) is in `dataset` already: the
        // rewrite sees the query as one on the default graph, which `Scoped` reads as
        // `dataset`.
        let marker = own(&mut query);
        let ctx = Scoped {
            engine: self,
            dataset: Some(dataset),
            index: &|| self.view_index(readable),
            rows: None,
        };
        let (mut rewritten, stats) = rewrite_query(query, &ctx);
        match &mut rewritten {
            spargebra::Query::Select { dataset, .. }
            | spargebra::Query::Construct { dataset, .. }
            | spargebra::Query::Describe { dataset, .. }
            | spargebra::Query::Ask { dataset, .. } => *dataset = marker,
        }
        *self.spatial.last.lock_unpoisoned() = stats;
        self.check_cancelled()?;
        Ok(rewritten)
    }

    /// Evaluates a parsed query: the nearest-neighbour path, the spatial rewrite, planning.
    fn run_query(&self, query: spargebra::Query) -> Result<QueryResults<'static>, Error> {
        self.run_query_with(query, None, Vec::new(), true)
    }

    fn run_query_with(
        &self,
        query: spargebra::Query,
        dataset: Option<&QueryDataset>,
        substitutions: Vec<(Variable, Term)>,
        rewrite: bool,
    ) -> Result<QueryResults<'static>, Error> {
        // The result tables the rewrite hands to the evaluation live until it has started.
        tripletrail_store::table_scope(|| {
            self.run_query_scoped(query, dataset, substitutions, rewrite)
        })
    }

    /// `query` with its `tt:search` calls (#188) answered from the full-text index. Only on
    /// the tripletrail store. On a `dataset` (a caller's readable graphs, the protocol's), from
    /// an index of its graphs alone (#203).
    fn search_rewrite(
        &self,
        query: spargebra::Query,
        dataset: Option<&QueryDataset>,
    ) -> Result<spargebra::Query, Error> {
        let Dataset::TripleTrail(store) = &self.store else {
            return Err(Error::Request(
                "tt:search needs the tripletrail store".to_owned(),
            ));
        };
        let source = dataset.map(|d| self.search_source(store, store.snapshot(), Some(d)));
        self.search_rewrite_on(query, store, source, dataset, None)
    }

    /// `update` with the `tt:search` calls of its WHERE clauses answered from the full-text
    /// index (#188), as of the store before the update: a call in an operation after one that
    /// changes data would not see those changes, and is refused. Not yet for a caller limited
    /// to some graphs or rows (`limited`).
    fn search_rewrite_update(
        &self,
        mut update: spargebra::Update,
        limited: bool,
        rewrite: bool,
    ) -> Result<spargebra::Update, Error> {
        use spargebra::GraphUpdateOperation as Op;
        let searches =
            |op: &Op| matches!(op, Op::DeleteInsert { pattern, .. } if text::calls_search(pattern));
        if !update.operations.iter().any(searches) {
            return Ok(update);
        }
        if limited {
            return Err(Error::Request(
                "tt:search and the other full-text searches are not supported in an update \
                 limited by access rules; search in a query first"
                    .to_owned(),
            ));
        }
        let first = update.operations.iter().position(searches).unwrap_or(0);
        if update.operations[..first].iter().any(|op| {
            !matches!(op, Op::DeleteInsert { delete, insert, .. } if delete.is_empty() && insert.is_empty())
        }) {
            return Err(Error::Request(
                "a full-text search in an update must be in its first operation: it reads the \
                 store as it was before the update; send the operations as separate requests"
                    .to_owned(),
            ));
        }
        if update.operations[first + 1..].iter().any(searches) {
            return Err(Error::Request(
                "a full-text search in an update must be in its first operation: it reads the \
                 store as it was before the update; send the operations as separate requests"
                    .to_owned(),
            ));
        }
        let Dataset::TripleTrail(store) = &self.store else {
            return Err(Error::Request(
                "tt:search needs the tripletrail store".to_owned(),
            ));
        };
        if !rewrite {
            return Ok(update);
        }
        let ctx = SearchCtx {
            engine: self,
            store,
            snapshot: OnceLock::new(),
            source: None,
            dataset: None,
            rows: None,
        };
        if let Op::DeleteInsert { using, pattern, .. } = &mut update.operations[first] {
            let p = std::mem::replace(
                pattern.as_mut(),
                spargebra::algebra::GraphPattern::Bgp {
                    patterns: Vec::new(),
                },
            );
            **pattern = text::rewrite_where(p, using.as_ref(), &ctx)?;
        }
        self.check_cancelled()?;
        Ok(update)
    }

    /// [`Self::search_rewrite`] on what `source` shows; `None` for the whole store.
    fn search_rewrite_on(
        &self,
        query: spargebra::Query,
        store: &tripletrail_store::Store,
        source: Option<text::Source>,
        dataset: Option<&QueryDataset>,
        rows: Option<(&Arc<RowFilter>, &tripletrail_store::Snapshot)>,
    ) -> Result<spargebra::Query, Error> {
        let ctx = SearchCtx {
            engine: self,
            store,
            snapshot: OnceLock::new(),
            source,
            dataset,
            rows,
        };
        let rewritten = text::rewrite(query, &ctx)?;
        self.check_cancelled()?;
        Ok(rewritten)
    }

    /// The full-text index of the store's current data: built on the first call, brought up
    /// to date with the changes since on a later one.
    pub fn text_index(&self) -> Result<Arc<text::TextIndex>, Error> {
        let Dataset::TripleTrail(store) = &self.store else {
            return Err(Error::Request(
                "tt:search needs the tripletrail store".to_owned(),
            ));
        };
        let properties = self.text_properties();
        let mut index = self.text.lock_unpoisoned();
        if let Some(i) = &*index
            && i.generation == store.generation()
            && *i.properties() == properties
        {
            return Ok(Arc::clone(i));
        }
        let source = text::Source {
            properties,
            ..text::Source::store(store)
        };
        let unstopped = || Ok(());
        // Patched in place unless a search still holds it, then on a copy.
        let updated = match index.take() {
            Some(mut i) => {
                Arc::make_mut(&mut i).update_from(&source, &unstopped)?;
                i
            }
            None => Arc::new(text::TextIndex::build_from(&source, &unstopped)?),
        };
        *index = Some(Arc::clone(&updated));
        Ok(updated)
    }

    /// What `tt:search` reads for a query on `dataset` (#203): the quads of `snapshot` (a view
    /// of the rows a caller sees, or the whole store) in the dataset's graphs, its default
    /// graph's and its named ones. Every graph for no dataset, or a default graph that is the
    /// union of all.
    fn search_source(
        &self,
        store: &tripletrail_store::Store,
        snapshot: tripletrail_store::Snapshot,
        dataset: Option<&QueryDataset>,
    ) -> text::Source {
        let graphs = dataset.and_then(|d| {
            let default = d.default_graph_graphs()?;
            let mut ids = Vec::new();
            for g in default {
                match g {
                    GraphName::DefaultGraph => ids.push(tripletrail_store::TermId(0)),
                    GraphName::NamedNode(n) => ids.extend(snapshot.term_id(n.as_ref().into())),
                    GraphName::BlankNode(b) => ids.extend(snapshot.term_id(b.as_ref().into())),
                }
            }
            let named = match d.available_named_graphs() {
                Some(named) => named.to_vec(),
                None => store.named_graphs(),
            };
            for g in &named {
                ids.extend(snapshot.term_id(g.as_ref().into()));
            }
            ids.sort_unstable();
            ids.dedup();
            Some(ids)
        });
        text::Source {
            snapshot,
            graphs,
            properties: self.text_properties(),
        }
    }

    /// The full-text index of what `source` shows (#203), with statistics of its own: a
    /// search on it ranks as on a store holding only that. Made on first use and brought up
    /// to date like [`Self::text_index`], for the same graphs and row filter; a few are kept.
    fn text_view(&self, source: &text::Source) -> Result<Arc<text::TextIndex>, Error> {
        const VIEWS_KEPT: usize = 8;
        let key = view_key(source);
        let generation = source.snapshot.generation().unwrap_or_default();
        // The view's slot, made if new; the list's lock is held only to find it.
        let slot = {
            let mut views = self.text_views.lock_unpoisoned();
            match views.iter().position(|(k, _)| *k == key) {
                Some(i) => {
                    // The latest used last, so the least recently used goes first.
                    let view = views.remove(i);
                    let slot = Arc::clone(&view.1);
                    views.push(view);
                    slot
                }
                None => {
                    if views.len() >= VIEWS_KEPT {
                        views.remove(0);
                    }
                    let slot = Arc::default();
                    views.push((key, Arc::clone(&slot)));
                    slot
                }
            }
        };
        // Searches of this view wait for the one that builds or patches it, then share it;
        // searches of other views go on. A waiter whose deadline passes gives up.
        let mut index = loop {
            match slot.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(e)) => break e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    self.check_cancelled()?;
                    std::thread::sleep(std::time::Duration::from_micros(500));
                }
            }
        };
        if let Some(i) = &*index
            && i.generation == generation
            && *i.properties() == source.properties
        {
            return Ok(Arc::clone(i));
        }
        // The builder stops at the query's deadline (a caller can ask for a new view with
        // each dataset). Taken out while it is patched: in place unless a search still holds
        // it, and gone if the patch fails.
        let stop = || self.check_cancelled();
        let updated = match index.take() {
            Some(mut i) => {
                Arc::make_mut(&mut i).update_from(source, &stop)?;
                self.text_view_work[1].fetch_add(1, Ordering::Relaxed);
                i
            }
            None => {
                let i = Arc::new(text::TextIndex::build_from(source, &stop)?);
                self.text_view_work[0].fetch_add(1, Ordering::Relaxed);
                i
            }
        };
        *index = Some(Arc::clone(&updated));
        Ok(updated)
    }

    /// The vector index of `property` in what `source` shows (`None`: the whole store), for
    /// `tt:nearest` (#236): built on first use, brought up to date after writes, and shared
    /// by the queries of the same view; the latest 16 are kept.
    pub fn vector_index(
        &self,
        source: Option<&text::Source>,
        property: &NamedNode,
    ) -> Result<Arc<vector::VectorIndex>, Error> {
        const KEPT: usize = 16;
        let Dataset::TripleTrail(store) = &self.store else {
            return Err(Error::Request(
                "tt:nearest needs the tripletrail store".to_owned(),
            ));
        };
        let whole;
        let (source, key) = match source {
            Some(s) => (s, view_key(s).max(1)),
            None => {
                whole = text::Source::store(store);
                (&whole, 0)
            }
        };
        let slot = {
            let mut slots = self.vectors.lock_unpoisoned();
            match slots
                .iter()
                .position(|(k, p, _)| *k == key && p == property)
            {
                Some(i) => {
                    let found = slots.remove(i);
                    let slot = Arc::clone(&found.2);
                    slots.push(found);
                    slot
                }
                None => {
                    if slots.len() >= KEPT {
                        slots.remove(0);
                    }
                    let slot = Arc::default();
                    slots.push((key, property.clone(), Arc::clone(&slot)));
                    slot
                }
            }
        };
        let mut index = loop {
            match slot.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(e)) => break e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    self.check_cancelled()?;
                    std::thread::sleep(std::time::Duration::from_micros(500));
                }
            }
        };
        let generation = source.snapshot.generation().unwrap_or_default();
        let graph_min = self.vector_graph_min.load(Ordering::Relaxed);
        // The graphs of the whole store's vectors are kept beside the routing networks, so
        // a restart reads them; those of a caller's view are built again.
        let file = (key == 0)
            .then(|| self.networks.dir())
            .flatten()
            .map(|dir| {
                dir.join("vectors")
                    .join(format!("{:016x}", fnv64(property.as_str())))
            });
        if let Some(i) = &*index
            && i.generation == generation
        {
            i.set_graph_min(graph_min);
            i.set_graph_file(file);
            return Ok(Arc::clone(i));
        }
        let stop = || self.check_cancelled();
        let updated = match index.take() {
            Some(mut i) => {
                Arc::make_mut(&mut i).update_from(source, &stop)?;
                i
            }
            None => Arc::new(vector::VectorIndex::build_from(source, property, &stop)?),
        };
        updated.set_graph_min(graph_min);
        updated.set_graph_file(file);
        *index = Some(Arc::clone(&updated));
        Ok(updated)
    }

    /// How many full-text indexes of callers' views (#203) this engine has built, and how
    /// many times it brought one up to date after a write.
    #[doc(hidden)]
    pub fn text_view_work(&self) -> (usize, usize) {
        (
            self.text_view_work[0].load(Ordering::Relaxed),
            self.text_view_work[1].load(Ordering::Relaxed),
        )
    }

    /// A row filter (#202) for this engine's store from terms: `resources` hidden as subjects
    /// (and as objects, with `references`), `predicates` hidden everywhere, and `(subject,
    /// predicate)` pairs hidden on that subject only. Terms the store does not hold are left
    /// out, as they hide nothing. Only on the tripletrail store.
    pub fn row_filter(
        &self,
        resources: &[NamedOrBlankNode],
        references: bool,
        predicates: &[NamedNode],
        subject_predicates: &[(NamedOrBlankNode, NamedNode)],
    ) -> Result<RowFilter, Error> {
        let Dataset::TripleTrail(store) = &self.store else {
            return Err(Error::Request(
                "Row-level access rules need the tripletrail store".to_owned(),
            ));
        };
        let id = |t: oxigraph::model::TermRef<'_>| store.term_id(t);
        let mut filter = RowFilter::new();
        for r in resources {
            if let Some(i) = id(r.as_ref().into()) {
                filter.hide_resource(i, references);
            }
        }
        for p in predicates {
            if let Some(i) = id(p.as_ref().into()) {
                filter.hide_predicate(i);
            }
        }
        for (s, p) in subject_predicates {
            if let (Some(s), Some(p)) = (id(s.as_ref().into()), id(p.as_ref().into())) {
                filter.hide_predicate_on(s, p);
            }
        }
        Ok(filter)
    }

    /// A query on a view without the quads `rows` hides (#202): evaluated on that view, so
    /// every scan, and with it every join, `OPTIONAL`, `MINUS`, `EXISTS`, path and
    /// aggregate, sees only what the caller may see.
    /// `query` with the count and id-row shortcuts (#204) a caller limited to some graphs or
    /// rows can take: those `run_query_scoped` takes on the whole store, on a view of what
    /// the caller sees. `rows` is what row rules hide; `dataset` the caller's (their graphs,
    /// the union, or `None` for the default graph). The view hides the quads of other graphs,
    /// the copies of a triple in several of the caller's graphs (each triple once, as their
    /// merge reads it) and the rows: the shortcuts read scans of it, as they read the union
    /// of all graphs. Unchanged for a query with a dataset of its own, and when nothing is
    /// taken. To be called inside a table scope, with the evaluation.
    fn view_shortcuts(
        &self,
        query: spargebra::Query,
        base: &Snapshot,
        rows: Option<&Arc<tripletrail_store::RowFilter>>,
        dataset: Option<&QueryDataset>,
    ) -> spargebra::Query {
        use oxigraph::model::GraphName;
        if has_own_dataset(&query) {
            return query;
        }
        let cancelled = || self.is_cancelled();
        let merge = |hidden: Option<Arc<tripletrail_store::RowFilter>>| match (hidden, rows) {
            (Some(h), Some(r)) => Some(Arc::new(h.merged(r))),
            (h, r) => h.or_else(|| r.cloned()),
        };
        let (filter, graph) = match dataset.map(QueryDataset::default_graph_graphs) {
            None => (rows.cloned(), GraphSelector::Default),
            Some(None) => {
                let Some(scan) = union_scan(&query, base) else {
                    return query;
                };
                let Some(filter) = self.view_filter(base, None, rows, || {
                    base.union_copies(scan, &cancelled).map(merge)
                }) else {
                    return query;
                };
                (filter, GraphSelector::All)
            }
            Some(Some(graphs)) => {
                let mut ids: Vec<tripletrail_store::TermId> = graphs
                    .iter()
                    .filter_map(|g| match g {
                        GraphName::DefaultGraph => Some(tripletrail_store::TermId::DEFAULT_GRAPH),
                        GraphName::NamedNode(n) => base.term_id(n.as_ref().into()),
                        GraphName::BlankNode(b) => base.term_id(b.as_ref().into()),
                    })
                    .collect();
                ids.sort_unstable();
                ids.dedup();
                match ids.as_slice() {
                    // One graph: read as it is.
                    [g] if *g == tripletrail_store::TermId::DEFAULT_GRAPH => {
                        (rows.cloned(), GraphSelector::Default)
                    }
                    [g] => (rows.cloned(), GraphSelector::Graph(*g)),
                    _ => {
                        let Some(scan) = union_scan(&query, base) else {
                            return query;
                        };
                        let key = ids.iter().map(|g| g.0).collect();
                        let Some(filter) = self.view_filter(base, Some(key), rows, || {
                            base.copies_within(&ids, scan, &cancelled)
                                .map(|within| merge(Some(within)))
                        }) else {
                            return query;
                        };
                        (filter, GraphSelector::All)
                    }
                }
            }
        };
        let view = base.clone().with_rows(filter);
        let (query, counted) = count::rewrite(query, &view, graph, &cancelled);
        let (query, read) = rows::rewrite(query, &view, graph, &cancelled);
        self.view_shortcuts
            .fetch_add((counted + read) as u64, Ordering::Relaxed);
        query
    }

    /// The filter of a view of [`Engine::view_shortcuts`] showing `graphs` (`None` for all)
    /// under `rows`, as `make` gives it, kept per store version. `None` when `make` gives none.
    fn view_filter(
        &self,
        base: &Snapshot,
        graphs: Option<Vec<u64>>,
        rows: Option<&Arc<tripletrail_store::RowFilter>>,
        make: impl FnOnce() -> Option<Option<Arc<tripletrail_store::RowFilter>>>,
    ) -> Option<Option<Arc<tripletrail_store::RowFilter>>> {
        let Some(generation) = base.generation() else {
            return make();
        };
        let key: ViewKey = (generation, graphs, rows.map_or(0, |r| r.fingerprint()));
        // What decides what a caller sees is never matched by a fingerprint alone.
        let same_rows = |kept: &ViewFilter| match (kept, rows) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) || **a == **b,
            _ => false,
        };
        {
            let mut kept = self.view_filters.lock_unpoisoned();
            if let Some(i) = kept.iter().position(|(k, r, _)| *k == key && same_rows(r)) {
                let entry = kept.remove(i);
                let filter = entry.2.clone();
                kept.push(entry);
                return Some(filter);
            }
        }
        let filter = make()?;
        let mut kept = self.view_filters.lock_unpoisoned();
        // Those of older versions are of no more use.
        kept.retain(|(k, _, _)| k.0 >= generation);
        if kept.len() >= VIEW_FILTERS {
            kept.remove(0);
        }
        kept.push((key, rows.cloned(), filter.clone()));
        Some(filter)
    }

    fn run_query_rows(
        &self,
        query: spargebra::Query,
        options: QueryOptions,
        rows: Arc<RowFilter>,
        snapshot: Option<tripletrail_store::Snapshot>,
    ) -> Result<QueryResults<'static>, Error> {
        let Dataset::TripleTrail(store) = &self.store else {
            return Err(Error::Request(
                "Row-level access rules need the tripletrail store".to_owned(),
            ));
        };
        // The built-in services read what the query reads (#202); others, and one inside
        // another, are refused.
        let text = query.to_string();
        if text.contains("SERVICE ") {
            let (spargebra::Query::Select { pattern, .. }
            | spargebra::Query::Construct { pattern, .. }
            | spargebra::Query::Describe { pattern, .. }
            | spargebra::Query::Ask { pattern, .. }) = &query;
            scoped_services_only(pattern, &text, "under row-level access rules")?;
        }
        let union = !has_own_dataset(&query);
        let dataset = options
            .dataset
            .filter(|d| !d.is_default_dataset())
            .or_else(|| self.union_dataset().filter(|_| union));
        tripletrail_store::table_scope(|| {
            let base = snapshot.unwrap_or_else(|| store.snapshot());
            let filtered = base.clone().with_rows(Some(Arc::clone(&rows)));
            // Searched in what the caller sees (#203).
            let query = if text::query_calls_search(&query) {
                let source = self.search_source(store, filtered.clone(), dataset.as_ref());
                self.search_rewrite_on(
                    query,
                    store,
                    Some(source),
                    dataset.as_ref(),
                    Some((&rows, &base)),
                )?
            } else {
                query
            };
            let query = zero_length::rewrite(query, &filtered, dataset.as_ref());
            // Substituted variables are bound already: they are not renamed.
            let query = if options.substitutions.is_empty() {
                equality::rewrite_query(query, &resources(&filtered))
            } else {
                query
            };
            // The spatial rewrite reads a view of the index holding only the geometries the
            // view shows, and evaluates its own queries on the view: what it hands on is
            // visible rows only. Not for a query with its own dataset.
            let query = if !has_own_dataset(&query) && self.spatial_rewrite_on() {
                let ctx = Scoped {
                    engine: self,
                    dataset: dataset.as_ref(),
                    index: &|| self.rows_index(options.readable.as_ref(), &rows, &filtered),
                    rows: Some((&rows, &base)),
                };
                let (rewritten, stats) = rewrite_query(query, &ctx);
                *self.spatial.last.lock_unpoisoned() = stats;
                self.check_cancelled()?;
                rewritten
            } else {
                query
            };
            // Substituted variables are bound when evaluation starts: the shortcuts would not
            // see them.
            let query = if options.substitutions.is_empty() {
                self.view_shortcuts(query, &base, Some(&rows), dataset.as_ref())
            } else {
                query
            };
            let evaluator = if text.contains("SERVICE ") {
                Self {
                    service_rows: Some(tripletrail_store::RowView {
                        rows: Arc::clone(&rows),
                        base: base.clone(),
                    }),
                    service_dataset: self.service_dataset.clone().or_else(|| dataset.clone()),
                    ..self.clone()
                }
                .evaluator()
            } else {
                self.evaluator()
            };
            let mut prepared = evaluator.for_query(plan::plan_query(query));
            if let Some(dataset) = &dataset {
                *prepared.dataset_mut() = dataset.clone();
            }
            for (variable, value) in options.substitutions {
                prepared = prepared.substitute_variable(variable, value);
            }
            Ok(self.store.execute_with_rows(prepared, rows, base)?)
        })
    }

    /// Sets which executor runs queries by default (#11): `true` (the default) for the id
    /// executor, which answers whole queries from tables of term ids when they cover them and
    /// hands the rest to the evaluator, `false` for the evaluator alone. `#pragma tt.executor` overrides it per query.
    pub fn set_default_executor_ids(ids: bool) {
        EXECUTOR_IDS.store(ids, Ordering::Relaxed);
    }

    fn run_query_scoped(
        &self,
        mut query: spargebra::Query,
        dataset: Option<&QueryDataset>,
        substitutions: Vec<(Variable, Term)>,
        rewrite: bool,
    ) -> Result<QueryResults<'static>, Error> {
        if text::query_calls_search(&query) {
            query = self.search_rewrite(query, dataset)?;
        }
        // Substituted variables are bound when evaluation starts. The spatial rewrite keeps
        // every variable it is given, so what it hands on still meets those bindings; the
        // shortcuts that answer a whole query or aggregate from the store do not see them.
        let shortcuts = rewrite && substitutions.is_empty();
        // On a dataset (the union default graph, a caller's readable graphs) from a copy of
        // its vectors only, as `tt:nearest`; not for a query with a dataset of its own.
        if substitutions.is_empty()
            && (rewrite || dataset.is_some())
            && (dataset.is_some() || !has_own_dataset(&query))
            && let Some(order) = cosine_order::cosine_order(&query)
            && let Some(results) = self.most_similar(&query, &order, dataset)?
        {
            return Ok(results);
        }
        if rewrite && self.spatial_rewrite_on() {
            if dataset.is_none()
                && shortcuts
                && let Some(knn) = tripletrail_spatial::knn(&query)
                && let Some(results) = self.nearest(&query, &knn)?
            {
                return Ok(results);
            }
            let (rewritten, stats) = rewrite_query(query, self);
            query = rewritten;
            if dataset.is_none() && shortcuts {
                query = range::rewrite(query, self).0;
            }
            *self.spatial.last.lock_unpoisoned() = stats;
            self.check_cancelled()?;
        }
        // Joins written as filters (#185), before the count shortcuts read the pattern.
        // Substituted variables are bound already: they are not renamed.
        if shortcuts && let Dataset::TripleTrail(store) = &self.store {
            query = equality::rewrite_query(query, &resources(&store.snapshot()));
        }
        // The view the rewrites read is the one the query is evaluated on: the terms they make
        // up are known only to it (#11).
        let mut base = None;
        if shortcuts && let Dataset::TripleTrail(store) = &self.store {
            let snapshot = store.snapshot();
            base = Some(snapshot.clone());
            // The union of all graphs as the default graph: every graph, without the copies
            // of a triple held by several (#808), so each triple is read once.
            let on = match dataset {
                None => Some((snapshot, GraphSelector::Default)),
                Some(d) if d.default_graph_graphs().is_none() => {
                    union_ids(&query, snapshot, &|| self.is_cancelled())
                }
                Some(_) => None,
            };
            if let Some((snapshot, graph)) = on {
                query = count::rewrite(query, &snapshot, graph, &|| self.is_cancelled()).0;
                // The id executor (#11): the whole query from tables of ids, if they cover it.
                if executor_ids() && !Hints::current().id_rewrites_off {
                    // With the tables the count pass just registered (groups read back, #185).
                    let tabled = snapshot.clone().with_pending_tables();
                    let columns = |p: &NamedNode| self.spatial.ranges.column(self, p);
                    let eval = idtree::Eval {
                        snapshot: &tabled,
                        cancelled: &|| self.is_cancelled(),
                        graph,
                        // Not for a caller with a dataset (readable graphs): as the range
                        // rewrite, the columns are the whole default graph's.
                        columns: (dataset.is_none() && graph == GraphSelector::Default)
                            .then_some(&columns),
                    };
                    if let Some(answer) = direct::answer(&query, &eval) {
                        self.check_cancelled()?;
                        return Ok(direct::results(Arc::new(answer)));
                    }
                    if let Some(triples) = direct::construct(&query, &eval) {
                        self.check_cancelled()?;
                        return Ok(triples);
                    }
                }
                query = rows::rewrite(query, &snapshot, graph, &|| self.is_cancelled()).0;
                if graph == GraphSelector::Default {
                    measures::warm(&query, &snapshot, &|| self.is_cancelled());
                }
            }
        }
        if let Dataset::TripleTrail(store) = &self.store {
            let snapshot = base.get_or_insert_with(|| store.snapshot());
            query = zero_length::rewrite(query, snapshot, dataset);
        }
        let mut prepared = self.evaluator().for_query(plan::plan_query(query));
        if let Some(dataset) = dataset {
            *prepared.dataset_mut() = dataset.clone();
        }
        for (variable, value) in substitutions {
            prepared = prepared.substitute_variable(variable, value);
        }
        Ok(self.store.execute_on(prepared, base)?)
    }

    /// Evaluates a query to the end and returns how it was evaluated: Oxigraph's plan with
    /// row counts and timings per operator, and what the spatial rewrite did, as JSON.
    pub fn explain(&self, query: &str) -> Result<String, Error> {
        let hints = Hints::parse(query).map_err(Error::Request)?;
        self.scope(hints, || {
            tripletrail_store::table_scope(|| self.explain_scoped(query))
        })
    }

    /// [`Self::explain`] for a caller who may read only `readable` graphs (#203): the query is
    /// evaluated on its dataset narrowed to them, as [`QueryOptions::readable`] does, and only
    /// its row count is given (`{"rows":N,"restricted":true}`). The plan and the spatial
    /// rewrite's counters are left out: they are chosen from what the whole store holds, so
    /// they would tell whether other graphs hold a guessed triple.
    pub fn explain_readable(
        &self,
        query: &str,
        readable: &ReadableGraphs,
    ) -> Result<String, Error> {
        let hints = Hints::parse(query).map_err(Error::Request)?;
        self.scope(hints, || {
            tripletrail_store::table_scope(|| self.explain_scoped_in(query, Some(readable)))
        })
    }

    /// The triple patterns of a query's `WHERE` (#202, the access explain page), each as a
    /// `WHERE` body of its own, inside its `GRAPH` when it has one. Property paths and the
    /// patterns of `EXISTS` filters are left out.
    pub fn query_patterns(query: &str) -> Result<Vec<String>, Error> {
        use spargebra::algebra::GraphPattern as P;
        fn walk(p: &P, graph: Option<&str>, out: &mut Vec<String>) {
            match p {
                P::Bgp { patterns } => {
                    for t in patterns {
                        let text = match graph {
                            Some(g) => format!("GRAPH {g} {{ {t} }}"),
                            None => format!("{t}"),
                        };
                        if !out.contains(&text) {
                            out.push(text);
                        }
                    }
                }
                P::Join { left, right }
                | P::LeftJoin { left, right, .. }
                | P::Lateral { left, right }
                | P::Union { left, right }
                | P::Minus { left, right } => {
                    walk(left, graph, out);
                    walk(right, graph, out);
                }
                P::Graph { name, inner } => walk(inner, Some(&name.to_string()), out),
                P::Filter { inner, .. }
                | P::Extend { inner, .. }
                | P::OrderBy { inner, .. }
                | P::Project { inner, .. }
                | P::Distinct { inner }
                | P::Reduced { inner }
                | P::Slice { inner, .. }
                | P::Group { inner, .. } => walk(inner, graph, out),
                P::Path { .. } | P::Values { .. } | P::Service { .. } => {}
            }
        }
        let query = spargebra::SparqlParser::new()
            .parse_query(query)
            .map_err(|e| Error::Request(e.to_string()))?;
        let pattern = match &query {
            spargebra::Query::Select { pattern, .. }
            | spargebra::Query::Construct { pattern, .. }
            | spargebra::Query::Describe { pattern, .. }
            | spargebra::Query::Ask { pattern, .. } => pattern,
        };
        let mut out = Vec::new();
        walk(pattern, None, &mut out);
        Ok(out)
    }

    /// [`Self::explain`] for a caller under row rules (#202), with `options` as the query
    /// would get them (`row_rules`, `readable`): the query is evaluated as such a query is,
    /// and only its row count is given (`{"rows":N,"restricted":true}`), as
    /// [`Self::explain_readable`] gives it, for the same reason.
    pub fn explain_restricted(&self, query: &str, options: QueryOptions) -> Result<String, Error> {
        let rows = match self.query_with_options(query, options)? {
            QueryResults::Solutions(s) => s
                .map(|r| r.map(|_| ()))
                .collect::<Result<Vec<_>, _>>()?
                .len(),
            QueryResults::Graph(t) => t
                .map(|r| r.map(|_| ()))
                .collect::<Result<Vec<_>, _>>()?
                .len(),
            QueryResults::Boolean(_) => 1,
        };
        Ok(format!(r#"{{"rows":{rows},"restricted":true}}"#))
    }

    fn explain_scoped(&self, query: &str) -> Result<String, Error> {
        self.explain_scoped_in(query, None)
    }

    fn explain_scoped_in(
        &self,
        query: &str,
        readable: Option<&ReadableGraphs>,
    ) -> Result<String, Error> {
        let mut query = Self::parse_query(query)?;
        if self.inline_functions(&mut query) && readable.is_some() {
            return Err(Error::Forbidden(FUNCTIONS_REFUSED.to_owned()));
        }
        if readable.is_none() {
            self.virtual_graphs(&mut query);
        }
        // Database calls are shown as they are made, batched as a query would batch them.
        let (bound, mut calls) = tripletrail_fx::record_database_calls(|| match readable {
            Some(_) => Ok(()),
            None => self.bind_join(&mut query, &QueryOptions::default()),
        });
        bound?;
        let mut stats = RewriteStats::default();
        let mut ranges = Vec::new();
        // Asked once: a named graph committed while the query is prepared must not make its
        // rewrites read the default graph and its evaluation the union (#808).
        let reads_union = self.reads_named_graphs_as_default();
        let narrowed = match readable {
            // Explain counts rows without the services' scope: no SERVICE at all here.
            Some(_) if query.to_string().contains("SERVICE ") => {
                return Err(Error::Request(
                    "SERVICE is not available with access to only some graphs".to_owned(),
                ));
            }
            Some(r) => Some(r.narrow(&query, None)?),
            None => None,
        };
        if let (Some(readable), Some(dataset)) = (readable, &narrowed) {
            query = self.scoped_spatial_rewrite(query, readable, dataset)?;
            stats = self.last_spatial_stats();
            stats.decisions.clear();
        } else if self.spatial_rewrite_on() {
            let (rewritten, s) = rewrite_query(query, self);
            stats = s;
            if reads_union && !has_own_dataset(&rewritten) {
                query = rewritten;
            } else {
                (query, ranges) = range::rewrite(rewritten, self);
            }
        }
        // While the ids executor is on: whether it answers the query or hands it on.
        let mut executor = None;
        let union = !has_own_dataset(&query);
        if let Dataset::TripleTrail(store) = &self.store {
            query = equality::rewrite_query(query, &resources(&store.snapshot()));
        }
        // As run_query_scoped: evaluated on the view the rewrites read.
        let mut base = None;
        if narrowed.is_none()
            && let Dataset::TripleTrail(store) = &self.store
        {
            let snapshot = store.snapshot();
            base = Some(snapshot.clone());
            // As run_query_scoped: the union reads every graph without the copies (#808).
            let on = if union && reads_union {
                union_ids(&query, snapshot, &|| self.is_cancelled())
            } else {
                Some((snapshot, GraphSelector::Default))
            };
            if let Some((snapshot, graph)) = on {
                query = count::rewrite(query, &snapshot, graph, &|| self.is_cancelled()).0;
                // Which executor answers it (#11), as run_query_scoped decides.
                if executor_ids() && !Hints::current().id_rewrites_off {
                    let tabled = snapshot.clone().with_pending_tables();
                    let columns = |p: &NamedNode| self.spatial.ranges.column(self, p);
                    let eval = idtree::Eval {
                        snapshot: &tabled,
                        cancelled: &|| self.is_cancelled(),
                        graph,
                        columns: (graph == GraphSelector::Default).then_some(&columns),
                    };
                    executor = Some(
                        if direct::answer(&query, &eval).is_some()
                            || direct::construct(&query, &eval).is_some()
                        {
                            "ids"
                        } else {
                            "spareval"
                        },
                    );
                }
                query = rows::rewrite(query, &snapshot, graph, &|| self.is_cancelled()).0;
            }
        }
        let dataset = match &narrowed {
            Some(d) => Some(d.clone()),
            None => (union && reads_union).then(union_dataset),
        };
        if let Dataset::TripleTrail(store) = &self.store {
            let snapshot = base.get_or_insert_with(|| store.snapshot());
            query = zero_length::rewrite(query, snapshot, dataset.as_ref());
        }
        let mut prepared = self.evaluator().for_query(plan::plan_query(query));
        if let Some(d) = dataset {
            *prepared.dataset_mut() = d;
        }
        let (result, evaluated) = tripletrail_fx::record_database_calls(|| -> Result<_, Error> {
            let (results, explanation) = self.store.explain(prepared, base);
            let rows = match results? {
                QueryResults::Solutions(s) => s
                    .map(|r| r.map(|_| ()))
                    .collect::<Result<Vec<_>, _>>()?
                    .len(),
                QueryResults::Graph(t) => t
                    .map(|r| r.map(|_| ()))
                    .collect::<Result<Vec<_>, _>>()?
                    .len(),
                QueryResults::Boolean(_) => 1,
            };
            Ok((explanation, rows))
        });
        let (explanation, rows) = result?;
        calls.extend(evaluated);
        if narrowed.is_some() {
            // The plan and the rewrite's counters depend on what the whole store holds (the
            // planner's statistics, the properties some graph asserts): shown to a caller who
            // may read only some graphs, they would tell whether hidden graphs hold a guessed
            // triple. Such a caller gets the row count alone.
            return Ok(format!(r#"{{"rows":{rows},"restricted":true}}"#));
        }
        let mut plan = Vec::new();
        explanation
            .write_in_json(&mut plan)
            .map_err(oxigraph::store::StorageError::Io)?;
        let decisions: Vec<String> = stats
            .decisions
            .iter()
            .map(|d| d.to_string())
            .chain(ranges)
            .map(|d| json_string(&d))
            .collect();
        let databases = if calls.is_empty() {
            String::new()
        } else {
            let calls: Vec<String> = calls
                .iter()
                .map(|c| {
                    let params: Vec<String> = c.params.iter().map(|p| json_string(p)).collect();
                    format!(
                        r#"{{"connection":{},"sql":{},"params":[{}],"rows":{},"ms":{:.3}}}"#,
                        c.connection.as_deref().map_or("null".into(), json_string),
                        json_string(&c.sql),
                        params.join(","),
                        c.rows,
                        c.millis
                    )
                })
                .collect();
            format!(r#","databases":[{}]"#, calls.join(","))
        };
        // The query hints in force (#238), as `#pragma` lines.
        let active = Hints::current();
        let hints = if active.is_empty() {
            String::new()
        } else {
            let lines: Vec<String> = active
                .to_string()
                .lines()
                .map(|l| json_string(&format!("#pragma {l}")))
                .collect();
            format!(r#","hints":[{}]"#, lines.join(","))
        };
        let executor = executor.map_or(String::new(), |e| format!(r#","executor":"{e}""#));
        Ok(format!(
            r#"{{"rows":{rows}{databases}{hints}{executor},"spatial":{{"filters":{},"joins":{},"triples":{},"virtual_properties":{},"candidates":{},"skipped":{},"sides_evaluated":{},"sweeps":{},"pinned":{},"materialized":{},"lookup_ms":{},"decisions":[{}]}},"plan":{}}}"#,
            stats.filters,
            stats.joins,
            stats.triples,
            stats.virtual_properties,
            stats.candidates,
            stats.skipped,
            stats.sides_evaluated,
            stats.sweeps,
            stats.pinned,
            stats.materialized,
            stats.lookup.as_secs_f64() * 1000.0,
            decisions.join(","),
            String::from_utf8_lossy(&plan)
        ))
    }

    /// Answers `ORDER BY distance LIMIT k` through the index: evaluates the query restricted
    /// to the geometries within `r` for growing `r` until it returns `k` rows (see
    /// [`tripletrail_spatial::Knn`]). `None` if the index cannot answer it.
    fn nearest(
        &self,
        query: &spargebra::Query,
        knn: &tripletrail_spatial::Knn,
    ) -> Result<Option<QueryResults<'static>>, Error> {
        // Metres or CRS units, depending on the distance function. The first radius takes in a
        // few times `limit` indexed geometries; each round grows it by the shortfall, until it
        // takes in every geometry, or for 16 rounds, after which the full query is used.
        let mut radius = knn.first_radius(self).unwrap_or(10.0);
        let mut total = RewriteStats::default();
        for _ in 0..16 {
            let Some(bounded) = knn.bounded(query, radius, self) else {
                return Ok(None);
            };
            total.filters += 1;
            total.candidates = bounded.stats.candidates;
            total.lookup += bounded.stats.lookup;
            let QueryResults::Solutions(solutions) = self
                .store
                .execute(self.evaluator().for_query(plan::plan_query(bounded.query)))?
            else {
                return Ok(None);
            };
            let variables: Arc<[Variable]> = solutions.variables().into();
            let rows = solutions.collect::<Result<Vec<_>, _>>()?;
            if rows.len() >= knn.limit() || bounded.complete {
                *self.spatial.last.lock_unpoisoned() = total;
                return Ok(Some(QueryResults::Solutions(QuerySolutionIter::new(
                    variables,
                    rows.into_iter().map(Ok),
                ))));
            }
            radius = knn.next_radius(radius, rows.len());
        }
        Ok(None)
    }

    /// Answers `ORDER BY DESC(tt:cosine(?v, vector)) LIMIT k` through the vector index (#236):
    /// evaluates the query with `?v` restricted to the `n` vectors most similar, and to the
    /// rows whose cosine is above what the vectors left out can reach, until it returns `k`
    /// rows (see [`cosine_order`]). Two rounds at most, from one comparison of the vectors:
    /// the second as large as the first says it takes, up to sixteen times the first and a
    /// quarter of the vectors; past that, or when the first keeps no rows, the query is
    /// evaluated as written, as is one whose `OFFSET` and `LIMIT` add up to more than a sixteenth of the vectors.
    /// `None` if the index does not answer it.
    fn most_similar(
        &self,
        query: &spargebra::Query,
        order: &cosine_order::CosineOrder,
        dataset: Option<&QueryDataset>,
    ) -> Result<Option<QueryResults<'static>>, Error> {
        let Dataset::TripleTrail(store) = &self.store else {
            return Ok(None);
        };
        let source = dataset.map(|d| self.search_source(store, store.snapshot(), Some(d)));
        let index = self.vector_index(source.as_ref(), &order.property)?;
        // A plain string in the form of a vector has a cosine too, but no place in the index.
        if order.limit == 0 || index.strings() > 0 {
            return Ok(None);
        }
        let dim = order.vector.len();
        let all = index.len_of(dim);
        // The rows the `ORDER BY` must give: those skipped by the `OFFSET` too.
        let need = order.offset.saturating_add(order.limit);
        if all == 0 || need.saturating_mul(16) > all {
            return Ok(None);
        }
        let first = (4 * need).max(64).min(all);
        let most = (first * 16).min(all / 4).max(first);
        let error = vector::VectorIndex::exact_error(dim);
        let hits = index.exact(&order.vector, most);
        let mut n = first;
        for round in 0..2 {
            self.check_cancelled()?;
            let kept = &hits[..n.min(hits.len())];
            let complete = kept.len() >= all;
            // A vector left out scores at most the last one kept, so its cosine is at most
            // that plus the error: the rows above it are the first of the whole query.
            let above = match kept.last() {
                Some(last) if !complete => Some(last.score + error),
                _ => None,
            };
            let mut literals: Vec<_> = kept.iter().map(|h| h.literal).collect();
            literals.sort_unstable();
            literals.dedup();
            if literals.is_empty() {
                return Ok(None);
            }
            let Some(table) = SpatialContext::table(
                self,
                std::slice::from_ref(&order.var),
                tripletrail_store::IdRows {
                    width: 1,
                    ids: literals,
                },
            ) else {
                return Ok(None);
            };
            let mut prepared = self
                .evaluator()
                .for_query(plan::plan_query(order.restricted(query, table, above)));
            if let Some(dataset) = dataset {
                *prepared.dataset_mut() = dataset.clone();
            }
            let QueryResults::Solutions(solutions) = self.store.execute(prepared)? else {
                return Ok(None);
            };
            let variables: Arc<[Variable]> = solutions.variables().into();
            let rows = solutions.collect::<Result<Vec<_>, _>>()?;
            if rows.len() >= order.limit {
                self.cosine_orders.fetch_add(1, Ordering::Relaxed);
                return Ok(Some(QueryResults::Solutions(QuerySolutionIter::new(
                    variables,
                    rows.into_iter().map(Ok),
                ))));
            }
            // Past every vector, the rows without a cosine come next: the query as written.
            if complete || round == 1 || rows.is_empty() {
                return Ok(None);
            }
            // As many vectors again as the rows kept so far say it takes, twice over: when
            // that is more than the most compared, the query keeps too few of its rows for
            // the index to help.
            let want = n.saturating_mul(need).saturating_mul(2) / (rows.len() + order.offset);
            if want > most {
                return Ok(None);
            }
            n = want.max(n + 1);
        }
        Ok(None)
    }

    /// An error if the cancellation token was cancelled while the query was planned: the
    /// deadline covers planning (the spatial rewrite's side evaluation and pairing) as well
    /// as evaluation.
    fn check_cancelled(&self) -> Result<(), Error> {
        if self.is_cancelled() {
            return Err(oxigraph::sparql::QueryEvaluationError::Cancelled.into());
        }
        Ok(())
    }

    /// Whether the cancellation token of this engine (a deadline, a client gone) has fired.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }

    /// Runs a SPARQL update. Spatial FILTERs in the `WHERE` of its first `DELETE`/`INSERT`
    /// operation use the spatial index, as in [`Engine::query`].
    pub fn update(&self, update: &str) -> Result<(), Error> {
        self.update_with_options(update, &UpdateOptions::default())
    }

    /// [`Engine::update`] with a base IRI and the SPARQL Protocol's `USING` dataset.
    pub fn update_with_options(&self, update: &str, options: &UpdateOptions) -> Result<(), Error> {
        let hints = Hints::parse(update).map_err(Error::Request)?;
        self.scope(hints, || self.update_hinted(update, options))
    }

    fn update_hinted(&self, update: &str, options: &UpdateOptions) -> Result<(), Error> {
        let mut update = Self::parse_update_with_base(update, options.base_iri.as_deref())?;
        if let Some((default, named)) = &options.using {
            for operation in &mut update.operations {
                if let spargebra::GraphUpdateOperation::DeleteInsert { using, .. } = operation {
                    if using.is_some() {
                        return Err(Error::Request(
                            "The update has USING or WITH of its own, and the request gives \
                             using-graph-uri or using-named-graph-uri"
                                .into(),
                        ));
                    }
                    *using = Some(spargebra::algebra::QueryDataset {
                        default: default.clone(),
                        named: Some(named.clone()),
                    });
                }
            }
        }
        let limited = options.readable.is_some() || options.writable.is_some();
        if self.udf.inline_update(&mut update) && (limited || options.row_rules.is_some()) {
            return Err(Error::Forbidden(FUNCTIONS_REFUSED.to_owned()));
        }
        let services = if limited {
            limit_update(
                &mut update,
                options.readable.as_ref(),
                options.writable.as_ref(),
            )?
        } else {
            None
        };
        copy_reads_before_drop(&mut update, self.union_default_graph());
        // Checked here, rewritten in the table scope that runs the update.
        let update =
            self.search_rewrite_update(update, limited || options.row_rules.is_some(), false)?;
        if let Some(rules) = &options.row_rules {
            return self.update_rows(update, rules);
        }
        // The scoped services read the graphs of the WHERE (#203).
        if let Some(dataset) = services {
            let scoped = Self {
                service_dataset: Some(dataset),
                ..self.clone()
            };
            return tripletrail_store::table_scope(|| scoped.update_scoped(update, false));
        }
        // The result tables the rewrite hands to the evaluation live until it ends.
        tripletrail_store::table_scope(|| self.update_scoped(update, !limited))
    }

    /// An update under row rules (#202), in one transaction of the tripletrail store.
    fn update_rows(&self, update: spargebra::Update, rules: &RowRules) -> Result<(), Error> {
        let Dataset::TripleTrail(store) = &self.store else {
            return Err(Error::Request(
                "Row-level access rules need the tripletrail store".to_owned(),
            ));
        };
        // A service's sub-select would read the store past the row filter, as for queries.
        if update.to_string().contains("SERVICE ") {
            return Err(Error::Request(
                "SERVICE is not available under row-level access rules".to_owned(),
            ));
        }
        for op in &update.operations {
            if !matches!(
                op,
                spargebra::GraphUpdateOperation::InsertData { .. }
                    | spargebra::GraphUpdateOperation::DeleteData { .. }
                    | spargebra::GraphUpdateOperation::DeleteInsert { .. }
            ) {
                return Err(Error::Forbidden(
                    "Row-level rules limit what you see of this dataset: only INSERT DATA, \
                     DELETE DATA and DELETE/INSERT … WHERE are taken from you"
                        .to_owned(),
                ));
            }
        }
        let update = plan::plan_update(update);
        self.write_rows(store, rules, |t, before| {
            store.apply_update(
                t,
                &update,
                || self.evaluator(),
                self.union_default_graph(),
                before,
            )
        })
    }

    /// Changes a graph under row rules (#202), as the Graph Store Protocol does: the quads of
    /// `clear` (a graph) that the caller sees are removed, then `add` added, in one
    /// transaction, with the checks of updates under row rules (adding what the rules hide
    /// is refused, as is leaving a touched resource hidden; hidden quads stay).
    pub fn write_graph_rows(
        &self,
        rules: &RowRules,
        clear: Option<oxigraph::model::GraphName>,
        add: Vec<oxigraph::model::Quad>,
    ) -> Result<(), Error> {
        let Dataset::TripleTrail(store) = &self.store else {
            return Err(Error::Request(
                "Row-level access rules need the tripletrail store".to_owned(),
            ));
        };
        self.write_rows(store, rules, |t, _| {
            if let Some(graph) = &clear {
                let quads: Vec<_> = t
                    .view()
                    .quads_for_pattern(None, None, None, Some(graph.as_ref()))
                    .collect();
                for q in &quads {
                    t.remove(q);
                }
            }
            t.insert_all(add);
            Ok(())
        })
    }

    /// A write of the tripletrail store under row rules, in one transaction (see
    /// [`tripletrail_store::RowGuard`]).
    fn write_rows(
        &self,
        store: &tripletrail_store::Store,
        rules: &RowRules,
        write: impl FnOnce(
            &mut tripletrail_store::Transaction<'_>,
            Option<&Arc<tripletrail_store::RowFilter>>,
        ) -> Result<(), tripletrail_store::UpdateError>,
    ) -> Result<(), Error> {
        let text = |e: Error| e.to_string();
        // Judged on the data before the write: the store's, since the transaction holds the
        // writer (the filter of that generation is shared with queries).
        let before = || self.row_filter_for(&store.snapshot(), rules).map_err(text);
        // Judged for the resources the write changes only (#700), not the whole store.
        let after = |view: &tripletrail_store::Snapshot, resources: &[u64]| {
            row_rules::judge(view, rules, resources, &|| self.is_cancelled()).map_err(text)
        };
        let editable = |view: &tripletrail_store::Snapshot, resources: &[u64]| {
            row_rules::judge_editable(view, rules, resources, &|| self.is_cancelled())
                .map(Option::unwrap_or_default)
                .map_err(text)
        };
        let has_editable = rules
            .rules
            .iter()
            .any(|r| matches!(r, row_rules::RowRule::EditableIf { .. }));
        let guard = tripletrail_store::RowGuard {
            before: &before,
            after: &after,
            editable: has_editable.then_some(&editable as &tripletrail_store::RowsOn<'_>),
        };
        let result = tripletrail_store::table_scope(|| store.write_guarded(Some(guard), write));
        match result {
            Ok(changes) => {
                self.refresh_changes(&changes);
                Ok(())
            }
            Err(tripletrail_store::UpdateError::Forbidden(m)) => Err(Error::Forbidden(m)),
            Err(e) => {
                self.invalidate();
                Err(DatasetError::from(e).into())
            }
        }
    }

    fn update_scoped(&self, mut update: spargebra::Update, rewrite: bool) -> Result<(), Error> {
        update = self.search_rewrite_update(update, false, true)?;
        if rewrite && self.spatial_rewrite_on() {
            let (rewritten, stats) = tripletrail_spatial::rewrite_update(update, self);
            *self.spatial.last.lock_unpoisoned() = stats;
            update = rewritten;
            self.check_cancelled()?;
        }
        // Joins written as filters (#185), from what the store held before the update and
        // only for predicates no operation of it may write.
        let written = equality::written_predicates(&update);
        let snapshot = match &self.store {
            Dataset::TripleTrail(store) => Some(store.snapshot()),
            _ => None,
        };
        let resource = |p: &NamedNode| -> bool {
            written.as_ref().is_some_and(|w| !w.contains(p))
                && snapshot.as_ref().is_some_and(|s| resources(s)(p))
        };
        // BINDs computed on every core (#292).
        let prepare = |pattern: spargebra::algebra::GraphPattern,
                       needed: &[Variable],
                       run: &tripletrail_store::RunWhere<'_>|
         -> Result<
            spargebra::algebra::GraphPattern,
            tripletrail_store::UpdateError,
        > {
            let pattern = equality::rewrite(pattern, &resource);
            Ok(bind::rewrite(pattern, Some(needed), run)?.0)
        };
        let result = self.store.update_with(
            plan::plan_update(update),
            || self.evaluator(),
            self.union_default_graph(),
            rewrite.then_some(&prepare as &tripletrail_store::PrepareWhere<'_>),
        );
        match &result {
            Ok(Some(changes)) => self.refresh_changes(changes),
            _ => self.invalidate(),
        }
        result?;
        Ok(())
    }

    /// [`Engine::refresh`] after a transaction of the tripletrail store. Its changes are net (a
    /// quad removed and inserted again by the same update is not among them), as term ids
    /// decoded here: one pass for the predicates and the geometries, another for the range
    /// columns if there are any, without holding all the quads (#382). Only the quads that can
    /// touch the spatial index are decoded (#292): a geometry literal, or a `geo:asWKT`.
    fn refresh_changes(&self, changes: &tripletrail_store::Changes) {
        count_changes(changes);
        let predicates = changes.predicates();
        let geometries: Vec<(bool, NamedNode, Term)> = changes
            .decoded_where(|p, o| {
                p.as_ref() == AS_WKT || o.kind() == tripletrail_store::Kind::Geometry
            })
            .into_iter()
            .map(|(added, q)| (added, q.predicate, q.object))
            .collect();
        let predicates = predicates.iter().map(NamedNode::as_str).collect();
        self.refresh(
            &predicates,
            Some(changes.iter()),
            geometries.iter().map(|(a, p, o)| (*a, p.as_ref(), o)),
        );
    }

    /// Applies a transaction another store logged (#217: a follower replaying its leader's
    /// write-ahead log, read with [`tripletrail_store::scan_log`]) in one commit, and brings
    /// the caches up to date. The changes are applied in order, each as it is: an insert of
    /// a quad present or a removal of one absent does nothing, so applying a run of
    /// transactions again onto a store that has them leaves it as it is. Only on the tripletrail
    /// store.
    pub fn apply_logged(&self, changes: &[(bool, Quad)]) -> Result<(), Error> {
        let Dataset::TripleTrail(store) = &self.store else {
            return Err(Error::Request(
                "Following a write-ahead log needs the tripletrail store".to_owned(),
            ));
        };
        let ((), applied) = store
            .transaction_with_changes(|t| {
                for (insert, quad) in changes {
                    if *insert {
                        t.insert(quad.as_ref());
                    } else {
                        t.remove(quad.as_ref());
                    }
                }
                Ok::<_, tripletrail_store::StoreError>(())
            })
            .map_err(DatasetError::from)?;
        self.refresh_changes(&applied);
        Ok(())
    }

    /// RDFS entailment (#187): makes the named graph `into` hold the triples the RDFS rules
    /// derive from the other graphs ([`tripletrail_store::entail`]), in one transaction, and
    /// says how many it holds, added and removed. Run again after the data changes. Only on
    /// the tripletrail store.
    pub fn entail_rdfs(
        &self,
        into: NamedNodeRef<'_>,
    ) -> Result<tripletrail_store::entail::Entailed, Error> {
        self.entail(into, tripletrail_store::entail::Regime::Rdfs)
    }

    /// [`Engine::entail_rdfs`] under a regime: RDFS, or RDFS with the OWL 2 RL rules
    /// [`tripletrail_store::entail::Regime::OwlRl`] lists.
    pub fn entail(
        &self,
        into: NamedNodeRef<'_>,
        regime: tripletrail_store::entail::Regime,
    ) -> Result<tripletrail_store::entail::Entailed, Error> {
        let Dataset::TripleTrail(store) = &self.store else {
            return Err(Error::Request(
                "Entailment needs the tripletrail store".to_owned(),
            ));
        };
        if let Some((kept, kept_regime)) = store.kept_entailment()
            && kept.as_ref() == into
            && kept_regime != regime
        {
            return Err(Error::Request(format!(
                "{into} holds the {} entailment this dataset keeps: entail into another graph",
                regime_name(kept_regime)
            )));
        }
        // The engine's cancellation (a pipeline step's timeout, say) stops it (#647): the
        // graph then stays as it was.
        let (count, changes) = store
            .transaction_with_changes(|t| {
                Ok::<_, tripletrail_store::StoreError>(
                    t.entail_until(into, regime, &|| self.is_cancelled()),
                )
            })
            .map_err(DatasetError::from)?;
        self.refresh_changes(&changes);
        count.ok_or_else(|| oxigraph::sparql::QueryEvaluationError::Cancelled.into())
    }

    /// What the dataset contradicts by the OWL 2 RL consistency rules (#187), in all its
    /// graphs: how many contradictions (up to `entail::CAP`), and the first `limit`. Run it after
    /// [`Self::entail`], which writes what the checks read (types through sub classes, say).
    /// The engine's cancellation stops it with what it found so far.
    /// `entailed` is the graph [`Self::entail`] wrote: lists are read outside it.
    pub fn inconsistencies(
        &self,
        limit: usize,
        entailed: Option<NamedNodeRef<'_>>,
    ) -> Result<tripletrail_store::entail::Inconsistencies, Error> {
        let Dataset::TripleTrail(store) = &self.store else {
            return Err(Error::Request(
                "Consistency checks need the tripletrail store".to_owned(),
            ));
        };
        let snapshot = store.snapshot();
        let entailed = entailed.and_then(|g| snapshot.term_id(g.into()));
        Ok(tripletrail_store::entail::inconsistencies(
            &snapshot,
            entailed,
            limit,
            &|| self.is_cancelled(),
        ))
    }

    /// Keeps the named graph `into` holding the RDFS entailment of the other graphs (#187)
    /// from now on, or stops (`None`): entails once, then every write brings it up to date in
    /// its own commit ([`tripletrail_store::Store::set_rdfs_entailment`]). Moved to another
    /// graph, the one it was kept in is emptied; stopped, the graph stays as it is (as data,
    /// no longer kept). Only on the tripletrail store.
    pub fn set_rdfs_entailment(
        &self,
        into: Option<NamedNode>,
    ) -> Result<Option<tripletrail_store::entail::Entailed>, Error> {
        self.set_entailment(into, tripletrail_store::entail::Regime::Rdfs)
    }

    /// [`Engine::set_rdfs_entailment`] under a regime: OWL 2 RL keeps the OWL 2 RL
    /// entailment (see [`tripletrail_store::Store::set_entailment`] for which writes it follows
    /// from their triples and which entail again).
    pub fn set_entailment(
        &self,
        into: Option<NamedNode>,
        regime: tripletrail_store::entail::Regime,
    ) -> Result<Option<tripletrail_store::entail::Entailed>, Error> {
        let Dataset::TripleTrail(store) = &self.store else {
            return Err(Error::Request(format!(
                "{} entailment needs the tripletrail store",
                regime_name(regime)
            )));
        };
        // Moved to another graph: the old one is emptied, or its triples, no longer kept,
        // would read as data.
        if let Some(old) = store.rdfs_entailment()
            && into.as_ref().is_some_and(|new| *new != old)
        {
            store.set_rdfs_entailment(None);
            let quads: Vec<Quad> = store
                .quads_for_pattern(None, None, None, Some(old.as_ref().into()))
                .collect();
            let ((), changes) = store
                .transaction_with_changes(|t| {
                    for q in &quads {
                        t.remove(q.as_ref());
                    }
                    Ok::<_, tripletrail_store::StoreError>(())
                })
                .map_err(DatasetError::from)?;
            self.refresh_changes(&changes);
        }
        store.set_entailment(into.clone(), regime);
        into.map(|g| self.entail(g.as_ref(), regime)).transpose()
    }

    /// The graph kept holding the entailment and its rules, if any (see
    /// [`Engine::set_entailment`]).
    pub fn kept_entailment(&self) -> Option<(NamedNode, tripletrail_store::entail::Regime)> {
        match &self.store {
            Dataset::TripleTrail(store) => store.kept_entailment(),
            Dataset::Oxigraph(_) => None,
        }
    }

    /// The graph kept holding the RDFS entailment, if any (see
    /// [`Engine::set_rdfs_entailment`]).
    pub fn rdfs_entailment(&self) -> Option<NamedNode> {
        match &self.store {
            Dataset::TripleTrail(store) => store.rdfs_entailment(),
            Dataset::Oxigraph(_) => None,
        }
    }

    /// Brings what is derived from the data up to date after writes of quads with the given
    /// predicates: networks whose edge query reads none of them are kept, and the spatial index
    /// is patched with the geometry literals (and other `geo:asWKT` objects) added (`true`) or
    /// removed (`false`), given with their predicate, rather than dropped. With `changes`, the
    /// quads the write added and removed (each once), the numeric range columns are patched
    /// too; without, those of the predicates are dropped.
    fn refresh<'a>(
        &self,
        predicates: &HashSet<&str>,
        changes: Option<impl IntoIterator<Item = (bool, Quad)>>,
        geometries: impl IntoIterator<Item = (bool, NamedNodeRef<'a>, &'a Term)>,
    ) {
        if predicates.is_empty() {
            return;
        }
        self.spatial.counts.lock_unpoisoned().invalidate(predicates);
        match changes {
            Some(changes) => self.spatial.ranges.apply(changes),
            None => self.spatial.ranges.invalidate(predicates.iter().copied()),
        }
        {
            let mut non_geometry = self.spatial.non_geometry.lock_unpoisoned();
            self.spatial.writes.fetch_add(1, Ordering::SeqCst);
            non_geometry.retain(|p, _| !predicates.contains(p.as_str()));
        }
        self.networks.invalidate_for(predicates);
        let mut geometries = geometries.into_iter().peekable();
        if geometries.peek().is_none() {
            return;
        }
        let mut index = self.spatial.index.lock_unpoisoned();
        self.spatial.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(i) = index.as_mut()
            && Arc::make_mut(i).apply(&self.store, geometries).is_err()
        {
            *index = None;
        }
    }

    /// The spatial index, built on first use after any change to the data.
    ///
    /// The build runs without holding the index lock, so writes go on meanwhile; a build
    /// that a write overtook is thrown away and done again (a few times at most; then the
    /// last one serves the query that asked without being kept).
    pub fn spatial_index(&self) -> Result<Arc<SpatialIndex>, Error> {
        if let Some(i) = &*self.spatial.index.lock_unpoisoned() {
            return Ok(Arc::clone(i));
        }
        let _building = self.spatial.build.lock_unpoisoned();
        const ATTEMPTS: usize = 3;
        for attempt in 1..=ATTEMPTS {
            if let Some(i) = &*self.spatial.index.lock_unpoisoned() {
                return Ok(Arc::clone(i));
            }
            // Read before the store: a write that the build may have missed moves it on.
            let generation = self.spatial.generation.load(Ordering::SeqCst);
            let predicates = self.spatial.predicates.lock_unpoisoned().clone();
            let built = Arc::new(SpatialIndex::build_with(&self.store, predicates)?);
            let mut index = self.spatial.index.lock_unpoisoned();
            if self.spatial.generation.load(Ordering::SeqCst) == generation {
                *index = Some(Arc::clone(&built));
                return Ok(built);
            }
            if attempt == ATTEMPTS {
                return Ok(built);
            }
        }
        unreachable!()
    }

    /// Sets which predicates' geometry literals the spatial index holds: every predicate by
    /// default. A FILTER on geometries the index leaves out is evaluated row by row. Drops the
    /// index, which is built again on first use.
    pub fn set_indexed_predicates(&self, predicates: IndexedPredicates) {
        *self.spatial.predicates.lock_unpoisoned() = predicates;
        self.drop_index();
    }

    /// Makes the default graph of queries and updates the union of all graphs (the default
    /// graph and every named graph), as Jena's `unionDefaultGraph` does, or the default graph
    /// alone again (the default). A query with `FROM`, a query given its own dataset and an
    /// update operation with `USING` keep theirs. Inserts and deletes without `GRAPH` still go
    /// to the real default graph. The spatial rewrite and the planner work as on the default
    /// graph, the rewrite's estimates counting every graph (#89); the nearest-neighbour
    /// shortcut, the range index and the parallel aggregates are left out on the union.
    pub fn set_union_default_graph(&self, union: bool) {
        self.spatial
            .union_default_graph
            .store(union, Ordering::Relaxed);
    }

    /// See [`Engine::set_union_default_graph`].
    pub fn union_default_graph(&self) -> bool {
        self.spatial.union_default_graph.load(Ordering::Relaxed)
    }

    /// Indexes the literals of these properties for full-text search (`tt:search`,
    /// `text:query`, #188) too, besides [`text::INDEXED`], as the fields of a jena-text entity
    /// map. The indexes are built again on the next search.
    pub fn set_text_properties(&self, extra: &[NamedNode]) {
        *self.text_properties.lock_unpoisoned() = text::properties_with(extra);
        *self.text.lock_unpoisoned() = None;
        self.text_views.lock_unpoisoned().clear();
    }

    /// The properties full-text search indexes: [`text::INDEXED`], then the dataset's own
    /// ([`Engine::set_text_properties`]).
    pub fn text_properties(&self) -> Arc<[NamedNode]> {
        Arc::clone(&self.text_properties.lock_unpoisoned())
    }

    /// [`Self::union_default_graph`] on a store that has named graphs: without, the union of
    /// all graphs is the default graph, and the default graph's id-level rewrites (counts,
    /// rows, ranges) answer it (#808).
    fn reads_named_graphs_as_default(&self) -> bool {
        self.union_default_graph()
            && match &self.store {
                Dataset::TripleTrail(s) => s.snapshot().has_named_graphs(),
                Dataset::Oxigraph(_) => true,
            }
    }

    /// The union of all graphs as a query dataset, when the engine reads it as its default
    /// graph.
    fn union_dataset(&self) -> Option<QueryDataset> {
        self.reads_named_graphs_as_default().then(union_dataset)
    }

    /// A query for the engine's evaluator, on the union of all graphs if the engine reads it
    /// as its default graph (for the rewrite's own queries).
    /// [`Self::prepare`], evaluated on `dataset` when given.
    fn prepare_in(
        &self,
        query: spargebra::Query,
        dataset: Option<&QueryDataset>,
    ) -> oxigraph::sparql::PreparedSparqlQuery {
        let mut prepared = self.prepare(query);
        if let Some(d) = dataset {
            *prepared.dataset_mut() = d.clone();
        }
        prepared
    }

    /// `query` planned and evaluated on `dataset` (else as the default graph is read), on the
    /// view `rows` gives when given (#202).
    fn execute_in(
        &self,
        query: spargebra::Query,
        dataset: Option<&QueryDataset>,
        rows: Option<(&Arc<RowFilter>, &tripletrail_store::Snapshot)>,
    ) -> Option<QueryResults<'static>> {
        let prepared = self.prepare_in(plan::plan_query(query), dataset);
        match rows {
            Some((filter, base)) => self
                .store
                .execute_with_rows(prepared, Arc::clone(filter), base.clone())
                .ok(),
            None => self.store.execute(prepared).ok(),
        }
    }

    /// [`SpatialContext::values_of`] on `dataset` (else as the default graph is read).
    fn values_in(
        &self,
        pattern: &spargebra::algebra::GraphPattern,
        var: &oxigraph::sparql::Variable,
        dataset: Option<&QueryDataset>,
        rows: Option<(&Arc<RowFilter>, &tripletrail_store::Snapshot)>,
    ) -> Option<Vec<oxigraph::model::Term>> {
        use spargebra::algebra::GraphPattern;
        let query = spargebra::Query::Select {
            dataset: None,
            pattern: GraphPattern::Distinct {
                inner: Box::new(GraphPattern::Project {
                    inner: Box::new(pattern.clone()),
                    variables: vec![var.clone()],
                }),
            },
            base_iri: None,
        };
        let QueryResults::Solutions(solutions) = self.execute_in(query, dataset, rows)? else {
            return None;
        };
        let mut out = Vec::new();
        for solution in solutions {
            if let Some(t) = solution.ok()?.get(var) {
                out.push(t.clone());
            }
        }
        Some(out)
    }

    /// [`SpatialContext::rows_of`] on `dataset` (else as the default graph is read).
    fn rows_in(
        &self,
        pattern: &spargebra::algebra::GraphPattern,
        variables: &[oxigraph::sparql::Variable],
        dataset: Option<&QueryDataset>,
        rows: Option<(&Arc<RowFilter>, &tripletrail_store::Snapshot)>,
    ) -> Option<Vec<Vec<Option<oxigraph::model::Term>>>> {
        use spargebra::algebra::GraphPattern;
        let query = spargebra::Query::Select {
            dataset: None,
            pattern: GraphPattern::Project {
                inner: Box::new(pattern.clone()),
                variables: variables.to_vec(),
            },
            base_iri: None,
        };
        let QueryResults::Solutions(solutions) = self.execute_in(query, dataset, rows)? else {
            return None;
        };
        let mut out = Vec::new();
        for solution in solutions {
            let solution = solution.ok()?;
            out.push(variables.iter().map(|v| solution.get(v).cloned()).collect());
        }
        Some(out)
    }

    fn prepare(&self, query: spargebra::Query) -> oxigraph::sparql::PreparedSparqlQuery {
        let mut prepared = self.evaluator().for_query(query);
        if let Some(d) = self.union_dataset() {
            *prepared.dataset_mut() = d;
        }
        prepared
    }

    /// How many parts of queries by callers limited to some graphs or rows (graph grants, row
    /// rules) the count and id-row shortcuts answered (#204), for tests.
    #[doc(hidden)]
    pub fn view_shortcuts_taken(&self) -> u64 {
        self.view_shortcuts.load(Ordering::Relaxed)
    }

    /// How many queries ordered by `DESC(tt:cosine(…))` under a `LIMIT` the vector index
    /// answered, for tests.
    #[doc(hidden)]
    pub fn cosine_orders_answered(&self) -> u64 {
        self.cosine_orders.load(Ordering::Relaxed)
    }

    /// How many vectors of one length a `tt:nearest` index needs before it makes a graph
    /// (HNSW) of them and searches it, approximately ([`vector::GRAPH_MIN`] by default).
    /// 0 keeps every search exact.
    pub fn set_vector_graph_min(&self, n: usize) {
        let n = if n == 0 { usize::MAX } else { n };
        self.vector_graph_min.store(n, Ordering::Relaxed);
    }

    /// The offset the temporal functions (`tt:period` and the rest, #300) read a date or
    /// date-time without a timezone at, in minutes east of UTC (0, UTC, by default):
    /// `"2026-11-26"^^xsd:date` is the day from 2026-11-25T23:00:00Z at 60. A call that gives
    /// its own (`tt:period(x, "+01:00")`) uses that. [`temporal::parse_offset`] reads `+01:00`.
    pub fn set_time_offset(&self, minutes: i64) {
        if self.time_offset.swap(minutes, Ordering::Relaxed) != minutes {
            // Answers kept at another offset would be wrong now.
            self.results.clear();
        }
    }

    /// The offset of [`Engine::set_time_offset`], in minutes.
    pub fn time_offset(&self) -> i64 {
        self.time_offset.load(Ordering::Relaxed)
    }

    /// Turns the spatial FILTER rewrite on or off (on by default; off is for comparisons).
    pub fn set_spatial_rewrite(&self, enabled: bool) {
        self.spatial.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Turns the spatial rewrite's cost model on or off (on by default). Off, the rewrite
    /// gets no estimates and makes its choices by fixed rules: every constant test gets its
    /// candidates as a `VALUES` block, and a join evaluates both sides to pick the one to probe
    /// the index with. For comparisons.
    pub fn set_cost_model(&self, enabled: bool) {
        self.spatial.costed.store(enabled, Ordering::Relaxed);
    }

    /// Lets the cost model answer a join of two large sides with the sweep-line join (on by
    /// default). Off, every join probes the index once per geometry of one side, as without
    /// the cost model. For comparisons.
    pub fn set_sweep_join(&self, enabled: bool) {
        self.spatial.sweep.store(enabled, Ordering::Relaxed);
    }

    /// What the spatial rewrite did for the most recent query, the cost model's decisions
    /// included.
    pub fn last_spatial_stats(&self) -> RewriteStats {
        self.spatial.last.lock_unpoisoned().clone()
    }

    /// About how many triples match a pattern (`None` positions match anything), in the
    /// default graph or, with `all_graphs`, in any graph: what the spatial rewrite's cost model
    /// is told. On the tripletrail store it is read from the quad indexes' block index; on
    /// Oxigraph's store the matches are counted, up to 250 000. Cached until a write touches
    /// the predicate.
    pub fn estimate_triples(
        &self,
        subject: Option<&Term>,
        predicate: Option<&NamedNode>,
        object: Option<&Term>,
        all_graphs: bool,
    ) -> Option<usize> {
        let key: CountKey = (
            subject.cloned(),
            predicate.cloned(),
            object.cloned(),
            all_graphs,
        );
        if let Some(n) = self.spatial.counts.lock_unpoisoned().entries.get(&key) {
            return Some(*n);
        }
        let subject = match subject {
            None => None,
            Some(Term::NamedNode(n)) => Some(NamedOrBlankNodeRef::from(n.as_ref())),
            Some(Term::BlankNode(b)) => Some(b.as_ref().into()),
            // A literal subject matches nothing.
            Some(_) => return Some(0),
        };
        let graph = (!all_graphs).then_some(GraphNameRef::DefaultGraph);
        let n = self
            .store
            .estimate_len(
                subject,
                predicate.map(NamedNode::as_ref),
                object.map(Term::as_ref),
                graph,
                COUNT_CAP,
            )
            .ok()?;
        let mut counts = self.spatial.counts.lock_unpoisoned();
        if counts.entries.len() >= COUNTS_KEPT {
            counts.entries.clear();
        }
        counts.entries.insert(key, n);
        Some(n)
    }

    /// Drops everything derived from the data (routing graphs, spatial index, cached
    /// results), so it is
    /// rebuilt on next use. The engine keeps them in step with its own writes
    /// ([`Engine::load`], [`Engine::update`]...); call this after writing to [`Engine::store`]
    /// directly.
    pub fn invalidate(&self) {
        // A write whose changes are not known: a load, a store written to directly.
        not_counted();
        self.networks.invalidate();
        self.results.clear();
        self.drop_index();
        self.spatial.counts.lock_unpoisoned().entries.clear();
        self.spatial.ranges.clear();
        let mut non_geometry = self.spatial.non_geometry.lock_unpoisoned();
        self.spatial.writes.fetch_add(1, Ordering::SeqCst);
        non_geometry.clear();
    }

    /// Drops the spatial index; a warm engine starts building the next one at once.
    /// The spatial index holding only the geometries that `readable` graphs hold (#203), or
    /// `None` when there is none to give: no index, or a store without write generations
    /// (Oxigraph's), whose views could not be kept up to date. Kept for a few views, until
    /// the next write.
    fn view_index(&self, readable: &ReadableGraphs) -> Option<Arc<SpatialIndex>> {
        const VIEWS_KEPT: usize = 8;
        let Dataset::TripleTrail(store) = &self.store else {
            return None;
        };
        let key = readable.fingerprint();
        let generation = store.generation();
        // A view is made from the index as it is now, which a write that has committed may
        // not have patched yet: it is kept for the index's generation too, so a patch makes
        // the next query make it again.
        let built_on = self.spatial.generation.load(Ordering::SeqCst);
        if let Some((_, _, _, view)) = self
            .spatial
            .views
            .lock_unpoisoned()
            .iter()
            .find(|(k, g, i, _)| *k == key && *g == generation && *i == built_on)
        {
            return Some(Arc::clone(view));
        }
        let index = self.spatial_index().ok()?;
        let mut graphs: Vec<GraphNameRef<'_>> =
            readable.named.iter().map(|n| n.as_ref().into()).collect();
        if readable.default_graph {
            graphs.push(GraphNameRef::DefaultGraph);
        }
        // A geometry is seen when a readable graph holds a quad with it as the object.
        let view = Arc::new(index.retain(&|_, literal| {
            graphs.iter().any(|g| {
                self.store
                    .quads_for_pattern(None, None, Some(literal.as_ref().into()), Some(*g))
                    .next()
                    .is_some()
            })
        }));
        if store.generation() == generation
            && self.spatial.generation.load(Ordering::SeqCst) == built_on
        {
            let mut views = self.spatial.views.lock_unpoisoned();
            views.retain(|(_, g, _, _)| *g == generation);
            if views.len() >= VIEWS_KEPT {
                views.remove(0);
            }
            views.push((key, generation, built_on, Arc::clone(&view)));
        }
        Some(view)
    }

    /// The spatial index holding only the geometries that a view without what `rows` hides
    /// shows (#202), in the `readable` graphs (all without): those a visible quad has as
    /// its object. Kept as [`Engine::view_index`]'s views are, by what the filter hides.
    fn rows_index(
        &self,
        readable: Option<&ReadableGraphs>,
        rows: &RowFilter,
        filtered: &tripletrail_store::Snapshot,
    ) -> Option<Arc<SpatialIndex>> {
        const VIEWS_KEPT: usize = 8;
        let generation = filtered.generation()?;
        let key = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            "rows".hash(&mut hasher);
            readable.map(ReadableGraphs::fingerprint).hash(&mut hasher);
            rows.fingerprint().hash(&mut hasher);
            hasher.finish()
        };
        // A view is made from the index as it is now, which a write that has committed may
        // not have patched yet: it is kept for the index's generation too, so a patch makes
        // the next query make it again.
        let built_on = self.spatial.generation.load(Ordering::SeqCst);
        if let Some((_, _, _, view)) = self
            .spatial
            .views
            .lock_unpoisoned()
            .iter()
            .find(|(k, g, i, _)| *k == key && *g == generation && *i == built_on)
        {
            return Some(Arc::clone(view));
        }
        let index = self.spatial_index().ok()?;
        let graphs: Vec<Option<GraphNameRef<'_>>> = match readable {
            None => vec![None],
            Some(r) => r
                .named
                .iter()
                .map(|n| Some(n.as_ref().into()))
                .chain(r.default_graph.then_some(Some(GraphNameRef::DefaultGraph)))
                .collect(),
        };
        let view = Arc::new(index.retain(&|_, literal| {
            graphs.iter().any(|g| {
                filtered
                    .quads_for_pattern(None, None, Some(literal.as_ref().into()), *g)
                    .next()
                    .is_some()
            })
        }));
        if self.spatial.generation.load(Ordering::SeqCst) == built_on {
            let mut views = self.spatial.views.lock_unpoisoned();
            views.retain(|(_, g, _, _)| *g == generation);
            if views.len() >= VIEWS_KEPT {
                views.remove(0);
            }
            views.push((key, generation, built_on, Arc::clone(&view)));
        }
        Some(view)
    }

    fn drop_index(&self) {
        let mut index = self.spatial.index.lock_unpoisoned();
        self.spatial.generation.fetch_add(1, Ordering::SeqCst);
        *index = None;
        drop(index);
        if self.spatial.warm.load(Ordering::Relaxed) {
            self.build_index_in_background();
        }
    }

    /// Builds the spatial index on a background thread, so that it is ready (or nearly) when
    /// the first spatial query comes, and from then on rebuilds it in the background whenever
    /// a write drops it (a bulk `load`, say) instead of on the next spatial query. For a
    /// server: call it once the data is loaded. Queries that need the index while it is
    /// being built wait for it; the others do not.
    pub fn warm_up(&self) {
        self.spatial.warm.store(true, Ordering::Relaxed);
        self.build_index_in_background();
    }

    fn build_index_in_background(&self) {
        if self.spatial.building.swap(true, Ordering::AcqRel) {
            return;
        }
        let engine = self.clone();
        std::thread::spawn(move || {
            // Cleared first: a write during the build drops what it builds and asks again.
            engine.spatial.building.store(false, Ordering::Release);
            let _ = engine.spatial_index();
        });
    }

    /// Whether the spatial index is built: a spatial query would not wait for it.
    pub fn spatial_index_ready(&self) -> bool {
        self.spatial
            .index
            .try_lock()
            .is_ok_and(|index| index.is_some())
    }

    /// Roughly the heap memory the engine holds for its dataset, by part (#225): what a
    /// memory quota counts. Memory-mapped files are not counted (the operating system pages
    /// them in and out); `None` on Oxigraph's store. An index being built or patched right
    /// now is left out until it is done.
    pub fn memory_use(&self) -> Option<MemoryUse> {
        let Dataset::TripleTrail(store) = &self.store else {
            return None;
        };
        let m = store.memory_use();
        let mut spatial: Vec<Arc<SpatialIndex>> = Vec::new();
        if let Ok(i) = self.spatial.index.try_lock() {
            spatial.extend(i.iter().cloned());
        }
        if let Ok(views) = self.spatial.views.try_lock() {
            spatial.extend(views.iter().map(|v| Arc::clone(&v.3)));
        }
        let mut text: Vec<Arc<text::TextIndex>> = Vec::new();
        if let Ok(i) = self.text.try_lock() {
            text.extend(i.iter().cloned());
        }
        if let Ok(views) = self.text_views.try_lock() {
            for (_, slot) in views.iter() {
                if let Ok(i) = slot.try_lock() {
                    text.extend(i.iter().cloned());
                }
            }
        }
        // A view that is the whole index is the same one: counted once.
        fn once<T>(v: &mut Vec<Arc<T>>) {
            let mut seen: Vec<*const T> = Vec::new();
            v.retain(|a| {
                let p = Arc::as_ptr(a);
                !seen.contains(&p) && {
                    seen.push(p);
                    true
                }
            });
        }
        once(&mut spatial);
        once(&mut text);
        let mut vectors = 0;
        if let Ok(slots) = self.vectors.try_lock() {
            for (_, _, slot) in slots.iter() {
                if let Ok(i) = slot.try_lock() {
                    vectors += i.as_ref().map_or(0, |i| i.heap_size());
                }
            }
        }
        Some(MemoryUse {
            dictionary: m.dictionary,
            quads: m.quads,
            geometries: m.geometry_bytes,
            spatial_index: spatial.iter().map(|i| i.heap_size()).sum(),
            text_index: text.iter().map(|i| i.heap_size()).sum(),
            vector_indexes: vectors,
            range_indexes: self.spatial.ranges.heap_size(),
            result_cache: self.results.stats().bytes,
            routing_networks: self.networks().bytes(),
            union_copies: m.union_copies,
        })
    }

    /// Drops the indexes and caches queries build from the data, largest first, until the
    /// engine's memory (see [`Engine::memory_use`]) is at most `target` bytes or nothing is
    /// left to drop (#18): the full-text index, the spatial index, the vector indexes, the
    /// range indexes and the result cache, with their views for callers who see some graphs.
    /// Each is built again by the next query that needs it. The data, its dictionary, the
    /// parsed geometries and the routing networks (which keep a budget of their own) stay.
    /// Returns the parts dropped, by their names in [`MemoryUse`]; none on Oxigraph's store.
    pub fn evict_derived(&self, target: usize) -> Vec<&'static str> {
        let Some(m) = self.memory_use() else {
            return Vec::new();
        };
        let mut parts = [
            ("textIndex", m.text_index),
            ("spatialIndex", m.spatial_index),
            ("vectorIndexes", m.vector_indexes),
            ("rangeIndexes", m.range_indexes),
            ("resultCache", m.result_cache),
        ];
        parts.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
        let mut total = m.total();
        let mut dropped = Vec::new();
        for (part, bytes) in parts {
            if total <= target || bytes == 0 {
                break;
            }
            match part {
                "textIndex" => {
                    *self.text.lock_unpoisoned() = None;
                    self.text_views.lock_unpoisoned().clear();
                }
                "spatialIndex" => {
                    // Not built again at once, even on a warm engine: the next spatial query
                    // builds it.
                    let mut index = self.spatial.index.lock_unpoisoned();
                    self.spatial.generation.fetch_add(1, Ordering::SeqCst);
                    *index = None;
                    drop(index);
                    self.spatial.views.lock_unpoisoned().clear();
                }
                "vectorIndexes" => {
                    // The slots are taken out of the list first: a slot's lock is held while
                    // its index is built, and waiting for it under the list's lock would hold
                    // up every vector query until that build ends.
                    let slots: Vec<_> = self
                        .vectors
                        .lock_unpoisoned()
                        .iter()
                        .map(|(_, _, slot)| Arc::clone(slot))
                        .collect();
                    for slot in slots {
                        *slot.lock_unpoisoned() = None;
                    }
                }
                "rangeIndexes" => self.spatial.ranges.clear(),
                _ => self.results.clear(),
            }
            total -= bytes;
            dropped.push(part);
        }
        dropped
    }

    /// Bulk-loads RDF (e.g. N-Triples or Turtle). Takes a format, or an [`RdfParser`] to set
    /// options such as the graph to load into (see also [`Engine::load_into`]). The
    /// document's blank nodes are its own, as with Jena's loaders: `_:b1` in two loads is two
    /// blank nodes (#840). [`Engine::load_with_blank_nodes`] can keep the labels instead.
    pub fn load(&self, parser: impl Into<RdfParser>, reader: impl Read) -> Result<(), Error> {
        self.load_with_blank_nodes(parser, reader, BlankNodes::PerLoad)
    }

    /// [`Engine::load`], keeping the document's blank node labels as written or not.
    pub fn load_with_blank_nodes(
        &self,
        parser: impl Into<RdfParser>,
        reader: impl Read,
        blank_nodes: BlankNodes,
    ) -> Result<(), Error> {
        let result = self
            .store
            .load_with_blank_nodes(parser, reader, blank_nodes);
        self.invalidate();
        Ok(result?)
    }

    /// Writes `quads` in one transaction, after emptying what `replace` names: a Graph Store
    /// `PUT` or `POST` of a small body (#702). Readers see the old content or the new, never
    /// an empty graph in between, and on disk it is one log entry. Unlike [`Engine::load`],
    /// the spatial index, counts and cached results are brought up to date from what changed,
    /// not dropped.
    pub fn write_quads(&self, replace: Replace<'_>, quads: Vec<Quad>) -> Result<(), Error> {
        let store = match &self.store {
            Dataset::TripleTrail(store) => store,
            Dataset::Oxigraph(_) => {
                match replace {
                    Replace::Nothing => {}
                    Replace::Graph(g) => self.store.clear_graph(g)?,
                    Replace::All => self.store.clear()?,
                }
                for q in &quads {
                    self.store.insert(q)?;
                }
                self.invalidate();
                return Ok(());
            }
        };
        let ((), changes) = store
            .transaction_with_changes(|t| {
                let graph = match replace {
                    Replace::Nothing => None,
                    Replace::Graph(g) => Some(Some(g)),
                    Replace::All => Some(None),
                };
                if let Some(graph) = graph {
                    let old: Vec<Quad> = t
                        .view()
                        .quads_for_pattern(None, None, None, graph)
                        .collect();
                    for q in &old {
                        t.remove(q.as_ref());
                    }
                }
                t.insert_all(quads);
                Ok::<_, tripletrail_store::StoreError>(())
            })
            .map_err(DatasetError::from)?;
        self.refresh_changes(&changes);
        Ok(())
    }

    /// Bulk-loads RDF into a graph: the triples of the input (and the default-graph quads of
    /// a quad format) go to `graph`.
    pub fn load_into(
        &self,
        graph: impl Into<GraphName>,
        parser: impl Into<RdfParser>,
        reader: impl Read,
    ) -> Result<(), Error> {
        self.load(parser.into().with_default_graph(graph), reader)
    }

    /// Adds a quad, keeping the spatial index and the networks it doesn't affect.
    pub fn insert(&self, quad: &Quad) -> Result<(), Error> {
        let changed = self.store.insert_reporting(quad)?;
        self.refresh_quad(true, quad, changed);
        Ok(())
    }

    /// Removes a quad, keeping the spatial index and the networks it doesn't affect.
    pub fn remove(&self, quad: &Quad) -> Result<(), Error> {
        let changed = self.store.remove_reporting(quad)?;
        self.refresh_quad(false, quad, changed);
        Ok(())
    }

    /// `changed`: whether the store says the quad was added or removed (`None`: it doesn't).
    fn refresh_quad(&self, added: bool, quad: &Quad, changed: Option<bool>) {
        not_counted();
        if self.rdfs_entailment().is_some() {
            // The write changed the entailment too, which the store does not report.
            return self.invalidate();
        }
        let predicates = HashSet::from([quad.predicate.as_str()]);
        let geometry = touches_index(quad.predicate.as_ref(), &quad.object).then_some((
            added,
            quad.predicate.as_ref(),
            &quad.object,
        ));
        let changes = changed.map(|c| c.then(|| (added, quad.clone())));
        self.refresh(&predicates, changes, geometry);
    }

    /// Bulk-loads quads. Networks whose edge query reads none of the loaded predicates are
    /// kept, and the spatial index is patched with the loaded geometries.
    pub fn load_quads(&self, quads: impl IntoIterator<Item = Quad>) -> Result<(), Error> {
        not_counted();
        let mut predicates = HashSet::new();
        let mut geometries = Vec::new();
        let result = self.store.load_quads(quads.into_iter().inspect(|q| {
            if touches_index(q.predicate.as_ref(), &q.object) {
                geometries.push((q.predicate.clone(), q.object.clone()));
            }
            if !predicates.contains(&q.predicate) {
                predicates.insert(q.predicate.clone());
            }
        }));
        if result.is_err() || self.rdfs_entailment().is_some() {
            self.invalidate();
            return Ok(result?);
        }
        let predicates = predicates.iter().map(|p| p.as_str()).collect();
        self.refresh(
            &predicates,
            None::<[(bool, Quad); 0]>,
            geometries.iter().map(|(p, o)| (true, p.as_ref(), o)),
        );
        Ok(())
    }
}

/// Whether a quad can change the spatial index: its object is a geometry literal, or it is a
/// `geo:asWKT` triple (whose other objects the index counts).
fn touches_index(predicate: NamedNodeRef<'_>, object: &Term) -> bool {
    predicate == AS_WKT
        || matches!(object, Term::Literal(l) if tripletrail_geo::is_geometry_datatype(l.datatype().as_str()))
}

impl SpatialContext for Engine {
    fn index(&self) -> Option<Arc<SpatialIndex>> {
        self.spatial_index().ok()
    }

    fn indexed_predicates(&self) -> Option<IndexedPredicates> {
        Some(self.spatial.predicates.lock_unpoisoned().clone())
    }

    fn cancelled(&self) -> bool {
        self.is_cancelled()
    }

    fn non_geometry_objects(&self, predicate: &str) -> Option<usize> {
        if let Some(&n) = self.spatial.non_geometry.lock_unpoisoned().get(predicate) {
            return Some(n);
        }
        let writes = self.spatial.writes.load(Ordering::SeqCst);
        let p = NamedNodeRef::new(predicate).ok()?;
        let n = self.store.non_geometry_objects(p).ok()?;
        let mut cache = self.spatial.non_geometry.lock_unpoisoned();
        if self.spatial.writes.load(Ordering::SeqCst) == writes {
            cache.insert(predicate.to_owned(), n);
        }
        Some(n)
    }

    fn geometry(&self, literal: &oxigraph::model::Literal) -> Option<Arc<tripletrail_geo::Geom>> {
        self.store
            .geometry(literal.as_ref().into())
            .or_else(|| tripletrail_geo::parse_wkt_literal(&Term::Literal(literal.clone())))
    }

    fn sweep_join(&self) -> bool {
        self.spatial.sweep.load(Ordering::Relaxed)
    }

    fn has_predicate(&self, iri: &str) -> bool {
        self.store
            .quads_for_pattern(
                None,
                Some(oxigraph::model::NamedNodeRef::new_unchecked(iri)),
                None,
                None,
            )
            .next()
            .is_some()
    }

    fn estimate(&self, pattern: &spargebra::term::TriplePattern, all_graphs: bool) -> Option<f64> {
        use spargebra::term::{NamedNodePattern, TermPattern};
        if !self.spatial.costed.load(Ordering::Relaxed) {
            return None;
        }
        let term = |t: &TermPattern| match t {
            TermPattern::NamedNode(n) => Some(Some(Term::NamedNode(n.clone()))),
            TermPattern::Literal(l) => Some(Some(Term::Literal(l.clone()))),
            TermPattern::Variable(_) | TermPattern::BlankNode(_) => Some(None),
            // Quoted triples: not estimated.
            #[allow(unreachable_patterns)]
            _ => None,
        };
        let predicate = match &pattern.predicate {
            NamedNodePattern::NamedNode(n) => Some(n.clone()),
            NamedNodePattern::Variable(_) => None,
        };
        // Under the union default graph, the default graph the query reads is every graph
        // (#89): estimating from the real one, often empty, left every window to the FILTER.
        let n = self.estimate_triples(
            term(&pattern.subject)?.as_ref(),
            predicate.as_ref(),
            term(&pattern.object)?.as_ref(),
            all_graphs || self.union_default_graph(),
        )?;
        Some(n as f64)
    }

    fn estimate_join(
        &self,
        triples: &[spargebra::term::TriplePattern],
        all_graphs: bool,
    ) -> Option<f64> {
        if !self.spatial.costed.load(Ordering::Relaxed) {
            return None;
        }
        let Dataset::TripleTrail(store) = &self.store else {
            return None;
        };
        let snapshot = store.snapshot();
        let compiled = snapshot.compile_bgp(triples, &[])?;
        if compiled.empty {
            return Some(0.0);
        }
        let graph = if all_graphs || self.union_default_graph() {
            tripletrail_store::GraphSelector::All
        } else {
            tripletrail_store::GraphSelector::Default
        };
        Some(snapshot.estimate_rows(&compiled.triples, graph))
    }

    fn values_of(
        &self,
        pattern: &spargebra::algebra::GraphPattern,
        var: &oxigraph::sparql::Variable,
    ) -> Option<Vec<oxigraph::model::Term>> {
        self.values_in(pattern, var, None, None)
    }

    fn side_ids(
        &self,
        pattern: &spargebra::algebra::GraphPattern,
        var: &Variable,
    ) -> Option<tripletrail_spatial::IdSide> {
        let Dataset::TripleTrail(store) = &self.store else {
            return None;
        };
        // Side queries read the union then, which the default graph scan below would not.
        if self.union_default_graph() {
            return None;
        }
        // A side in one named graph (`GRAPH <g> { ... }`, #703) reads that graph. The rewrite
        // asks for sides only outside `GRAPH` and without a dataset of the query's own.
        let (pattern, named) = match pattern {
            spargebra::algebra::GraphPattern::Graph {
                name: spargebra::term::NamedNodePattern::NamedNode(n),
                inner,
            } => (&**inner, Some(n)),
            _ => (pattern, None),
        };
        let mut triples = Vec::new();
        if !plain_triples(pattern, &mut triples) || triples.is_empty() {
            return None;
        }
        // A graph the store doesn't hold matches nothing.
        let graph = match named {
            Some(n) => store
                .term_id(n.as_ref().into())
                .map(tripletrail_store::GraphSelector::Graph),
            None => Some(tripletrail_store::GraphSelector::Default),
        };
        // The side's variables, the geometry's first.
        let mut variables = vec![var.clone()];
        for t in &triples {
            let mut add = |v: &Variable| {
                if !variables.contains(v) {
                    variables.push(v.clone());
                }
            };
            for p in [&t.subject, &t.object] {
                if let spargebra::term::TermPattern::Variable(v) = p {
                    add(v);
                }
            }
            if let spargebra::term::NamedNodePattern::Variable(v) = &t.predicate {
                add(v);
            }
        }
        let snapshot = store.snapshot();
        let compiled = snapshot.compile_bgp(&triples, &variables)?;
        let width = variables.len();
        let Some(graph) = graph.filter(|_| !compiled.empty) else {
            return Some(tripletrail_spatial::IdSide {
                variables,
                rows: tripletrail_store::IdRows {
                    width,
                    ids: Vec::new(),
                },
                geometry: 0,
                geometries: Vec::new(),
                load: Arc::new(|_| None),
                read: Arc::new(|_| None),
            });
        };
        let all = snapshot.join_ids(&compiled.triples, compiled.columns.len(), graph, &|| {
            self.is_cancelled()
        })?;
        // The named variables only.
        let mut ids = Vec::with_capacity(all.len() * width);
        for row in all.rows() {
            ids.extend_from_slice(&row[..width]);
        }
        let rows = tripletrail_store::IdRows { width, ids };
        let mut distinct: Vec<tripletrail_store::TermId> = rows
            .rows()
            .map(|r| r[0])
            .filter(|id| id.kind() == tripletrail_store::Kind::Geometry)
            .collect();
        distinct.sort_unstable_by_key(|id| id.0);
        distinct.dedup();
        // Bounding boxes only, read from the saved records on every core; the join parses the
        // geometries it tests (#754). Empty and invalid geometries meet nothing.
        let threads = tripletrail_store::par::cores();
        let chunk = distinct.len().div_ceil(threads).max(4096);
        let geometries = std::thread::scope(|scope| {
            let workers: Vec<_> = distinct
                .chunks(chunk)
                .map(|part| {
                    scope.spawn(move || {
                        part.iter()
                            .filter_map(|&id| {
                                let (crs, bbox) = store.geometry_bbox(id)?;
                                Some(tripletrail_spatial::SideGeometry {
                                    id,
                                    crs,
                                    bbox: bbox?,
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().unwrap())
                .collect()
        });
        let parse = snapshot.clone();
        let read = snapshot.clone();
        Some(tripletrail_spatial::IdSide {
            variables,
            rows,
            geometry: 0,
            geometries,
            load: Arc::new(move |id| parse.geometry(id)),
            read: Arc::new(move |id| read.geometry_transient(id)),
        })
    }

    fn table(
        &self,
        variables: &[Variable],
        rows: tripletrail_store::IdRows,
    ) -> Option<spargebra::algebra::GraphPattern> {
        use spargebra::term::{TermPattern, TriplePattern};
        let predicates = tripletrail_store::register_table(rows)?;
        let row = oxigraph::model::BlankNode::default();
        Some(spargebra::algebra::GraphPattern::Bgp {
            patterns: variables
                .iter()
                .zip(predicates)
                .map(|(v, p)| TriplePattern {
                    subject: TermPattern::BlankNode(row.clone()),
                    predicate: p.into(),
                    object: v.clone().into(),
                })
                .collect(),
        })
    }

    fn rows_of(
        &self,
        pattern: &spargebra::algebra::GraphPattern,
        variables: &[oxigraph::sparql::Variable],
    ) -> Option<Vec<Vec<Option<oxigraph::model::Term>>>> {
        self.rows_in(pattern, variables, None, None)
    }
}

/// What `tt:search` reads (#188).
struct SearchCtx<'a> {
    engine: &'a Engine,
    store: &'a tripletrail_store::Store,
    /// For [`text::SearchContext::holds`], taken on first use.
    snapshot: OnceLock<tripletrail_store::Snapshot>,
    /// What a caller limited to some graphs or rows sees (#203); `None` for the whole store.
    source: Option<text::Source>,
    /// The caller's dataset, which replaces the query's own, for the values of a query
    /// string read from the query (#188).
    dataset: Option<&'a QueryDataset>,
    /// Row rules (#202): those values read the view without what the filter hides.
    rows: Option<(&'a Arc<RowFilter>, &'a tripletrail_store::Snapshot)>,
}

impl text::SearchContext for SearchCtx<'_> {
    fn index(&self) -> Result<Arc<text::TextIndex>, Error> {
        match &self.source {
            Some(source) => self.engine.text_view(source),
            None => self.engine.text_index(),
        }
    }

    fn vectors(&self, property: &NamedNode) -> Result<Arc<vector::VectorIndex>, Error> {
        self.engine.vector_index(self.source.as_ref(), property)
    }

    fn properties(&self) -> Arc<[NamedNode]> {
        match &self.source {
            Some(source) => Arc::clone(&source.properties),
            None => self.engine.text_properties(),
        }
    }

    fn term_id(&self, term: oxigraph::model::TermRef<'_>) -> Option<tripletrail_store::TermId> {
        self.store.term_id(term)
    }

    fn holds(
        &self,
        s: tripletrail_store::TermId,
        p: tripletrail_store::TermId,
        o: tripletrail_store::TermId,
    ) -> bool {
        if let Some(source) = &self.source {
            return source.holds(s, p, o);
        }
        self.snapshot
            .get_or_init(|| self.store.snapshot())
            .quad_ids_for_pattern(
                Some(s),
                Some(p),
                Some(o),
                tripletrail_store::GraphSelector::All,
            )
            .next()
            .is_some()
    }

    fn table(
        &self,
        variables: &[Variable],
        rows: tripletrail_store::IdRows,
    ) -> Option<spargebra::algebra::GraphPattern> {
        SpatialContext::table(self.engine, variables, rows)
    }

    fn literal(&self, id: tripletrail_store::TermId) -> Option<oxigraph::model::Literal> {
        let snapshot = match &self.source {
            Some(source) => &source.snapshot,
            None => self.snapshot.get_or_init(|| self.store.snapshot()),
        };
        match snapshot.term(id) {
            Ok(oxigraph::model::Term::Literal(l)) => Some(l),
            _ => None,
        }
    }

    fn values(
        &self,
        pattern: &spargebra::algebra::GraphPattern,
        var: &Variable,
        dataset: Option<&spargebra::algebra::QueryDataset>,
    ) -> Result<Vec<Term>, Error> {
        let own = dataset.map(|d| QueryDataset::from(d.clone()));
        let values = self
            .engine
            .values_in(pattern, var, self.dataset.or(own.as_ref()), self.rows)
            .ok_or_else(|| {
                Error::Request(format!(
                    "the values of the query string ?{} could not be read",
                    var.as_str()
                ))
            });
        self.engine.check_cancelled()?;
        values
    }
}

/// The spatial rewrite's view of the engine for a caller who may read some graphs only
/// (#203): the index holds only the geometries of those graphs, and the rewrite's own
/// queries are evaluated on the caller's dataset. Estimates come from the whole store: they
/// only steer the plan.
struct Scoped<'a> {
    engine: &'a Engine,
    /// The dataset its own queries read (as the default graph is read without).
    dataset: Option<&'a QueryDataset>,
    /// Makes the index, when the rewrite asks for it: a query without a spatial filter does
    /// not pay for finding the view (#708).
    index: &'a (dyn Fn() -> Option<Arc<SpatialIndex>> + Sync),
    /// Row rules (#202): its own queries read this view, without what the filter hides.
    rows: Option<(&'a Arc<RowFilter>, &'a tripletrail_store::Snapshot)>,
}

impl SpatialContext for Scoped<'_> {
    fn index(&self) -> Option<Arc<SpatialIndex>> {
        (self.index)()
    }

    fn has_predicate(&self, iri: &str) -> bool {
        self.engine.has_predicate(iri)
    }

    fn values_of(
        &self,
        pattern: &spargebra::algebra::GraphPattern,
        var: &Variable,
    ) -> Option<Vec<Term>> {
        self.engine.values_in(pattern, var, self.dataset, self.rows)
    }

    fn rows_of(
        &self,
        pattern: &spargebra::algebra::GraphPattern,
        variables: &[Variable],
    ) -> Option<Vec<Vec<Option<Term>>>> {
        self.engine
            .rows_in(pattern, variables, self.dataset, self.rows)
    }

    // `side_ids` is left out: it reads the store's default graph directly.

    fn table(
        &self,
        variables: &[Variable],
        rows: tripletrail_store::IdRows,
    ) -> Option<spargebra::algebra::GraphPattern> {
        self.engine.table(variables, rows)
    }

    fn estimate(&self, pattern: &spargebra::term::TriplePattern, all_graphs: bool) -> Option<f64> {
        self.engine.estimate(pattern, all_graphs)
    }

    fn estimate_join(
        &self,
        triples: &[spargebra::term::TriplePattern],
        all_graphs: bool,
    ) -> Option<f64> {
        self.engine.estimate_join(triples, all_graphs)
    }

    fn sweep_join(&self) -> bool {
        self.engine.sweep_join()
    }

    fn geometry(&self, literal: &oxigraph::model::Literal) -> Option<Arc<tripletrail_geo::Geom>> {
        self.engine.geometry(literal)
    }

    fn cancelled(&self) -> bool {
        self.engine.is_cancelled()
    }
}

/// Whether a query names its own dataset (`FROM` / `FROM NAMED`).
fn has_own_dataset(query: &spargebra::Query) -> bool {
    match query {
        spargebra::Query::Select { dataset, .. }
        | spargebra::Query::Construct { dataset, .. }
        | spargebra::Query::Describe { dataset, .. }
        | spargebra::Query::Ask { dataset, .. } => dataset.is_some(),
    }
}

/// `s` as a JSON string literal.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The triples of a pattern made of basic graph patterns and joins only, into `out`; `false`
/// for anything else.
fn plain_triples(
    p: &spargebra::algebra::GraphPattern,
    out: &mut Vec<spargebra::term::TriplePattern>,
) -> bool {
    use spargebra::algebra::GraphPattern;
    match p {
        GraphPattern::Bgp { patterns } => {
            out.extend(patterns.iter().cloned());
            true
        }
        GraphPattern::Join { left, right } => plain_triples(left, out) && plain_triples(right, out),
        _ => false,
    }
}

/// SERVICE calls to IRIs without their own handler: Facade-X (`x-sparql-anything:`) and remote
/// SPARQL endpoints, each when enabled.
struct Services {
    fx: Option<FxService>,
    #[cfg(feature = "remote-service")]
    remote: Option<remote::RemoteService>,
}

/// A SERVICE call that failed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct ServiceError(String);

impl oxigraph::sparql::DefaultServiceHandler for Services {
    type Error = ServiceError;

    fn handle(
        &self,
        service_name: &NamedNode,
        pattern: &spargebra::algebra::GraphPattern,
        base_iri: Option<&oxiri::Iri<String>>,
    ) -> Result<oxigraph::sparql::QuerySolutionIter<'static>, ServiceError> {
        let name = service_name.as_str();
        if let Some(fx) = &self.fx
            && name.starts_with(tripletrail_fx::SCHEME)
        {
            return oxigraph::sparql::DefaultServiceHandler::handle(
                fx,
                service_name,
                pattern,
                base_iri,
            )
            .map_err(|e| ServiceError(e.to_string()));
        }
        #[cfg(feature = "remote-service")]
        if let Some(remote) = &self.remote {
            return remote
                .call(name, pattern, base_iri)
                .map_err(|e| ServiceError(e.to_string()));
        }
        Err(ServiceError(if name.starts_with(tripletrail_fx::SCHEME) {
            format!("SERVICE <{name}>: Facade-X calls are not enabled here")
        } else {
            format!("SERVICE <{name}>: no such service (remote endpoints must be allowed first)")
        }))
    }
}

/// How a regime is named in messages and settings: `rdfs` or `owl-rl`.
pub fn regime_name(regime: tripletrail_store::entail::Regime) -> &'static str {
    match regime {
        tripletrail_store::entail::Regime::Rdfs => "rdfs",
        tripletrail_store::entail::Regime::OwlRl => "owl-rl",
    }
}

thread_local! {
    /// The writes counted by [`counting_changes`] on this thread, by graph; `None` inside
    /// once a write was made that is not counted.
    static COUNTED: std::cell::RefCell<Option<Option<Vec<GraphChanges>>>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs `f` and says what the writes it made on this thread did to each graph: the quads
/// added and removed (net, as [`tripletrail_store::Changes`] has them), in the order the graphs
/// were first written. `None` when `f` made a write that is not counted: a bulk load
/// ([`Engine::load`], [`Engine::load_quads`]), a single [`Engine::insert`] or
/// [`Engine::remove`], a write to another store than tripletrail's, or one to the store itself
/// followed by [`Engine::invalidate`]. For the audit log (#207): counting costs a pass over
/// the changes, so it is done only here.
pub fn counting_changes<T>(f: impl FnOnce() -> T) -> (T, Option<Vec<GraphChanges>>) {
    let outer = COUNTED.with(|c| c.borrow_mut().replace(Some(Vec::new())));
    let result = f();
    let counted = COUNTED.with(|c| {
        let mut c = c.borrow_mut();
        let mine = c.take().flatten();
        // A count around this one sees these writes too.
        *c = outer.map(|o| match (o, &mine) {
            (Some(mut o), Some(m)) => {
                merge_counts(&mut o, m);
                Some(o)
            }
            _ => None,
        });
        mine
    });
    (result, counted)
}

fn count_changes(changes: &tripletrail_store::Changes) {
    COUNTED.with(|c| {
        if let Some(Some(counted)) = c.borrow_mut().as_mut() {
            merge_counts(counted, &changes.per_graph());
        }
    });
}

/// A write [`counting_changes`] cannot count was made.
fn not_counted() {
    COUNTED.with(|c| {
        if let Some(counted) = c.borrow_mut().as_mut() {
            *counted = None;
        }
    });
}

fn merge_counts(into: &mut Vec<GraphChanges>, more: &[GraphChanges]) {
    if into.is_empty() {
        into.extend_from_slice(more);
        return;
    }
    let mut at: HashMap<GraphName, usize> = into
        .iter()
        .enumerate()
        .map(|(i, g)| (g.graph.clone(), i))
        .collect();
    for m in more {
        match at.get(&m.graph) {
            Some(&i) => {
                into[i].added += m.added;
                into[i].removed += m.removed;
            }
            None => {
                at.insert(m.graph.clone(), into.len());
                into.push(m.clone());
            }
        }
    }
}

/// Whether every object of a predicate is surely an IRI or a blank node in `snapshot` (a
/// predicate it does not hold has none), for [`equality`].
fn resources(snapshot: &Snapshot) -> impl Fn(&NamedNode) -> bool + '_ {
    move |p: &NamedNode| match snapshot.term_id(p.as_ref().into()) {
        None => true,
        Some(id) => snapshot.objects_are_resources(id) == Some(true),
    }
}

/// The 64-bit FNV-1a hash of `s`: a stable file name for an IRI.
fn fnv64(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod counting_tests {
    use super::*;

    #[test]
    fn writes_are_counted_by_graph_and_nested_counts_see_them() {
        let engine = Engine::new_tripletrail();
        engine
            .update(
                "INSERT DATA { <urn:a> <urn:b> <urn:c> . GRAPH <urn:g> { <urn:a> <urn:b> 1, 2 } }",
            )
            .unwrap();
        // Not counted: no count around it.
        let ((), outer) = counting_changes(|| {
            let ((), inner) = counting_changes(|| {
                engine
                    .update(
                        "DELETE DATA { GRAPH <urn:g> { <urn:a> <urn:b> 1 } } ; \
                         INSERT DATA { GRAPH <urn:h> { <urn:a> <urn:b> 3 } }",
                    )
                    .unwrap();
            });
            let g = |name: &str| GraphName::from(NamedNode::new_unchecked(name));
            assert_eq!(
                inner.unwrap(),
                vec![
                    GraphChanges {
                        graph: g("urn:g"),
                        added: 0,
                        removed: 1
                    },
                    GraphChanges {
                        graph: g("urn:h"),
                        added: 1,
                        removed: 0
                    },
                ]
            );
            // An insert of what is there changes nothing.
            engine
                .update("INSERT DATA { <urn:a> <urn:b> <urn:c> . <urn:x> <urn:y> <urn:z> }")
                .unwrap();
        });
        let outer = outer.unwrap();
        assert_eq!(outer.len(), 3, "{outer:?}");
        assert_eq!(
            outer[2],
            GraphChanges {
                graph: GraphName::DefaultGraph,
                added: 1,
                removed: 0
            }
        );
    }

    #[test]
    fn a_write_that_is_not_counted_says_so() {
        let engine = Engine::new_tripletrail();
        let ((), outer) = counting_changes(|| {
            let ((), inner) = counting_changes(|| {
                engine
                    .load(
                        oxigraph::io::RdfFormat::NTriples,
                        &b"<urn:a> <urn:b> <urn:c> .\n"[..],
                    )
                    .unwrap();
            });
            assert_eq!(inner, None);
        });
        assert_eq!(outer, None);
        let quad = Quad::new(
            NamedNode::new_unchecked("urn:a"),
            NamedNode::new_unchecked("urn:b"),
            NamedNode::new_unchecked("urn:d"),
            GraphName::DefaultGraph,
        );
        assert_eq!(counting_changes(|| engine.insert(&quad).unwrap()).1, None);
        assert_eq!(
            counting_changes(|| engine.load_quads([quad.clone()]).unwrap()).1,
            None
        );
        // A count that wrote nothing is counted, and empty.
        assert_eq!(counting_changes(|| ()).1, Some(Vec::new()));
    }
}
