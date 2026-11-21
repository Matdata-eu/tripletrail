use crate::network::{
    BuildStats, Constrained, Network, NetworkCache, edge_key, number, predicates_read,
};
use crate::request::{EdgeSource, PgrFunction, Request};
use crate::tables::{self, Row, View};
use crate::{PGR, PGR_SERVICE, geometry, shapes, time, topology};
use geo::Coord;
use oxigraph::model::{Literal, NamedNode, Term};
use oxigraph::sparql::{
    CancellationToken, PreparedSparqlQuery, QueryDataset, QueryEvaluationError, QueryResults,
    QuerySolution, QuerySolutionIter, ServiceHandler, SparqlEvaluator, Variable,
};
use oxiri::Iri;
use spargebra::algebra::GraphPattern;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tripletrail_geo::Crs;
use tripletrail_geo::sync::LockUnpoisoned;
use tripletrail_routing::{
    Cch, CchSpace, CostMatrix, EdgeRow, Graph, Heuristic, InternedGraph, Path, Period, Restriction,
    Restrictions, SearchSpace, ShortestTree, TimeTable, astar, bd_dijkstra, bellman_ford,
    binary_bfs, cch_dijkstra, chinese_postman, circuits, connected_components,
    cut_vertices_and_edges, dag_shortest_paths, dijkstra, dijkstra_between, driving_distance,
    edge_disjoint_paths, edward_moore, ksp, minimum_spanning_forest, strong_components,
    td_dijkstra, trsp, trsp_avoiding, turn_restricted_path,
};
use tripletrail_store::{Dataset, RowView};

#[derive(Debug, thiserror::Error)]
pub enum PgrError {
    #[error("pgr: {0}")]
    Request(String),
    #[error("pgr: edge query failed: {0}")]
    EdgeQuery(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The query's cancellation token was cancelled (a timeout) while the function ran.
    #[error("pgr: the query was cancelled")]
    Cancelled,
}

/// Timings of the last routing call, for benchmarks and EXPLAIN-style diagnostics.
#[derive(Debug, Clone, Copy, Default)]
pub struct QueryStats {
    /// The network came from the cache (edge query and build skipped).
    pub cache_hit: bool,
    /// Build timings of the network used (those of the original build on a cache hit).
    pub build: BuildStats,
    /// Building the contraction hierarchy (zero when not used or already built).
    pub contraction: Duration,
    /// Applying `pgr:constraint`, `pgr:shapes` or `pgr:vehicle`: evaluating the constraint or
    /// validating the focus nodes, filtering the graph (and customizing the hierarchy for
    /// it); zero when cached.
    pub constraint: Duration,
    /// Shortest-path search time.
    pub search: Duration,
    pub result_rows: usize,
}

type EvaluatorFactory = Arc<dyn Fn() -> SparqlEvaluator + Send + Sync>;

/// The `SERVICE <http://w3id.org/tripletrail/pgr>` handler.
#[derive(Clone)]
pub struct PgrService {
    store: Dataset,
    cache: Arc<NetworkCache>,
    evaluator: EvaluatorFactory,
    stats: Arc<Mutex<QueryStats>>,
    /// Stops the algorithms once the query's timeout has passed.
    cancellation: Option<CancellationToken>,
    /// The graphs every sub-select reads, for a caller limited to some graphs (#203): the
    /// calling query's dataset. Networks built on it are cached apart.
    dataset: Option<QueryDataset>,
    /// What a caller under row rules sees (#202): every sub-select reads only those rows,
    /// and networks built on them are cached apart.
    rows: Option<RowView>,
}

impl PgrService {
    /// `evaluator` builds the evaluator used for edge queries (so they can call geo functions).
    pub fn new(
        store: Dataset,
        cache: Arc<NetworkCache>,
        evaluator: impl Fn() -> SparqlEvaluator + Send + Sync + 'static,
    ) -> Self {
        Self {
            store,
            cache,
            evaluator: Arc::new(evaluator),
            stats: Arc::default(),
            cancellation: None,
            dataset: None,
            rows: None,
        }
    }

    /// The same service, its sub-selects (edges, points, restrictions, named edge queries)
    /// reading only `dataset` (#203). The shape filters and tables, which read the store
    /// directly, are refused.
    #[must_use]
    pub fn with_dataset(&self, dataset: QueryDataset) -> Self {
        Self {
            dataset: Some(dataset),
            ..self.clone()
        }
    }

    /// The same service, every sub-select reading only what `rows` shows (#202). As with
    /// [`PgrService::with_dataset`], what reads the store directly is refused.
    #[must_use]
    pub fn with_rows(&self, rows: RowView) -> Self {
        Self {
            rows: Some(rows),
            ..self.clone()
        }
    }

    /// `query` prepared with `evaluator` on this service's dataset.
    fn prepare(&self, evaluator: SparqlEvaluator, query: spargebra::Query) -> PreparedSparqlQuery {
        let mut prepared = evaluator.for_query(query);
        if let Some(d) = &self.dataset {
            *prepared.dataset_mut() = d.clone();
        }
        prepared
    }

    /// A network cache key: `key`, and the dataset when there is one.
    fn key(&self, key: String) -> String {
        let key = match &self.dataset {
            Some(d) => format!("{key}\n#dataset {d:?}"),
            None => key,
        };
        match &self.rows {
            Some(r) => format!("{key}\n#rows {:016x}", r.fingerprint()),
            None => key,
        }
    }

    /// Refuses what reads the store directly, for a caller limited to some graphs.
    fn unscoped(&self, what: &str) -> Result<(), PgrError> {
        if self.rows.is_some() {
            return Err(PgrError::Request(format!(
                "{what} is not available under row-level access rules"
            )));
        }
        if self.dataset.is_some() {
            return Err(PgrError::Request(format!(
                "{what} is not available with access to only some graphs"
            )));
        }
        Ok(())
    }

    /// The same service, with its algorithms stopping once `token` is cancelled: the
    /// SERVICE call then fails with [`PgrError::Cancelled`].
    pub fn with_cancellation_token(&self, token: CancellationToken) -> Self {
        Self {
            cancellation: Some(token),
            ..self.clone()
        }
    }

    pub fn last_stats(&self) -> QueryStats {
        *self.stats.lock_unpoisoned()
    }

    pub fn stats_handle(&self) -> Arc<Mutex<QueryStats>> {
        Arc::clone(&self.stats)
    }

    fn network(&self, request: &Request) -> Result<(Arc<Network>, bool), PgrError> {
        let Some(edges) = &request.edges else {
            return Err(PgrError::Request(
                "missing edge sub-select or pgr:network".into(),
            ));
        };
        let (network, hit) = self.network_of(edges, request)?;
        if request.function.needs_cost() && !network.has_cost {
            return Err(PgrError::Request(
                "this function needs ?cost in the edge set".into(),
            ));
        }
        Ok((network, hit))
    }

    fn network_of(
        &self,
        edges: &EdgeSource,
        request: &Request,
    ) -> Result<(Arc<Network>, bool), PgrError> {
        match edges {
            EdgeSource::Query(pattern) => self.cache.get_or_build(
                self.key(pattern.to_string()),
                request.directed,
                predicates_read(pattern),
                false,
                || {
                    Network::build_prefixed(
                        &self.store,
                        self.dataset.as_ref(),
                        self.rows.as_ref(),
                        &*self.evaluator,
                        pattern,
                        request.directed,
                        "",
                    )
                },
            ),
            EdgeSource::Network(iri) => {
                let pattern = self.named_edge_query(iri)?;
                // The network also depends on its definition.
                let reads = predicates_read(&pattern).map(|mut r| {
                    r.insert(format!("{PGR}edgeQuery"));
                    r
                });
                self.cache.get_or_build(
                    self.key(iri.as_str().to_owned()),
                    request.directed,
                    reads,
                    true,
                    || {
                        Network::build_prefixed(
                            &self.store,
                            self.dataset.as_ref(),
                            self.rows.as_ref(),
                            &*self.evaluator,
                            &pattern,
                            request.directed,
                            "",
                        )
                    },
                )
            }
        }
    }

    /// The network without the edges the request's filter (`pgr:constraint`, `pgr:shapes`,
    /// `pgr:vehicle`) rejects, cached on the network, and whether it was built now.
    fn filtered(
        &self,
        network: &Network,
        request: &Request,
    ) -> Result<Option<(Arc<Constrained>, bool)>, PgrError> {
        let Some(key) = shapes::filter_key(request) else {
            return Ok(None);
        };
        if request.shapes.is_some() || request.vehicle.is_some() {
            // The shapes are checked on the store's graphs, not the caller's.
            self.unscoped("pgr:shapes or pgr:vehicle")?;
        }
        network
            .filtered(&key, |n| {
                shapes::rejects(&self.store, &*self.evaluator, n, request)
            })
            .map(Some)
    }

    /// Reads `<iri> pgr:edgeQuery "SELECT ..."` from the store.
    fn named_edge_query(&self, iri: &NamedNode) -> Result<GraphPattern, PgrError> {
        let lookup = format!("SELECT ?q WHERE {{ {iri} <{PGR}edgeQuery> ?q }} LIMIT 1");
        let mut prepared = SparqlEvaluator::new()
            .parse_query(&lookup)
            .map_err(|e| PgrError::EdgeQuery(Box::new(e)))?;
        if let Some(d) = &self.dataset {
            *prepared.dataset_mut() = d.clone();
        }
        let results = self
            .store
            .execute_in(prepared, self.rows.as_ref())
            .map_err(|e| PgrError::EdgeQuery(Box::new(e)))?;
        let QueryResults::Solutions(mut solutions) = results else {
            unreachable!()
        };
        let Some(Term::Literal(q)) = solutions
            .next()
            .transpose()
            .map_err(|e| PgrError::EdgeQuery(Box::new(e)))?
            .and_then(|s| s.get("q").cloned())
        else {
            return Err(PgrError::Request(format!(
                "no pgr:edgeQuery for network {iri}"
            )));
        };
        match spargebra::SparqlParser::new().parse_query(q.value()) {
            Ok(spargebra::Query::Select { pattern, .. }) => Ok(pattern),
            Ok(_) => Err(PgrError::Request(format!(
                "pgr:edgeQuery of {iri} must be a SELECT"
            ))),
            Err(e) => Err(PgrError::EdgeQuery(Box::new(e))),
        }
    }

    /// Evaluates a restriction sub-select. Two shapes are accepted:
    /// `?path` (edge ids separated by spaces or commas) with an optional `?cost`, or one row
    /// per position: `?id ?seq ?edge` with an optional `?cost`. An unbound or missing `?cost`
    /// forbids the sequence; otherwise it is added to paths that traverse it.
    fn restrictions(
        &self,
        network: &Network,
        pattern: &GraphPattern,
    ) -> Result<Restrictions, PgrError> {
        let query = spargebra::Query::Select {
            dataset: None,
            pattern: pattern.clone(),
            base_iri: None,
        };
        let QueryResults::Solutions(solutions) = self
            .store
            .execute_in(self.prepare((self.evaluator)(), query), self.rows.as_ref())
            .map_err(|e| PgrError::EdgeQuery(Box::new(e)))?
        else {
            unreachable!()
        };
        let position = |name: &str| {
            solutions
                .variables()
                .iter()
                .position(|v| v.as_str() == name)
        };
        let (path, id, seq, edge, cost) = (
            position("path"),
            position("id"),
            position("seq"),
            position("edge"),
            position("cost"),
        );
        let cost_of = |values: &[Option<Term>]| {
            cost.and_then(|c| values[c].as_ref())
                .and_then(number)
                .filter(|c| *c >= 0.0)
                .unwrap_or(f64::INFINITY)
        };
        let mut list = Vec::new();
        // Rows of the per-position shape, grouped by ?id.
        let mut sequences: std::collections::BTreeMap<String, Sequence> = Default::default();
        for solution in solutions {
            let solution = solution.map_err(|e| PgrError::EdgeQuery(Box::new(e)))?;
            let values = solution.values();
            if let Some(p) = path {
                let Some(Term::Literal(l)) = &values[p] else {
                    continue;
                };
                // An unknown edge can never be traversed, so neither can the sequence.
                let ids: Option<Vec<u32>> = l
                    .value()
                    .split(|c: char| c.is_whitespace() || c == ',')
                    .filter(|s| !s.is_empty())
                    .map(|s| network.edge_id(s.trim_start_matches('<').trim_end_matches('>')))
                    .collect();
                if let Some(path) = ids {
                    list.push(Restriction {
                        path,
                        cost: cost_of(values),
                    });
                }
            } else {
                let (Some(i), Some(q), Some(e)) = (id, seq, edge) else {
                    return Err(PgrError::Request(
                        "a restriction sub-select projects ?path, or ?id ?seq ?edge".into(),
                    ));
                };
                let (Some(i), Some(q), Some(e)) = (&values[i], &values[q], &values[e]) else {
                    continue;
                };
                let Some(q) = number(q) else { continue };
                let entry = sequences
                    .entry(i.to_string())
                    .or_insert((f64::INFINITY, Vec::new()));
                entry.0 = entry.0.min(cost_of(values));
                entry
                    .1
                    .push((q, edge_key(e).and_then(|k| network.edge_id(k))));
            }
        }
        for (_, (cost, mut steps)) in sequences {
            steps.sort_by(|a, b| a.0.total_cmp(&b.0));
            if let Some(path) = steps.into_iter().map(|s| s.1).collect() {
                list.push(Restriction { path, cost });
            }
        }
        Ok(Restrictions::new(&list))
    }

    /// Evaluates a points sub-select: `?pid ?edge ?fraction` rows, the point's term, the edge
    /// it lies on and how far along it (0 at the source, 1 at the target).
    fn points(
        &self,
        network: &Network,
        pattern: &GraphPattern,
    ) -> Result<Vec<(Term, u32, f64)>, PgrError> {
        let query = spargebra::Query::Select {
            dataset: None,
            pattern: pattern.clone(),
            base_iri: None,
        };
        let QueryResults::Solutions(solutions) = self
            .store
            .execute_in(self.prepare((self.evaluator)(), query), self.rows.as_ref())
            .map_err(|e| PgrError::EdgeQuery(Box::new(e)))?
        else {
            unreachable!()
        };
        let position = |name: &str| {
            solutions
                .variables()
                .iter()
                .position(|v| v.as_str() == name)
                .ok_or_else(|| PgrError::Request(format!("the points query must project ?{name}")))
        };
        let (pid, edge, fraction) = (position("pid")?, position("edge")?, position("fraction")?);
        let mut points = Vec::new();
        for solution in solutions {
            let solution = solution.map_err(|e| PgrError::EdgeQuery(Box::new(e)))?;
            let values = solution.values();
            let (Some(p), Some(e), Some(f)) = (&values[pid], &values[edge], &values[fraction])
            else {
                continue;
            };
            let (Some(e), Some(f)) = (edge_key(e).and_then(|k| network.edge_id(k)), number(f))
            else {
                continue;
            };
            points.push((p.clone(), e, f));
        }
        Ok(points)
    }

    /// Evaluates a sub-select: its variables and rows. The sub-select may call the pgr
    /// SERVICE itself, so that pgr_TSP can run over a pgr_dijkstraCostMatrix.
    fn select(&self, pattern: &GraphPattern) -> Result<SelectRows, PgrError> {
        let query = spargebra::Query::Select {
            dataset: None,
            pattern: pattern.clone(),
            base_iri: None,
        };
        let evaluator = (self.evaluator)()
            .with_service_handler(NamedNode::new_unchecked(PGR_SERVICE), self.clone());
        let QueryResults::Solutions(solutions) = self
            .store
            .execute_in(self.prepare(evaluator, query), self.rows.as_ref())
            .map_err(|e| PgrError::EdgeQuery(Box::new(e)))?
        else {
            unreachable!()
        };
        let variables = solutions.variables().to_vec();
        let mut rows = Vec::new();
        for solution in solutions {
            let solution = solution.map_err(|e| PgrError::EdgeQuery(Box::new(e)))?;
            rows.push(solution.values().to_vec());
        }
        Ok((variables, rows))
    }

    /// The points and cost matrix of pgr_TSP (from `?start_vid ?end_vid ?agg_cost` rows) or
    /// pgr_TSPeuclidean (from `?id ?x ?y` rows).
    fn tsp_input(&self, request: &Request) -> Result<(Vec<Term>, CostMatrix), PgrError> {
        let (pattern, names) = match (&request.matrix, &request.coordinates) {
            (Some(m), _) => (m, ["start_vid", "end_vid", "agg_cost"]),
            (_, Some(c)) => (c, ["id", "x", "y"]),
            _ => unreachable!("checked by Request::parse"),
        };
        let (variables, rows) = self.select(pattern)?;
        let position = |name: &str| {
            variables
                .iter()
                .position(|v| v.as_str() == name)
                .ok_or_else(|| PgrError::Request(format!("the sub-select must project ?{name}")))
        };
        let (a, b, c) = (
            position(names[0])?,
            position(names[1])?,
            position(names[2])?,
        );
        let mut terms = Vec::new();
        for r in &rows {
            if request.matrix.is_some() {
                terms.extend(r[a].iter().chain(&r[b]).cloned());
            } else {
                terms.extend(r[a].iter().cloned());
            }
        }
        terms.sort_by_cached_key(Term::to_string);
        terms.dedup();
        let points = tables::sorted_points(terms);
        let index: HashMap<&Term, u32> = points
            .iter()
            .enumerate()
            .map(|(i, t)| (t, i as u32))
            .collect();
        let matrix = if request.matrix.is_some() {
            CostMatrix::new(
                points.len(),
                rows.iter().filter_map(|r| {
                    let (Some(s), Some(t), Some(cost)) = (&r[a], &r[b], r[c].as_ref()) else {
                        return None;
                    };
                    Some((index[s], index[t], number(cost)?))
                }),
            )
        } else {
            let mut xy = vec![(f64::NAN, f64::NAN); points.len()];
            for r in &rows {
                if let (Some(id), Some(x), Some(y)) = (
                    &r[a],
                    r[b].as_ref().and_then(number),
                    r[c].as_ref().and_then(number),
                ) {
                    xy[index[id] as usize] = (x, y);
                }
            }
            CostMatrix::euclidean(&xy)
        };
        Ok((points, matrix))
    }

    /// The functions on edge geometries (see [`topology`]), over the rows of the edge set.
    fn topology_table(&self, request: &Request) -> Result<Vec<tables::Row>, PgrError> {
        let pattern = match &request.edges {
            Some(EdgeSource::Query(p)) => p.clone(),
            Some(EdgeSource::Network(iri)) => self.named_edge_query(iri)?,
            None => unreachable!("checked by Request::parse"),
        };
        let (variables, rows) = self.select(&pattern)?;
        let table = topology::Table { variables, rows };
        let tol = request.tolerance.unwrap_or(0.0);
        match request.function {
            PgrFunction::CheckTopology => topology::check_topology(&table, tol),
            PgrFunction::AnalyzeGraph => topology::analyze_graph(&table, tol),
            PgrFunction::BuildTopology => topology::build_topology(&table, tol),
            PgrFunction::NodeNetwork => topology::node_network(&table, tol),
            PgrFunction::SeparateCrossing | PgrFunction::SeparateTouching => topology::separate(
                &table,
                tol,
                request.function == PgrFunction::SeparateCrossing,
            ),
            PgrFunction::FindCloseEdges => {
                let points: Vec<(Term, Term)> = match &request.point_rows {
                    Some(p) => {
                        let (vars, rows) = self.select(p)?;
                        let at = |n: &str| vars.iter().position(|v| v.as_str() == n);
                        let (Some(pid), Some(geom)) = (at("pid"), at("geom")) else {
                            unreachable!("checked by Request::parse")
                        };
                        rows.into_iter()
                            .filter_map(|r| Some((r[pid].clone()?, r[geom].clone()?)))
                            .collect()
                    }
                    None => request
                        .point_geoms
                        .iter()
                        .map(|g| (g.clone(), g.clone()))
                        .collect(),
                };
                topology::find_close_edges(&table, &points, tol, request.cap)
            }
            f => unreachable!("{f:?} is not a topology function"),
        }
    }

    /// The functions whose results are tables rather than paths (see [`tables`]).
    fn run_table(
        &self,
        request: &Request,
        columns: &[&str],
    ) -> Result<(Arc<[Variable]>, Vec<QuerySolution>), PgrError> {
        let index: Vec<usize> = request
            .outputs
            .iter()
            .map(|(name, _)| tables::column_index(columns, name))
            .collect::<Result<_, _>>()?;
        let variables: Arc<[Variable]> = request
            .outputs
            .iter()
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
            .into();
        let mut stats = QueryStats::default();
        let table = match request.function {
            PgrFunction::Version | PgrFunction::FullVersion => {
                tables::version_table(request.function)
            }
            PgrFunction::CompatibilityShapes => {
                self.unscoped("pgr:compatibilityShapes")?;
                let start = Instant::now();
                let table = shapes::compatibility_table(&self.store, request)?;
                stats.search = start.elapsed();
                table
            }
            PgrFunction::ShaclRules => {
                self.unscoped("pgr:shaclRules")?;
                let start = Instant::now();
                let table = shapes::rules_table(&self.store, &*self.evaluator, request)?;
                stats.search = start.elapsed();
                table
            }
            PgrFunction::Tsp | PgrFunction::TspEuclidean => {
                let (points, matrix) = self.tsp_input(request)?;
                let start = Instant::now();
                let table = tables::tsp_table(request, &points, &matrix)?;
                stats.search = start.elapsed();
                table
            }
            f if f.is_vrp() => {
                let (orders, vehicles) = request
                    .orders
                    .as_ref()
                    .zip(request.vehicles.as_ref())
                    .expect("checked by Request::parse");
                let orders = self.select(orders)?;
                let vehicles = self.select(vehicles)?;
                let matrix = request
                    .matrix
                    .as_ref()
                    .map(|m| self.select(m))
                    .transpose()?;
                let start = Instant::now();
                let table = crate::vrp::vrp_table(request, &orders, &vehicles, matrix.as_ref())?;
                stats.search = start.elapsed();
                table
            }
            PgrFunction::NetworkInfo => {
                let (network, cache_hit) = self.network(request)?;
                (stats.cache_hit, stats.build) = (cache_hit, network.stats);
                tables::network_info_table(request, &network, cache_hit)
            }
            f if f.is_topology() => {
                let start = Instant::now();
                let table = self.topology_table(request)?;
                stats.search = start.elapsed();
                table
            }
            _ => {
                let (network, cache_hit) = self.network(request)?;
                let start = Instant::now();
                let table = tables::graph_table(request, &network)?;
                (stats.cache_hit, stats.build, stats.search) =
                    (cache_hit, network.stats, start.elapsed());
                table
            }
        };
        stats.result_rows = table.len();
        *self.stats.lock_unpoisoned() = stats;
        let solutions = table
            .into_iter()
            .map(|row| {
                let values: Vec<Option<Term>> = index.iter().map(|&i| row[i].clone()).collect();
                QuerySolution::from((Arc::clone(&variables), values))
            })
            .collect();
        Ok((variables, solutions))
    }

    fn run(&self, request: &Request) -> Result<(Arc<[Variable]>, Vec<QuerySolution>), PgrError> {
        if let Some(columns) = tables::columns(request.function) {
            return self.run_table(request, columns);
        }
        if request.function == PgrFunction::ParetoRoutes {
            return self.run_pareto(request);
        }
        let columns: Vec<Column> = request
            .outputs
            .iter()
            .map(|(name, _)| Column::parse(name, request.function))
            .collect::<Result<_, _>>()?;
        let variables: Arc<[Variable]> = request
            .outputs
            .iter()
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
            .into();
        let rows = match request.function {
            PgrFunction::RouteHierarchical | PgrFunction::Refine => {
                self.run_hierarchical(request, &columns)?
            }
            _ => self.run_paths(request, &columns)?,
        };
        let solutions = rows
            .into_iter()
            .map(|r| QuerySolution::from((Arc::clone(&variables), r)))
            .collect();
        Ok((variables, solutions))
    }

    /// `rail:paretoRoutes` (see [`crate::pareto`]).
    fn run_pareto(
        &self,
        request: &Request,
    ) -> Result<(Arc<[Variable]>, Vec<QuerySolution>), PgrError> {
        let (network, cache_hit) = self.network(request)?;
        let filtered = self.filtered(&network, request)?;
        let disabled = filtered.as_ref().map(|(c, _)| c.disabled.as_slice());
        let names: Vec<String> = request.outputs.iter().map(|(n, _)| n.clone()).collect();
        let start = Instant::now();
        let table = crate::pareto::pareto_table(request, &network, disabled, &names)?;
        *self.stats.lock_unpoisoned() = QueryStats {
            cache_hit,
            build: network.stats,
            search: start.elapsed(),
            result_rows: table.len(),
            ..QueryStats::default()
        };
        let variables: Arc<[Variable]> = request
            .outputs
            .iter()
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
            .into();
        let solutions = table
            .into_iter()
            .map(|r| QuerySolution::from((Arc::clone(&variables), r)))
            .collect();
        Ok((variables, solutions))
    }

    fn run_paths(&self, request: &Request, columns: &[Column]) -> Result<Vec<Row>, PgrError> {
        let (network, cache_hit) = self.network(request)?;
        let restrictions = match &request.restrictions {
            Some(pattern) => self.restrictions(&network, pattern)?,
            None => Restrictions::default(),
        };
        let (constrained, built) = match self.filtered(&network, request)? {
            Some((c, built)) => (Some(c), built),
            None => (None, false),
        };
        let disabled = constrained.as_ref().map(|c| c.disabled.as_slice());
        let mut constraint = match &constrained {
            Some(c) if built => c.build,
            _ => Duration::ZERO,
        };
        let augmented;
        let mut point_list = Vec::new();
        let g = match &request.points {
            Some(pattern) => {
                point_list = self.points(&network, pattern)?;
                augmented = network.graph.with_points(&point_list, request.directed);
                &augmented
            }
            None => &network.graph,
        };
        let masked;
        let graph: &Graph = match (&constrained, &request.points) {
            (Some(c), None) => &c.graph,
            (Some(c), Some(_)) => {
                masked = g.graph.without_edges(&c.disabled);
                &masked
            }
            (None, _) => &g.graph,
        };
        // The contraction hierarchy, when the query asks for it (built once per network, and
        // customized once per constraint).
        let contraction_start = Instant::now();
        let ch = (request.contracted && request.points.is_none()).then(|| network.hierarchy());
        let contraction = contraction_start.elapsed();
        let ch = match (&constrained, ch) {
            (Some(c), Some(_)) => {
                let start = Instant::now();
                let fresh = !c.is_customized();
                let h = network.constrained_hierarchy(c);
                if fresh {
                    constraint += start.elapsed();
                }
                Some(h)
            }
            (_, ch) => ch,
        };
        if constrained.is_some()
            && matches!(
                request.function,
                PgrFunction::EdgeDisjointPaths
                    | PgrFunction::HawickCircuits
                    | PgrFunction::ChinesePostman
            )
        {
            return Err(PgrError::Request(
                "pgr:constraint applies to the path and distance functions".into(),
            ));
        }
        let periods = match request.function {
            PgrFunction::RouteAt => Some(self.time_table(&network, request)?),
            _ => None,
        };
        let vid = |t: &Term| g.vertex_id(t);
        let starts: Vec<(u32, &Term)> = request
            .start_vids
            .iter()
            .filter_map(|t| Some((vid(t)?, t)))
            .collect();
        let ends: Vec<(u32, &Term)> = request
            .end_vids
            .iter()
            .filter_map(|t| Some((vid(t)?, t)))
            .collect();

        let mut w = Writer::new(&network, g, columns, &point_list);
        let search_start = Instant::now();
        let mut rows: Vec<Row> = Vec::new();
        let none = Extra::default();

        // One `start_vid, end_vid, agg_cost` row (the *Cost functions).
        let push_cost = |rows: &mut Vec<Row>, s: &Term, t: &Term, cost: f64| {
            let cell = |c: &Column| match c {
                Column::StartVid => Some(s.clone()),
                Column::EndVid => Some(t.clone()),
                Column::AggCost => Some(Literal::from(cost).into()),
                _ => None,
            };
            rows.push(columns.iter().map(cell).collect());
        };

        SPACES.with(|spaces| -> Result<(), PgrError> {
            let (a, b, c) = &mut *spaces.borrow_mut();
            match request.function {
                PgrFunction::Dijkstra => {
                    let targets: Vec<u32> = ends.iter().map(|e| e.0).collect();
                    for &(s, _) in &starts {
                        for p in shortest(graph, ch, a, c, s, &targets)
                            .into_iter()
                            .flatten()
                        {
                            if p.start_vid != p.end_vid {
                                w.path(&mut rows, &p, 1, 0.0, &none);
                            }
                        }
                    }
                }
                PgrFunction::RouteAt => {
                    let table = periods.as_ref().expect("built above");
                    let departure = request.departure.as_ref().expect("checked by Request");
                    let (start, date_time) = time::seconds(departure).ok_or_else(|| {
                        PgrError::Request(
                            "pgr:departure must be a number or an xsd:dateTime".into(),
                        )
                    })?;
                    let extra = Extra {
                        time: Some((start, date_time)),
                        ..Extra::default()
                    };
                    let targets: Vec<u32> = ends.iter().map(|e| e.0).collect();
                    for &(s, _) in &starts {
                        for p in td_dijkstra(graph, a, table, s, &targets, start, request.wait)
                            .into_iter()
                            .flatten()
                        {
                            if p.start_vid != p.end_vid {
                                w.path(&mut rows, &p, 1, 0.0, &extra);
                            }
                        }
                    }
                }
                PgrFunction::BdDijkstra
                | PgrFunction::BdDijkstraCost
                | PgrFunction::AStar
                | PgrFunction::AStarCost
                | PgrFunction::BdAStar
                | PgrFunction::BdAStarCost => {
                    let astar_like = !matches!(
                        request.function,
                        PgrFunction::BdDijkstra | PgrFunction::BdDijkstraCost
                    );
                    if astar_like && !graph.has_coords() {
                        return Err(PgrError::Request(
                            "pgr_aStar needs ?x1 ?y1 ?x2 ?y2 in the edge set".into(),
                        ));
                    }
                    let heuristic = Heuristic::from_code(request.heuristic)
                        .ok_or_else(|| PgrError::Request("pgr:heuristic must be 0..6".into()))?;
                    // The bidirectional A* returns the same paths as A*.
                    for &(s, s_term) in &starts {
                        for &(t, t_term) in &ends {
                            if s == t {
                                continue;
                            }
                            let p = if astar_like {
                                astar(graph, a, s, t, heuristic, request.factor)
                            } else {
                                match ch {
                                    Some(_) => shortest(graph, ch, a, c, s, &[t]).pop().flatten(),
                                    None => bd_dijkstra(graph, a, b, s, t),
                                }
                            };
                            match p {
                                Some(p) if request.function.is_cost() => {
                                    push_cost(&mut rows, s_term, t_term, p.total_cost())
                                }
                                Some(p) => w.path(&mut rows, &p, 1, 0.0, &none),
                                None => {}
                            }
                        }
                    }
                }
                PgrFunction::Ksp => {
                    for &(s, _) in &starts {
                        for &(t, _) in &ends {
                            for (i, p) in ksp(graph, a, s, t, request.k, request.heap_paths)
                                .iter()
                                .enumerate()
                            {
                                w.path(&mut rows, p, i as i64 + 1, 0.0, &none);
                            }
                        }
                    }
                }
                PgrFunction::TurnRestrictedPath => {
                    for &(s, _) in &starts {
                        for &(t, _) in &ends {
                            let paths = turn_restricted_path(
                                graph,
                                a,
                                &restrictions,
                                s,
                                t,
                                request.k,
                                request.heap_paths,
                                request.stop_on_first,
                                request.strict,
                            );
                            for (i, p) in paths.iter().enumerate() {
                                w.path(&mut rows, p, i as i64 + 1, 0.0, &none);
                            }
                        }
                    }
                }
                PgrFunction::Trsp => {
                    let targets: Vec<u32> = ends.iter().map(|e| e.0).collect();
                    for &(s, _) in &starts {
                        for p in trsp(graph, a, &restrictions, s, &targets)
                            .into_iter()
                            .flatten()
                        {
                            if p.start_vid != p.end_vid {
                                w.path(&mut rows, &p, 1, 0.0, &none);
                            }
                        }
                    }
                }
                PgrFunction::DijkstraVia => {
                    // One leg per consecutive pair of via vertices; an unknown vertex or a
                    // missing leg is skipped, or empties the result when pgr:strict is true.
                    let vias: Vec<Option<u32>> = request.vias.iter().map(vid).collect();
                    let mut legs = Vec::new();
                    for pair in vias.windows(2) {
                        let leg = match (pair[0], pair[1]) {
                            (Some(s), Some(t)) if s != t => {
                                shortest(graph, ch, a, c, s, &[t]).pop().flatten()
                            }
                            _ => None,
                        };
                        if leg.is_none() && request.strict {
                            legs.clear();
                            break;
                        }
                        legs.extend(leg);
                    }
                    let mut route = 0.0;
                    for (i, p) in legs.iter().enumerate() {
                        w.path(&mut rows, p, i as i64 + 1, route, &none);
                        route += p.total_cost();
                    }
                }
                PgrFunction::ConnectedComponents | PgrFunction::StrongComponents => {
                    let labels = if request.function == PgrFunction::ConnectedComponents {
                        connected_components(graph)
                    } else {
                        strong_components(graph)
                    };
                    // Name each component by its smallest vertex term, as pgRouting names it
                    // by its smallest vertex id; list components and their vertices in that
                    // order. Terms are compared by their text, through ranks.
                    let local;
                    let rank: &[u32] = if std::ptr::eq(g, &network.graph) {
                        network.text_ranks()
                    } else {
                        local = crate::network::text_ranks(&g.vertices);
                        &local
                    };
                    let mut name: std::collections::HashMap<u32, usize> = Default::default();
                    for (v, &l) in labels.iter().enumerate() {
                        let n = name.entry(l).or_insert(v);
                        if rank[v] < rank[*n] {
                            *n = v;
                        }
                    }
                    let mut members: Vec<usize> = (0..labels.len()).collect();
                    members.sort_unstable_by_key(|&v| {
                        (u64::from(rank[name[&labels[v]]]) << 32) | u64::from(rank[v])
                    });
                    let names: std::collections::HashMap<u32, &Term> =
                        name.iter().map(|(&l, &v)| (l, &g.vertices[v])).collect();
                    let wants = |k: fn(&Column) -> bool| columns.iter().any(k);
                    let mut sizes: std::collections::HashMap<u32, usize> = Default::default();
                    if wants(|c| matches!(c, Column::Size)) {
                        for &l in &labels {
                            *sizes.entry(l).or_default() += 1;
                        }
                    }
                    if wants(|c| matches!(c, Column::Size))
                        && !wants(|c| matches!(c, Column::Node))
                    {
                        // One row per component, in the order of their names.
                        let mut seq = 0;
                        for (i, &v) in members.iter().enumerate() {
                            if i > 0 && labels[members[i - 1]] == labels[v] {
                                continue;
                            }
                            seq += 1;
                            let l = labels[v];
                            let cell = |c: &Column| -> Option<Term> {
                                match c {
                                    Column::Seq => Some(Literal::from(seq as i64).into()),
                                    Column::Component => Some(names[&l].clone()),
                                    Column::Size => Some(Literal::from(sizes[&l] as i64).into()),
                                    _ => None,
                                }
                            };
                            rows.push(columns.iter().map(cell).collect());
                        }
                    } else {
                        for (seq, v) in members.into_iter().enumerate() {
                            let cell = |c: &Column| -> Option<Term> {
                                match c {
                                    Column::Seq => Some(Literal::from(seq as i64 + 1).into()),
                                    Column::Component => Some(names[&labels[v]].clone()),
                                    Column::Node => Some(g.vertices[v].clone()),
                                    Column::Size => {
                                        Some(Literal::from(sizes[&labels[v]] as i64).into())
                                    }
                                    _ => None,
                                }
                            };
                            rows.push(columns.iter().map(cell).collect());
                        }
                    }
                }
                PgrFunction::ArticulationPoints | PgrFunction::Bridges => {
                    let (cuts, bridges) = cut_vertices_and_edges(graph);
                    let terms: Vec<&Term> = if request.function == PgrFunction::Bridges {
                        bridges.iter().map(|&e| &g.edges[e as usize]).collect()
                    } else {
                        cuts.iter().map(|&v| &g.vertices[v as usize]).collect()
                    };
                    let mut terms: Vec<(String, &Term)> =
                        terms.into_iter().map(|t| (t.to_string(), t)).collect();
                    terms.sort_by(|x, y| x.0.cmp(&y.0));
                    for (seq, (_, t)) in terms.into_iter().enumerate() {
                        let cell = |c: &Column| -> Option<Term> {
                            match c {
                                Column::Seq => Some(Literal::from(seq as i64 + 1).into()),
                                Column::Node | Column::Edge => Some(t.clone()),
                                _ => None,
                            }
                        };
                        rows.push(columns.iter().map(cell).collect());
                    }
                }
                PgrFunction::DijkstraCost => {
                    // One search per start, in parallel: the graph is read-only and each
                    // thread has its own search space.
                    use rayon::prelude::*;
                    let targets: Vec<u32> = ends.iter().map(|e| e.0).collect();
                    let costs: Vec<Vec<Option<f64>>> = if starts.len() > 1 {
                        let check = tripletrail_routing::cancel::current();
                        starts
                            .par_iter()
                            .map_init(
                                || (SearchSpace::new(), CchSpace::new()),
                                |(a, c), &(s, _)| {
                                    tripletrail_routing::cancel::scope(check.clone(), || {
                                        shortest(graph, ch, a, c, s, &targets)
                                    })
                                        .into_iter()
                                        .map(|p| {
                                            p.filter(|p| p.start_vid != p.end_vid)
                                                .map(|p| p.total_cost())
                                        })
                                        .collect()
                                },
                            )
                            .collect()
                    } else {
                        starts
                            .iter()
                            .map(|&(s, _)| {
                                shortest(graph, ch, a, c, s, &targets)
                                    .into_iter()
                                    .map(|p| {
                                        p.filter(|p| p.start_vid != p.end_vid)
                                            .map(|p| p.total_cost())
                                    })
                                    .collect()
                            })
                            .collect()
                    };
                    for (&(_, s_term), costs) in starts.iter().zip(costs) {
                        for (cost, (_, t_term)) in costs.into_iter().zip(&ends) {
                            if let Some(cost) = cost {
                                push_cost(&mut rows, s_term, t_term, cost);
                            }
                        }
                    }
                }
                PgrFunction::DijkstraNear | PgrFunction::DijkstraNearCost => {
                    // The `cap` cheapest paths from any start to any end (ties by input order).
                    let targets: Vec<u32> = ends.iter().map(|e| e.0).collect();
                    let mut found = Vec::new();
                    for &(s, s_term) in &starts {
                        for (p, (_, t_term)) in shortest(graph, ch, a, c, s, &targets)
                            .into_iter()
                            .zip(&ends)
                        {
                            match p {
                                Some(p) if p.start_vid != p.end_vid => {
                                    found.push((p, s_term, *t_term))
                                }
                                _ => {}
                            }
                        }
                    }
                    found.sort_by(|x, y| x.0.total_cost().total_cmp(&y.0.total_cost()));
                    found.truncate(request.cap);
                    for (i, (p, s_term, t_term)) in found.iter().enumerate() {
                        if request.function == PgrFunction::DijkstraNearCost {
                            push_cost(&mut rows, s_term, t_term, p.total_cost());
                        } else {
                            w.path(&mut rows, p, i as i64 + 1, 0.0, &none);
                        }
                    }
                }
                PgrFunction::FloydWarshall | PgrFunction::Johnson => {
                    // All pairs: one full search from every vertex.
                    let n = graph.num_vertices() as u32;
                    let mut pairs: Vec<(String, String, &Term, &Term, f64)> = Vec::new();
                    for s in 0..n {
                        if tripletrail_routing::cancel::stopped_now() {
                            return Err(PgrError::Cancelled);
                        }
                        for r in driving_distance(graph, a, s, f64::INFINITY) {
                            if r.node != s {
                                let (st, tt) =
                                    (&g.vertices[s as usize], &g.vertices[r.node as usize]);
                                pairs.push((st.to_string(), tt.to_string(), st, tt, r.agg_cost));
                            }
                        }
                    }
                    pairs.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(&y.1)));
                    for (_, _, st, tt, cost) in pairs {
                        push_cost(&mut rows, st, tt, cost);
                    }
                }
                PgrFunction::Kruskal | PgrFunction::Prim => {
                    // Cheapest first; equal costs by edge term, so the order does not depend
                    // on how the network was built.
                    let mut forest = minimum_spanning_forest(graph);
                    forest.sort_by_cached_key(|&(e, cost)| {
                        (OrderedCost(cost), g.edges[e as usize].to_string())
                    });
                    let mut total = 0.0;
                    for (seq, (e, cost)) in forest.into_iter().enumerate() {
                        total += cost;
                        let cell = |c: &Column| -> Option<Term> {
                            match c {
                                Column::Seq => Some(Literal::from(seq as i64 + 1).into()),
                                Column::Edge => Some(g.edges[e as usize].clone()),
                                Column::Cost => Some(Literal::from(cost).into()),
                                Column::AggCost => Some(Literal::from(total).into()),
                                _ => None,
                            }
                        };
                        rows.push(columns.iter().map(cell).collect());
                    }
                }
                PgrFunction::BellmanFord
                | PgrFunction::EdwardMoore
                | PgrFunction::DagShortestPath
                | PgrFunction::BinaryBreadthFirstSearch => {
                    // Bellman-Ford and Moore keep arcs with negative costs.
                    let negative = matches!(
                        request.function,
                        PgrFunction::BellmanFord | PgrFunction::EdwardMoore
                    ) && g
                        .rows
                        .iter()
                        .any(|r| r.cost < 0.0 || r.reverse_cost.is_some_and(|c| c < 0.0));
                    let signed;
                    let graph = if negative {
                        let kept: Vec<EdgeRow> = g
                            .rows
                            .iter()
                            .filter(|r| disabled.is_none_or(|d| !d[r.id as usize]))
                            .copied()
                            .collect();
                        signed = Graph::build_keeping_negative(
                            graph.num_vertices(),
                            &kept,
                            request.directed,
                        );
                        &signed
                    } else {
                        graph
                    };
                    for &(s, _) in &starts {
                        // No tree (a negative cycle, or any cycle for the DAG search) gives no
                        // paths, as in pgRouting.
                        let tree: Option<ShortestTree> = match request.function {
                            PgrFunction::BellmanFord => bellman_ford(graph, s),
                            PgrFunction::EdwardMoore => edward_moore(graph, s),
                            PgrFunction::DagShortestPath => dag_shortest_paths(graph, s),
                            _ => Some(binary_bfs(graph, s).map_err(|w| {
                                PgrError::Request(format!(
                                    "pgr_binaryBreadthFirstSearch needs arc costs of 0 and one other value, not also {w}"
                                ))
                            })?),
                        };
                        let Some(tree) = tree else { continue };
                        for &(t, _) in &ends {
                            if t != s
                                && let Some(p) = tree.path(t)
                            {
                                w.path(&mut rows, &p, 1, 0.0, &none);
                            }
                        }
                    }
                }
                PgrFunction::TrspVia => {
                    // pgr_dijkstraVia under restrictions; with U_turn_on_edge false a leg may
                    // not leave along the edge the previous one arrived by.
                    let vias: Vec<Option<u32>> = request.vias.iter().map(vid).collect();
                    let mut legs: Vec<Path> = Vec::new();
                    for pair in vias.windows(2) {
                        let avoid = legs
                            .last()
                            .filter(|_| !request.u_turn_on_edge)
                            .and_then(|p| p.steps.iter().rev().nth(1))
                            .and_then(|s| s.edge);
                        let leg = match (pair[0], pair[1]) {
                            (Some(s), Some(t)) if s != t => {
                                trsp_avoiding(graph, a, &restrictions, s, &[t], avoid)
                                    .pop()
                                    .flatten()
                            }
                            _ => None,
                        };
                        if leg.is_none() && request.strict {
                            legs.clear();
                            break;
                        }
                        legs.extend(leg);
                    }
                    let mut route = 0.0;
                    for (i, p) in legs.iter().enumerate() {
                        w.path(&mut rows, p, i as i64 + 1, route, &none);
                        route += p.total_cost();
                    }
                }
                PgrFunction::EdgeDisjointPaths
                | PgrFunction::HawickCircuits
                | PgrFunction::ChinesePostman => {
                    // On the view, so ties break by id as in pgRouting.
                    // Points are not accepted here, so `g` is the network's graph.
                    let view = View::new(&network, request.directed);
                    let paths: Vec<Path> = match request.function {
                        PgrFunction::EdgeDisjointPaths => {
                            let mut paths = Vec::new();
                            for s in request.start_vids.iter().filter_map(|t| view.vertex(t)) {
                                for t in request.end_vids.iter().filter_map(|t| view.vertex(t)) {
                                    paths.extend(edge_disjoint_paths(&view.graph, s, t));
                                }
                            }
                            paths
                        }
                        PgrFunction::HawickCircuits => circuits(&view.graph),
                        _ => chinese_postman(&view.graph)
                            .filter(|p| !p.steps.is_empty())
                            .into_iter()
                            .collect(),
                    };
                    for (i, p) in paths.into_iter().enumerate() {
                        w.path(&mut rows, &view.to_network(p), i as i64 + 1, 0.0, &none);
                    }
                }
                PgrFunction::DrivingDistance => {
                    let max = request.distance.unwrap_or(f64::INFINITY);
                    let mut seq = 0i64;
                    for &(s, _) in &starts {
                        for n in driving_distance(graph, a, s, max) {
                            seq += 1;
                            let cell = |c: &Column| -> Option<Term> {
                                match c {
                                    Column::Seq => Some(Literal::from(seq).into()),
                                    Column::StartVid => {
                                        Some(g.vertices[n.start_vid as usize].clone())
                                    }
                                    Column::Node => Some(g.vertices[n.node as usize].clone()),
                                    Column::Pred => Some(g.vertices[n.pred as usize].clone()),
                                    Column::Edge => n.edge.map(|e| g.edges[e as usize].clone()),
                                    Column::Cost => Some(Literal::from(n.cost).into()),
                                    Column::AggCost => Some(Literal::from(n.agg_cost).into()),
                                    Column::Geom => {
                                        let (line, crs) = w.oriented(n.pred, n.node, n.edge?)?;
                                        Some(geometry::line_literal(line, &crs))
                                    }
                                    _ => None,
                                }
                            };
                            rows.push(columns.iter().map(cell).collect());
                        }
                    }
                }
                PgrFunction::Isochrone => {
                    let max = request.distance.unwrap_or(f64::INFINITY);
                    let xy = network.vertex_xy();
                    let at = |v: u32| -> Option<Coord> {
                        let (x, y) = match graph.coords() {
                            Some(c) => c[v as usize],
                            None => (*xy.get(v as usize)?)?,
                        };
                        Some(Coord { x, y })
                    };
                    for (seq, &(s, s_term)) in starts.iter().enumerate() {
                        let reached = driving_distance(graph, a, s, max);
                        let mut crs = None;
                        let mut points = Vec::new();
                        for n in &reached {
                            let u = n.node;
                            points.extend(at(u));
                            let d = n.agg_cost;
                            for (v, cost, e) in graph.arcs(u) {
                                let line = w.oriented(u, v, e);
                                if crs.is_none() {
                                    crs = line.as_ref().map(|l| l.1.clone());
                                }
                                if d + cost <= max {
                                    // Reached all along: its shape, if it has one.
                                    if let Some((l, _)) = line {
                                        points.extend_from_slice(&l);
                                    }
                                    continue;
                                }
                                // Reached part of the way.
                                let f = if cost > 0.0 { (max - d) / cost } else { 1.0 };
                                match (line, at(u), at(v)) {
                                    (Some((l, _)), _, _) => {
                                        points.push(geometry::interpolate(&l, f))
                                    }
                                    (None, Some(p), Some(q)) => points.push(Coord {
                                        x: p.x + (q.x - p.x) * f,
                                        y: p.y + (q.y - p.y) * f,
                                    }),
                                    _ => {}
                                }
                            }
                        }
                        let hull = geometry::hull(&points, request.concavity)
                            .map(|h| geometry::literal(&h, crs.as_ref().unwrap_or(&Crs::Crs84)));
                        let far = reached.iter().map(|n| n.agg_cost).fold(0.0, f64::max);
                        let cell = |c: &Column| -> Option<Term> {
                            match c {
                                Column::Seq => Some(Literal::from(seq as i64 + 1).into()),
                                Column::StartVid => Some(s_term.clone()),
                                Column::Geom => hull.clone(),
                                Column::AggCost => Some(Literal::from(far).into()),
                                _ => None,
                            }
                        };
                        rows.push(columns.iter().map(cell).collect());
                    }
                }
                f => unreachable!("{f:?} is a table function"),
            }
            Ok(())
        })?;
        let search = search_start.elapsed();
        *self.stats.lock_unpoisoned() = QueryStats {
            cache_hit,
            build: network.stats,
            contraction,
            constraint,
            search,
            result_rows: rows.len(),
        };
        Ok(rows)
    }

    /// rail:routeAt's periods: `?edge ?t_from ?t_to [?t_cost] [?t_reverse_cost]` rows. An
    /// unbound `?t_from` or `?t_to` leaves the period open on that side; an unbound,
    /// negative or infinite `?t_cost` closes the edge.
    fn time_table(&self, network: &Network, request: &Request) -> Result<TimeTable, PgrError> {
        let rows = network.edge_rows();
        let edge_source: Vec<u32> = rows
            .iter()
            .map(|&r| network.graph.rows[r as usize].source)
            .collect();
        let Some(pattern) = &request.periods else {
            return Ok(TimeTable::new(edge_source, []));
        };
        let (vars, values) = self.select(pattern)?;
        let at = |n: &str| vars.iter().position(|v| v.as_str() == n);
        let (edge, from, to, cost, reverse) = (
            at("edge").expect("checked by Request::parse"),
            at("t_from"),
            at("t_to"),
            at("t_cost"),
            at("t_reverse_cost"),
        );
        let mut periods = Vec::new();
        for r in &values {
            let Some(e) = r[edge]
                .as_ref()
                .and_then(edge_key)
                .and_then(|k| network.edge_id(k))
            else {
                continue;
            };
            let time = |p: Option<usize>, open: f64| -> Result<f64, PgrError> {
                match p.and_then(|p| r[p].as_ref()) {
                    None => Ok(open),
                    Some(t) => time::seconds(t).map(|t| t.0).ok_or_else(|| {
                        PgrError::Request(format!("{t} is not a time (a number or xsd:dateTime)"))
                    }),
                }
            };
            let num = |p: Option<usize>| p.and_then(|p| r[p].as_ref()).and_then(number);
            periods.push((
                e,
                Period {
                    from: time(from, f64::NEG_INFINITY)?,
                    to: time(to, f64::INFINITY)?,
                    cost: num(cost).unwrap_or(f64::INFINITY),
                    reverse_cost: num(reverse),
                },
            ));
        }
        Ok(TimeTable::new(edge_source, periods))
    }

    /// The fine network of rail:routeHierarchical.
    fn fine_network(&self, request: &Request) -> Result<Arc<Network>, PgrError> {
        let fine = request.fine.as_ref().expect("checked by Request::parse");
        let (network, _) = match fine {
            EdgeSource::Query(pattern) => self.cache.get_or_build(
                self.key(pattern.to_string()),
                request.directed,
                predicates_read(pattern),
                false,
                || {
                    Network::build_prefixed(
                        &self.store,
                        self.dataset.as_ref(),
                        self.rows.as_ref(),
                        &*self.evaluator,
                        pattern,
                        request.directed,
                        "fine_",
                    )
                },
            )?,
            EdgeSource::Network(_) => self.network_of(fine, request)?,
        };
        Ok(network)
    }

    /// rail:routeHierarchical and rail:refine. The coarse route (searched on the coarse
    /// network, or given) names coarse edges; the mapping gives the fine edges of each. The
    /// fine route is the cheapest path between the fine start and end vertices using only
    /// those fine edges (the corridor), or, when the corridor does not connect them, using
    /// any fine edge.
    fn run_hierarchical(
        &self,
        request: &Request,
        columns: &[Column],
    ) -> Result<Vec<Row>, PgrError> {
        let mut stats = QueryStats::default();
        let (fine, coarse_edges): (Arc<Network>, Vec<Term>) = match request.function {
            PgrFunction::RouteHierarchical => {
                let (coarse, hit) = self.network(request)?;
                stats.cache_hit = hit;
                stats.build = coarse.stats;
                let vid = |t: &Term| coarse.graph.vertex_id(t);
                let path = match (vid(&request.start_vids[0]), vid(&request.end_vids[0])) {
                    (Some(s), Some(t)) if s != t => SPACES.with(|spaces| {
                        let (a, _, c) = &mut *spaces.borrow_mut();
                        let ch = request.contracted.then(|| coarse.hierarchy());
                        shortest(&coarse.graph.graph, ch, a, c, s, &[t])
                            .pop()
                            .flatten()
                    }),
                    _ => None,
                };
                let Some(path) = path else {
                    return Ok(Vec::new());
                };
                let edges = path
                    .steps
                    .iter()
                    .filter_map(|s| s.edge)
                    .map(|e| coarse.graph.edges[e as usize].clone())
                    .collect();
                (self.fine_network(request)?, edges)
            }
            _ => {
                let (fine, hit) = self.network(request)?;
                stats.cache_hit = hit;
                stats.build = fine.stats;
                let pattern = request.coarse_route.as_ref().expect("checked");
                let (vars, rows) = self.select(pattern)?;
                let at = |n: &str| vars.iter().position(|v| v.as_str() == n);
                let edge = at("coarse_edge").expect("checked by Request::parse");
                // In the order of ?seq or ?path_seq when given, else as the rows come.
                let mut list: Vec<(f64, Term)> = rows
                    .iter()
                    .enumerate()
                    .filter_map(|(i, r)| {
                        let order = at("seq")
                            .or(at("path_seq"))
                            .and_then(|p| r[p].as_ref())
                            .and_then(number)
                            .unwrap_or(i as f64);
                        Some((order, r[edge].clone()?))
                    })
                    .collect();
                list.sort_by(|a, b| a.0.total_cmp(&b.0));
                (fine, list.into_iter().map(|x| x.1).collect())
            }
        };
        if request.function == PgrFunction::RouteHierarchical && !fine.has_cost {
            return Err(PgrError::Request(
                "the fine network needs ?fine_cost".into(),
            ));
        }
        // Fine edges by coarse edge.
        let (vars, rows) = self.select(request.mapping.as_ref().expect("checked"))?;
        let at = |n: &str| vars.iter().position(|v| v.as_str() == n);
        let (ce, fe) = (
            at("coarse_edge").expect("checked"),
            at("fine_edge").expect("checked"),
        );
        let route: HashMap<&Term, usize> = coarse_edges
            .iter()
            .enumerate()
            .rev()
            .map(|(i, t)| (t, i))
            .collect();
        // Which coarse edge of the route each corridor edge belongs to (the first, in order).
        let mut corridor: HashMap<u32, usize> = HashMap::new();
        for r in &rows {
            let (Some(c), Some(f)) = (&r[ce], &r[fe]) else {
                continue;
            };
            let (Some(&i), Some(e)) = (route.get(c), edge_key(f).and_then(|k| fine.edge_id(k)))
            else {
                continue;
            };
            let slot = corridor.entry(e).or_insert(i);
            *slot = (*slot).min(i);
        }
        let (constrained, built) = match self.filtered(&fine, request)? {
            Some((c, built)) => (Some(c), built),
            None => (None, false),
        };
        let graph = constrained.as_ref().map_or(&fine.graph.graph, |c| &c.graph);
        let terms = |list: &[Term]| -> Vec<u32> {
            list.iter()
                .filter_map(|t| fine.graph.vertex_id(t))
                .collect()
        };
        let (sources, targets) = if request.function == PgrFunction::RouteHierarchical {
            let pick =
                |fine: &[Term], coarse: &[Term]| terms(if fine.is_empty() { coarse } else { fine });
            (
                pick(&request.fine_start_vids, &request.start_vids),
                pick(&request.fine_end_vids, &request.end_vids),
            )
        } else {
            (terms(&request.start_vids), terms(&request.end_vids))
        };
        let search = Instant::now();
        let mut in_corridor = vec![false; fine.graph.edges.len()];
        for &e in corridor.keys() {
            in_corridor[e as usize] = true;
        }
        let found = SPACES.with(|spaces| {
            let (a, _, _) = &mut *spaces.borrow_mut();
            if let Some(p) =
                dijkstra_between(graph, a, &sources, &targets, |e| in_corridor[e as usize])
            {
                return Some((p, true));
            }
            dijkstra_between(graph, a, &sources, &targets, |_| true).map(|p| (p, false))
        });
        let mut out = Vec::new();
        if let Some((path, inside)) = found
            && path.start_vid != path.end_vid
        {
            let coarse_of: HashMap<u32, Term> = corridor
                .iter()
                .map(|(&e, &i)| (e, coarse_edges[i].clone()))
                .collect();
            let extra = Extra {
                coarse: Some(&coarse_of),
                corridor: Some(inside),
                ..Extra::default()
            };
            let mut w = Writer::new(&fine, &fine.graph, columns, &[]);
            w.path(&mut out, &path, 1, 0.0, &extra);
        }
        stats.search = search.elapsed();
        stats.constraint = match &constrained {
            Some(c) if built => c.build,
            _ => Duration::ZERO,
        };
        stats.result_rows = out.len();
        *self.stats.lock_unpoisoned() = stats;
        Ok(out)
    }
}

/// What a path's rows say beyond the path itself.
#[derive(Default)]
struct Extra<'a> {
    /// The departure time and whether to write times as `xsd:dateTime` (rail:routeAt).
    time: Option<(f64, bool)>,
    /// The coarse edge each fine edge refines (rail:routeHierarchical).
    coarse: Option<&'a HashMap<u32, Term>>,
    /// Whether the fine route stayed in the corridor.
    corridor: Option<bool>,
}

/// Writes paths as result rows, with edge and route geometries when asked for.
struct Writer<'a> {
    network: &'a Network,
    g: &'a InternedGraph<Term>,
    columns: &'a [Column],
    /// Where each point vertex (`withPoints`) lies on its edge.
    points: HashMap<u32, f64>,
    /// One row per path (only path-level columns were asked for) instead of one per step.
    per_path: bool,
    seq: i64,
}

impl<'a> Writer<'a> {
    fn new(
        network: &'a Network,
        g: &'a InternedGraph<Term>,
        columns: &'a [Column],
        points: &[(Term, u32, f64)],
    ) -> Self {
        let wants_geom = columns
            .iter()
            .any(|c| matches!(c, Column::Geom | Column::RouteGeom));
        let points = if wants_geom {
            points
                .iter()
                .filter_map(|(t, _, f)| Some((g.vertex_id(t)?, *f)))
                .collect()
        } else {
            HashMap::new()
        };
        let per_path = columns
            .iter()
            .any(|c| matches!(c, Column::RouteGeom | Column::RouteList))
            && columns.iter().all(|c| {
                matches!(
                    c,
                    Column::PathId
                        | Column::StartVid
                        | Column::EndVid
                        | Column::RouteGeom
                        | Column::RouteList
                        | Column::AggCost
                        | Column::Corridor
                )
            });
        Self {
            network,
            g,
            columns,
            points,
            per_path,
            seq: 0,
        }
    }

    /// The geometry of edge `edge` from vertex `from` to vertex `to` (either end, or a point
    /// on it), oriented that way.
    fn oriented(&self, from: u32, to: u32, edge: u32) -> Option<(Vec<Coord>, Crs)> {
        let (line, crs) = geometry::line(self.network.geom(edge)?)?;
        let row = self.network.graph.rows[*self.network.edge_rows().get(edge as usize)? as usize];
        if row.source == row.target {
            return Some((line, crs));
        }
        let at = |v: u32| {
            if v == row.source {
                Some(0.0)
            } else if v == row.target {
                Some(1.0)
            } else {
                self.points.get(&v).copied()
            }
        };
        Some((geometry::substring(&line, at(from)?, at(to)?), crs))
    }

    fn route_geometry(&self, p: &Path) -> Option<Term> {
        let mut crs = None;
        let parts: Vec<Vec<Coord>> = p
            .steps
            .windows(2)
            .filter_map(|w| {
                let (line, c) = self.oriented(w[0].node, w[1].node, w[0].edge?)?;
                crs.get_or_insert(c);
                Some(line)
            })
            .collect();
        let g = geometry::merge(parts)?;
        Some(geometry::literal(&g, crs.as_ref().unwrap_or(&Crs::Crs84)))
    }

    fn path(&mut self, rows: &mut Vec<Row>, p: &Path, path_id: i64, route_offset: f64, x: &Extra) {
        let g = self.g;
        let wants = |c: Column| {
            self.columns
                .iter()
                .any(|x| std::mem::discriminant(x) == std::mem::discriminant(&c))
        };
        let route_geom = if wants(Column::RouteGeom) {
            self.route_geometry(p)
        } else {
            None
        };
        let route_list = wants(Column::RouteList).then(|| {
            let items: Vec<String> = p
                .steps
                .iter()
                .filter_map(|s| s.edge)
                .map(|e| tables::list_item(&g.edges[e as usize]))
                .collect();
            Term::from(Literal::from(items.join(" ")))
        });
        if self.per_path {
            let cell = |c: &Column| -> Option<Term> {
                match c {
                    Column::PathId => Some(Literal::from(path_id).into()),
                    Column::StartVid => Some(g.vertices[p.start_vid as usize].clone()),
                    Column::EndVid => Some(g.vertices[p.end_vid as usize].clone()),
                    Column::RouteGeom => route_geom.clone(),
                    Column::RouteList => route_list.clone(),
                    Column::AggCost => Some(Literal::from(p.total_cost()).into()),
                    Column::Corridor => x.corridor.map(|b| Literal::from(b).into()),
                    _ => None,
                }
            };
            rows.push(self.columns.iter().map(cell).collect());
            return;
        }
        for (i, step) in p.steps.iter().enumerate() {
            self.seq += 1;
            let seq = self.seq;
            let next = p.steps.get(i + 1).map(|s| s.node);
            let cell = |c: &Column| -> Option<Term> {
                match c {
                    Column::Seq => Some(Literal::from(seq).into()),
                    Column::PathSeq => Some(Literal::from(i as i64 + 1).into()),
                    Column::PathId => Some(Literal::from(path_id).into()),
                    Column::StartVid => Some(g.vertices[p.start_vid as usize].clone()),
                    Column::EndVid => Some(g.vertices[p.end_vid as usize].clone()),
                    Column::Node => Some(g.vertices[step.node as usize].clone()),
                    Column::Edge => step.edge.map(|e| g.edges[e as usize].clone()),
                    Column::Cost => Some(Literal::from(step.cost).into()),
                    Column::AggCost => Some(Literal::from(step.agg_cost).into()),
                    Column::RouteAggCost => {
                        Some(Literal::from(route_offset + step.agg_cost).into())
                    }
                    Column::Geom => {
                        let (line, crs) = self.oriented(step.node, next?, step.edge?)?;
                        Some(geometry::line_literal(line, &crs))
                    }
                    Column::RouteGeom => route_geom.clone(),
                    Column::RouteList => route_list.clone(),
                    Column::Time => x.time.map(|(t, dt)| time::literal(t + step.agg_cost, dt)),
                    Column::CoarseEdge => x.coarse?.get(&step.edge?).cloned(),
                    Column::Corridor => x.corridor.map(|b| Literal::from(b).into()),
                    Column::Pred | Column::Component | Column::Size => None,
                }
            };
            rows.push(self.columns.iter().map(cell).collect());
        }
    }
}

/// An `f64` cost ordered by `total_cmp`, for sort keys.
#[derive(PartialEq)]
struct OrderedCost(f64);

impl Eq for OrderedCost {}

impl PartialOrd for OrderedCost {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedCost {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// One-to-many shortest paths, on the contraction hierarchy when there is one.
fn shortest(
    graph: &tripletrail_routing::Graph,
    ch: Option<&Cch>,
    space: &mut SearchSpace,
    ch_space: &mut CchSpace,
    source: u32,
    targets: &[u32],
) -> Vec<Option<Path>> {
    match ch {
        Some(ch) => cch_dijkstra(ch, ch_space, source, targets),
        None => dijkstra(graph, space, source, targets),
    }
}

/// The variables and rows of an evaluated sub-select.
type SelectRows = (Vec<Variable>, Vec<Vec<Option<Term>>>);

/// A restriction being assembled from per-position rows: cost and `(seq, edge)` steps.
type Sequence = (f64, Vec<(f64, Option<u32>)>);

thread_local! {
    static SPACES: RefCell<(SearchSpace, SearchSpace, CchSpace)> =
        RefCell::new((SearchSpace::new(), SearchSpace::new(), CchSpace::new()));
}

#[derive(Debug, Clone, Copy)]
enum Column {
    Seq,
    PathSeq,
    StartVid,
    EndVid,
    Node,
    Edge,
    Cost,
    AggCost,
    Pred,
    PathId,
    RouteAggCost,
    Component,
    /// The number of vertices of the component. Without `pgr:node`, the component functions
    /// give one row per component.
    Size,
    /// The edge's geometry, oriented the way the path runs.
    Geom,
    /// The whole route's geometry.
    RouteGeom,
    /// The route's edges, in order, separated by spaces.
    RouteList,
    /// The time a vertex is reached (rail:routeAt).
    Time,
    /// The coarse edge a fine edge refines (rail:routeHierarchical).
    CoarseEdge,
    /// Whether the fine route stayed in the corridor.
    Corridor,
}

impl Column {
    fn parse(name: &str, function: PgrFunction) -> Result<Self, PgrError> {
        let c = match name {
            "seq" => Self::Seq,
            "path_seq" => Self::PathSeq,
            "start_vid" | "from_v" => Self::StartVid,
            "end_vid" => Self::EndVid,
            "node" => Self::Node,
            "edge" => Self::Edge,
            "cost" => Self::Cost,
            "agg_cost" => Self::AggCost,
            "pred" => Self::Pred,
            "path_id" => Self::PathId,
            "route_agg_cost" => Self::RouteAggCost,
            "component" => Self::Component,
            "size" | "component_size" => Self::Size,
            "geom" => Self::Geom,
            "route_geom" | "routeGeometry" | "route_geometry" => Self::RouteGeom,
            "route_list" | "routeAsList" | "route_as_list" => Self::RouteList,
            "time" => Self::Time,
            "coarse_edge" => Self::CoarseEdge,
            "corridor" => Self::Corridor,
            _ => {
                return Err(PgrError::Request(format!(
                    "unknown result column pgr:{name}"
                )));
            }
        };
        let components = matches!(
            function,
            PgrFunction::ConnectedComponents | PgrFunction::StrongComponents
        );
        if matches!(c, Self::Size) && !components {
            return Err(PgrError::Request(format!(
                "pgr:{name} is a result column of the component functions only"
            )));
        }
        let special = matches!(c, Self::Time | Self::CoarseEdge | Self::Corridor);
        let ok = match function {
            PgrFunction::Isochrone => {
                matches!(c, Self::Seq | Self::StartVid | Self::Geom | Self::AggCost)
            }
            PgrFunction::RouteAt => !matches!(
                c,
                Self::Pred
                    | Self::PathId
                    | Self::RouteAggCost
                    | Self::Component
                    | Self::CoarseEdge
                    | Self::Corridor
            ),
            PgrFunction::RouteHierarchical | PgrFunction::Refine => !matches!(
                c,
                Self::Pred | Self::PathId | Self::RouteAggCost | Self::Component | Self::Time
            ),
            _ if special => false,
            f if f.is_cost() => matches!(c, Self::StartVid | Self::EndVid | Self::AggCost),
            PgrFunction::Kruskal | PgrFunction::Prim => {
                matches!(c, Self::Seq | Self::Edge | Self::Cost | Self::AggCost)
            }
            PgrFunction::DijkstraNear => {
                !matches!(c, Self::Pred | Self::RouteAggCost | Self::Component)
            }
            PgrFunction::DrivingDistance => !matches!(
                c,
                Self::PathSeq
                    | Self::EndVid
                    | Self::PathId
                    | Self::RouteAggCost
                    | Self::Component
                    | Self::RouteGeom
                    | Self::RouteList
            ),
            PgrFunction::Ksp
            | PgrFunction::TurnRestrictedPath
            | PgrFunction::EdgeDisjointPaths
            | PgrFunction::HawickCircuits => {
                !matches!(c, Self::Pred | Self::RouteAggCost | Self::Component)
            }
            PgrFunction::DijkstraVia | PgrFunction::TrspVia => {
                !matches!(c, Self::Pred | Self::Component)
            }
            PgrFunction::ConnectedComponents | PgrFunction::StrongComponents => {
                matches!(c, Self::Seq | Self::Component | Self::Node | Self::Size)
            }
            PgrFunction::ArticulationPoints => matches!(c, Self::Seq | Self::Node),
            PgrFunction::Bridges => matches!(c, Self::Seq | Self::Edge),
            _ => !matches!(
                c,
                Self::Pred | Self::PathId | Self::RouteAggCost | Self::Component
            ),
        };
        if ok {
            Ok(c)
        } else {
            Err(PgrError::Request(format!(
                "pgr:{name} is not a result column of this function"
            )))
        }
    }
}

impl ServiceHandler for PgrService {
    type Error = PgrError;

    fn handle(
        &self,
        pattern: &GraphPattern,
        _base_iri: Option<&Iri<String>>,
    ) -> Result<QuerySolutionIter<'static>, PgrError> {
        let request = Request::parse(pattern)?;
        let token = self.cancellation.clone();
        let cancelled = move || token.as_ref().is_some_and(CancellationToken::is_cancelled);
        if cancelled() {
            return Err(PgrError::Cancelled);
        }
        let check: tripletrail_routing::cancel::Check = Arc::new(cancelled.clone());
        let ran = tripletrail_routing::cancel::scope(Some(check), || self.run(&request));
        // A stopped algorithm returns what it had so far: never answer with that.
        if cancelled() {
            return Err(PgrError::Cancelled);
        }
        let (variables, solutions) = ran?;
        Ok(QuerySolutionIter::new(
            variables,
            solutions.into_iter().map(Ok::<_, QueryEvaluationError>),
        ))
    }
}
