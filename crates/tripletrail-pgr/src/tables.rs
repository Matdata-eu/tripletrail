//! The functions whose results are not paths: traversals, flows, orderings, colourings and
//! other graph analysis. Each declares its result columns (pgRouting's names) and fills one
//! row of terms per result row.

use crate::network::Network;
use crate::request::{PgrFunction, Request};
use crate::service::PgrError;
use oxigraph::model::{Literal, NamedNode, Term};
use tripletrail_routing::{
    ContractionMethod, CostMatrix, EdgeRow, FlowArc, Graph, InternedGraph, MaxFlowAlgorithm, Path,
    PathStep, SearchSpace, TreeStep, bandwidth, biconnected_components, bipartite, breadth_first,
    chinese_postman, contract, depth_first, dominator_tree, driving_distance, edge_coloring,
    is_planar, king_ordering, line_graph, line_graph_full, make_connected,
    max_cardinality_matching, max_flow, min_cost_max_flow, minimum_spanning_forest_arcs,
    sequential_vertex_coloring, sloan_ordering, stoer_wagner, topological_sort, transitive_closure,
    tsp,
};

/// One result row, in the order of the function's columns.
pub(crate) type Row = Vec<Option<Term>>;

const TREE: &[&str] = &[
    "seq",
    "depth",
    "start_vid",
    "pred",
    "node",
    "edge",
    "cost",
    "agg_cost",
];

/// The columns of `pgr:networkInfo`.
pub(crate) const NETWORK_INFO: &[&str] = &[
    "network",
    "directed",
    "vertices",
    "edges",
    "rows",
    "arcs",
    "has_cost",
    "columns",
    "hierarchy",
    "filters",
    "customized",
    "edge_query_time",
    "build_time",
    "hierarchy_time",
    "customize_time",
    "cached",
    "persisted",
];

/// The `pgr:networkInfo` row of a network: sizes, what is cached on it, timings in seconds.
pub(crate) fn network_info_table(request: &Request, network: &Network, cached: bool) -> Vec<Row> {
    let info = network.info();
    let n = |v: usize| int(v as i64);
    let flag = |b: bool| Some(Literal::from(b).into());
    let secs = |d: std::time::Duration| float(d.as_secs_f64());
    let network_term = match &request.edges {
        Some(crate::request::EdgeSource::Network(iri)) => Some(iri.clone().into()),
        _ => None,
    };
    vec![vec![
        network_term,
        flag(request.directed),
        n(info.vertices),
        n(info.edges),
        n(info.rows),
        n(info.arcs),
        flag(info.has_cost),
        Some(Literal::from(info.columns.join(" ")).into()),
        flag(info.hierarchy),
        n(info.filters),
        n(info.customized),
        secs(info.stats.edge_query),
        secs(info.stats.build),
        info.hierarchy_time.and_then(secs),
        info.stats.customize.and_then(secs),
        flag(cached),
        flag(info.persisted),
    ]]
}

/// The result columns of a table function; `a|b` lists another accepted name. `None` for the
/// path functions, whose columns the service handles itself.
pub(crate) fn columns(f: PgrFunction) -> Option<&'static [&'static str]> {
    use PgrFunction as F;
    Some(match f {
        F::BreadthFirstSearch
        | F::DepthFirstSearch
        | F::KruskalBfs
        | F::KruskalDfs
        | F::KruskalDd
        | F::PrimBfs
        | F::PrimDfs
        | F::PrimDd => TREE,
        F::MaxFlow => &["pgr_maxflow|flow|max_flow"],
        F::BoykovKolmogorov | F::EdmondsKarp | F::PushRelabel => &[
            "seq",
            "edge",
            "start_vid|source",
            "end_vid|target",
            "flow",
            "residual_capacity",
        ],
        F::MaxFlowMinCost => &[
            "seq",
            "edge",
            "source|start_vid",
            "target|end_vid",
            "flow",
            "residual_capacity",
            "cost",
            "agg_cost",
        ],
        F::MaxFlowMinCostCost => &["pgr_maxflowmincost_cost|cost"],
        F::MaxCardinalityMatch => &["seq", "edge", "source", "target"],
        F::StoerWagner => &["seq", "edge", "cost", "mincut"],
        F::Tsp | F::TspEuclidean => &["seq", "node", "cost", "agg_cost"],
        F::LineGraph => &["seq", "source", "target", "cost", "reverse_cost"],
        F::LineGraphFull => &["seq", "source", "target", "cost", "edge"],
        F::Contraction => &[
            "seq",
            "type",
            "id",
            "contracted_vertices",
            "source",
            "target",
            "cost",
        ],
        F::BiconnectedComponents => &["seq", "edge", "component"],
        F::Degree => &["node", "degree"],
        F::ExtractVertices => &["id", "in_edges", "out_edges", "x", "y", "geom"],
        F::TopologicalSort => &["seq", "sorted_v"],
        F::TransitiveClosure => &["seq", "vid", "target_array"],
        F::MakeConnected => &["seq", "start_vid", "end_vid"],
        F::IsPlanar => &["pgr_isplanar|is_planar|planar"],
        F::LengauerTarjanDominatorTree => &["seq", "vid", "idom"],
        F::SequentialVertexColoring | F::Bipartite => &["vertex_id", "color_id"],
        F::EdgeColoring => &["edge_id", "color_id"],
        F::Bandwidth => &["pgr_bandwidth|bandwidth"],
        F::KingOrdering | F::SloanOrdering => &["seq", "node"],
        F::ChinesePostmanCost => &["pgr_chinesepostmancost|cost"],
        F::CheckTopology => crate::topology::CHECK_COLUMNS,
        F::AnalyzeGraph => crate::topology::ANALYZE_COLUMNS,
        F::BuildTopology => crate::topology::BUILD_COLUMNS,
        F::NodeNetwork => crate::topology::NODE_COLUMNS,
        F::SeparateCrossing | F::SeparateTouching => crate::topology::SEPARATE_COLUMNS,
        F::FindCloseEdges => crate::topology::CLOSE_COLUMNS,
        F::PickDeliver | F::PickDeliverEuclidean => crate::vrp::PICK_DELIVER_COLUMNS,
        F::VrpOneDepot => crate::vrp::ONE_DEPOT_COLUMNS,
        F::CompatibilityShapes | F::ShaclRules => &["subject", "predicate", "object"],
        F::NetworkInfo => NETWORK_INFO,
        F::Version => &["pgr_version|version"],
        F::FullVersion => &[
            "version",
            "build_type",
            "compile_date",
            "library",
            "system",
            "postgresql",
            "compiler",
            "boost",
            "hash",
        ],
        _ => return None,
    })
}

/// The position of result column `name` in `columns`.
pub(crate) fn column_index(columns: &[&str], name: &str) -> Result<usize, PgrError> {
    columns
        .iter()
        .position(|c| c.split('|').any(|alias| alias == name))
        .ok_or_else(|| {
            PgrError::Request(format!(
                "pgr:{name} is not a result column of this function (it has {})",
                columns
                    .iter()
                    .map(|c| format!("pgr:{}", c.split('|').next().unwrap()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

fn int(i: i64) -> Option<Term> {
    Some(Literal::from(i).into())
}

fn float(f: f64) -> Option<Term> {
    Some(Literal::from(f).into())
}

/// How a term is written in a list: an IRI or a literal's value, as in restriction paths.
pub(crate) fn list_item(t: &Term) -> String {
    match t {
        Term::NamedNode(n) => n.as_str().to_owned(),
        Term::Literal(l) => l.value().to_owned(),
        other => other.to_string(),
    }
}

/// pgRouting's arrays: the items separated by spaces, in a string.
fn list<'a>(terms: impl IntoIterator<Item = &'a Term>) -> Option<Term> {
    let items: Vec<String> = terms.into_iter().map(list_item).collect();
    Some(Literal::from(items.join(" ")).into())
}

/// The order pgRouting sorts ids in: numbers by value, then everything else by its text.
fn sort_key(t: &Term) -> (u8, f64, String) {
    if let Term::Literal(l) = t
        && let Ok(v) = l.value().parse::<f64>()
    {
        return (0, v, String::new());
    }
    (1, 0.0, t.to_string())
}

/// The indices of `terms`, sorted in pgRouting's id order.
pub(crate) fn ranks(terms: &[Term]) -> Vec<u32> {
    let keys: Vec<(u8, f64, String)> = terms.iter().map(sort_key).collect();
    let mut order: Vec<u32> = (0..terms.len() as u32).collect();
    order.sort_by(|&a, &b| {
        let (x, y) = (&keys[a as usize], &keys[b as usize]);
        x.0.cmp(&y.0)
            .then(x.1.total_cmp(&y.1))
            .then_with(|| x.2.cmp(&y.2))
    });
    order
}

/// A network renumbered so that vertex and edge ids follow pgRouting's id order, which the
/// algorithms use to break ties: results then do not depend on how the network was built.
pub(crate) struct View<'a> {
    pub g: &'a InternedGraph<Term>,
    pub graph: Graph,
    /// Vertex and edge terms by new id.
    pub vertices: Vec<&'a Term>,
    pub edges: Vec<&'a Term>,
    /// New id of every network vertex and edge id, and back.
    new_vertex: Vec<u32>,
    old_vertex: Vec<u32>,
    old_edge: Vec<u32>,
    /// The edge-set rows in new ids, by edge; `row_index` is each one's index in `g.rows`.
    pub rows: Vec<EdgeRow>,
    pub row_index: Vec<usize>,
}

impl<'a> View<'a> {
    pub(crate) fn new(network: &'a Network, directed: bool) -> Self {
        let g = &network.graph;
        let (old_vertex, old_edge) = network.id_order().clone();
        let mut new_vertex = vec![0u32; old_vertex.len()];
        for (new, &old) in old_vertex.iter().enumerate() {
            new_vertex[old as usize] = new as u32;
        }
        let mut new_edge = vec![0u32; old_edge.len()];
        for (new, &old) in old_edge.iter().enumerate() {
            new_edge[old as usize] = new as u32;
        }
        let mut row_index: Vec<usize> = (0..g.rows.len()).collect();
        row_index.sort_by_key(|&i| new_edge[g.rows[i].id as usize]);
        let rows: Vec<EdgeRow> = row_index
            .iter()
            .map(|&i| {
                let r = g.rows[i];
                EdgeRow {
                    id: new_edge[r.id as usize],
                    source: new_vertex[r.source as usize],
                    target: new_vertex[r.target as usize],
                    ..r
                }
            })
            .collect();
        let mut graph = Graph::build(old_vertex.len(), &rows, directed);
        if let Some(c) = g.graph.coords() {
            graph = graph.with_coords(old_vertex.iter().map(|&o| c[o as usize]).collect());
        }
        Self {
            g,
            graph,
            vertices: old_vertex
                .iter()
                .map(|&o| &g.vertices[o as usize])
                .collect(),
            edges: old_edge.iter().map(|&o| &g.edges[o as usize]).collect(),
            new_vertex,
            old_vertex,
            old_edge,
            rows,
            row_index,
        }
    }

    pub(crate) fn vertex(&self, t: &Term) -> Option<u32> {
        self.g.vertex_id(t).map(|v| self.new_vertex[v as usize])
    }

    /// A path found on the view, in the network's ids.
    pub(crate) fn to_network(&self, p: Path) -> Path {
        Path {
            start_vid: self.old_vertex[p.start_vid as usize],
            end_vid: self.old_vertex[p.end_vid as usize],
            steps: p
                .steps
                .into_iter()
                .map(|s| PathStep {
                    node: self.old_vertex[s.node as usize],
                    edge: s.edge.map(|e| self.old_edge[e as usize]),
                    ..s
                })
                .collect(),
        }
    }

    fn v(&self, v: u32) -> Option<Term> {
        Some(self.vertices[v as usize].clone())
    }

    fn e(&self, e: u32) -> Option<Term> {
        Some(self.edges[e as usize].clone())
    }

    fn tree_row(&self, seq: usize, s: &TreeStep) -> Row {
        vec![
            int(seq as i64 + 1),
            int(i64::from(s.depth)),
            self.v(s.start_vid),
            self.v(s.pred),
            self.v(s.node),
            s.edge.and_then(|e| self.e(e)),
            float(s.cost),
            float(s.agg_cost),
        ]
    }
}

/// The table of a function over a network.
pub(crate) fn graph_table(request: &Request, network: &Network) -> Result<Vec<Row>, PgrError> {
    use PgrFunction as F;
    let view = View::new(network, request.directed);
    let roots: Vec<u32> = request
        .start_vids
        .iter()
        .filter_map(|t| view.vertex(t))
        .collect();
    let mut rows: Vec<Row> = Vec::new();
    match request.function {
        F::BreadthFirstSearch | F::DepthFirstSearch => {
            for &r in &roots {
                let steps = if request.function == F::BreadthFirstSearch {
                    breadth_first(&view.graph, r, request.max_depth)
                } else {
                    depth_first(&view.graph, r, request.max_depth)
                };
                for s in steps {
                    rows.push(view.tree_row(rows.len(), &s));
                }
            }
        }
        F::KruskalBfs | F::KruskalDfs | F::KruskalDd | F::PrimBfs | F::PrimDfs | F::PrimDd => {
            // The minimum spanning forest as a graph of its own, then traversed from the roots.
            let forest: Vec<EdgeRow> = minimum_spanning_forest_arcs(&view.graph)
                .into_iter()
                .map(|(e, cost, a, b)| EdgeRow {
                    id: e,
                    source: a,
                    target: b,
                    cost,
                    reverse_cost: None,
                })
                .collect();
            let tree = Graph::build(view.graph.num_vertices(), &forest, false);
            let mut space = SearchSpace::new();
            for &r in &roots {
                let steps: Vec<TreeStep> = match request.function {
                    F::KruskalBfs | F::PrimBfs => breadth_first(&tree, r, request.max_depth),
                    F::KruskalDfs | F::PrimDfs => depth_first(&tree, r, request.max_depth),
                    _ => {
                        let max = request.distance.unwrap_or(f64::INFINITY);
                        let mut depth = vec![0u32; tree.num_vertices()];
                        driving_distance(&tree, &mut space, r, max)
                            .into_iter()
                            .map(|n| {
                                if n.node != r {
                                    depth[n.node as usize] = depth[n.pred as usize] + 1;
                                }
                                TreeStep {
                                    depth: depth[n.node as usize],
                                    start_vid: r,
                                    pred: n.pred,
                                    node: n.node,
                                    edge: n.edge,
                                    cost: n.cost,
                                    agg_cost: n.agg_cost,
                                }
                            })
                            .collect()
                    }
                };
                for s in steps {
                    rows.push(view.tree_row(rows.len(), &s));
                }
            }
        }
        F::MaxFlow
        | F::BoykovKolmogorov
        | F::EdmondsKarp
        | F::PushRelabel
        | F::MaxFlowMinCost
        | F::MaxFlowMinCostCost => {
            let Some(capacities) = &network.capacities else {
                return Err(PgrError::Request(
                    "the flow functions need ?capacity (and optionally ?reverse_capacity) in the edge set"
                        .into(),
                ));
            };
            // Arcs: (row, forward), each with its capacity and cost.
            let mut arcs = Vec::new();
            let mut which = Vec::new();
            for (i, r) in view.rows.iter().enumerate() {
                let (cap, rcap) = capacities[view.row_index[i]];
                if cap >= 0.0 {
                    arcs.push(FlowArc {
                        tail: r.source,
                        head: r.target,
                        capacity: cap as i64,
                        cost: r.cost,
                    });
                    which.push((i, true));
                }
                if rcap >= 0.0 {
                    arcs.push(FlowArc {
                        tail: r.target,
                        head: r.source,
                        capacity: rcap as i64,
                        cost: r.reverse_cost.filter(|c| *c >= 0.0).unwrap_or(r.cost),
                    });
                    which.push((i, false));
                }
            }
            let ends = |terms: &[Term]| -> Vec<u32> {
                let mut v: Vec<u32> = terms.iter().filter_map(|t| view.vertex(t)).collect();
                v.sort_unstable();
                v.dedup();
                v
            };
            let (sources, sinks) = (ends(&request.start_vids), ends(&request.end_vids));
            let n = view.graph.num_vertices();
            let (total, cost, mut flows) = match request.function {
                F::MaxFlowMinCost | F::MaxFlowMinCostCost => {
                    min_cost_max_flow(n, &arcs, &sources, &sinks)
                }
                f => {
                    let algorithm = match f {
                        F::EdmondsKarp => MaxFlowAlgorithm::EdmondsKarp,
                        F::BoykovKolmogorov => MaxFlowAlgorithm::Dinic,
                        _ => MaxFlowAlgorithm::PushRelabel,
                    };
                    let (total, flows) = max_flow(n, &arcs, &sources, &sinks, algorithm);
                    (total, 0.0, flows)
                }
            };
            match request.function {
                F::MaxFlow => return Ok(vec![vec![int(total)]]),
                F::MaxFlowMinCostCost => return Ok(vec![vec![float(cost)]]),
                _ => {}
            }
            // Flow both ways along one row cancels out.
            for k in 1..arcs.len() {
                if which[k].0 == which[k - 1].0 {
                    let m = flows[k].min(flows[k - 1]);
                    flows[k] -= m;
                    flows[k - 1] -= m;
                }
            }
            let mut agg = 0.0;
            for (k, a) in arcs.iter().enumerate() {
                if flows[k] <= 0 {
                    continue;
                }
                let (i, _) = which[k];
                let row = view.rows[i];
                let mut out = vec![
                    int(rows.len() as i64 + 1),
                    view.e(row.id),
                    view.v(a.tail),
                    view.v(a.head),
                    int(flows[k]),
                    int(a.capacity - flows[k]),
                ];
                if request.function == F::MaxFlowMinCost {
                    let c = flows[k] as f64 * a.cost;
                    agg += c;
                    out.extend([float(c), float(agg)]);
                }
                rows.push(out);
            }
        }
        F::MaxCardinalityMatch => {
            let first_row = first_rows(&view);
            for e in max_cardinality_matching(&view.graph) {
                let r = view.rows[first_row[e as usize]];
                rows.push(vec![
                    int(rows.len() as i64 + 1),
                    view.e(e),
                    view.v(r.source),
                    view.v(r.target),
                ]);
            }
        }
        F::StoerWagner => {
            if let Some((_, cut)) = stoer_wagner(&view.graph) {
                let mut total = 0.0;
                for (e, c) in cut {
                    total += c;
                    rows.push(vec![
                        int(rows.len() as i64 + 1),
                        view.e(e),
                        float(c),
                        float(total),
                    ]);
                }
            }
        }
        F::LineGraph => {
            for l in line_graph(&view.graph) {
                rows.push(vec![
                    int(rows.len() as i64 + 1),
                    view.e(l.source),
                    view.e(l.target),
                    float(l.cost),
                    float(l.reverse_cost),
                ]);
            }
        }
        F::LineGraphFull => {
            let full = line_graph_full(&view.graph);
            // A vertex with a single arc end keeps its term; the others are split into new
            // vertices numbered -1, -2, ... as pgRouting numbers them.
            let mut ends = vec![0u32; view.graph.num_vertices()];
            for &v in &full.vertex {
                ends[v as usize] += 1;
            }
            let mut next = 0i64;
            let names: Vec<Option<Term>> = full
                .vertex
                .iter()
                .map(|&v| {
                    if ends[v as usize] == 1 {
                        view.v(v)
                    } else {
                        next -= 1;
                        int(next)
                    }
                })
                .collect();
            for (a, b, cost, edge) in full.arcs {
                rows.push(vec![
                    int(rows.len() as i64 + 1),
                    names[a as usize].clone(),
                    names[b as usize].clone(),
                    float(cost),
                    edge.and_then(|e| view.e(e)),
                ]);
            }
        }
        F::Contraction => {
            let methods: Vec<ContractionMethod> = request
                .methods
                .iter()
                .map(|&m| {
                    if m == 1 {
                        ContractionMethod::DeadEnd
                    } else {
                        ContractionMethod::Linear
                    }
                })
                .collect();
            let mut forbidden = vec![false; view.graph.num_vertices()];
            for v in request.forbidden.iter().filter_map(|t| view.vertex(t)) {
                forbidden[v as usize] = true;
            }
            let c = contract(
                &view.graph,
                request.directed,
                &methods,
                request.cycles,
                &forbidden,
            );
            let terms = |ids: &[u32]| list(ids.iter().map(|&v| view.vertices[v as usize]));
            let kind = |k: &str| Some(Literal::from(k).into());
            for (v, absorbed) in &c.vertices {
                rows.push(vec![
                    int(rows.len() as i64 + 1),
                    kind("v"),
                    view.v(*v),
                    terms(absorbed),
                    None,
                    None,
                    None,
                ]);
            }
            for (i, s) in c.shortcuts.iter().enumerate() {
                rows.push(vec![
                    int(rows.len() as i64 + 1),
                    kind("e"),
                    int(-(i as i64) - 1),
                    terms(&s.contracted),
                    view.v(s.source),
                    view.v(s.target),
                    float(s.cost),
                ]);
            }
        }
        F::BiconnectedComponents => {
            for (e, c) in biconnected_components(&view.graph) {
                rows.push(vec![int(rows.len() as i64 + 1), view.e(e), view.e(c)]);
            }
        }
        F::Degree | F::ExtractVertices => {
            let n = view.graph.num_vertices();
            let mut ins: Vec<Vec<u32>> = vec![Vec::new(); n];
            let mut outs: Vec<Vec<u32>> = vec![Vec::new(); n];
            for r in &view.rows {
                outs[r.source as usize].push(r.id);
                ins[r.target as usize].push(r.id);
            }
            for v in 0..n as u32 {
                let (i, o) = (&mut ins[v as usize], &mut outs[v as usize]);
                if request.function == F::Degree {
                    rows.push(vec![view.v(v), int((i.len() + o.len()) as i64)]);
                    continue;
                }
                i.dedup();
                o.dedup();
                let edges = |ids: &[u32]| {
                    if ids.is_empty() {
                        None
                    } else {
                        list(ids.iter().map(|&e| view.edges[e as usize]))
                    }
                };
                let xy = view.graph.coords().map(|c| c[v as usize]);
                rows.push(vec![
                    view.v(v),
                    edges(i),
                    edges(o),
                    xy.and_then(|p| float(p.0)),
                    xy.and_then(|p| float(p.1)),
                    xy.map(|(x, y)| {
                        Literal::new_typed_literal(
                            format!("POINT({x} {y})"),
                            NamedNode::new_unchecked(
                                "http://www.opengis.net/ont/geosparql#wktLiteral",
                            ),
                        )
                        .into()
                    }),
                ]);
            }
        }
        F::TopologicalSort => {
            // A graph with a cycle has no order (pgRouting warns and returns nothing).
            for v in topological_sort(&view.graph).unwrap_or_default() {
                rows.push(vec![int(rows.len() as i64 + 1), view.v(v)]);
            }
        }
        F::TransitiveClosure => {
            for (v, reached) in transitive_closure(&view.graph).iter().enumerate() {
                rows.push(vec![
                    int(rows.len() as i64 + 1),
                    view.v(v as u32),
                    list(reached.iter().map(|&w| view.vertices[w as usize])),
                ]);
            }
        }
        F::MakeConnected => {
            for (a, b) in make_connected(&view.graph) {
                rows.push(vec![int(rows.len() as i64 + 1), view.v(a), view.v(b)]);
            }
        }
        F::IsPlanar => rows.push(vec![Some(Literal::from(is_planar(&view.graph)).into())]),
        F::LengauerTarjanDominatorTree => {
            let Some(&root) = roots.first() else {
                return Ok(rows);
            };
            for (v, d) in dominator_tree(&view.graph, root).into_iter().enumerate() {
                rows.push(vec![
                    int(rows.len() as i64 + 1),
                    view.v(v as u32),
                    d.and_then(|d| view.v(d)),
                ]);
            }
        }
        F::SequentialVertexColoring | F::Bipartite => {
            let colors = if request.function == F::Bipartite {
                bipartite(&view.graph).unwrap_or_default()
            } else {
                sequential_vertex_coloring(&view.graph)
            };
            for (v, c) in colors.into_iter().enumerate() {
                rows.push(vec![view.v(v as u32), int(i64::from(c))]);
            }
        }
        F::EdgeColoring => {
            for (e, c) in edge_coloring(&view.graph) {
                rows.push(vec![view.e(e), int(i64::from(c))]);
            }
        }
        F::Bandwidth => rows.push(vec![int(i64::from(bandwidth(&view.graph)))]),
        F::KingOrdering | F::SloanOrdering => {
            let order = if request.function == F::KingOrdering {
                king_ordering(&view.graph)
            } else {
                sloan_ordering(&view.graph)
            };
            for v in order {
                rows.push(vec![int(rows.len() as i64 + 1), view.v(v)]);
            }
        }
        F::ChinesePostmanCost => {
            if let Some(p) = chinese_postman(&view.graph) {
                rows.push(vec![float(p.total_cost())]);
            }
        }
        f => unreachable!("{f:?} is not a table function over a network"),
    }
    Ok(rows)
}

/// The first row (in the view) of every edge id.
fn first_rows(view: &View) -> Vec<usize> {
    let mut first = vec![usize::MAX; view.edges.len()];
    for (i, r) in view.rows.iter().enumerate() {
        if first[r.id as usize] == usize::MAX {
            first[r.id as usize] = i;
        }
    }
    first
}

/// pgr_TSP and pgr_TSPeuclidean: `points` are the points' terms, `matrix` their costs.
pub(crate) fn tsp_table(
    request: &Request,
    points: &[Term],
    matrix: &CostMatrix,
) -> Result<Vec<Row>, PgrError> {
    if points.is_empty() {
        return Ok(Vec::new());
    }
    let find = |t: &Option<Term>, name: &str| -> Result<Option<u32>, PgrError> {
        match t {
            None => Ok(None),
            Some(t) => points
                .iter()
                .position(|p| p == t)
                .map(|i| Some(i as u32))
                .ok_or_else(|| PgrError::Request(format!("pgr:{name} {t} is not in the matrix"))),
        }
    };
    let start = find(&request.start_id, "start_id")?.unwrap_or(0);
    let end = find(&request.end_id, "end_id")?;
    let tour = tsp(matrix, start, end).ok_or_else(|| {
        PgrError::Request("pgr_TSP: the matrix has no tour through every point".into())
    })?;
    let mut agg = 0.0;
    Ok(tour
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let cost = if i == 0 {
                0.0
            } else {
                matrix.get(tour[i - 1], v)
            };
            agg += cost;
            vec![
                int(i as i64 + 1),
                Some(points[v as usize].clone()),
                float(cost),
                float(agg),
            ]
        })
        .collect())
}

/// Points sorted in pgRouting's id order, so that ties break the same way.
pub(crate) fn sorted_points(mut points: Vec<Term>) -> Vec<Term> {
    let order = ranks(&points);
    let mut sorted = Vec::with_capacity(points.len());
    let mut taken: Vec<Option<Term>> = points.drain(..).map(Some).collect();
    for i in order {
        sorted.push(taken[i as usize].take().unwrap());
    }
    sorted
}

/// pgr_version and pgr_full_version.
pub(crate) fn version_table(function: PgrFunction) -> Vec<Row> {
    let text = |s: &str| Some(Literal::from(s).into());
    let version = env!("CARGO_PKG_VERSION");
    if function == PgrFunction::Version {
        return vec![vec![text(version)]];
    }
    vec![vec![
        text(version),
        text(if cfg!(debug_assertions) {
            "Debug"
        } else {
            "Release"
        }),
        None,
        text(&format!("tripletrail-pgr-{version}")),
        text(&format!(
            "{}-{}",
            std::env::consts::ARCH,
            std::env::consts::OS
        )),
        None,
        text("rustc"),
        None,
        None,
    ]]
}
