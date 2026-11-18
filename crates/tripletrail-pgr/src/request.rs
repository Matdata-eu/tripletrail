use crate::service::PgrError;
use crate::{PGR, RAIL};
use oxigraph::model::{NamedNode, Term};
use oxigraph::sparql::Variable;
use spargebra::algebra::GraphPattern;
use spargebra::term::{NamedNodePattern, TermPattern};

/// The pgRouting functions implemented so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgrFunction {
    Dijkstra,
    DijkstraCost,
    BdDijkstra,
    AStar,
    DrivingDistance,
    Ksp,
    Trsp,
    TurnRestrictedPath,
    DijkstraVia,
    ConnectedComponents,
    StrongComponents,
    ArticulationPoints,
    Bridges,
    DijkstraNear,
    DijkstraNearCost,
    BdDijkstraCost,
    AStarCost,
    BdAStar,
    BdAStarCost,
    Kruskal,
    Prim,
    FloydWarshall,
    Johnson,
    BellmanFord,
    EdwardMoore,
    DagShortestPath,
    BreadthFirstSearch,
    BinaryBreadthFirstSearch,
    DepthFirstSearch,
    TrspVia,
    KruskalBfs,
    KruskalDfs,
    KruskalDd,
    PrimBfs,
    PrimDfs,
    PrimDd,
    MaxFlow,
    BoykovKolmogorov,
    EdmondsKarp,
    PushRelabel,
    EdgeDisjointPaths,
    MaxCardinalityMatch,
    MaxFlowMinCost,
    MaxFlowMinCostCost,
    StoerWagner,
    Tsp,
    TspEuclidean,
    LineGraph,
    LineGraphFull,
    Contraction,
    BiconnectedComponents,
    Degree,
    ExtractVertices,
    TopologicalSort,
    TransitiveClosure,
    MakeConnected,
    IsPlanar,
    LengauerTarjanDominatorTree,
    HawickCircuits,
    SequentialVertexColoring,
    Bipartite,
    EdgeColoring,
    Bandwidth,
    KingOrdering,
    SloanOrdering,
    ChinesePostman,
    ChinesePostmanCost,
    Version,
    FullVersion,
    /// `rail:isochrone`: the hull of what a driving distance reaches.
    Isochrone,
    /// `rail:routeAt`: time-dependent shortest paths.
    RouteAt,
    /// `rail:routeHierarchical`: a coarse route refined on a fine network.
    RouteHierarchical,
    /// `rail:refine`: a given coarse route refined on a fine network.
    Refine,
    /// `rail:checkTopology`.
    CheckTopology,
    /// `rail:analyzeGraph` (pgr_analyzeGraph).
    AnalyzeGraph,
    /// `rail:buildTopology` (pgr_createTopology).
    BuildTopology,
    NodeNetwork,
    SeparateCrossing,
    SeparateTouching,
    FindCloseEdges,
    /// pgr_pickDeliver: orders, vehicles and a cost matrix.
    PickDeliver,
    /// pgr_pickDeliverEuclidean: orders and vehicles with coordinates.
    PickDeliverEuclidean,
    /// pgr_vrpOneDepot: deliveries from one depot.
    VrpOneDepot,
    /// `rail:compatibilityShapes`: the SHACL shapes compatibility rules compile to for a
    /// vehicle, as triples.
    CompatibilityShapes,
    /// `pgr:networkInfo`: the state of a (named) network in the cache.
    NetworkInfo,
    /// `rail:paretoRoutes`: the Pareto-optimal paths under several cost columns.
    ParetoRoutes,
    /// `rail:shaclRules`: the triples the SHACL-AF rules of a shapes graph infer.
    ShaclRules,
}

impl PgrFunction {
    /// Functions on edge geometries, which read the edge sub-select's rows as they are
    /// rather than a network.
    pub fn is_topology(self) -> bool {
        matches!(
            self,
            Self::CheckTopology
                | Self::AnalyzeGraph
                | Self::BuildTopology
                | Self::NodeNetwork
                | Self::SeparateCrossing
                | Self::SeparateTouching
                | Self::FindCloseEdges
        )
    }

    /// The vehicle routing functions, which read orders and vehicles.
    pub fn is_vrp(self) -> bool {
        matches!(
            self,
            Self::PickDeliver | Self::PickDeliverEuclidean | Self::VrpOneDepot
        )
    }

    /// Functions over the whole graph, without start or end vertices.
    pub fn is_graph_analysis(self) -> bool {
        self.is_topology()
            || self.is_vrp()
            || matches!(
                self,
                Self::ConnectedComponents
                    | Self::StrongComponents
                    | Self::ArticulationPoints
                    | Self::Bridges
                    | Self::Kruskal
                    | Self::Prim
                    | Self::FloydWarshall
                    | Self::Johnson
                    | Self::MaxCardinalityMatch
                    | Self::StoerWagner
                    | Self::Tsp
                    | Self::TspEuclidean
                    | Self::LineGraph
                    | Self::LineGraphFull
                    | Self::Contraction
                    | Self::BiconnectedComponents
                    | Self::Degree
                    | Self::ExtractVertices
                    | Self::TopologicalSort
                    | Self::TransitiveClosure
                    | Self::MakeConnected
                    | Self::IsPlanar
                    | Self::HawickCircuits
                    | Self::SequentialVertexColoring
                    | Self::Bipartite
                    | Self::EdgeColoring
                    | Self::Bandwidth
                    | Self::KingOrdering
                    | Self::SloanOrdering
                    | Self::ChinesePostman
                    | Self::ChinesePostmanCost
                    | Self::Version
                    | Self::FullVersion
                    | Self::CompatibilityShapes
                    | Self::NetworkInfo
                    | Self::ShaclRules
            )
    }

    /// Functions returning one `start_vid, end_vid, agg_cost` row per pair.
    pub fn is_cost(self) -> bool {
        matches!(
            self,
            Self::DijkstraCost
                | Self::DijkstraNearCost
                | Self::BdDijkstraCost
                | Self::AStarCost
                | Self::BdAStarCost
                | Self::FloydWarshall
                | Self::Johnson
        )
    }

    /// Functions taking start vertices (roots) but no end vertices.
    fn is_one_sided(self) -> bool {
        matches!(
            self,
            Self::DrivingDistance
                | Self::BreadthFirstSearch
                | Self::DepthFirstSearch
                | Self::KruskalBfs
                | Self::KruskalDfs
                | Self::KruskalDd
                | Self::PrimBfs
                | Self::PrimDfs
                | Self::PrimDd
                | Self::LengauerTarjanDominatorTree
                | Self::Isochrone
        )
    }

    /// Functions that take `pgr:via` vertices instead of start and end vertices.
    pub fn is_via(self) -> bool {
        matches!(self, Self::DijkstraVia | Self::TrspVia)
    }

    /// Functions that read no edge set.
    pub fn without_edges(self) -> bool {
        self.is_vrp()
            || matches!(
                self,
                Self::Tsp
                    | Self::TspEuclidean
                    | Self::Version
                    | Self::FullVersion
                    | Self::CompatibilityShapes
                    | Self::ShaclRules
            )
    }

    /// Flow functions, which read `?capacity` (and `?reverse_capacity`) from the edge set.
    pub fn is_flow(self) -> bool {
        matches!(
            self,
            Self::MaxFlow
                | Self::BoykovKolmogorov
                | Self::EdmondsKarp
                | Self::PushRelabel
                | Self::MaxFlowMinCost
                | Self::MaxFlowMinCostCost
        )
    }

    /// Path and distance functions, which a points sub-select can add vertices to.
    fn accepts_points(self) -> bool {
        matches!(self, Self::DrivingDistance | Self::Isochrone)
            || !(self.is_graph_analysis()
                || self.is_one_sided()
                || self.is_flow()
                || matches!(
                    self,
                    Self::EdgeDisjointPaths
                        | Self::RouteAt
                        | Self::RouteHierarchical
                        | Self::Refine
                        | Self::ParetoRoutes
                ))
    }

    /// Functions that read the costs of the edge set (the others only need its arcs, so their
    /// edge sub-select may leave `?cost` out).
    pub fn needs_cost(self) -> bool {
        !self.without_edges()
            && !self.is_topology()
            && !matches!(
                self,
                Self::MaxFlow
                    | Self::BoykovKolmogorov
                    | Self::EdmondsKarp
                    | Self::PushRelabel
                    | Self::ConnectedComponents
                    | Self::StrongComponents
                    | Self::ArticulationPoints
                    | Self::Bridges
                    | Self::MaxCardinalityMatch
                    | Self::BiconnectedComponents
                    | Self::Degree
                    | Self::ExtractVertices
                    | Self::TopologicalSort
                    | Self::TransitiveClosure
                    | Self::MakeConnected
                    | Self::IsPlanar
                    | Self::LengauerTarjanDominatorTree
                    | Self::SequentialVertexColoring
                    | Self::Bipartite
                    | Self::EdgeColoring
                    | Self::Bandwidth
                    | Self::KingOrdering
                    | Self::SloanOrdering
                    | Self::NetworkInfo
                    | Self::ParetoRoutes
            )
    }
}

impl PgrFunction {
    /// The function an IRI names, and whether it is a `withPoints` form (which runs the same
    /// search on the network with the points of `points_sql` added).
    fn from_iri(iri: &str) -> Option<(Self, bool)> {
        let local = iri.strip_prefix(PGR).or_else(|| iri.strip_prefix(RAIL))?;
        let local = local.strip_prefix("pgr_").unwrap_or(local);
        let with_points = match local {
            "withPoints" => Some(Self::Dijkstra),
            "withPointsCost" | "withPointsCostMatrix" => Some(Self::DijkstraCost),
            "withPointsKSP" | "withPointsKsp" => Some(Self::Ksp),
            "withPointsDD" => Some(Self::DrivingDistance),
            "withPointsVia" => Some(Self::DijkstraVia),
            "trsp_withPoints" => Some(Self::Trsp),
            "trspVia_withPoints" => Some(Self::TrspVia),
            _ => None,
        };
        if let Some(f) = with_points {
            return Some((f, true));
        }
        Self::from_local(local).map(|f| (f, false))
    }

    fn from_local(local: &str) -> Option<Self> {
        Some(match local {
            "dijkstra" => Self::Dijkstra,
            "dijkstraCost" | "dijkstraCostMatrix" => Self::DijkstraCost,
            "bdDijkstra" => Self::BdDijkstra,
            "aStar" => Self::AStar,
            "drivingDistance" => Self::DrivingDistance,
            "KSP" | "ksp" => Self::Ksp,
            "trsp" => Self::Trsp,
            "turnRestrictedPath" => Self::TurnRestrictedPath,
            "dijkstraVia" => Self::DijkstraVia,
            "connectedComponents" => Self::ConnectedComponents,
            "strongComponents" => Self::StrongComponents,
            "articulationPoints" => Self::ArticulationPoints,
            "bridges" => Self::Bridges,
            "dijkstraNear" => Self::DijkstraNear,
            "dijkstraNearCost" => Self::DijkstraNearCost,
            "bdDijkstraCost" | "bdDijkstraCostMatrix" => Self::BdDijkstraCost,
            "aStarCost" | "aStarCostMatrix" => Self::AStarCost,
            "bdAstar" | "bdAStar" => Self::BdAStar,
            "bdAstarCost" | "bdAStarCost" | "bdAstarCostMatrix" => Self::BdAStarCost,
            "kruskal" => Self::Kruskal,
            "prim" => Self::Prim,
            "floydWarshall" => Self::FloydWarshall,
            "johnson" => Self::Johnson,
            "bellmanFord" => Self::BellmanFord,
            "edwardMoore" => Self::EdwardMoore,
            "dagShortestPath" => Self::DagShortestPath,
            "breadthFirstSearch" => Self::BreadthFirstSearch,
            "binaryBreadthFirstSearch" => Self::BinaryBreadthFirstSearch,
            "depthFirstSearch" => Self::DepthFirstSearch,
            "trspVia" => Self::TrspVia,
            "kruskalBFS" => Self::KruskalBfs,
            "kruskalDFS" => Self::KruskalDfs,
            "kruskalDD" => Self::KruskalDd,
            "primBFS" => Self::PrimBfs,
            "primDFS" => Self::PrimDfs,
            "primDD" => Self::PrimDd,
            "maxFlow" => Self::MaxFlow,
            "boykovKolmogorov" => Self::BoykovKolmogorov,
            "edmondsKarp" => Self::EdmondsKarp,
            "pushRelabel" => Self::PushRelabel,
            "edgeDisjointPaths" => Self::EdgeDisjointPaths,
            "maxCardinalityMatch" => Self::MaxCardinalityMatch,
            "maxFlowMinCost" => Self::MaxFlowMinCost,
            "maxFlowMinCost_Cost" => Self::MaxFlowMinCostCost,
            "stoerWagner" => Self::StoerWagner,
            "TSP" | "tsp" => Self::Tsp,
            "TSPeuclidean" | "tspEuclidean" => Self::TspEuclidean,
            "lineGraph" => Self::LineGraph,
            "lineGraphFull" => Self::LineGraphFull,
            "contraction" => Self::Contraction,
            "biconnectedComponents" => Self::BiconnectedComponents,
            "degree" => Self::Degree,
            "extractVertices" => Self::ExtractVertices,
            "topologicalSort" => Self::TopologicalSort,
            "transitiveClosure" => Self::TransitiveClosure,
            "makeConnected" => Self::MakeConnected,
            "isPlanar" => Self::IsPlanar,
            "lengauerTarjanDominatorTree" => Self::LengauerTarjanDominatorTree,
            "hawickCircuits" => Self::HawickCircuits,
            "sequentialVertexColoring" => Self::SequentialVertexColoring,
            "bipartite" => Self::Bipartite,
            "edgeColoring" => Self::EdgeColoring,
            "bandwidth" => Self::Bandwidth,
            "kingOrdering" => Self::KingOrdering,
            "sloanOrdering" => Self::SloanOrdering,
            "chinesePostman" => Self::ChinesePostman,
            "chinesePostmanCost" => Self::ChinesePostmanCost,
            "version" => Self::Version,
            "full_version" => Self::FullVersion,
            // Beyond pgRouting (usually written in the rail: namespace).
            "route" => Self::Dijkstra,
            "reachable" => Self::DrivingDistance,
            "isochrone" => Self::Isochrone,
            "routeAt" => Self::RouteAt,
            "routeHierarchical" => Self::RouteHierarchical,
            "refine" => Self::Refine,
            "checkTopology" => Self::CheckTopology,
            "analyzeGraph" => Self::AnalyzeGraph,
            "buildTopology" | "createTopology" => Self::BuildTopology,
            "nodeNetwork" => Self::NodeNetwork,
            "separateCrossing" => Self::SeparateCrossing,
            "separateTouching" => Self::SeparateTouching,
            "findCloseEdges" => Self::FindCloseEdges,
            "pickDeliver" => Self::PickDeliver,
            "pickDeliverEuclidean" => Self::PickDeliverEuclidean,
            "vrpOneDepot" => Self::VrpOneDepot,
            "compatibilityShapes" => Self::CompatibilityShapes,
            "networkInfo" => Self::NetworkInfo,
            "paretoRoutes" => Self::ParetoRoutes,
            "shaclRules" => Self::ShaclRules,
            _ => return None,
        })
    }
}

/// Where the edge set comes from.
#[derive(Debug, Clone)]
pub enum EdgeSource {
    /// A sub-select evaluated at query time (pgRouting's `edges_sql`).
    Query(GraphPattern),
    /// A named network described in the store.
    Network(NamedNode),
}

/// A parsed `SERVICE <pgr>` block.
#[derive(Debug, Clone)]
pub struct Request {
    pub function: PgrFunction,
    /// The edge set; `None` only for functions that read none (pgr_TSP, pgr_version, ...).
    pub edges: Option<EdgeSource>,
    pub restrictions: Option<GraphPattern>,
    /// pgRouting's `points_sql`: `?pid ?edge ?fraction` rows placing extra vertices on edges.
    pub points: Option<GraphPattern>,
    /// pgr_TSP's and pgr_pickDeliver's `matrix_cell_sql`: `?start_vid ?end_vid ?agg_cost`
    /// rows (pgr_vrpOneDepot's `?src_id ?dest_id ?traveltime` too).
    pub matrix: Option<GraphPattern>,
    /// The orders of the vehicle routing functions: rows projecting `?demand` (or
    /// `?order_unit` for pgr_vrpOneDepot).
    pub orders: Option<GraphPattern>,
    /// The vehicles of the vehicle routing functions: rows projecting `?capacity`.
    pub vehicles: Option<GraphPattern>,
    /// `depot_id` of pgr_vrpOneDepot.
    pub depot: Option<Term>,
    /// `initial_sol` of the vehicle routing functions (accepted, see docs/routing.md).
    pub initial_sol: i64,
    /// pgr_TSPeuclidean's `coordinates_sql`: `?id ?x ?y` rows.
    pub coordinates: Option<GraphPattern>,
    pub start_vids: Vec<Term>,
    pub end_vids: Vec<Term>,
    /// `via_vertices` of pgr_dijkstraVia and pgr_trspVia, in the order written.
    pub vias: Vec<Term>,
    /// `strict` of the via functions: no result at all if one leg has no path.
    pub strict: bool,
    /// `U_turn_on_edge` of pgr_trspVia: a leg may leave along the edge the last one arrived by.
    pub u_turn_on_edge: bool,
    pub directed: bool,
    pub heuristic: i64,
    pub factor: f64,
    pub distance: Option<f64>,
    /// `max_depth` of the traversals.
    pub max_depth: u64,
    /// `K` of pgr_KSP.
    pub k: usize,
    /// `heap_paths` of pgr_KSP.
    pub heap_paths: bool,
    /// `stop_on_first` of pgr_turnRestrictedPath.
    pub stop_on_first: bool,
    /// `pgr:engine "ch"` (or `"cch"`): answer shortest-path searches from a customizable
    /// contraction hierarchy.
    pub contracted: bool,
    /// `cap` of pgr_dijkstraNear: how many of the nearest paths to return.
    pub cap: usize,
    /// `methods` of pgr_contraction (1 dead end, 2 linear), in the order written.
    pub methods: Vec<i64>,
    /// `cycles` of pgr_contraction, `max_cycles` of the vehicle routing functions.
    pub cycles: usize,
    /// `forbidden` vertices of pgr_contraction.
    pub forbidden: Vec<Term>,
    /// `start_id` and `end_id` of pgr_TSP.
    pub start_id: Option<Term>,
    pub end_id: Option<Term>,
    /// `pgr:constraint`: a boolean expression over the edge set's `?c_*` columns; edges it
    /// rejects are left out of the search.
    pub constraint: Option<String>,
    /// `pgr:shapes`: a named graph of SHACL shapes; edges whose focus node does not conform
    /// are left out of the search.
    pub shapes: Option<NamedNode>,
    /// `pgr:shape`: shapes every focus node is checked against, whatever their targets (in
    /// `pgr:shapes`); for `rail:compatibilityShapes`, the IRI of the shape it makes.
    pub shape_nodes: Vec<NamedNode>,
    /// `pgr:focus`: the `?c_*` column holding each edge's focus node (default: `?id`).
    pub focus: Option<String>,
    /// `pgr:vehicle` and `pgr:rules`: a vehicle and a named graph of compatibility rules,
    /// compiled into shapes like those of `pgr:shapes`.
    pub vehicle: Option<Term>,
    pub rules: Option<NamedNode>,
    /// `pgr:data_graph`: the graph shapes validate against (default: the default graph).
    pub data_graph: Option<NamedNode>,
    /// `pgr:departure` of rail:routeAt (a number or an `xsd:dateTime`).
    pub departure: Option<Term>,
    /// `pgr:wait` of rail:routeAt: whether a train may wait at a vertex for an edge to open.
    pub wait: bool,
    /// `pgr:tolerance` of the topology functions.
    pub tolerance: Option<f64>,
    /// `pgr:concavity` of rail:isochrone.
    pub concavity: f64,
    /// `pgr:point` geometries of pgr_findCloseEdges.
    pub point_geoms: Vec<Term>,
    /// pgr_findCloseEdges' points as a sub-select: `?pid ?geom` rows.
    pub point_rows: Option<GraphPattern>,
    /// rail:routeAt's periods: `?edge ?t_from ?t_to [?t_cost] [?t_reverse_cost]` rows.
    pub periods: Option<GraphPattern>,
    /// The fine network of rail:routeHierarchical: `pgr:fine_network <iri>`, or a sub-select
    /// projecting `?fine_id ?fine_source ?fine_target ?fine_cost ...`.
    pub fine: Option<EdgeSource>,
    /// `?coarse_edge ?fine_edge` rows: which fine edges make up each coarse edge.
    pub mapping: Option<GraphPattern>,
    /// rail:refine's coarse route: `?coarse_edge` rows.
    pub coarse_route: Option<GraphPattern>,
    /// Start and end vertices in the fine network of rail:routeHierarchical (by default
    /// `start_vids` and `end_vids`); several are alternatives.
    pub fine_start_vids: Vec<Term>,
    pub fine_end_vids: Vec<Term>,
    /// `pgr:criteria` of rail:paretoRoutes: the cost columns (`cost` or `c_*`), in order.
    pub criteria: Vec<String>,
    /// `pgr:max_paths` of rail:paretoRoutes: at most this many paths, and labels per vertex.
    pub max_paths: usize,
    /// Result column name (pgRouting's) -> SPARQL variable.
    pub outputs: Vec<(String, Variable)>,
}

impl Request {
    pub fn parse(pattern: &GraphPattern) -> Result<Self, PgrError> {
        let mut triples = Vec::new();
        let mut subselects = Vec::new();
        collect(pattern, &mut triples, &mut subselects)?;

        let mut function = None;
        let mut with_points = false;
        let mut network = None;
        let mut start_vids = Vec::new();
        let mut end_vids = Vec::new();
        let mut vias = Vec::new();
        let mut strict = false;
        let mut u_turn_on_edge = true;
        let mut directed = true;
        let mut heuristic = 5;
        let mut factor = 1.0;
        let mut distance = None;
        let mut max_depth = None;
        let mut k = None;
        let mut heap_paths = false;
        let mut stop_on_first = true;
        let mut cap = 1.0;
        let mut contracted = false;
        let mut methods = Vec::new();
        let mut cycles = None;
        let mut initial_sol = 4.0;
        let mut depot = None;
        let mut forbidden = Vec::new();
        let mut start_id = None;
        let mut end_id = None;
        let mut constraint = None;
        let mut shapes = None;
        let mut shape_nodes = Vec::new();
        let mut focus = None;
        let mut vehicle = None;
        let mut rules = None;
        let mut data_graph = None;
        let mut departure = None;
        let mut wait = true;
        let mut tolerance = None;
        let mut concavity = 2.0;
        let mut point_geoms = Vec::new();
        let mut fine_network = None;
        let mut fine_start_vids = Vec::new();
        let mut fine_end_vids = Vec::new();
        let mut criteria: Vec<String> = Vec::new();
        let mut max_paths = 100.0;
        let mut outputs = Vec::new();
        for (predicate, object) in triples {
            let name = predicate
                .strip_prefix(PGR)
                .or_else(|| predicate.strip_prefix(RAIL))
                .ok_or_else(|| PgrError::Request(format!("unknown predicate <{predicate}>")))?
                .to_owned();
            // Parameters whose value is a vertex, given as an IRI or a literal.
            let vertex: Option<Term> = match &object {
                TermPattern::NamedNode(n) => Some(n.clone().into()),
                TermPattern::Literal(l) => Some(l.clone().into()),
                _ => None,
            };
            if let Some(v) = vertex.clone() {
                match name.as_str() {
                    "start_vid" | "root_vid" | "root_vertex" => {
                        start_vids.push(v);
                        continue;
                    }
                    "end_vid" => {
                        end_vids.push(v);
                        continue;
                    }
                    "via" => {
                        vias.push(v);
                        continue;
                    }
                    "forbidden" | "forbidden_vertices" => {
                        forbidden.push(v);
                        continue;
                    }
                    "start_id" => {
                        start_id = Some(v);
                        continue;
                    }
                    "end_id" => {
                        end_id = Some(v);
                        continue;
                    }
                    "depot_id" | "depot" => {
                        depot = Some(v);
                        continue;
                    }
                    "fine_start_vid" => {
                        fine_start_vids.push(v);
                        continue;
                    }
                    "fine_end_vid" => {
                        fine_end_vids.push(v);
                        continue;
                    }
                    "point" => {
                        point_geoms.push(v);
                        continue;
                    }
                    "departure" | "departure_time" => {
                        departure = Some(v);
                        continue;
                    }
                    "vehicle" => {
                        vehicle = Some(v);
                        continue;
                    }
                    "constraint" => {
                        let Term::Literal(l) = v else {
                            return Err(PgrError::Request(
                                "pgr:constraint is a string holding an expression".into(),
                            ));
                        };
                        constraint = Some(l.value().to_owned());
                        continue;
                    }
                    _ => {}
                }
            }
            match object {
                TermPattern::Variable(v) => outputs.push((name, v)),
                TermPattern::NamedNode(n) => match name.as_str() {
                    "function" => {
                        let (f, points) = PgrFunction::from_iri(n.as_str()).ok_or_else(|| {
                            PgrError::Request(format!("unsupported function {n}"))
                        })?;
                        function = Some(f);
                        with_points = points;
                    }
                    "network" => network = Some(n),
                    "shapes" => shapes = Some(n),
                    "shape" => shape_nodes.push(n),
                    "rules" => rules = Some(n),
                    "data_graph" => data_graph = Some(n),
                    "fine_network" => fine_network = Some(n),
                    _ => return Err(PgrError::Request(format!("unexpected IRI for pgr:{name}"))),
                },
                TermPattern::Literal(l) => {
                    let value = l.value();
                    let number = || {
                        value
                            .parse::<f64>()
                            .map_err(|_| PgrError::Request(format!("pgr:{name} must be numeric")))
                    };
                    let flag = matches!(value, "true" | "1");
                    match name.as_str() {
                        "strict" => strict = flag,
                        "U_turn_on_edge" | "u_turn_on_edge" => u_turn_on_edge = flag,
                        "directed" => directed = flag,
                        "heuristic" => heuristic = number()? as i64,
                        "factor" => factor = number()?,
                        "distance" | "max_cost" => distance = Some(number()?),
                        "wait" => wait = flag,
                        "tolerance" => tolerance = Some(number()?),
                        "concavity" => concavity = number()?,
                        "max_depth" => max_depth = Some(number()?),
                        "K" | "k" => k = Some(number()?),
                        "heap_paths" => heap_paths = flag,
                        "stop_on_first" => stop_on_first = flag,
                        "focus" => focus = Some(value.trim_start_matches('?').to_owned()),
                        "cap" => cap = number()?,
                        "max_paths" => max_paths = number()?,
                        "criteria" | "criterion" => criteria.extend(
                            value
                                .split(|c: char| c.is_whitespace() || c == ',')
                                .filter(|c| !c.is_empty())
                                .map(|c| c.trim_start_matches('?').to_owned()),
                        ),
                        "methods" | "contraction_order" => methods.push(number()? as i64),
                        "cycles" | "max_cycles" => cycles = Some(number()?),
                        "initial_sol" => initial_sol = number()?,
                        "engine" => {
                            contracted = match value {
                                "ch" | "cch" => true,
                                "dijkstra" => false,
                                _ => {
                                    return Err(PgrError::Request(
                                        "pgr:engine is \"dijkstra\" or \"ch\"".into(),
                                    ));
                                }
                            }
                        }
                        _ => {
                            return Err(PgrError::Request(format!(
                                "unexpected literal for pgr:{name}"
                            )));
                        }
                    }
                }
                _ => {
                    return Err(PgrError::Request(format!(
                        "unsupported object for pgr:{name}"
                    )));
                }
            }
        }

        let mut edges = None;
        let mut restrictions = None;
        let mut points = None;
        let mut matrix = None;
        let mut coordinates = None;
        let mut periods = None;
        let mut mapping = None;
        let mut coarse_route = None;
        let mut fine_edges = None;
        let mut point_rows = None;
        let mut orders = None;
        let mut vehicles = None;
        for sub in subselects {
            let vars = projected(&sub);
            let has = |name: &str| vars.iter().any(|v| v.as_str() == name);
            let slot = if has("demand") || has("order_unit") {
                (&mut orders, "orders")
            } else if has("fraction") {
                if !(has("pid") && has("edge")) {
                    return Err(PgrError::Request(
                        "a points sub-select projects ?pid ?edge ?fraction".into(),
                    ));
                }
                (&mut points, "points")
            } else if has("t_from") || has("t_to") {
                if !has("edge") {
                    return Err(PgrError::Request(
                        "a periods sub-select projects ?edge ?t_from ?t_to [?t_cost]".into(),
                    ));
                }
                (&mut periods, "periods")
            } else if has("coarse_edge") && has("fine_edge") {
                (&mut mapping, "mapping")
            } else if has("coarse_edge") {
                (&mut coarse_route, "coarse route")
            } else if has("fine_source") {
                (&mut fine_edges, "fine edge")
            } else if has("source") {
                (&mut edges, "edge")
            } else if has("capacity") {
                (&mut vehicles, "vehicles")
            } else if has("agg_cost") || (has("src_id") && has("dest_id")) {
                (&mut matrix, "matrix")
            } else if has("x") && has("y") {
                (&mut coordinates, "coordinates")
            } else if has("pid") && has("geom") {
                (&mut point_rows, "points")
            } else if has("id") && has("geom") && function.is_some_and(PgrFunction::is_topology) {
                (&mut edges, "edge")
            } else if has("path") || has("edge") {
                (&mut restrictions, "restriction")
            } else {
                return Err(PgrError::Request(
                    "a sub-select must project ?source/?target (edges), ?pid ?edge ?fraction (points), ?path or ?edge (restrictions), ?start_vid ?end_vid ?agg_cost (a cost matrix), ?id ?x ?y (coordinates), ?demand or ?order_unit (orders), ?capacity (vehicles), ?edge ?t_from ?t_to (periods), ?coarse_edge ?fine_edge (a mapping), ?fine_id ?fine_source ?fine_target (fine edges) or ?id ?geom (edge geometries)"
                        .into(),
                ));
            };
            if slot.0.replace(sub).is_some() {
                return Err(PgrError::Request(format!(
                    "more than one {} sub-select",
                    slot.1
                )));
            }
        }
        let function = function.ok_or_else(|| PgrError::Request("missing pgr:function".into()))?;
        let edges = match (edges, network) {
            (Some(_), Some(_)) => {
                return Err(PgrError::Request(
                    "give either pgr:network or an edge sub-select".into(),
                ));
            }
            (Some(e), None) => Some(EdgeSource::Query(e)),
            (None, Some(n)) => Some(EdgeSource::Network(n)),
            (None, None) if function.without_edges() => None,
            (None, None) => {
                return Err(PgrError::Request(
                    "missing edge sub-select or pgr:network".into(),
                ));
            }
        };
        match function {
            PgrFunction::Tsp if matrix.is_none() => {
                return Err(PgrError::Request(
                    "pgr_TSP needs a sub-select projecting ?start_vid ?end_vid ?agg_cost".into(),
                ));
            }
            PgrFunction::TspEuclidean if coordinates.is_none() => {
                return Err(PgrError::Request(
                    "pgr_TSPeuclidean needs a sub-select projecting ?id ?x ?y".into(),
                ));
            }
            _ => {}
        }
        if matrix.is_some()
            && !matches!(
                function,
                PgrFunction::Tsp | PgrFunction::PickDeliver | PgrFunction::VrpOneDepot
            )
        {
            return Err(PgrError::Request(
                "a cost matrix sub-select is only for pgr_TSP, pgr_pickDeliver and pgr_vrpOneDepot"
                    .into(),
            ));
        }
        if function.is_vrp() {
            if orders.is_none() || vehicles.is_none() {
                return Err(PgrError::Request(
                    "the vehicle routing functions need an orders sub-select (projecting ?demand, or ?order_unit for pgr_vrpOneDepot) and a vehicles sub-select (projecting ?capacity)".into(),
                ));
            }
            if function == PgrFunction::PickDeliver && matrix.is_none() {
                return Err(PgrError::Request(
                    "pgr_pickDeliver needs a sub-select projecting ?start_vid ?end_vid ?agg_cost"
                        .into(),
                ));
            }
            if (function == PgrFunction::VrpOneDepot) != depot.is_some() {
                return Err(PgrError::Request(
                    "pgr_vrpOneDepot needs pgr:depot_id (and only it takes one)".into(),
                ));
            }
            if !(1.0..=6.0).contains(&initial_sol) {
                return Err(PgrError::Request(
                    "pgr:initial_sol is between 1 and 6".into(),
                ));
            }
            if !(factor > 0.0 && factor.is_finite()) {
                return Err(PgrError::Request("pgr:factor must be positive".into()));
            }
            if cycles.is_some_and(|c| c < 0.0) {
                return Err(PgrError::Request(
                    "pgr:max_cycles must be at least 0".into(),
                ));
            }
        } else if orders.is_some() || vehicles.is_some() {
            return Err(PgrError::Request(
                "orders and vehicles sub-selects are only for pgr_pickDeliver, pgr_pickDeliverEuclidean and pgr_vrpOneDepot".into(),
            ));
        } else if depot.is_some() {
            return Err(PgrError::Request(
                "pgr:depot_id is only for pgr_vrpOneDepot".into(),
            ));
        }
        let cycles = cycles.unwrap_or(if function.is_vrp() { 10.0 } else { 1.0 });
        if coordinates.is_some() && function != PgrFunction::TspEuclidean {
            return Err(PgrError::Request(
                "a coordinates sub-select is only for pgr_TSPeuclidean".into(),
            ));
        }
        let needs_ends =
            !function.is_graph_analysis() && !function.is_one_sided() && !function.is_via();
        if function.is_via() {
            if vias.len() < 2 {
                return Err(PgrError::Request(
                    "the via functions need at least two pgr:via vertices".into(),
                ));
            }
        } else if !vias.is_empty() {
            return Err(PgrError::Request(
                "pgr:via is only for pgr_dijkstraVia and pgr_trspVia".into(),
            ));
        } else if start_vids.is_empty() && !function.is_graph_analysis() {
            return Err(PgrError::Request("missing pgr:start_vid".into()));
        }
        if matches!(
            function,
            PgrFunction::DrivingDistance | PgrFunction::KruskalDd | PgrFunction::PrimDd
        ) && distance.is_none()
        {
            return Err(PgrError::Request(
                "the driving distance functions need pgr:distance".into(),
            ));
        }
        if needs_ends && end_vids.is_empty() {
            return Err(PgrError::Request("missing pgr:end_vid".into()));
        }
        let k = match (function, k) {
            (PgrFunction::Ksp, None) => {
                return Err(PgrError::Request("pgr_KSP needs pgr:K".into()));
            }
            (PgrFunction::TurnRestrictedPath, None) => {
                return Err(PgrError::Request(
                    "pgr_turnRestrictedPath needs pgr:K".into(),
                ));
            }
            (_, Some(k)) if k < 1.0 => {
                return Err(PgrError::Request("pgr:K must be at least 1".into()));
            }
            (_, k) => k.unwrap_or(1.0) as usize,
        };
        let max_depth = match max_depth {
            Some(d) if d < 0.0 => {
                return Err(PgrError::Request("pgr:max_depth must be at least 0".into()));
            }
            Some(d) => d as u64,
            None => u64::MAX,
        };
        if with_points && points.is_none() {
            return Err(PgrError::Request(
                "pgr_withPoints functions need a points sub-select (?pid ?edge ?fraction)".into(),
            ));
        }
        if points.is_some() && !function.accepts_points() {
            return Err(PgrError::Request(
                "a points sub-select only works with path and distance functions".into(),
            ));
        }
        if cap < 1.0 {
            return Err(PgrError::Request("pgr:cap must be at least 1".into()));
        }
        if cycles < 1.0 && !function.is_vrp() {
            return Err(PgrError::Request("pgr:cycles must be at least 1".into()));
        }
        if let Some(m) = methods.iter().find(|m| !matches!(m, 1 | 2)) {
            return Err(PgrError::Request(format!(
                "pgr:methods are 1 (dead end) and 2 (linear), not {m}"
            )));
        }
        if methods.is_empty() {
            methods = vec![1, 2];
        }
        if restrictions.is_some()
            && !matches!(
                function,
                PgrFunction::Trsp | PgrFunction::TrspVia | PgrFunction::TurnRestrictedPath
            )
        {
            return Err(PgrError::Request(
                "a restriction sub-select needs pgr_trsp, pgr_trspVia or pgr_turnRestrictedPath"
                    .into(),
            ));
        }
        if matches!(
            function,
            PgrFunction::DrivingDistance | PgrFunction::Isochrone
        ) && distance.is_none()
        {
            return Err(PgrError::Request(
                "rail:isochrone and rail:reachable need pgr:distance".into(),
            ));
        }
        if periods.is_some() && function != PgrFunction::RouteAt {
            return Err(PgrError::Request(
                "a periods sub-select is only for rail:routeAt".into(),
            ));
        }
        if function == PgrFunction::RouteAt && departure.is_none() {
            return Err(PgrError::Request("rail:routeAt needs pgr:departure".into()));
        }
        let hierarchical = matches!(
            function,
            PgrFunction::RouteHierarchical | PgrFunction::Refine
        );
        if mapping.is_some() != hierarchical {
            return Err(PgrError::Request(
                "rail:routeHierarchical and rail:refine need a ?coarse_edge ?fine_edge mapping sub-select (and only they take one)".into(),
            ));
        }
        if (coarse_route.is_some()) != (function == PgrFunction::Refine) {
            return Err(PgrError::Request(
                "rail:refine needs a ?coarse_edge route sub-select (and only it takes one)".into(),
            ));
        }
        let fine = match (fine_edges, fine_network) {
            (Some(_), Some(_)) => {
                return Err(PgrError::Request(
                    "give either pgr:fine_network or a fine edge sub-select".into(),
                ));
            }
            (Some(e), None) => Some(EdgeSource::Query(e)),
            (None, Some(n)) => Some(EdgeSource::Network(n)),
            (None, None) => None,
        };
        if fine.is_some() != (function == PgrFunction::RouteHierarchical) {
            return Err(PgrError::Request(
                "rail:routeHierarchical needs pgr:fine_network or a ?fine_id ?fine_source ?fine_target ?fine_cost sub-select (and only it takes one)".into(),
            ));
        }
        if function == PgrFunction::RouteHierarchical
            && (start_vids.len() != 1 || end_vids.len() != 1)
        {
            return Err(PgrError::Request(
                "rail:routeHierarchical takes one pgr:start_vid and one pgr:end_vid".into(),
            ));
        }
        if function == PgrFunction::FindCloseEdges {
            if point_geoms.is_empty() == point_rows.is_none() {
                return Err(PgrError::Request(
                    "pgr_findCloseEdges takes pgr:point geometries or a ?pid ?geom sub-select"
                        .into(),
                ));
            }
            if tolerance.is_none() {
                return Err(PgrError::Request(
                    "pgr_findCloseEdges needs pgr:tolerance".into(),
                ));
            }
        } else if point_rows.is_some() || !point_geoms.is_empty() {
            return Err(PgrError::Request(
                "pgr:point and ?pid ?geom sub-selects are only for pgr_findCloseEdges".into(),
            ));
        }
        if constraint.is_some()
            && (function.is_graph_analysis() || function.is_flow() || function.without_edges())
        {
            return Err(PgrError::Request(
                "pgr:constraint applies to the path and distance functions".into(),
            ));
        }
        if function == PgrFunction::ParetoRoutes {
            if criteria.is_empty() {
                return Err(PgrError::Request(
                    "rail:paretoRoutes needs pgr:criteria (cost columns: cost or ?c_*)".into(),
                ));
            }
            if let Some(c) = criteria
                .iter()
                .find(|c| *c != "cost" && !c.starts_with("c_"))
            {
                return Err(PgrError::Request(format!(
                    "pgr:criteria names cost or ?c_* columns, not {c}"
                )));
            }
            if max_paths < 1.0 {
                return Err(PgrError::Request("pgr:max_paths must be at least 1".into()));
            }
            if contracted {
                return Err(PgrError::Request(
                    "rail:paretoRoutes does not run on a contraction hierarchy".into(),
                ));
            }
        } else if !criteria.is_empty() {
            return Err(PgrError::Request(
                "pgr:criteria is only for rail:paretoRoutes".into(),
            ));
        }
        if function == PgrFunction::CompatibilityShapes {
            if vehicle.is_none() || rules.is_none() || shape_nodes.len() > 1 {
                return Err(PgrError::Request(
                    "rail:compatibilityShapes takes pgr:vehicle, pgr:rules and at most one pgr:shape"
                        .into(),
                ));
            }
        } else if function == PgrFunction::ShaclRules {
            if shapes.is_none() || vehicle.is_some() || !shape_nodes.is_empty() {
                return Err(PgrError::Request(
                    "rail:shaclRules takes pgr:shapes (and optionally pgr:data_graph)".into(),
                ));
            }
        } else {
            if (shapes.is_some() || vehicle.is_some())
                && (function.is_graph_analysis() || function.is_flow() || function.without_edges())
            {
                return Err(PgrError::Request(
                    "pgr:shapes and pgr:vehicle apply to the path and distance functions".into(),
                ));
            }
            if vehicle.is_some() != rules.is_some() {
                return Err(PgrError::Request(
                    "pgr:vehicle and pgr:rules go together".into(),
                ));
            }
            if !shape_nodes.is_empty() && shapes.is_none() {
                return Err(PgrError::Request(
                    "pgr:shape names shapes in the pgr:shapes graph".into(),
                ));
            }
            if (focus.is_some() || data_graph.is_some()) && shapes.is_none() && vehicle.is_none() {
                return Err(PgrError::Request(
                    "pgr:focus and pgr:data_graph go with pgr:shapes or pgr:vehicle".into(),
                ));
            }
        }
        Ok(Self {
            constraint,
            shapes,
            shape_nodes,
            focus,
            vehicle,
            rules,
            data_graph,
            departure,
            wait,
            tolerance,
            concavity,
            point_geoms,
            point_rows,
            periods,
            fine,
            mapping,
            coarse_route,
            fine_start_vids,
            fine_end_vids,
            criteria,
            max_paths: max_paths as usize,
            function,
            k,
            heap_paths,
            stop_on_first,
            cap: cap as usize,
            contracted,
            edges,
            restrictions,
            points,
            matrix,
            orders,
            vehicles,
            depot,
            initial_sol: initial_sol as i64,
            coordinates,
            start_vids,
            end_vids,
            vias,
            strict,
            u_turn_on_edge,
            directed,
            heuristic,
            factor,
            distance,
            max_depth,
            methods,
            cycles: cycles as usize,
            forbidden,
            start_id,
            end_id,
            outputs,
        })
    }
}

fn collect(
    pattern: &GraphPattern,
    triples: &mut Vec<(String, TermPattern)>,
    subselects: &mut Vec<GraphPattern>,
) -> Result<(), PgrError> {
    match pattern {
        GraphPattern::Bgp { patterns } => {
            for t in patterns {
                let NamedNodePattern::NamedNode(p) = &t.predicate else {
                    return Err(PgrError::Request(
                        "variable predicates are not allowed".into(),
                    ));
                };
                triples.push((p.as_str().to_owned(), t.object.clone()));
            }
            Ok(())
        }
        GraphPattern::Join { left, right } => {
            collect(left, triples, subselects)?;
            collect(right, triples, subselects)
        }
        GraphPattern::Project { .. }
        | GraphPattern::Distinct { .. }
        | GraphPattern::Reduced { .. }
        | GraphPattern::Slice { .. }
        | GraphPattern::OrderBy { .. } => {
            subselects.push(pattern.clone());
            Ok(())
        }
        other => Err(PgrError::Request(format!(
            "unsupported pattern in pgr SERVICE: {other}"
        ))),
    }
}

fn projected(pattern: &GraphPattern) -> Vec<Variable> {
    match pattern {
        GraphPattern::Project { variables, .. } => variables.clone(),
        GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::OrderBy { inner, .. } => projected(inner),
        _ => Vec::new(),
    }
}
