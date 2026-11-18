//! `rail:paretoRoutes`: the Pareto-optimal paths between start and end vertices under several
//! cost columns of the edge set (see [`tripletrail_routing::pareto_paths`]).
//!
//! `pgr:criteria` names the columns: `cost` (`?cost` one way, `?reverse_cost` the other) and
//! `?c_*` columns, whose value applies in both directions of the edge. Which arcs exist follows
//! `?cost`/`?reverse_cost` as for every other function; a row with an unbound or non-numeric
//! criterion is left out, as are arcs with a negative one.

use crate::network::{Network, number};
use crate::request::Request;
use crate::service::PgrError;
use crate::tables::{Row, list_item};
use oxigraph::model::{Literal, Term};
use tripletrail_routing::{MultiEdge, MultiGraph, ParetoPath, pareto_paths};

/// What a criterion column holds.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Measure {
    /// The step's edge cost.
    Step,
    /// The cost so far, before the step's edge.
    Agg,
    /// The whole path's cost.
    Total,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Column {
    Seq,
    PathId,
    PathSeq,
    StartVid,
    EndVid,
    Node,
    Edge,
    RouteList,
    Criterion(usize, Measure),
}

impl Column {
    /// A column name: pgRouting's path columns, a criterion's name (its step cost),
    /// `agg_<name>` (cost so far) or `total_<name>` (the path's cost). `cost`, `agg_cost` and
    /// `total_cost` are the first criterion's unless a criterion is named `cost`.
    fn parse(name: &str, criteria: &[String]) -> Result<Self, PgrError> {
        let index = |n: &str| criteria.iter().position(|c| c == n);
        if let Some(i) = index(name) {
            return Ok(Self::Criterion(i, Measure::Step));
        }
        for (prefix, m) in [("agg_", Measure::Agg), ("total_", Measure::Total)] {
            if let Some(i) = name.strip_prefix(prefix).and_then(index) {
                return Ok(Self::Criterion(i, m));
            }
        }
        Ok(match name {
            "seq" => Self::Seq,
            "path_id" => Self::PathId,
            "path_seq" => Self::PathSeq,
            "start_vid" => Self::StartVid,
            "end_vid" => Self::EndVid,
            "node" => Self::Node,
            "edge" => Self::Edge,
            "route_list" | "routeAsList" | "route_as_list" => Self::RouteList,
            "cost" => Self::Criterion(0, Measure::Step),
            "agg_cost" => Self::Criterion(0, Measure::Agg),
            "total_cost" => Self::Criterion(0, Measure::Total),
            _ => {
                return Err(PgrError::Request(format!(
                    "pgr:{name} is not a result column of rail:paretoRoutes (seq, path_id, path_seq, start_vid, end_vid, node, edge, route_list, cost, agg_cost, total_cost, or a criterion's name, agg_<name>, total_<name>)"
                )));
            }
        })
    }

    fn per_path(self) -> bool {
        matches!(
            self,
            Self::PathId
                | Self::StartVid
                | Self::EndVid
                | Self::RouteList
                | Self::Criterion(_, Measure::Total)
        )
    }
}

/// The multi-criteria graph of a network for `criteria`, without the `disabled` edges.
fn multi_graph(
    network: &Network,
    criteria: &[String],
    directed: bool,
    disabled: Option<&[bool]>,
) -> Result<MultiGraph, PgrError> {
    let names = network
        .extras
        .as_ref()
        .map(|x| x.names.as_slice())
        .unwrap_or(&[]);
    // For each criterion: None for `cost`, else its extra column.
    let mut sources = Vec::with_capacity(criteria.len());
    for c in criteria {
        if c == "cost" {
            if !network.has_cost {
                return Err(PgrError::Request(
                    "the criterion cost needs ?cost in the edge set".into(),
                ));
            }
            sources.push(None);
        } else {
            let i = names
                .iter()
                .position(|n| n == c)
                .ok_or_else(|| PgrError::Request(format!("the edge set does not project ?{c}")))?;
            sources.push(Some(i));
        }
    }
    let g = &network.graph;
    let mut edges = Vec::with_capacity(g.rows.len());
    'rows: for (i, r) in g.rows.iter().enumerate() {
        if disabled.is_some_and(|d| d.get(r.id as usize).copied().unwrap_or(false)) {
            continue;
        }
        let extra = network.extras.as_ref().map(|x| x.row(i));
        let reverse = r.reverse_cost.filter(|c| *c >= 0.0);
        let mut forward = Vec::with_capacity(criteria.len());
        let mut backward = Vec::with_capacity(criteria.len());
        for s in &sources {
            match s {
                None => {
                    forward.push(r.cost);
                    backward.push(reverse.unwrap_or(-1.0));
                }
                Some(c) => {
                    let Some(v) = extra.and_then(|x| x[*c].as_ref()).and_then(number) else {
                        continue 'rows;
                    };
                    forward.push(v);
                    backward.push(v);
                }
            }
        }
        edges.push(MultiEdge {
            id: r.id,
            source: r.source,
            target: r.target,
            forward: (r.cost >= 0.0).then_some(forward),
            backward: reverse.map(|_| backward),
        });
    }
    Ok(MultiGraph::build(
        g.vertices.len(),
        criteria.len(),
        &edges,
        directed,
    ))
}

/// The result rows of `rail:paretoRoutes`, one per path step (or one per path when only
/// path-level columns are asked for).
pub(crate) fn pareto_table(
    request: &Request,
    network: &Network,
    disabled: Option<&[bool]>,
    names: &[String],
) -> Result<Vec<Row>, PgrError> {
    let columns: Vec<Column> = names
        .iter()
        .map(|n| Column::parse(n, &request.criteria))
        .collect::<Result<_, _>>()?;
    let graph = multi_graph(network, &request.criteria, request.directed, disabled)?;
    let g = &network.graph;
    let per_path = columns.iter().all(|c| c.per_path());
    let float = |v: f64| Some(Term::from(Literal::from(v)));
    let mut rows = Vec::new();
    let mut seq = 0i64;
    for s in &request.start_vids {
        let Some(s) = g.vertex_id(s) else { continue };
        for t in &request.end_vids {
            let Some(t) = g.vertex_id(t) else { continue };
            let paths: Vec<ParetoPath> = pareto_paths(&graph, s, t, request.max_paths);
            for (p, path) in paths.iter().enumerate() {
                let path_id = p as i64 + 1;
                let route_list = || {
                    let items: Vec<String> = path
                        .steps
                        .iter()
                        .filter_map(|s| s.edge)
                        .map(|e| list_item(&g.edges[e as usize]))
                        .collect();
                    Some(Term::from(Literal::from(items.join(" "))))
                };
                let path_cell = |c: &Column| -> Option<Term> {
                    match *c {
                        Column::PathId => Some(Literal::from(path_id).into()),
                        Column::StartVid => Some(g.vertices[path.start_vid as usize].clone()),
                        Column::EndVid => Some(g.vertices[path.end_vid as usize].clone()),
                        Column::RouteList => route_list(),
                        Column::Criterion(i, Measure::Total) => float(path.total[i]),
                        _ => None,
                    }
                };
                if per_path {
                    rows.push(columns.iter().map(path_cell).collect());
                    continue;
                }
                for (i, step) in path.steps.iter().enumerate() {
                    seq += 1;
                    let cell = |c: &Column| -> Option<Term> {
                        match *c {
                            Column::Seq => Some(Literal::from(seq).into()),
                            Column::PathSeq => Some(Literal::from(i as i64 + 1).into()),
                            Column::Node => Some(g.vertices[step.node as usize].clone()),
                            Column::Edge => step.edge.map(|e| g.edges[e as usize].clone()),
                            Column::Criterion(k, Measure::Step) => float(step.cost[k]),
                            Column::Criterion(k, Measure::Agg) => float(step.agg_cost[k]),
                            other => path_cell(&other),
                        }
                    };
                    rows.push(columns.iter().map(cell).collect());
                }
            }
        }
    }
    Ok(rows)
}
