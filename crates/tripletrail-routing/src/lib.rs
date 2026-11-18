//! Data-model agnostic routing core.
//!
//! The only input is a pgRouting-shaped edge set (`id, source, target, cost, reverse_cost`)
//! whose terms have already been mapped to dense `u32` ids. Nothing in this crate knows about
//! RDF, ERA, RailTopoModel, OSM or OpenTNF: those are network profiles, i.e. queries that
//! produce the edge set.

mod analysis;
pub mod cancel;
mod cch;
mod ch;
mod components;
mod contraction;
mod flow;
mod graph;
mod ksp;
mod linegraph;
mod ordering;
mod pareto;
mod paths;
mod persist;
pub use persist::SavedHierarchy;
mod planar;
mod search;
mod spanning;
mod timedep;
mod trsp;
mod tsp;
mod vrp;

pub use analysis::{
    biconnected_components, circuits, dominator_tree, make_connected, max_cardinality_matching,
    stoer_wagner, transitive_closure,
};
pub use cch::{Cch, CchSpace, cch_dijkstra};
pub use ch::{ChSpace, ContractionHierarchy, ch_dijkstra};
pub use components::{connected_components, cut_vertices_and_edges, strong_components};
pub use contraction::{Contracted, ContractionMethod, Shortcut, contract};
pub use flow::{
    FlowArc, MaxFlowAlgorithm, chinese_postman, edge_disjoint_paths, max_flow, min_cost_max_flow,
};
pub use graph::{EdgeRow, Graph, GraphBuilder, InternedGraph};
pub use ksp::ksp;
pub use linegraph::{LineEdge, LineGraphFull, line_graph, line_graph_full};
pub use ordering::{
    bandwidth, bipartite, edge_coloring, king_ordering, sequential_vertex_coloring, sloan_ordering,
};
pub use pareto::{MultiEdge, MultiGraph, ParetoPath, ParetoStep, pareto_paths};
pub use paths::{
    ShortestTree, TreeStep, bellman_ford, binary_bfs, breadth_first, dag_shortest_paths,
    depth_first, edward_moore, topological_sort,
};
pub use planar::is_planar;
pub use search::{
    Heuristic, Path, PathStep, ReachedNode, SearchSpace, astar, bd_dijkstra, dijkstra,
    dijkstra_between, driving_distance,
};
pub use spanning::{minimum_spanning_forest, minimum_spanning_forest_arcs};
pub use timedep::{Period, TimeTable, td_dijkstra};
pub use trsp::{Restriction, Restrictions, trsp, trsp_avoiding, turn_restricted_path};
pub use tsp::{CostMatrix, tsp};
pub use vrp::{Order, Plan, PlannedStop, Stop, StopKind, Vehicle, pick_deliver};
