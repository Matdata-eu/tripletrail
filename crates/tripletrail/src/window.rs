//! `SERVICE <http://w3id.org/tripletrail/window>`: SQL window functions over the rows of a
//! sub-select (#237).
//!
//! ```sparql
//! PREFIX win: <http://w3id.org/tripletrail/window#>
//! SERVICE <http://w3id.org/tripletrail/window> {
//!   { SELECT ?line ?signal ?km WHERE { ?signal ex:line ?line ; ex:km ?km } }
//!   [] win:partitionBy ?line ; win:orderBy ?km ;
//!      win:rowNumber ?n ; win:lag (?km ?previousKm) ; win:sum (?km ?runningKm) .
//! }
//! ```
//!
//! Every row of the sub-select comes out once, with its own columns and one column per
//! function. Rows are grouped by the `win:partitionBy` variables (all rows are one partition
//! without them) and ordered within a partition by `win:orderBy` (ascending) and
//! `win:orderByDesc` (descending) keys, in the order the triples give them, as SPARQL's ORDER
//! BY orders terms. Rows with equal keys are peers.
//!
//! Functions, each binding the variable last in its list (or the object):
//!
//! - `win:rowNumber ?n`, `win:rank ?r`, `win:denseRank ?d`, `win:percentRank ?p`,
//!   `win:cumeDist ?c` and `win:ntile (4 ?q)`, as SQL;
//! - `win:lag (?x ?out)`, `win:lag (?x 2 ?out)`, `win:lag (?x 2 "default" ?out)`, and
//!   `win:lead` the same way: the value `n` rows before or after in the partition;
//! - `win:firstValue (?x ?out)`, `win:lastValue (?x ?out)`, `win:sum`, `win:avg`, `win:min`,
//!   `win:max` and `win:count`, over the frame of the row.
//!
//! The frame, as SQL's default: with an order, from the start of the partition to the last
//! peer of the row; without one, the whole partition. `win:rows (start end)` sets a frame of
//! rows instead, each end an offset from the row (`-2` is two rows before, `0` the row
//! itself) or `"unbounded"`.
//!
//! The aggregates skip unbound values, as SQL skips NULLs; a value that is not a number makes
//! a sum, average, minimum or maximum unbound where the frame holds it. Sums and averages of
//! integers and decimals are exact (a sum of integers is an integer, an average a decimal), a
//! float or double makes them a float or double, as SPARQL's SUM and AVG.

use crate::statagg::{self, Number};
use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, Term};
use oxigraph::sparql::{
    QueryEvaluationError, QueryResults, QuerySolution, QuerySolutionIter, ServiceHandler,
    SparqlEvaluator, Variable,
};
use oxiri::Iri;
use oxsdatatypes::Decimal;
use spargebra::algebra::{Expression, GraphPattern, OrderExpression};
use spargebra::term::{NamedNodePattern, TermPattern};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use tripletrail_store::Dataset;

pub const WINDOW_SERVICE: &str = "http://w3id.org/tripletrail/window";
const WIN: &str = "http://w3id.org/tripletrail/window#";
const RDF_FIRST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#first";
const RDF_REST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#rest";
const RDF_NIL: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#nil";

#[derive(Debug, thiserror::Error)]
pub enum WindowError {
    #[error("window: {0}")]
    Request(String),
    #[error("window: input query failed: {0}")]
    Input(#[source] Box<dyn std::error::Error + Send + Sync>),
}

fn bad(msg: impl Into<String>) -> WindowError {
    WindowError::Request(msg.into())
}

/// The handler of the window SERVICE.
#[derive(Clone)]
pub struct WindowService {
    store: Dataset,
    evaluator: Arc<dyn Fn() -> SparqlEvaluator + Send + Sync>,
    /// The calling query's token: its sub-selects stop with it (a timeout).
    cancellation: Option<oxigraph::sparql::CancellationToken>,
    /// The dataset its sub-selects read: the calling query's, for a caller who reads only
    /// some graphs (#203). `None` reads the store's own default graph and named graphs.
    dataset: Option<oxigraph::sparql::QueryDataset>,
    /// What a caller under row rules sees (#202): its sub-selects read only those rows.
    rows: Option<tripletrail_store::RowView>,
}

impl WindowService {
    pub fn new(
        store: Dataset,
        evaluator: impl Fn() -> SparqlEvaluator + Send + Sync + 'static,
    ) -> Self {
        Self {
            store,
            evaluator: Arc::new(evaluator),
            cancellation: None,
            dataset: None,
            rows: None,
        }
    }

    /// The same service, its sub-selects reading `dataset` only (#203).
    #[must_use]
    pub fn with_dataset(&self, dataset: oxigraph::sparql::QueryDataset) -> Self {
        Self {
            dataset: Some(dataset),
            ..self.clone()
        }
    }

    /// The same service, its sub-selects reading only what `rows` shows (#202).
    #[must_use]
    pub fn with_rows(&self, rows: tripletrail_store::RowView) -> Self {
        Self {
            rows: Some(rows),
            ..self.clone()
        }
    }

    /// A sub-select prepared on this service's dataset.
    fn prepare(&self, query: spargebra::Query) -> oxigraph::sparql::PreparedSparqlQuery {
        let mut prepared = self.evaluator().for_query(query);
        if let Some(dataset) = &self.dataset {
            *prepared.dataset_mut() = dataset.clone();
        }
        prepared
    }

    /// The same service, its sub-selects stopping once `token` is cancelled.
    #[must_use]
    pub fn with_cancellation_token(&self, token: oxigraph::sparql::CancellationToken) -> Self {
        Self {
            cancellation: Some(token),
            ..self.clone()
        }
    }

    /// An evaluator for a sub-select, with the calling query's token.
    fn evaluator(&self) -> SparqlEvaluator {
        let evaluator = (self.evaluator)();
        match &self.cancellation {
            Some(token) => evaluator.with_cancellation_token(token.clone()),
            None => evaluator,
        }
    }
}

/// One function to compute.
#[derive(Debug, Clone)]
enum Function {
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile(u64),
    /// Offset (negative: before), default.
    Shift(usize, i64, Option<Term>),
    FirstValue(usize),
    LastValue(usize),
    Sum(usize),
    Avg(usize),
    Min(usize),
    Max(usize),
    Count(usize),
}

/// A frame end: an offset from the row, or the partition's end.
#[derive(Debug, Clone, Copy)]
enum Bound {
    Unbounded,
    Offset(i64),
}

/// An object of the service's triples: a term, or the members of an RDF list.
pub(crate) enum Value {
    One(TermPattern),
    List(Vec<TermPattern>),
}

impl ServiceHandler for WindowService {
    type Error = WindowError;

    fn handle(
        &self,
        pattern: &GraphPattern,
        _base_iri: Option<&Iri<String>>,
    ) -> Result<QuerySolutionIter<'static>, WindowError> {
        let mut triples = Vec::new();
        let mut subselects = Vec::new();
        collect(pattern, &mut triples, &mut subselects, "window").map_err(WindowError::Request)?;
        let [input] = subselects.as_slice() else {
            return Err(bad(
                "give exactly one sub-select ({ SELECT … }) of the rows",
            ));
        };
        let params = parameters(&triples, WIN, "win").map_err(WindowError::Request)?;

        // The sub-select's rows, ordered by the order keys.
        let inner_vars = projected(input);
        let column = |v: &Variable| -> Result<usize, WindowError> {
            inner_vars.iter().position(|x| x == v).ok_or_else(|| {
                bad(format!(
                    "?{} is not a variable of the sub-select",
                    v.as_str()
                ))
            })
        };
        let mut partition = Vec::new();
        let mut order: Vec<(usize, bool)> = Vec::new();
        let mut functions: Vec<(Variable, Function)> = Vec::new();
        let mut rows_frame: Option<(Bound, Bound)> = None;
        for (name, value) in &params {
            let vars = |value: &Value| -> Result<Vec<Variable>, WindowError> {
                let items = match value {
                    Value::One(t) => std::slice::from_ref(t),
                    Value::List(items) => items.as_slice(),
                };
                items
                    .iter()
                    .map(|t| match t {
                        TermPattern::Variable(v) => Ok(v.clone()),
                        _ => Err(bad(format!("win:{name} takes variables"))),
                    })
                    .collect()
            };
            match name.as_str() {
                "partitionBy" => {
                    for v in vars(value)? {
                        partition.push(column(&v)?);
                    }
                }
                "orderBy" | "orderByDesc" => {
                    for v in vars(value)? {
                        order.push((column(&v)?, name == "orderByDesc"));
                    }
                }
                "rows" => {
                    let Value::List(items) = value else {
                        return Err(bad("win:rows takes a list: (start end)"));
                    };
                    let [start, end] = items.as_slice() else {
                        return Err(bad("win:rows takes a list of two: (start end)"));
                    };
                    rows_frame = Some((bound(start)?, bound(end)?));
                }
                _ => {
                    let (out, f) = function(name, value, &column)?;
                    if inner_vars.contains(&out) || functions.iter().any(|(v, _)| *v == out) {
                        return Err(bad(format!(
                            "?{} is bound twice: give each function its own variable",
                            out.as_str()
                        )));
                    }
                    functions.push((out, f));
                }
            }
        }
        if functions.is_empty() {
            return Err(bad("give at least one function, such as win:rowNumber ?n"));
        }

        let query = spargebra::Query::Select {
            dataset: None,
            pattern: if order.is_empty() {
                input.clone()
            } else {
                GraphPattern::OrderBy {
                    inner: Box::new(input.clone()),
                    expression: order
                        .iter()
                        .map(|&(c, desc)| {
                            let e = Expression::Variable(inner_vars[c].clone());
                            if desc {
                                OrderExpression::Desc(e)
                            } else {
                                OrderExpression::Asc(e)
                            }
                        })
                        .collect(),
                }
            },
            base_iri: None,
        };
        let QueryResults::Solutions(solutions) = self
            .store
            .execute_in(self.prepare(query), self.rows.as_ref())
            .map_err(|e| WindowError::Input(Box::new(e)))?
        else {
            return Err(bad("the input query must be a SELECT"));
        };
        let mut rows: Vec<Vec<Option<Term>>> = Vec::new();
        for s in solutions {
            let s = s.map_err(|e| WindowError::Input(Box::new(e)))?;
            rows.push(inner_vars.iter().map(|v| s.get(v).cloned()).collect());
        }

        // Partitions, in the order of their first row; members in order.
        let mut index: HashMap<Vec<Option<Term>>, usize> = HashMap::new();
        let mut partitions: Vec<Vec<usize>> = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            let key: Vec<Option<Term>> = partition.iter().map(|&c| row[c].clone()).collect();
            let p = *index.entry(key).or_insert_with(|| {
                partitions.push(Vec::new());
                partitions.len() - 1
            });
            partitions[p].push(i);
        }

        let mut out: Vec<Vec<Option<Term>>> = vec![vec![None; functions.len()]; rows.len()];
        let peer_key = |i: usize| -> Vec<Option<&Term>> {
            order.iter().map(|&(c, _)| rows[i][c].as_ref()).collect()
        };
        for members in &partitions {
            let n = members.len();
            // Peer groups: [start, end) of each member's group.
            let mut group_start = vec![0; n];
            let mut group_end = vec![n; n];
            if !order.is_empty() {
                let mut start = 0;
                for k in 1..=n {
                    if k == n || peer_key(members[k]) != peer_key(members[start]) {
                        for g in start..k {
                            group_start[g] = start;
                            group_end[g] = k;
                        }
                        start = k;
                    }
                }
            }
            // Each member's frame, [lo, hi] inclusive (hi < lo: empty).
            let frames: Vec<(usize, isize)> = (0..n)
                .map(|k| match rows_frame {
                    Some((s, e)) => {
                        let lo = match s {
                            Bound::Unbounded => 0,
                            Bound::Offset(o) => (k as i64 + o).clamp(0, n as i64) as usize,
                        };
                        let hi = match e {
                            Bound::Unbounded => n as isize - 1,
                            Bound::Offset(o) => (k as i64 + o).clamp(-1, n as i64 - 1) as isize,
                        };
                        (lo, hi)
                    }
                    None if order.is_empty() => (0, n as isize - 1),
                    None => (0, group_end[k] as isize - 1),
                })
                .collect();
            for (f_i, (_, f)) in functions.iter().enumerate() {
                let values: Vec<Option<Term>> = match f {
                    Function::RowNumber => (0..n).map(|k| Some(int(k as i64 + 1))).collect(),
                    Function::Rank => (0..n)
                        .map(|k| Some(int(group_start[k] as i64 + 1)))
                        .collect(),
                    Function::DenseRank => {
                        let mut rank = 0;
                        let mut last = usize::MAX;
                        (0..n)
                            .map(|k| {
                                if group_start[k] != last {
                                    rank += 1;
                                    last = group_start[k];
                                }
                                Some(int(rank))
                            })
                            .collect()
                    }
                    Function::PercentRank => (0..n)
                        .map(|k| {
                            let v = if n <= 1 {
                                0.0
                            } else {
                                group_start[k] as f64 / (n - 1) as f64
                            };
                            Some(double(v))
                        })
                        .collect(),
                    Function::CumeDist => (0..n)
                        .map(|k| Some(double(group_end[k] as f64 / n as f64)))
                        .collect(),
                    Function::Ntile(buckets) => {
                        let b = (*buckets as usize).min(n.max(1));
                        let (size, extra) = (n / b, n % b);
                        (0..n)
                            .map(|k| {
                                // The first `extra` buckets hold one more row.
                                let big = extra * (size + 1);
                                let bucket = if k < big {
                                    k / (size + 1)
                                } else {
                                    extra + (k - big) / size.max(1)
                                };
                                Some(int(bucket as i64 + 1))
                            })
                            .collect()
                    }
                    Function::Shift(c, offset, default) => (0..n)
                        .map(|k| {
                            let j = k as i64 + offset;
                            if (0..n as i64).contains(&j) {
                                rows[members[j as usize]][*c].clone()
                            } else {
                                default.clone()
                            }
                        })
                        .collect(),
                    Function::FirstValue(c) => frames
                        .iter()
                        .map(|&(lo, hi)| {
                            (hi >= lo as isize).then(|| rows[members[lo]][*c].clone())?
                        })
                        .collect(),
                    Function::LastValue(c) => frames
                        .iter()
                        .map(|&(lo, hi)| {
                            (hi >= lo as isize).then(|| rows[members[hi as usize]][*c].clone())?
                        })
                        .collect(),
                    Function::Sum(c) | Function::Avg(c) | Function::Count(c) => {
                        let column: Vec<Option<&Term>> =
                            members.iter().map(|&i| rows[i][*c].as_ref()).collect();
                        let sums = Prefix::new(&column);
                        frames
                            .iter()
                            .map(|&(lo, hi)| {
                                let hi = (hi + 1).max(lo as isize) as usize;
                                match f {
                                    Function::Count(_) => Some(int(sums.count(lo, hi) as i64)),
                                    Function::Sum(_) => sums.sum(lo, hi),
                                    _ => sums.avg(lo, hi),
                                }
                            })
                            .collect()
                    }
                    Function::Min(c) | Function::Max(c) => {
                        let max = matches!(f, Function::Max(_));
                        let column: Vec<Option<&Term>> =
                            members.iter().map(|&i| rows[i][*c].as_ref()).collect();
                        extremes(&column, &frames, max)
                    }
                };
                for (k, v) in values.into_iter().enumerate() {
                    out[members[k]][f_i] = v;
                }
            }
        }

        let mut variables = inner_vars.clone();
        variables.extend(functions.iter().map(|(v, _)| v.clone()));
        let variables: Arc<[Variable]> = variables.into();
        let solutions: Vec<QuerySolution> = rows
            .into_iter()
            .zip(out)
            .map(|(mut row, extra)| {
                row.extend(extra);
                QuerySolution::from((Arc::clone(&variables), row))
            })
            .collect();
        Ok(QuerySolutionIter::new(
            variables,
            solutions.into_iter().map(Ok::<_, QueryEvaluationError>),
        ))
    }
}

/// The variables a sub-select projects.
pub(crate) fn projected(pattern: &GraphPattern) -> Vec<Variable> {
    match pattern {
        GraphPattern::Project { variables, .. } => variables.clone(),
        GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::OrderBy { inner, .. } => projected(inner),
        other => {
            let mut vars = Vec::new();
            other.on_in_scope_variable(|v| {
                if !vars.contains(v) {
                    vars.push(v.clone());
                }
            });
            vars
        }
    }
}

/// The triples and sub-selects of a SERVICE pattern of `service` (for messages).
pub(crate) fn collect(
    pattern: &GraphPattern,
    triples: &mut Vec<(TermPattern, String, TermPattern)>,
    subselects: &mut Vec<GraphPattern>,
    service: &str,
) -> Result<(), String> {
    match pattern {
        GraphPattern::Bgp { patterns } => {
            for t in patterns {
                let NamedNodePattern::NamedNode(p) = &t.predicate else {
                    return Err("variable predicates are not allowed".to_owned());
                };
                triples.push((t.subject.clone(), p.as_str().to_owned(), t.object.clone()));
            }
            Ok(())
        }
        GraphPattern::Join { left, right } => {
            collect(left, triples, subselects, service)?;
            collect(right, triples, subselects, service)
        }
        GraphPattern::Project { .. }
        | GraphPattern::Distinct { .. }
        | GraphPattern::Reduced { .. }
        | GraphPattern::Slice { .. }
        | GraphPattern::OrderBy { .. } => {
            subselects.push(pattern.clone());
            Ok(())
        }
        other => Err(format!(
            "unsupported pattern in the {service} SERVICE: {other}"
        )),
    }
}

/// The parameters in namespace `ns` (written `prefix:`) in the order given, RDF lists read.
pub(crate) fn parameters(
    triples: &[(TermPattern, String, TermPattern)],
    ns: &str,
    prefix: &str,
) -> Result<Vec<(String, Value)>, String> {
    // The list cells: blank node -> (first, rest).
    let mut first: HashMap<&TermPattern, &TermPattern> = HashMap::new();
    let mut rest: HashMap<&TermPattern, &TermPattern> = HashMap::new();
    for (s, p, o) in triples {
        match p.as_str() {
            RDF_FIRST => {
                first.insert(s, o);
            }
            RDF_REST => {
                rest.insert(s, o);
            }
            _ => {}
        }
    }
    let mut params = Vec::new();
    for (_, p, o) in triples {
        if p == RDF_FIRST || p == RDF_REST {
            continue;
        }
        let name = p.strip_prefix(ns).ok_or_else(|| {
            format!("unknown predicate <{p}>: use the {prefix}: namespace <{ns}>")
        })?;
        let value = if first.contains_key(o) {
            let mut items = Vec::new();
            let mut cell = o;
            loop {
                if items.len() > 16 {
                    return Err(format!("{prefix}:{name}: the list is too long"));
                }
                let Some(item) = first.get(cell) else {
                    return Err(format!("{prefix}:{name}: a malformed list"));
                };
                items.push((*item).clone());
                match rest.get(cell) {
                    Some(TermPattern::NamedNode(n)) if n.as_str() == RDF_NIL => break,
                    Some(next) => cell = next,
                    None => return Err(format!("{prefix}:{name}: a malformed list")),
                }
            }
            Value::List(items)
        } else {
            Value::One(o.clone())
        };
        params.push((name.to_owned(), value));
    }
    Ok(params)
}

fn bound(t: &TermPattern) -> Result<Bound, WindowError> {
    match t {
        TermPattern::Literal(l) if l.value() == "unbounded" => Ok(Bound::Unbounded),
        TermPattern::Literal(l) => l.value().parse().map(Bound::Offset).map_err(|_| {
            bad(format!(
                "win:rows: {l} is neither an integer nor \"unbounded\""
            ))
        }),
        _ => Err(bad("win:rows takes integers or \"unbounded\"")),
    }
}

/// A function triple: its output variable and what to compute.
fn function(
    name: &str,
    value: &Value,
    column: &dyn Fn(&Variable) -> Result<usize, WindowError>,
) -> Result<(Variable, Function), WindowError> {
    let items: Vec<&TermPattern> = match value {
        Value::One(t) => vec![t],
        Value::List(items) => items.iter().collect(),
    };
    let Some((TermPattern::Variable(out), args)) = items.split_last() else {
        return Err(bad(format!("win:{name} ends with the variable it binds")));
    };
    let arg_column = |i: usize| -> Result<usize, WindowError> {
        match args.get(i) {
            Some(TermPattern::Variable(v)) => column(v),
            _ => Err(bad(format!("win:{name} takes (?value … ?result)"))),
        }
    };
    let integer = |t: &TermPattern| -> Result<i64, WindowError> {
        match t {
            TermPattern::Literal(l) => l
                .value()
                .parse()
                .map_err(|_| bad(format!("win:{name}: {l} is not an integer"))),
            _ => Err(bad(format!("win:{name} takes an integer"))),
        }
    };
    let none = |f: Function| -> Result<Function, WindowError> {
        if args.is_empty() {
            Ok(f)
        } else {
            Err(bad(format!("win:{name} takes only the variable it binds")))
        }
    };
    let one = |f: fn(usize) -> Function| -> Result<Function, WindowError> {
        if args.len() == 1 {
            Ok(f(arg_column(0)?))
        } else {
            Err(bad(format!("win:{name} takes (?value ?result)")))
        }
    };
    let f = match name {
        "rowNumber" => none(Function::RowNumber)?,
        "rank" => none(Function::Rank)?,
        "denseRank" => none(Function::DenseRank)?,
        "percentRank" => none(Function::PercentRank)?,
        "cumeDist" => none(Function::CumeDist)?,
        "ntile" => match args {
            [n] => {
                let n = integer(n)?;
                if n < 1 {
                    return Err(bad("win:ntile takes a positive number of buckets"));
                }
                Function::Ntile(n as u64)
            }
            _ => return Err(bad("win:ntile takes (buckets ?result)")),
        },
        "lag" | "lead" => {
            let sign = if name == "lag" { -1 } else { 1 };
            let c = arg_column(0)?;
            let offset = args.get(1).map(|t| integer(t)).transpose()?.unwrap_or(1);
            if !(0..=1_000_000).contains(&offset) {
                return Err(bad(format!(
                    "win:{name}: the offset must be 0 to 1,000,000"
                )));
            }
            let default = match args.get(2) {
                None => None,
                Some(TermPattern::Literal(l)) => Some(l.clone().into()),
                Some(TermPattern::NamedNode(n)) => Some(n.clone().into()),
                Some(_) => return Err(bad(format!("win:{name}: the default must be a constant"))),
            };
            if args.len() > 3 {
                return Err(bad(format!(
                    "win:{name} takes (?value [offset [default]] ?result)"
                )));
            }
            Function::Shift(c, sign * offset, default)
        }
        "firstValue" => one(Function::FirstValue)?,
        "lastValue" => one(Function::LastValue)?,
        "sum" => one(Function::Sum)?,
        "avg" => one(Function::Avg)?,
        "min" => one(Function::Min)?,
        "max" => one(Function::Max)?,
        "count" => one(Function::Count)?,
        other => return Err(bad(format!("unknown function win:{other}"))),
    };
    Ok((out.clone(), f))
}

fn int(v: i64) -> Term {
    Literal::from(v).into()
}

fn double(v: f64) -> Term {
    Literal::from(v).into()
}

/// Prefix sums of a column, for sums, averages and counts over any frame. Floating-point sums
/// come from a segment tree instead, which adds only the frame's own values: a difference of
/// prefix sums would lose small values next to large ones (`1e20 + 1 - 1e20` is 0) and turn
/// every frame after an infinity or NaN into NaN.
struct Prefix {
    /// Exact sums (`None` past an overflow).
    exact: Vec<Option<Decimal>>,
    float: SumTree,
    /// Bound values, values that are not numbers, non-integers, floats, doubles.
    count: Vec<usize>,
    bad: Vec<usize>,
    fractional: Vec<usize>,
    floats: Vec<usize>,
    doubles: Vec<usize>,
}

impl Prefix {
    fn new(column: &[Option<&Term>]) -> Self {
        let n = column.len();
        let mut p = Self {
            exact: Vec::with_capacity(n + 1),
            float: SumTree::default(),
            count: Vec::with_capacity(n + 1),
            bad: Vec::with_capacity(n + 1),
            fractional: Vec::with_capacity(n + 1),
            floats: Vec::with_capacity(n + 1),
            doubles: Vec::with_capacity(n + 1),
        };
        let zero = Decimal::from(0);
        let mut e = Some(zero);
        let mut values = Vec::with_capacity(n);
        let (mut c, mut b, mut fr, mut fl, mut d) = (0, 0, 0, 0, 0);
        let push = |p: &mut Self, e, c, b, fr, fl, d| {
            p.exact.push(e);
            p.count.push(c);
            p.bad.push(b);
            p.fractional.push(fr);
            p.floats.push(fl);
            p.doubles.push(d);
        };
        push(&mut p, e, c, b, fr, fl, d);
        for t in column {
            let mut f = 0.0;
            if let Some(t) = t {
                c += 1;
                match statagg::number(t) {
                    Some(Number::Exact(x, integer)) => {
                        e = e.and_then(|e| e.checked_add(x));
                        f = statagg::as_f64(Number::Exact(x, integer));
                        if !integer {
                            fr += 1;
                        }
                    }
                    Some(Number::Float(x)) => {
                        f = x;
                        fl += 1;
                    }
                    Some(Number::Double(x)) => {
                        f = x;
                        d += 1;
                    }
                    None => b += 1,
                }
            }
            values.push(f);
            push(&mut p, e, c, b, fr, fl, d);
        }
        p.float = SumTree::new(values);
        p
    }

    fn count(&self, lo: usize, hi: usize) -> usize {
        self.count[hi] - self.count[lo]
    }

    /// The sum of `[lo, hi)`, or `None` when it holds a non-number or overflows.
    fn total(&self, lo: usize, hi: usize) -> Option<(Number, u8)> {
        if self.bad[hi] != self.bad[lo] {
            return None;
        }
        if self.doubles[hi] != self.doubles[lo] {
            return Some((Number::Double(self.float.sum(lo, hi)), 3));
        }
        if self.floats[hi] != self.floats[lo] {
            return Some((Number::Float(self.float.sum(lo, hi)), 2));
        }
        let sum = self.exact[hi]?.checked_sub(self.exact[lo]?)?;
        let integer = self.fractional[hi] == self.fractional[lo];
        Some((Number::Exact(sum, integer), 0))
    }

    fn sum(&self, lo: usize, hi: usize) -> Option<Term> {
        Some(match self.total(lo, hi)? {
            (Number::Exact(d, true), _) => Literal::new_typed_literal(
                oxsdatatypes::Integer::try_from(d).ok()?.to_string(),
                xsd::INTEGER,
            )
            .into(),
            (Number::Exact(d, false), _) => statagg::decimal_term(d),
            (n, level) => statagg::float_term(statagg::as_f64(n), level),
        })
    }

    fn avg(&self, lo: usize, hi: usize) -> Option<Term> {
        let count = self.count(lo, hi);
        if count == 0 {
            return None;
        }
        Some(match self.total(lo, hi)? {
            // As the AVG aggregate (#8): Jena's decimal division, exact or 24 fraction digits.
            (Number::Exact(d, _), _) => {
                spareval::decimal_divide(d, Decimal::from(i64::try_from(count).ok()?))?.into()
            }
            (n, level) => statagg::float_term(statagg::as_f64(n) / count as f64, level),
        })
    }
}

/// Sums of `f64` ranges in O(log n) that add only the range's values.
#[derive(Default)]
struct SumTree {
    n: usize,
    /// Leaves at `n..2n`, each inner node the sum of its two children.
    nodes: Vec<f64>,
}

impl SumTree {
    fn new(values: Vec<f64>) -> Self {
        let n = values.len();
        let mut nodes = vec![0.0; n];
        nodes.extend(values);
        for i in (1..n).rev() {
            nodes[i] = nodes[2 * i] + nodes[2 * i + 1];
        }
        Self { n, nodes }
    }

    /// The sum of `[lo, hi)`.
    fn sum(&self, lo: usize, hi: usize) -> f64 {
        let (mut lo, mut hi) = (lo + self.n, hi + self.n);
        let (mut left, mut right) = (0.0, 0.0);
        while lo < hi {
            if lo & 1 == 1 {
                left += self.nodes[lo];
                lo += 1;
            }
            if hi & 1 == 1 {
                hi -= 1;
                right += self.nodes[hi];
            }
            lo /= 2;
            hi /= 2;
        }
        left + right
    }
}

fn compare(a: Number, b: Number) -> Ordering {
    match (a, b) {
        (Number::Exact(x, _), Number::Exact(y, _)) => x.cmp(&y),
        _ => statagg::as_f64(a).total_cmp(&statagg::as_f64(b)),
    }
}

/// The minimum or maximum of each frame, with a monotonic queue: the frames only move
/// forward, so this is linear. Of equal values the first counts.
fn extremes(column: &[Option<&Term>], frames: &[(usize, isize)], max: bool) -> Vec<Option<Term>> {
    // Per row: None unbound, Some(None) not a number, Some(Some(n)) a number.
    let values: Vec<Option<Option<Number>>> =
        column.iter().map(|t| t.map(statagg::number)).collect();
    let mut bad = vec![0usize; values.len() + 1];
    for (k, v) in values.iter().enumerate() {
        bad[k + 1] = bad[k] + usize::from(matches!(v, Some(None)));
    }
    let number = |k: usize| values[k].flatten().expect("a number");
    // `a` beats `b` strictly.
    let beats = |a: Number, b: Number| {
        let o = compare(a, b);
        if max {
            o == Ordering::Greater
        } else {
            o == Ordering::Less
        }
    };
    let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    let mut next = 0;
    frames
        .iter()
        .map(|&(lo, hi)| {
            if hi < lo as isize {
                return None;
            }
            let hi = hi as usize;
            while next <= hi {
                if let Some(Some(n)) = values[next] {
                    while queue.back().is_some_and(|&b| beats(n, number(b))) {
                        queue.pop_back();
                    }
                    queue.push_back(next);
                }
                next += 1;
            }
            while queue.front().is_some_and(|&f| f < lo) {
                queue.pop_front();
            }
            if bad[hi + 1] != bad[lo] {
                return None;
            }
            queue.front().and_then(|&k| column[k].cloned())
        })
        .collect()
}
