//! `COUNT` over a whole triple pattern, answered from the counts kept with the quad indexes
//! (issue #166).
//!
//! `SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }` and its variants (`COUNT(DISTINCT ?s)`,
//! `COUNT(?o)` with `FILTER isLiteral(?o)`, ...) would otherwise read and count every quad of
//! the default graph one row at a time. The tripletrail store keeps, per index permutation, how
//! many quads it holds and how many distinct values each position has, per graph and per term
//! kind (see `tripletrail_store::Snapshot::count_triples`). This pass replaces such a `GROUP`
//! with no keys by a one-row `VALUES` block of the counts, so what is around it (`BIND`,
//! `HAVING`, the projection) is evaluated as usual.
//!
//! It covers the default graph only (patterns inside `GRAPH` and queries with a dataset are
//! left alone), and does nothing when a count is not available: a permutation with writes not
//! yet folded into its segment, or a combination the counts don't keep.

use crate::HashMap;
use oxigraph::model::{Literal, Term};
use oxigraph::sparql::Variable;
use spargebra::Query;
use spargebra::algebra::{
    AggregateExpression, AggregateFunction, Expression, Function, GraphPattern, OrderExpression,
};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern};
use tripletrail_store::{BgpCount, GraphSelector, Kind, Snapshot, TermId, TripleCount};

const LITERALS: [Kind; 9] = [
    Kind::String,
    Kind::LangString,
    Kind::Typed,
    Kind::Geometry,
    Kind::Integer,
    Kind::Boolean,
    Kind::Double,
    Kind::Decimal,
    Kind::DateTime,
];

/// Groups a `GROUP BY` on ids may produce: more are left to the evaluator.
const MAX_GROUPS: usize = 1 << 20;

/// The most groups of `width` keys and `n` counts a GROUP BY on ids may hold under the query
/// memory limit (#874), or no bound without one: each holds its keys and counts, and about as
/// much again once read back. Past it the query is left to the evaluator, whose GROUP BY counts
/// its groups against the same limit.
fn group_cap(width: usize, n: usize) -> usize {
    tripletrail_store::memory_limit().map_or(usize::MAX, |limit| limit / (2 * (width + n) * 8 + 32))
}

/// Distinct values eager aggregation may turn into a `VALUES` block (each is decoded once).
const MAX_EAGER_GROUPS: usize = 1 << 18;

/// Adds the variables `e` reads to `vars`; `false` if its value may differ between two rows
/// with the same values (`RAND()`, `NOW()`, `BNODE()`, `UUID()`, `EXISTS`).
/// From this many groups on, grouped results are read back from an id table (see
/// `Pass::grouped_counts`); fewer go into a `VALUES`, cheaper for the evaluator.
const READ_BACK_GROUPS: usize = 1024;

/// The value of one aggregate of one group.
enum Value {
    Id(TermId),
    Literal(Literal),
}

impl Value {
    /// A literal with an id (an inline number, or one in the store) as that id.
    fn literal(snapshot: &Snapshot, l: Literal) -> Self {
        match snapshot.term_id(l.as_ref().into()) {
            Some(id) => Self::Id(id),
            None => Self::Literal(l),
        }
    }
}

/// A sort key of grouped results: a key column or a count.
#[derive(Clone, Copy)]
enum By {
    Key(usize),
    Count(usize),
}

/// The first `k` of the groups `order` under the sort keys `by` (each with descending or not),
/// in no particular order; with `complete` false, `by` is only the first keys of the order,
/// and the groups that tie with the k-th on them are kept too (a later key decides). Keys order as `ORDER BY` orders terms ([`crate::idtree::Pick`]).
/// `None` when a sort key is neither a key nor a count, or a key holds values that can't be
/// ordered on ids: then every group is kept.
fn top_groups(
    pick: &mut crate::idtree::Pick<'_>,
    order: &[usize],
    k: usize,
    mut complete: bool,
    by: impl Iterator<Item = Option<(By, bool)>>,
    key: impl Fn(usize, usize) -> TermId,
    count: impl Fn(usize, usize) -> u64,
) -> Option<Vec<usize>> {
    // The keys up to the first that is neither a key nor a count: the later ones only decide
    // between groups that tie on these (#657).
    let all: Vec<Option<(By, bool)>> = by.collect();
    let mut by: Vec<(By, bool)> = all.iter().map_while(|b| *b).collect();
    complete &= by.len() == all.len();
    if k == 0 {
        return Some(Vec::new());
    }
    let mut candidates = order.to_vec();
    // A leading count first narrows the groups to those at least as far up as the k-th one,
    // so only their keys are ranked.
    if let Some(&(By::Count(i), desc)) = by.first() {
        let mut values: Vec<u64> = candidates.iter().map(|&g| count(g, i)).collect();
        let at = if desc { values.len() - k } else { k - 1 };
        let (_, &mut bound, _) = values.select_nth_unstable(at);
        candidates.retain(|&g| {
            let c = count(g, i);
            if desc { c >= bound } else { c <= bound }
        });
    }
    // The rank of each value of each key column used, in the order of terms. Ranks rely on
    // `Pick::order` never calling two different ids of one class equal (1 and "01" as
    // numbers): a later sort key would then prune between values that tie in `ORDER BY`.
    let mut ranks: Vec<HashMap<TermId, u32>> = Vec::new();
    let mut slot = Vec::with_capacity(by.len());
    for j in 0..by.len() {
        let By::Key(c) = by[j].0 else {
            slot.push(usize::MAX);
            continue;
        };
        let mut ids: Vec<TermId> = candidates.iter().map(|&g| key(g, c)).collect();
        ids.sort_unstable();
        ids.dedup();
        // Only values `pick` orders, and literals of one class (a sort must not meet a pair it
        // can't order).
        let mut literal_class = None;
        let mut failed = false;
        for &id in &ids {
            if id == TermId(0) {
                continue;
            }
            match pick.order_class(id) {
                None => failed = true,
                Some(0) => {}
                Some(class) if *literal_class.get_or_insert(class) != class => failed = true,
                Some(_) => {}
            }
            if failed {
                break;
            }
        }
        if !failed {
            ids.sort_by(|&a, &b| {
                pick.order(a, b).unwrap_or_else(|| {
                    failed = true;
                    std::cmp::Ordering::Equal
                })
            });
        }
        if failed {
            // Values this key can't order on ids: the keys before it decide, with ties kept.
            by.truncate(j);
            complete = false;
            break;
        }
        slot.push(ranks.len());
        ranks.push(
            ids.into_iter()
                .enumerate()
                .map(|(r, id)| (id, r as u32))
                .collect(),
        );
    }
    if by.is_empty() {
        return None;
    }
    let rank = |g: usize, j: usize| -> u64 {
        match by[j].0 {
            By::Count(i) => count(g, i),
            By::Key(c) => u64::from(ranks[slot[j]][&key(g, c)]),
        }
    };
    let cmp = |a: &usize, b: &usize| {
        (0..by.len())
            .map(|j| {
                let o = rank(*a, j).cmp(&rank(*b, j));
                if by[j].1 { o.reverse() } else { o }
            })
            .find(|o| o.is_ne())
            .unwrap_or(std::cmp::Ordering::Equal)
    };
    if candidates.len() > k {
        candidates.select_nth_unstable_by(k - 1, cmp);
        if complete {
            candidates.truncate(k);
        } else {
            let kth = candidates[k - 1];
            candidates.retain(|g| cmp(g, &kth) != std::cmp::Ordering::Greater);
        }
    }
    Some(candidates)
}

fn deterministic_vars<'a>(e: &'a Expression, vars: &mut Vec<&'a Variable>) -> bool {
    use Expression as E;
    match e {
        E::NamedNode(_) | E::Literal(_) => true,
        E::Variable(v) | E::Bound(v) => {
            vars.push(v);
            true
        }
        E::Or(a, b)
        | E::And(a, b)
        | E::Equal(a, b)
        | E::SameTerm(a, b)
        | E::Greater(a, b)
        | E::GreaterOrEqual(a, b)
        | E::Less(a, b)
        | E::LessOrEqual(a, b)
        | E::Add(a, b)
        | E::Subtract(a, b)
        | E::Multiply(a, b)
        | E::Divide(a, b) => deterministic_vars(a, vars) && deterministic_vars(b, vars),
        E::UnaryPlus(a) | E::UnaryMinus(a) | E::Not(a) => deterministic_vars(a, vars),
        E::In(a, list) => {
            deterministic_vars(a, vars) && list.iter().all(|x| deterministic_vars(x, vars))
        }
        E::If(a, b, c) => {
            deterministic_vars(a, vars)
                && deterministic_vars(b, vars)
                && deterministic_vars(c, vars)
        }
        E::Coalesce(list) => list.iter().all(|x| deterministic_vars(x, vars)),
        E::FunctionCall(f, args) => {
            !matches!(
                f,
                Function::Rand
                    | Function::Now
                    | Function::Uuid
                    | Function::StrUuid
                    | Function::BNode
            ) && args.iter().all(|x| deterministic_vars(x, vars))
        }
        E::Exists(_) => false,
    }
}

/// The kinds a filter of kind tests of `v` keeps (`isIRI(?v)`, `isBlank(?v)`,
/// `isLiteral(?v)`, and `||` of them).
fn kind_test(e: &Expression, v: &Variable) -> Option<Vec<Kind>> {
    match e {
        Expression::Or(a, b) => {
            let mut k = kind_test(a, v)?;
            k.extend(kind_test(b, v)?);
            Some(k)
        }
        Expression::FunctionCall(f, args) if matches!(args.as_slice(), [Expression::Variable(x)] if x == v) => {
            match f {
                Function::IsIri => Some(vec![Kind::Iri]),
                Function::IsBlank => Some(vec![Kind::Blank]),
                Function::IsLiteral => Some(LITERALS.to_vec()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether `e` reads `v` only through `LANG`, `DATATYPE`, `isLiteral`, `isIRI`, `isBlank`
/// and `BOUND`: what the kind and tag of its values decide (see [`tag_classes`]).
fn tag_only(e: &Expression, v: &Variable) -> bool {
    use Expression as E;
    match e {
        E::NamedNode(_) | E::Literal(_) | E::Bound(_) => true,
        E::Variable(x) => x != v,
        E::Or(a, b)
        | E::And(a, b)
        | E::Equal(a, b)
        | E::SameTerm(a, b)
        | E::Greater(a, b)
        | E::GreaterOrEqual(a, b)
        | E::Less(a, b)
        | E::LessOrEqual(a, b)
        | E::Add(a, b)
        | E::Subtract(a, b)
        | E::Multiply(a, b)
        | E::Divide(a, b) => tag_only(a, v) && tag_only(b, v),
        E::UnaryPlus(a) | E::UnaryMinus(a) | E::Not(a) => tag_only(a, v),
        E::In(a, list) => tag_only(a, v) && list.iter().all(|x| tag_only(x, v)),
        E::If(a, b, c) => tag_only(a, v) && tag_only(b, v) && tag_only(c, v),
        E::Coalesce(list) => list.iter().all(|x| tag_only(x, v)),
        E::FunctionCall(
            Function::Lang
            | Function::Datatype
            | Function::IsLiteral
            | Function::IsIri
            | Function::IsBlank,
            args,
        ) if matches!(args.as_slice(), [E::Variable(x)] if x == v) => true,
        E::FunctionCall(_, args) => args.iter().all(|x| tag_only(x, v)),
        E::Exists(_) => false,
    }
}

/// The value that stands for the blank nodes of a variable [`tag_classes`] grouped.
const BLANK_CLASS: &str = "urn:x-tripletrail:blank-node-class";

/// `e` with `isBlank(?v)` and `isIRI(?v)` reading [`BLANK_CLASS`] as a blank node.
fn blank_class(e: &Expression, v: Option<&Variable>) -> Expression {
    use Expression as E;
    let Some(v) = v else { return e.clone() };
    let class = || {
        E::SameTerm(
            Box::new(E::Variable(v.clone())),
            Box::new(E::NamedNode(oxigraph::model::NamedNode::new_unchecked(
                BLANK_CLASS,
            ))),
        )
    };
    let b = |x: &E| Box::new(blank_class(x, Some(v)));
    match e {
        E::FunctionCall(Function::IsBlank, args) if matches!(args.as_slice(), [E::Variable(x)] if x == v) => {
            class()
        }
        E::FunctionCall(Function::IsIri, args) if matches!(args.as_slice(), [E::Variable(x)] if x == v) => {
            E::And(Box::new(e.clone()), Box::new(E::Not(Box::new(class()))))
        }
        E::Or(x, y) => E::Or(b(x), b(y)),
        E::And(x, y) => E::And(b(x), b(y)),
        E::Equal(x, y) => E::Equal(b(x), b(y)),
        E::SameTerm(x, y) => E::SameTerm(b(x), b(y)),
        E::Greater(x, y) => E::Greater(b(x), b(y)),
        E::GreaterOrEqual(x, y) => E::GreaterOrEqual(b(x), b(y)),
        E::Less(x, y) => E::Less(b(x), b(y)),
        E::LessOrEqual(x, y) => E::LessOrEqual(b(x), b(y)),
        E::Add(x, y) => E::Add(b(x), b(y)),
        E::Subtract(x, y) => E::Subtract(b(x), b(y)),
        E::Multiply(x, y) => E::Multiply(b(x), b(y)),
        E::Divide(x, y) => E::Divide(b(x), b(y)),
        E::UnaryPlus(x) => E::UnaryPlus(b(x)),
        E::UnaryMinus(x) => E::UnaryMinus(b(x)),
        E::Not(x) => E::Not(b(x)),
        E::In(x, list) => E::In(b(x), list.iter().map(|y| blank_class(y, Some(v))).collect()),
        E::If(x, y, z) => E::If(b(x), b(y), b(z)),
        E::Coalesce(list) => E::Coalesce(list.iter().map(|y| blank_class(y, Some(v))).collect()),
        E::FunctionCall(f, args) => E::FunctionCall(
            f.clone(),
            args.iter().map(|y| blank_class(y, Some(v))).collect(),
        ),
        _ => e.clone(),
    }
}

/// `pattern` under the `BIND`s and `FILTER`s of `chain` (outermost first), as [`blank_class`]
/// rewrites them.
fn wrap(
    mut pattern: GraphPattern,
    chain: &[&GraphPattern],
    tagged: Option<&Variable>,
) -> GraphPattern {
    for step in chain.iter().rev() {
        pattern = match step {
            GraphPattern::Extend {
                variable,
                expression,
                ..
            } => GraphPattern::Extend {
                inner: Box::new(pattern),
                variable: variable.clone(),
                expression: blank_class(expression, tagged),
            },
            GraphPattern::Filter { expr, .. } => GraphPattern::Filter {
                expr: blank_class(expr, tagged),
                inner: Box::new(pattern),
            },
            _ => unreachable!(),
        };
    }
    pattern
}

/// Whether `e` calls a function outside SPARQL 1.1, which only the engine knows.
fn has_custom(e: &Expression) -> bool {
    use Expression as E;
    match e {
        E::NamedNode(_) | E::Literal(_) | E::Variable(_) | E::Bound(_) => false,
        E::Or(a, b)
        | E::And(a, b)
        | E::Equal(a, b)
        | E::SameTerm(a, b)
        | E::Greater(a, b)
        | E::GreaterOrEqual(a, b)
        | E::Less(a, b)
        | E::LessOrEqual(a, b)
        | E::Add(a, b)
        | E::Subtract(a, b)
        | E::Multiply(a, b)
        | E::Divide(a, b) => has_custom(a) || has_custom(b),
        E::UnaryPlus(a) | E::UnaryMinus(a) | E::Not(a) => has_custom(a),
        E::In(a, list) => has_custom(a) || list.iter().any(has_custom),
        E::If(a, b, c) => has_custom(a) || has_custom(b) || has_custom(c),
        E::Coalesce(list) => list.iter().any(has_custom),
        E::FunctionCall(Function::Custom(_), _) | E::Exists(_) => true,
        E::FunctionCall(_, args) => args.iter().any(has_custom),
    }
}

/// Replaces the values of column `v` by one value per class of values with the same kind and
/// tag (language or datatype): what `LANG`, `DATATYPE`, `isLiteral`, `isIRI` and `isBlank`
/// give the same answer for. Numeric literals keep their own values: the evaluator reads
/// their datatype from their lexical form. `None` for values a `VALUES` block cannot hold.
fn tag_classes(snapshot: &Snapshot, t: &mut crate::idtree::Table, v: &Variable) -> Option<()> {
    let c = t.col(v)?;
    let width = t.vars.len();
    let column: Vec<TermId> = (0..t.len()).map(|r| t.ids[r * width + c]).collect();
    let mut tag_of: Vec<u32> = vec![u32::MAX; column.len()];
    let mut tags: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut numeric: Vec<bool> = Vec::new();
    snapshot
        .literal_tags(&column, &mut |i, tag| {
            let next = tags.len() as u32;
            let n = *tags.entry(tag.to_vec()).or_insert_with(|| {
                numeric.push(std::str::from_utf8(tag).is_ok_and(|d| {
                    crate::idtree::is_numeric_datatype(
                        oxigraph::model::NamedNodeRef::new_unchecked(d),
                    )
                }));
                next
            });
            tag_of[i] = n;
        })
        .ok()?;
    let mut classes: HashMap<(u64, u32), TermId> = HashMap::new();
    for (i, &id) in column.iter().enumerate() {
        if id == TermId(0) {
            continue;
        }
        let kind = id.kind();
        match kind {
            Kind::Iri
            | Kind::Blank
            | Kind::String
            | Kind::LangString
            | Kind::Geometry
            | Kind::Integer
            | Kind::Boolean
            // One datatype per kind.
            | Kind::Double
            | Kind::Decimal => {}
            Kind::Typed if tag_of[i] != u32::MAX && !numeric[tag_of[i] as usize] => {}
            Kind::Typed => continue,
            // `xsd:date` and `xsd:dateTime` share the kind.
            Kind::DateTime => {
                let date = id.date_time_value()?.date;
                let class = *classes.entry((kind as u64, u32::from(date))).or_insert(id);
                t.ids[i * width + c] = class;
                continue;
            }
            _ => return None,
        }
        let class = *classes.entry((kind as u64, tag_of[i])).or_insert(id);
        t.ids[i * width + c] = class;
    }
    Some(())
}

/// Each aggregate as (counted variable, distinct), if all are `COUNT(*)` or `COUNT(?v)`.
fn count_aggregates(
    aggregates: &[(Variable, AggregateExpression)],
) -> Option<Vec<(Option<&Variable>, bool)>> {
    aggregates
        .iter()
        .map(|(_, a)| match a {
            AggregateExpression::CountSolutions { distinct } => Some((None, *distinct)),
            AggregateExpression::FunctionCall {
                name: AggregateFunction::Count,
                expr: Expression::Variable(v),
                distinct,
            } => Some((Some(v), *distinct)),
            _ => None,
        })
        .collect()
}

/// An aggregate a group of an id table gives on ids.
enum Agg<'a> {
    /// As [`count_aggregates`].
    Count(Option<&'a Variable>, bool),
    Min(&'a Variable),
    Max(&'a Variable),
    Sample(&'a Variable),
    /// `SUM` of numbers on ids ([`crate::idvalue`]), of the distinct values if `true`.
    Sum(&'a Expression, bool),
    /// `AVG` of numbers on ids (#62), of the distinct values if `true`.
    Avg(&'a Expression, bool),
}

/// Each aggregate as an [`Agg`], if all are counts, or `MIN`, `MAX` or `SAMPLE` of a variable.
fn id_aggregates(aggregates: &[(Variable, AggregateExpression)]) -> Option<Vec<Agg<'_>>> {
    aggregates
        .iter()
        .map(|(_, a)| match a {
            AggregateExpression::CountSolutions { distinct } => Some(Agg::Count(None, *distinct)),
            AggregateExpression::FunctionCall {
                name,
                expr: e @ Expression::Variable(v),
                distinct,
            } => match name {
                AggregateFunction::Count => Some(Agg::Count(Some(v), *distinct)),
                // DISTINCT changes neither.
                AggregateFunction::Min => Some(Agg::Min(v)),
                AggregateFunction::Max => Some(Agg::Max(v)),
                AggregateFunction::Sample => Some(Agg::Sample(v)),
                AggregateFunction::Sum => Some(Agg::Sum(e, *distinct)),
                AggregateFunction::Avg => Some(Agg::Avg(e, *distinct)),
                _ => None,
            },
            AggregateExpression::FunctionCall {
                name: AggregateFunction::Sum,
                expr,
                distinct,
            } if crate::idtree::value_expression(expr) => Some(Agg::Sum(expr, *distinct)),
            AggregateExpression::FunctionCall {
                name: AggregateFunction::Avg,
                expr,
                distinct,
            } if crate::idtree::value_expression(expr) => Some(Agg::Avg(expr, *distinct)),
            _ => None,
        })
        .collect()
}

/// Rewrites the `GROUP`s of `query` it can answer from the counts, and says how many.
pub(crate) fn rewrite(
    query: Query,
    snapshot: &Snapshot,
    graph: GraphSelector,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> (Query, usize) {
    // `#pragma tt.id-rewrites off` (#238).
    if crate::Hints::current().id_rewrites_off {
        return (query, 0);
    }
    let mut pass = Pass {
        snapshot,
        cancelled,
        graph,
        done: 0,
        top: None,
    };
    let query = match query {
        Query::Select {
            dataset: None,
            pattern,
            base_iri,
        } => Query::Select {
            dataset: None,
            pattern: pass.walk(pattern),
            base_iri,
        },
        Query::Ask {
            dataset: None,
            pattern,
            base_iri,
        } => Query::Ask {
            dataset: None,
            pattern: pass.walk(pattern),
            base_iri,
        },
        Query::Construct {
            template,
            dataset: None,
            pattern,
            base_iri,
        } => Query::Construct {
            template,
            dataset: None,
            pattern: pass.walk(pattern),
            base_iri,
        },
        other => other,
    };
    (query, pass.done)
}

struct Pass<'a> {
    snapshot: &'a Snapshot,
    cancelled: &'a (dyn Fn() -> bool + Sync),
    /// As [`crate::idtree::Eval::graph`].
    graph: GraphSelector,
    done: usize,
    /// The order (variable, descending) and the number of rows a `Slice` keeps of the
    /// pattern being walked, while only `Project`, `OrderBy` and `Extend` are in between.
    top: Option<Top>,
}

/// The order's keys that are variables, the rows a `Slice` keeps, and whether those keys are
/// the whole order.
type Top = (Vec<(Variable, bool)>, usize, bool);

/// The `ORDER BY` under `p`, through `Project` and `Extend`s that rename a variable, as the
/// variables of the `Group` below them: its keys up to the first that is not a variable
/// (`STR(?v)`, #657), and whether that was all of them. `None` when the first key is not a
/// variable.
fn top_order(p: &GraphPattern) -> Option<(Vec<(Variable, bool)>, bool)> {
    let mut p = p;
    let mut order: Option<&[OrderExpression]> = None;
    let mut renames: Vec<(&Variable, &Variable)> = Vec::new();
    loop {
        match p {
            GraphPattern::Project { inner, .. } => p = inner,
            GraphPattern::OrderBy { inner, expression } if order.is_none() => {
                order = Some(expression);
                p = inner;
            }
            GraphPattern::Extend {
                inner,
                variable,
                expression: Expression::Variable(x),
            } => {
                renames.push((variable, x));
                p = inner;
            }
            GraphPattern::Group { .. } => break,
            _ => return None,
        }
    }
    let order = order?;
    let keys: Vec<(Variable, bool)> = order
        .iter()
        .map_while(|o| {
            let (e, desc) = match o {
                OrderExpression::Asc(e) => (e, false),
                OrderExpression::Desc(e) => (e, true),
            };
            let Expression::Variable(v) = e else {
                return None;
            };
            let mut v = v;
            for (to, from) in &renames {
                if v == *to {
                    v = from;
                }
            }
            Some((v.clone(), desc))
        })
        .collect();
    let complete = keys.len() == order.len();
    (!keys.is_empty()).then_some((keys, complete))
}

/// The rows a group must have on average for [`Pass::eager_group`] to group first when the
/// kernels could compute the expressions on every row (#11).
const EAGER_ROWS_ON_IDS: u64 = 16;

impl Pass<'_> {
    fn walk(&mut self, p: GraphPattern) -> GraphPattern {
        use GraphPattern as G;
        let walk = |s: &mut Self, p: Box<GraphPattern>| Box::new(s.walk(*p));
        // The order and limit of a `Slice` reach the `Group` below it only through these.
        let top = self.top.take();
        if matches!(p, G::Project { .. } | G::OrderBy { .. } | G::Extend { .. }) {
            self.top = top.clone();
        }
        match p {
            G::Group {
                inner,
                variables,
                aggregates,
            } => {
                if variables.is_empty()
                    && let Some(row) = self
                        .counts(&inner, &aggregates)
                        .or_else(|| self.joined_counts(&inner, &aggregates))
                        .or_else(|| self.tree_counts(&inner, &aggregates))
                {
                    self.done += 1;
                    return G::Values {
                        variables: aggregates.into_iter().map(|(v, _)| v).collect(),
                        bindings: vec![
                            row.into_iter()
                                .map(|n| Some(GroundTerm::Literal(Literal::from(n))))
                                .collect(),
                        ],
                    };
                }
                // Over a `BIND`, counting the rows per distinct input first computes the
                // expression once per input value, not once per row: under the id executor
                // the grouped counts below would compute it for every row (#11).
                let computed = matches!(*inner, G::Extend { .. })
                    || matches!(&*inner, G::Filter { inner, .. } if matches!(**inner, G::Extend { .. }));
                // When the kernels compute the expressions on ids, a row costs little there:
                // grouping first pays only with many rows a group (a `DATATYPE` read as a
                // few classes), not with a `MONTH` of 21,000 distinct dates.
                let per_group = if crate::direct::covered(&inner) {
                    EAGER_ROWS_ON_IDS
                } else {
                    2
                };
                if computed
                    && crate::executor_ids()
                    && let Some(group) =
                        self.eager_group(&inner, &variables, &aggregates, per_group)
                {
                    self.done += 1;
                    return group;
                }
                // Without keys, counts are answered above; MIN, MAX and SAMPLE here.
                if (!variables.is_empty() || count_aggregates(&aggregates).is_none())
                    && let Some(values) =
                        self.grouped_counts(&inner, &variables, &aggregates, top.as_ref())
                {
                    self.done += 1;
                    return values;
                }
                if let Some(group) = self.eager_group(&inner, &variables, &aggregates, 2) {
                    self.done += 1;
                    return group;
                }
                G::Group {
                    inner: walk(self, inner),
                    variables,
                    aggregates,
                }
            }
            // The counts cover the default graph only.
            G::Graph { .. } | G::Service { .. } => p,
            // The right side sees the left side's bindings.
            G::Lateral { left, right } => G::Lateral {
                left: walk(self, left),
                right,
            },
            G::Join { left, right } => G::Join {
                left: walk(self, left),
                right: walk(self, right),
            },
            G::LeftJoin {
                left,
                right,
                expression,
            } => G::LeftJoin {
                left: walk(self, left),
                right: walk(self, right),
                expression,
            },
            G::Union { left, right } => G::Union {
                left: walk(self, left),
                right: walk(self, right),
            },
            G::Minus { left, right } => G::Minus {
                left: walk(self, left),
                right: walk(self, right),
            },
            G::Filter { expr, inner } => G::Filter {
                expr,
                inner: walk(self, inner),
            },
            G::Extend {
                inner,
                variable,
                expression,
            } => G::Extend {
                inner: walk(self, inner),
                variable,
                expression,
            },
            G::Project { inner, variables } => G::Project {
                inner: walk(self, inner),
                variables,
            },
            G::Distinct { inner } => G::Distinct {
                inner: walk(self, inner),
            },
            G::Reduced { inner } => G::Reduced {
                inner: walk(self, inner),
            },
            G::Slice {
                inner,
                start,
                length,
            } => {
                if let Some(length) = length {
                    self.top = top_order(&inner)
                        .map(|(order, complete)| (order, start.saturating_add(length), complete));
                }
                let inner = walk(self, inner);
                self.top = None;
                G::Slice {
                    inner,
                    start,
                    length,
                }
            }
            G::OrderBy { inner, expression } => G::OrderBy {
                inner: walk(self, inner),
                expression,
            },
            other => other,
        }
    }

    /// The value of every aggregate, if `inner` is a basic graph pattern and every aggregate
    /// is `COUNT(*)`, `COUNT(?v)` or `COUNT(DISTINCT ?v)` of one of its variables: the
    /// pattern is joined on term ids in the store (merge, hash or index nested-loop joins),
    /// without the rows of the last join (issue #167).
    fn joined_counts(
        &self,
        inner: &GraphPattern,
        aggregates: &[(Variable, AggregateExpression)],
    ) -> Option<Vec<i64>> {
        let GraphPattern::Bgp { patterns } = inner else {
            return None;
        };
        // Triples that read a result table of the spatial rewrite are answered by the snapshot
        // during evaluation, not from the quads: only a pattern of one table's columns alone
        // is counted here, off the table (`COUNT(*)` over a spatial join).
        let virtual_triple = |t: &spargebra::term::TriplePattern| matches!(&t.predicate, NamedNodePattern::NamedNode(p) if p.as_str().starts_with("urn:tripletrail:"));
        let table_triple = |t: &spargebra::term::TriplePattern| matches!(&t.predicate, NamedNodePattern::NamedNode(p) if p.as_str().starts_with("urn:tripletrail:table:"));
        if patterns.is_empty()
            || (patterns.iter().any(virtual_triple) && !patterns.iter().all(table_triple))
        {
            return None;
        }
        let compiled = self.snapshot.compile_bgp(patterns, &[])?;
        if patterns.iter().any(virtual_triple)
            && !self
                .snapshot
                .reads_table_whole(&compiled.triples, compiled.columns.len())
        {
            return None;
        }
        let blanks = compiled.columns.iter().any(Option::is_none);
        let column = |v: &Variable| compiled.columns.iter().position(|c| c.as_ref() == Some(v));
        let mut counts: Vec<(BgpCount, Option<u64>)> = Vec::new();
        for (_, a) in aggregates {
            let count = match a {
                // Solutions are distinct when every column is a variable of the projection.
                AggregateExpression::CountSolutions { distinct } if !distinct || !blanks => {
                    BgpCount::Rows
                }
                AggregateExpression::FunctionCall {
                    name: AggregateFunction::Count,
                    expr: Expression::Variable(v),
                    distinct,
                } => {
                    let c = column(v)?;
                    if *distinct {
                        BgpCount::Distinct(c)
                    } else {
                        BgpCount::Rows
                    }
                }
                _ => return None,
            };
            counts.push((count, None));
        }
        let mut out = Vec::with_capacity(counts.len());
        for i in 0..counts.len() {
            let count = counts[i].0;
            // The same count asked twice is computed once.
            let n = match counts[..i].iter().find(|(c, n)| *c == count && n.is_some()) {
                Some((_, n)) => n.unwrap_or(0),
                None if compiled.empty => 0,
                None => self.snapshot.count_ids(
                    &compiled.triples,
                    compiled.columns.len(),
                    self.graph,
                    count,
                    self.cancelled,
                )?,
            };
            counts[i].1 = Some(n);
            out.push(i64::try_from(n).ok()?);
        }
        Some(out)
    }

    /// Eager aggregation (Yan and Larson, 1995) of a `GROUP` whose pattern is a basic graph
    /// pattern under `BIND`s and `FILTER`s, with `COUNT(*)` / `COUNT(?v)` aggregates (#243):
    /// the pattern is first counted on term ids per distinct value of the variables the keys,
    /// the expressions and the counts read, and those values become a `VALUES` block with a
    /// count column. The `BIND`s and `FILTER`s then run once per distinct value instead of
    /// once per row, and the group sums the counts. `GROUP BY REPLACE(STR(?p), ...)` over all
    /// triples evaluates the expression once per predicate.
    fn eager_group(
        &self,
        inner: &GraphPattern,
        keys: &[Variable],
        aggregates: &[(Variable, AggregateExpression)],
        per_group: u64,
    ) -> Option<GraphPattern> {
        // `BIND`s and `FILTER`s, outermost first, down to the basic graph pattern.
        let mut chain: Vec<&GraphPattern> = Vec::new();
        let mut p = inner;
        let patterns = loop {
            match p {
                GraphPattern::Extend { inner, .. } | GraphPattern::Filter { inner, .. } => {
                    chain.push(p);
                    p = inner;
                }
                GraphPattern::Bgp { patterns } => break patterns,
                _ => return None,
            }
        };
        if chain.is_empty() || patterns.is_empty() {
            return None;
        }
        let mut bgp_vars: Vec<&Variable> = Vec::new();
        for t in patterns {
            for v in [
                match &t.subject {
                    TermPattern::Variable(v) => Some(v),
                    _ => None,
                },
                match &t.predicate {
                    NamedNodePattern::Variable(v) => Some(v),
                    _ => None,
                },
                match &t.object {
                    TermPattern::Variable(v) => Some(v),
                    _ => None,
                },
            ]
            .into_iter()
            .flatten()
            {
                if !bgp_vars.contains(&v) {
                    bgp_vars.push(v);
                }
            }
        }
        // The variables of the pattern something reads, in pattern order.
        let mut read: Vec<&Variable> = Vec::new();
        for step in &chain {
            let expr = match step {
                GraphPattern::Extend { expression, .. } => expression,
                GraphPattern::Filter { expr, .. } => expr,
                _ => unreachable!(),
            };
            if !deterministic_vars(expr, &mut read) {
                return None;
            }
        }
        read.extend(keys.iter());
        let mut counted: Vec<&Variable> = Vec::new();
        let mut extremes: Vec<&Variable> = Vec::new();
        for (_, a) in aggregates {
            match a {
                AggregateExpression::CountSolutions { distinct: false } => {}
                AggregateExpression::FunctionCall {
                    name: AggregateFunction::Count,
                    expr: Expression::Variable(v),
                    distinct: false,
                } => {
                    read.push(v);
                    counted.push(v);
                }
                // Of the variable read only through its tag (`tagged` below): the groups that
                // survive the FILTERs are then grouped again on its values.
                AggregateExpression::FunctionCall {
                    name:
                        AggregateFunction::Min | AggregateFunction::Max | AggregateFunction::Sample,
                    expr: Expression::Variable(v),
                    ..
                } => {
                    read.push(v);
                    extremes.push(v);
                }
                _ => return None,
            }
        }
        let used: Vec<Variable> = bgp_vars
            .iter()
            .filter(|v| read.contains(v))
            .map(|v| (*v).clone())
            .collect();
        // One triple pattern, with fewer distinct values of the variables read than there are
        // rows (a bound from the counts: the product of their distinct values), so the
        // `VALUES` block stays small. Otherwise the rows would be evaluated anyway.
        let [t] = patterns.as_slice() else {
            return None;
        };
        if used.len() == bgp_vars.len() {
            return None;
        }
        // A variable read only through LANG, DATATYPE and kind tests takes one value per
        // kind and tag: few, whatever its distinct values.
        let tagged = match &t.object {
            TermPattern::Variable(o)
                if used.contains(o)
                    && !keys.contains(o)
                    && !counted.contains(&o)
                    && chain.iter().all(|step| match step {
                        GraphPattern::Extend { expression, .. } => tag_only(expression, o),
                        GraphPattern::Filter { expr, .. } => tag_only(expr, o),
                        _ => false,
                    }) =>
            {
                Some(o.clone())
            }
            _ => None,
        };
        if extremes.iter().any(|v| tagged.as_ref() != Some(*v)) {
            return None;
        }
        let mut bound: u64 = 1;
        for v in &used {
            if tagged.as_ref() == Some(v) {
                continue;
            }
            let position = [
                matches!(&t.subject, TermPattern::Variable(x) if x == v),
                matches!(&t.predicate, NamedNodePattern::Variable(x) if x == v),
                matches!(&t.object, TermPattern::Variable(x) if x == v),
            ]
            .iter()
            .position(|b| *b)?;
            let distinct = self.snapshot.count_triples(
                GraphSelector::All,
                TripleCount::Distinct(position),
                None,
            )?;
            bound = bound.saturating_mul(distinct);
        }
        if bound > MAX_EAGER_GROUPS as u64 {
            return None;
        }
        let rows = [(None, false)];
        let groups = match tagged
            .is_none()
            .then(|| self.predicate_groups(p, &used, &rows))
            .flatten()
        {
            Some(g) => g,
            None => {
                let mut table = crate::idtree::Eval {
                    snapshot: self.snapshot,
                    cancelled: self.cancelled,
                    graph: self.graph,
                }
                .eval(p)?;
                if let Some(v) = &tagged {
                    let values = (!extremes.is_empty()).then(|| table.ids.clone());
                    tag_classes(self.snapshot, &mut table, v)?;
                    if let Some(values) = values {
                        table = self.surviving_values(table, values, &used, v, &chain)?;
                    }
                }
                // No more groups than pay (below): stops as soon as there are more.
                let cap = (table.len() as u64 / per_group) as usize;
                crate::idtree::group_counts(&table, &used, &rows, cap.min(MAX_EAGER_GROUPS))?
            }
        };
        // Grouping pays when the expressions then run once per group instead of once per
        // row. With nearly as many groups as rows (a filter on prices), the block of groups
        // costs more than the rows: 300 k distinct decimals took 600 ms instead of 30.
        let total: u64 = groups.iter().map(|(_, counts)| counts[0]).sum();
        // `per_group`: the rows a group must have on average.
        if groups.len() as u64 * per_group > total {
            return None;
        }
        let count = Variable::new_unchecked("__tripletrail_count");
        let mut bindings = Vec::with_capacity(groups.len());
        for (key, counts) in groups {
            let mut row = Vec::with_capacity(key.len() + 1);
            for (id, v) in key.into_iter().zip(&used) {
                row.push(if id == TermId(0) {
                    None
                } else if id.kind() == Kind::Blank && tagged.as_ref() == Some(v) {
                    if !extremes.is_empty() {
                        // The values are the nodes themselves, which VALUES cannot hold.
                        return None;
                    }
                    // A VALUES block cannot hold a blank node: the class of blank nodes is
                    // an IRI that `blank_class` makes the kind tests read as one.
                    Some(GroundTerm::NamedNode(
                        oxigraph::model::NamedNode::new_unchecked(BLANK_CLASS),
                    ))
                } else {
                    Some(match self.snapshot.term(id).ok()? {
                        Term::NamedNode(n) => GroundTerm::NamedNode(n),
                        Term::Literal(l) => GroundTerm::Literal(l),
                        _ => return None,
                    })
                });
            }
            row.push(Some(GroundTerm::Literal(Literal::from(
                i64::try_from(counts[0]).ok()?,
            ))));
            bindings.push(row);
        }
        let pattern = wrap(
            GraphPattern::Values {
                variables: used.into_iter().chain([count.clone()]).collect(),
                bindings,
            },
            &chain,
            tagged.as_ref(),
        );
        let sum = |e: Expression| AggregateExpression::FunctionCall {
            name: AggregateFunction::Sum,
            expr: e,
            distinct: false,
        };
        let aggregates =
            aggregates
                .iter()
                .map(|(v, a)| {
                    let a = match a {
                        AggregateExpression::FunctionCall {
                            name:
                                AggregateFunction::Min
                                | AggregateFunction::Max
                                | AggregateFunction::Sample,
                            ..
                        } => a.clone(),
                        AggregateExpression::FunctionCall {
                            expr: Expression::Variable(x),
                            ..
                        } => sum(Expression::If(
                            Box::new(Expression::Bound(x.clone())),
                            Box::new(Expression::Variable(count.clone())),
                            Box::new(Expression::Literal(Literal::from(0_i64))),
                        )),
                        _ => sum(Expression::Variable(count.clone())),
                    };
                    (v.clone(), a)
                })
                .collect();
        Some(GraphPattern::Group {
            inner: Box::new(pattern),
            variables: keys.to_vec(),
            aggregates,
        })
    }

    /// The rows of `table`, whose column `tagged` [`tag_classes`] replaced, back with their
    /// `values` (the ids before), but only those of the groups on `used` that the `BIND`s and
    /// `FILTER`s of `chain` keep. They read `tagged` only through its tag, so one row per group
    /// (with the class for its value) tells. `MIN`, `MAX` and `SAMPLE` of `tagged` then group
    /// these rows on its values: few, when the FILTERs keep few groups (dates of a datatype,
    /// not all the strings).
    fn surviving_values(
        &self,
        table: crate::idtree::Table,
        values: Vec<TermId>,
        used: &[Variable],
        tagged: &Variable,
        chain: &[&GraphPattern],
    ) -> Option<crate::idtree::Table> {
        use oxigraph::sparql::{QueryResults, SparqlEvaluator};
        if chain.iter().any(|step| match step {
            GraphPattern::Extend { expression, .. } => has_custom(expression),
            GraphPattern::Filter { expr, .. } => has_custom(expr),
            _ => true,
        }) {
            return None;
        }
        let width = table.vars.len();
        let cols: Vec<usize> = used.iter().map(|v| table.col(v)).collect::<Option<_>>()?;
        let key =
            |r: usize| -> Vec<TermId> { cols.iter().map(|&c| table.ids[r * width + c]).collect() };
        let mut groups: HashMap<Vec<TermId>, usize> = HashMap::new();
        let mut keys: Vec<Vec<TermId>> = Vec::new();
        for r in 0..table.len() {
            let k = key(r);
            if !groups.contains_key(&k) {
                if keys.len() >= MAX_EAGER_GROUPS {
                    return None;
                }
                groups.insert(k.clone(), keys.len());
                keys.push(k);
            }
        }
        let row_var = Variable::new_unchecked("__tripletrail_row");
        let mut bindings = Vec::with_capacity(keys.len());
        for (i, k) in keys.iter().enumerate() {
            let mut row = Vec::with_capacity(k.len() + 1);
            for (&id, v) in k.iter().zip(used) {
                row.push(if id == TermId(0) {
                    None
                } else if id.kind() == Kind::Blank && v == tagged {
                    Some(GroundTerm::NamedNode(
                        oxigraph::model::NamedNode::new_unchecked(BLANK_CLASS),
                    ))
                } else {
                    Some(match self.snapshot.term(id).ok()? {
                        Term::NamedNode(n) => GroundTerm::NamedNode(n),
                        Term::Literal(l) => GroundTerm::Literal(l),
                        _ => return None,
                    })
                });
            }
            row.push(Some(GroundTerm::Literal(Literal::from(i as i64))));
            bindings.push(row);
        }
        let query = Query::Select {
            dataset: None,
            pattern: GraphPattern::Project {
                inner: Box::new(wrap(
                    GraphPattern::Values {
                        variables: used.iter().cloned().chain([row_var.clone()]).collect(),
                        bindings,
                    },
                    chain,
                    Some(tagged),
                )),
                variables: vec![row_var.clone()],
            },
            base_iri: None,
        };
        let empty = oxigraph::model::Dataset::new();
        let QueryResults::Solutions(solutions) = SparqlEvaluator::new()
            .for_query(query)
            .on_queryable_dataset(&empty)
            .execute()
            .ok()?
        else {
            return None;
        };
        let mut keep = vec![false; keys.len()];
        for solution in solutions {
            let Some(Term::Literal(l)) = solution.ok()?.get(&row_var).cloned() else {
                return None;
            };
            keep[l.value().parse::<usize>().ok()?] = true;
        }
        let tc = table.col(tagged)?;
        let rows: Vec<usize> = (0..table.len())
            .filter(|&r| keep[groups[&key(r)]])
            .collect();
        // More rows than groups allowed: likely more values too, so leave them to the rows.
        if rows.len() > MAX_EAGER_GROUPS {
            return None;
        }
        let mut ids = Vec::with_capacity(rows.len() * width);
        for &r in &rows {
            let row = &values[r * width..(r + 1) * width];
            debug_assert_eq!(row[tc].kind(), table.ids[r * width + tc].kind());
            ids.extend_from_slice(row);
        }
        Some(crate::idtree::Table {
            vars: table.vars,
            ids,
            maybe: table.maybe,
            ones: rows.len(),
        })
    }

    /// `?s ?p ?o GROUP BY ?p` with counts of rows or of distinct subjects or objects, from the
    /// per-predicate counts kept with the indexes, without reading the quads (#243). MatGUI
    /// and other explorers run it on every endpoint they open.
    fn predicate_groups(
        &self,
        inner: &GraphPattern,
        keys: &[Variable],
        wanted: &[(Option<&Variable>, bool)],
    ) -> Option<Vec<(Vec<TermId>, Vec<u64>)>> {
        let GraphPattern::Bgp { patterns } = inner else {
            return None;
        };
        let [t] = patterns.as_slice() else {
            return None;
        };
        let (TermPattern::Variable(s), NamedNodePattern::Variable(p), TermPattern::Variable(o)) =
            (&t.subject, &t.predicate, &t.object)
        else {
            return None;
        };
        if s == p || s == o || p == o || keys != std::slice::from_ref(p) {
            return None;
        }
        // Per aggregate: 0 rows, 1 distinct subjects, 2 distinct objects, 3 one (the key).
        let picks: Vec<usize> = wanted
            .iter()
            .map(|&(v, distinct)| match (v, distinct) {
                (None, _) => Some(0),
                (Some(v), false) if v == s || v == p || v == o => Some(0),
                (Some(v), true) if v == s => Some(1),
                (Some(v), true) if v == o => Some(2),
                (Some(v), true) if v == p => Some(3),
                _ => None,
            })
            .collect::<Option<_>>()?;
        let counts = self.snapshot.predicate_counts()?;
        Some(
            counts
                .into_iter()
                .map(|(pred, rows, subjects, objects)| {
                    let values = picks
                        .iter()
                        .map(|&i| [rows, subjects, objects, 1][i])
                        .collect();
                    (vec![pred], values)
                })
                .collect(),
        )
    }

    /// One triple pattern grouped by one of its variables, with counts of rows or of distinct
    /// keys: `?f a ?c GROUP BY ?c` with `COUNT(?f)`. The quads come sorted by the key, and
    /// each key's count is read from the block index past its first few blocks (see
    /// `Snapshot::value_counts`), so no row is built per quad.
    fn value_groups(
        &self,
        inner: &GraphPattern,
        keys: &[Variable],
        wanted: &[(Option<&Variable>, bool)],
        cap: usize,
    ) -> Option<(Vec<TermId>, Vec<u64>)> {
        let GraphPattern::Bgp { patterns } = inner else {
            return None;
        };
        let [t] = patterns.as_slice() else {
            return None;
        };
        let [key] = keys else { return None };
        let predicate = match &t.predicate {
            NamedNodePattern::Variable(v) => TermPattern::Variable(v.clone()),
            NamedNodePattern::NamedNode(n) => TermPattern::NamedNode(n.clone()),
        };
        let positions = [&t.subject, &predicate, &t.object];
        // Each position: a term (`Some(id)`) or a variable (`None`); variables all distinct.
        let mut vars: Vec<&Variable> = Vec::new();
        let mut ids = [None; 3];
        for (i, x) in positions.iter().enumerate() {
            ids[i] = match x {
                TermPattern::Variable(v) => {
                    if vars.contains(&v) {
                        return None;
                    }
                    vars.push(v);
                    None
                }
                TermPattern::NamedNode(n) => Some(self.snapshot.term_id(n.into())?),
                TermPattern::Literal(l) => Some(self.snapshot.term_id(l.into())?),
                // Blank nodes of a query are variables; quoted triples are left alone.
                _ => return None,
            };
        }
        let [s, p, o] = ids;
        let at = positions
            .iter()
            .position(|x| matches!(x, TermPattern::Variable(v) if v == key))?;
        // A column of a result table is not in the quad indexes.
        if p.is_some_and(|p| p.kind() == Kind::Column) {
            return None;
        }
        // Every variable is bound in every row: a count of one is of rows, a distinct count
        // only of the key (one per group).
        let picks: Vec<bool> = wanted
            .iter()
            .map(|&(v, distinct)| match (v, distinct) {
                (None, _) => Some(false),
                (Some(v), false) if vars.contains(&v) => Some(false),
                (Some(v), true) if v == key => Some(true),
                _ => None,
            })
            .collect::<Option<_>>()?;
        let counts = self
            .snapshot
            .value_counts(s, p, o, self.graph, at, cap, self.cancelled)?;
        let mut keys = Vec::with_capacity(counts.len());
        let mut values = Vec::with_capacity(counts.len() * picks.len());
        for (id, n) in counts {
            keys.push(id);
            values.extend(picks.iter().map(|&one| if one { 1 } else { n }));
        }
        Some((keys, values))
    }

    /// The degree of each node, `{ ?n ?p ?o } UNION { ?s ?p ?n }` grouped by `?n` with
    /// `COUNT(*)` (MatGUI's "hubs", #243), each side possibly under a `FILTER` of kind tests
    /// of `?n` (`isIRI(?n)`): runs of equal subjects in subject order and of equal objects in
    /// object order, merged on ids, without a row per quad.
    /// Counts per value of one key over a star of triple patterns (`?s a ?c ; ex:f ?f GROUP
    /// BY ?c`), from each triple's matches per centre value, without the rows of the join
    /// (issue #179, [`tripletrail_store::Snapshot::count_star_groups`]). Only `COUNT(*)` and
    /// `COUNT(?v)` of a variable of the pattern, which are the same there.
    fn star_groups(
        &self,
        inner: &GraphPattern,
        keys: &[Variable],
        wanted: &[(Option<&Variable>, bool)],
        cap: usize,
    ) -> Option<(Vec<TermId>, Vec<u64>)> {
        let GraphPattern::Bgp { patterns } = inner else {
            return None;
        };
        let [key] = keys else { return None };
        if patterns.iter().any(|t| {
            matches!(&t.predicate, NamedNodePattern::NamedNode(p) if p.as_str().starts_with("urn:tripletrail:"))
        }) {
            return None;
        }
        let compiled = self.snapshot.compile_bgp(patterns, &[])?;
        let column = |v: &Variable| compiled.columns.iter().position(|c| c.as_ref() == Some(v));
        for &(v, distinct) in wanted {
            if distinct || v.is_some_and(|v| column(v).is_none()) {
                return None;
            }
        }
        if compiled.empty {
            return Some((Vec::new(), Vec::new()));
        }
        let groups = self.snapshot.count_star_groups(
            &compiled.triples,
            self.graph,
            column(key)?,
            MAX_GROUPS.min(cap),
            self.cancelled,
        )??;
        let mut keys = Vec::with_capacity(groups.len());
        let mut values = Vec::with_capacity(groups.len() * wanted.len());
        for (id, n) in groups {
            keys.push(id);
            values.extend(std::iter::repeat_n(n, wanted.len()));
        }
        Some((keys, values))
    }

    fn degree_groups(
        &self,
        inner: &GraphPattern,
        keys: &[Variable],
        wanted: &[(Option<&Variable>, bool)],
        cap: usize,
    ) -> Option<(Vec<TermId>, Vec<u64>)> {
        let GraphPattern::Union { left, right } = inner else {
            return None;
        };
        let [key] = keys else { return None };
        if wanted.iter().any(|&(v, distinct)| v.is_some() || distinct) {
            return None;
        }
        // The position of the key in a side's one triple pattern of distinct variables, and
        // the kinds its filter keeps (`None`: all).
        let side = |p: &GraphPattern| -> Option<(usize, Option<Vec<Kind>>)> {
            let (bgp, kinds) = match p {
                GraphPattern::Filter { expr, inner } => (&**inner, Some(kind_test(expr, key)?)),
                other => (other, None),
            };
            let GraphPattern::Bgp { patterns } = bgp else {
                return None;
            };
            let [t] = patterns.as_slice() else {
                return None;
            };
            let (TermPattern::Variable(s), NamedNodePattern::Variable(p), TermPattern::Variable(o)) =
                (&t.subject, &t.predicate, &t.object)
            else {
                return None;
            };
            if s == p || s == o || p == o {
                return None;
            }
            let at = [s, p, o].iter().position(|v| *v == key)?;
            (at != 1).then_some((at, kinds))
        };
        let (a, b) = (side(left)?, side(right)?);
        let runs = |(at, kinds): (usize, Option<Vec<Kind>>)| -> Option<Vec<(TermId, u64)>> {
            let mut out: Vec<(TermId, u64)> = Vec::new();
            for (n, q) in self
                .snapshot
                .quad_ids_sorted(None, None, None, self.graph, at)
                .enumerate()
            {
                if n % 65_536 == 0 && (self.cancelled)() {
                    return None;
                }
                let id = q[at];
                if kinds.as_ref().is_some_and(|k| !k.contains(&id.kind())) {
                    continue;
                }
                let full = out.len() >= cap;
                match out.last_mut() {
                    Some((last, c)) if *last == id => *c += 1,
                    Some((last, _)) if *last > id => return None,
                    _ if full => return None,
                    _ => out.push((id, 1)),
                }
            }
            Some(out)
        };
        let (a, b) = (runs(a)?, runs(b)?);
        let (mut i, mut j) = (0, 0);
        let mut keys = Vec::with_capacity(a.len().max(b.len()));
        let mut values = Vec::with_capacity(a.len().max(b.len()) * wanted.len());
        while i < a.len() || j < b.len() {
            let (id, n) = match (a.get(i), b.get(j)) {
                (Some(x), Some(y)) if x.0 == y.0 => {
                    i += 1;
                    j += 1;
                    (x.0, x.1 + y.1)
                }
                (Some(x), Some(y)) if x.0 < y.0 => {
                    i += 1;
                    *x
                }
                (Some(x), None) => {
                    i += 1;
                    *x
                }
                (_, Some(y)) => {
                    j += 1;
                    *y
                }
                (None, None) => unreachable!(),
            };
            keys.push(id);
            values.extend(std::iter::repeat_n(n, wanted.len()));
        }
        Some((keys, values))
    }

    /// The groups of `inner` by `keys` with their aggregates, as a pattern of the keys and the
    /// aggregates, if every aggregate is a count, `SUM`, or `MIN`, `MAX` or `SAMPLE` of a
    /// variable, `inner` is evaluated on ids (`crate::idtree`), and there are at most
    /// [`MAX_GROUPS`] groups. From [`READ_BACK_GROUPS`] groups on the pattern reads them back
    /// from a table of ids; fewer, or values without an id, go into a `VALUES` block (every
    /// key and value an IRI, a literal or unbound then).
    fn grouped_counts(
        &self,
        inner: &GraphPattern,
        keys: &[Variable],
        aggregates: &[(Variable, AggregateExpression)],
        top: Option<&Top>,
    ) -> Option<GraphPattern> {
        let aggs = id_aggregates(aggregates)?;
        let counts_only = aggs.iter().all(|a| matches!(a, Agg::Count(..)));
        let wanted: Vec<(Option<&Variable>, bool)> = aggs
            .iter()
            .map(|a| match a {
                Agg::Count(v, d) => (*v, *d),
                // Placeholders, filled in below.
                _ => (None, false),
            })
            .collect();
        // Groups of one key each, keys and counts flat: one allocation for all the groups,
        // not two a group (#165: 70,000 groups spent most of their time in malloc).
        let from_predicates = if counts_only && keys.len() == 1 {
            let cap = group_cap(1, aggs.len());
            self.predicate_groups(inner, keys, &wanted)
                .map(|groups| {
                    let mut flat = (Vec::new(), Vec::new());
                    for (k, c) in groups {
                        flat.0.extend(k);
                        flat.1.extend(c);
                    }
                    flat
                })
                .or_else(|| self.star_groups(inner, keys, &wanted, cap))
                .or_else(|| self.degree_groups(inner, keys, &wanted, cap))
                .or_else(|| self.value_groups(inner, keys, &wanted, cap))
        } else {
            None
        };
        // The groups: their keys and counts (flat, `width` keys and `n` counts a group), and
        // the table and rows of each group for the other aggregates.
        let (width, n) = (keys.len(), aggs.len());
        let mut table = None;
        let mut by_rows = None;
        let (groups, group_keys, counts): (usize, Vec<TermId>, Vec<u64>) = match from_predicates {
            Some((group_keys, counts)) => (group_keys.len(), group_keys, counts),
            None => {
                let t = crate::idtree::Eval {
                    snapshot: self.snapshot,
                    cancelled: self.cancelled,
                    graph: self.graph,
                }
                .eval(inner)?;
                let groups = crate::idtree::group_rows(&t, keys, MAX_GROUPS)?;
                let mut group_keys = Vec::with_capacity(groups.len() * width);
                let mut counts = Vec::with_capacity(groups.len() * n);
                for g in 0..groups.len() {
                    group_keys.extend_from_slice(groups.key(g));
                    let rows = groups.rows(g);
                    counts.extend(wanted.iter().zip(&aggs).map(|(&(v, d), a)| match a {
                        Agg::Count(..) => crate::idtree::count_rows(&t, rows, v, d),
                        _ => 0,
                    }));
                }
                let len = groups.len();
                table = Some(t);
                by_rows = Some(groups);
                (len, group_keys, counts)
            }
        };
        // Under a query memory limit (#874), groups past it are left to the evaluator, whose
        // GROUP BY counts them against the same limit.
        if groups > group_cap(width, n) {
            return None;
        }
        let key = |g: usize| &group_keys[g * width..(g + 1) * width];
        // The groups to give, in this order.
        let mut order: Vec<usize> = (0..groups).collect();
        let mut pick = crate::idtree::Pick::new(self.snapshot);
        // Under `ORDER BY` of counts and keys and `LIMIT k`, only the first k groups are kept
        // (ties may come in any order, as SPARQL leaves it).
        if let Some((sort, k, complete)) = top
            && order.len() > *k
            && let Some(kept) = top_groups(
                &mut pick,
                &order,
                *k,
                *complete,
                sort.iter().map(|(v, desc)| {
                    if let Some(c) = keys.iter().position(|key| key == v) {
                        return Some((By::Key(c), *desc));
                    }
                    let i = aggregates.iter().position(|(a, _)| a == v)?;
                    matches!(aggs[i], Agg::Count(..)).then_some((By::Count(i), *desc))
                }),
                |g, c| key(g)[c],
                |g, i| counts[g * n + i],
            )
        {
            order = kept;
        }
        // Each aggregate of each group, as an id when it has one (the counts and sums are
        // inline numbers, MIN, MAX and SAMPLE ids of the table) or else as a literal.
        let mut kernels = Vec::with_capacity(n);
        for a in &aggs {
            kernels.push(match (a, &table) {
                (Agg::Sum(e, _) | Agg::Avg(e, _), Some(t)) => {
                    Some(crate::idvalue::compile(e, t, self.snapshot)?)
                }
                _ => None,
            });
        }
        // A group's values from its rows; groups of many rows on several cores (#4), each chunk
        // of groups with its own `Pick`, the values back in the order of the groups.
        let value_of = |pick: &mut crate::idtree::Pick, g: usize, values: &mut Vec<Value>| {
            let rows = by_rows.as_ref().map_or(&[][..], |r| r.rows(g));
            for (i, a) in aggs.iter().enumerate() {
                values.push(match (a, &table) {
                    (Agg::Count(..), _) => Value::literal(
                        self.snapshot,
                        Literal::from(i64::try_from(counts[g * n + i]).ok()?),
                    ),
                    (Agg::Min(v), Some(t)) => Value::Id(pick.extreme(t, rows, v, false)?),
                    (Agg::Max(v), Some(t)) => Value::Id(pick.extreme(t, rows, v, true)?),
                    (Agg::Sample(v), Some(t)) => Value::Id(crate::idtree::Pick::sample(t, rows, v)),
                    (Agg::Sum(_, distinct) | Agg::Avg(_, distinct), Some(t)) => {
                        let kernel = kernels[i].as_ref()?;
                        let mut sum = crate::idvalue::Sum::default();
                        // With DISTINCT, each value once, as the evaluator's terms: equal
                        // numbers of two datatypes are two values.
                        let mut seen = std::collections::HashSet::new();
                        let mut added = 0;
                        for &r in rows {
                            // An error (unbound) makes the evaluator's sum an error: leave
                            // it to the evaluator.
                            let v = kernel(t.row(r as usize))?.ok()?;
                            if *distinct && !seen.insert(crate::idvalue::distinct_key(v)?) {
                                continue;
                            }
                            sum.add(v)?;
                            added += 1;
                        }
                        let value = match a {
                            Agg::Avg(..) => sum.average(added)?,
                            _ => sum.literal()?,
                        };
                        Value::literal(self.snapshot, value)
                    }
                    (_, None) => return None,
                });
            }
            Some(())
        };
        let rows = table.as_ref().map_or(0, |t| t.len());
        let parallel = if rows >= tripletrail_store::par::PARALLEL_ROWS {
            2
        } else {
            usize::MAX
        };
        let parts = tripletrail_store::par::chunks_sized(order.len(), parallel, 1, |range| {
            let mut pick = crate::idtree::Pick::new(self.snapshot);
            let mut values = Vec::with_capacity(range.len() * n);
            for &g in &order[range] {
                value_of(&mut pick, g, &mut values)?;
            }
            Some(values)
        });
        let mut values: Vec<Value> = Vec::with_capacity(order.len() * n);
        for part in parts {
            values.extend(part?);
        }
        let variables: Vec<Variable> = keys
            .iter()
            .cloned()
            .chain(aggregates.iter().map(|(v, _)| v.clone()))
            .collect();
        // Many groups go to the evaluator as an id table, read back without decoding every
        // value into the query (#185): decoding them into `VALUES` cost more than grouping.
        if order.len() >= READ_BACK_GROUPS
            && let Some(ids) = values
                .iter()
                .map(|v| match v {
                    Value::Id(id) => Some(*id),
                    Value::Literal(_) => None,
                })
                .collect::<Option<Vec<TermId>>>()
        {
            let all_width = variables.len();
            let mut all = Vec::with_capacity(order.len() * all_width);
            for (i, &g) in order.iter().enumerate() {
                all.extend_from_slice(key(g));
                all.extend_from_slice(&ids[i * n..(i + 1) * n]);
            }
            let maybe = (0..all_width)
                .map(|c| {
                    all.iter()
                        .skip(c)
                        .step_by(all_width)
                        .any(|id| *id == TermId(0))
                })
                .collect();
            let t = crate::idtree::Table {
                vars: variables.clone(),
                ids: all,
                maybe,
                ones: order.len(),
            };
            if let Some(p) = crate::rows::read_back(t) {
                return Some(p);
            }
        }
        let ground = |id: TermId| -> Option<Option<GroundTerm>> {
            if id == TermId(0) {
                return Some(None);
            }
            Some(Some(match self.snapshot.term(id).ok()? {
                Term::NamedNode(n) => GroundTerm::NamedNode(n),
                Term::Literal(l) => GroundTerm::Literal(l),
                // A blank node can't be written in VALUES.
                _ => return None,
            }))
        };
        let mut bindings = Vec::with_capacity(order.len());
        let mut values = values.into_iter();
        for &g in &order {
            let mut row = Vec::with_capacity(variables.len());
            for &id in key(g) {
                row.push(ground(id)?);
            }
            for _ in &aggs {
                row.push(match values.next()? {
                    Value::Id(id) => ground(id)?,
                    Value::Literal(l) => Some(GroundTerm::Literal(l)),
                });
            }
            bindings.push(row);
        }
        Some(GraphPattern::Values {
            variables,
            bindings,
        })
    }

    /// The solutions of `?s <p>+ ?o` or `?s <q>/<p>+ ?o` in the default graph, counted on the
    /// edges of `p` read once (#165): for each start, the nodes it reaches by a depth-first
    /// search (each pair once, as `+` gives them); a sequence adds up the reach of each `q`
    /// edge's end. `None` for other patterns (a fixed start included), and past a budget of
    /// steps (the evaluator then gives the rows).
    fn path_count(&self, inner: &GraphPattern) -> Option<u64> {
        use spargebra::algebra::PropertyPathExpression as P;
        // The parser writes `?s <q>/<p>+ ?o` as a triple joined with a path from a blank node.
        if let GraphPattern::Join { left, right } = inner {
            let ((
                GraphPattern::Bgp { patterns },
                GraphPattern::Path {
                    subject,
                    path,
                    object,
                },
            )
            | (
                GraphPattern::Path {
                    subject,
                    path,
                    object,
                },
                GraphPattern::Bgp { patterns },
            )) = (&**left, &**right)
            else {
                return None;
            };
            let [t] = patterns.as_slice() else {
                return None;
            };
            let (NamedNodePattern::NamedNode(q), TermPattern::Variable(s)) =
                (&t.predicate, &t.subject)
            else {
                return None;
            };
            let named = |t: &TermPattern| match t {
                TermPattern::Variable(v) => Some(format!("?{}", v.as_str())),
                TermPattern::BlankNode(b) => Some(format!("_:{}", b.as_str())),
                _ => None,
            };
            let (x, start, end) = (named(&t.object)?, named(subject)?, named(object)?);
            if x != start
                || x == end
                || end == format!("?{}", s.as_str())
                || x == format!("?{}", s.as_str())
            {
                return None;
            }
            let spargebra::algebra::PropertyPathExpression::OneOrMore(p) = path else {
                return None;
            };
            let spargebra::algebra::PropertyPathExpression::NamedNode(p) = &**p else {
                return None;
            };
            return self.reach_count(Some(q), p);
        }
        let GraphPattern::Path {
            subject: TermPattern::Variable(s),
            path,
            object: TermPattern::Variable(o),
        } = inner
        else {
            // One start: the evaluator follows its edges without reading them all.
            return None;
        };
        if s == o {
            return None;
        }
        match path {
            P::OneOrMore(p) => match &**p {
                P::NamedNode(p) => self.reach_count(None, p),
                _ => None,
            },
            P::Sequence(a, b) => match (&**a, &**b) {
                (P::NamedNode(q), P::OneOrMore(p)) => match &**p {
                    P::NamedNode(p) => self.reach_count(Some(q), p),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
    }

    /// The pairs `?s <p>+ ?o` (with `first`, `?s <first>/<p>+ ?o`), counted as
    /// [`Pass::path_count`] says.
    fn reach_count(
        &self,
        first: Option<&spargebra::term::NamedNode>,
        p: &spargebra::term::NamedNode,
    ) -> Option<u64> {
        /// Nodes visited in all searches together.
        const BUDGET: u64 = 20_000_000;
        let iri = |n: &spargebra::term::NamedNode| self.snapshot.term_id(n.into());
        // A term the store doesn't have matches nothing.
        let Some(p) = iri(p) else {
            return Some(0);
        };
        // Under a query memory limit (#874), a graph whose edges would pass it is left to the
        // evaluator, whose closures count against the same limit: each edge takes about 48
        // bytes here (the pair, its two nodes, its target and its node's offset and mark), and
        // each solution of `first` 16.
        if let Some(limit) = tripletrail_store::memory_limit() {
            let edges = self.snapshot.estimate_ids(None, Some(p), None, self.graph);
            let starts = match first.map(iri) {
                Some(Some(q)) => self.snapshot.estimate_ids(None, Some(q), None, self.graph),
                _ => 0,
            };
            if edges
                .saturating_mul(48)
                .saturating_add(starts.saturating_mul(16))
                > limit
            {
                return None;
            }
        }
        let mut edges: Vec<(u64, u64)> = self
            .snapshot
            .quad_ids_for_pattern(None, Some(p), None, self.graph)
            .map(|[s, _, o, _]| (s.0, o.0))
            .collect();
        edges.sort_unstable();
        edges.dedup();
        // Dense numbers for the nodes, and the edges as lists per node.
        let mut nodes: Vec<u64> = edges.iter().flat_map(|&(s, o)| [s, o]).collect();
        nodes.sort_unstable();
        nodes.dedup();
        let index = |id: u64| nodes.binary_search(&id).ok();
        let mut offsets = vec![0usize; nodes.len() + 1];
        for &(s, _) in &edges {
            offsets[index(s)? + 1] += 1;
        }
        for i in 0..nodes.len() {
            offsets[i + 1] += offsets[i];
        }
        let targets: Vec<usize> = edges
            .iter()
            .map(|&(_, o)| index(o))
            .collect::<Option<_>>()?;
        // The starts, each with how many solutions of `first` end there.
        let starts: Vec<(u64, u64)> = match first {
            None => {
                let mut v: Vec<(u64, u64)> = edges.iter().map(|&(s, _)| (s, 1)).collect();
                v.dedup_by_key(|(s, _)| *s);
                v
            }
            Some(q) => {
                let Some(q) = iri(q) else {
                    return Some(0);
                };
                let mut ends: Vec<u64> = self
                    .snapshot
                    .quad_ids_for_pattern(None, Some(q), None, self.graph)
                    .map(|[_, _, o, _]| o.0)
                    .collect();
                ends.sort_unstable();
                let mut v: Vec<(u64, u64)> = Vec::new();
                for x in ends {
                    match v.last_mut() {
                        Some((last, n)) if *last == x => *n += 1,
                        _ => v.push((x, 1)),
                    }
                }
                v
            }
        };
        let mut seen = vec![u32::MAX; nodes.len()];
        let mut stack = Vec::new();
        let mut steps = 0u64;
        let mut total = 0u64;
        for (search, (x, times)) in starts.into_iter().enumerate() {
            let Some(from) = index(x) else { continue };
            // The nodes reachable from `from` in one step or more.
            let stamp = u32::try_from(search).ok()?;
            let mut n = 0u64;
            stack.clear();
            stack.push(from);
            while let Some(at) = stack.pop() {
                for &next in &targets[offsets[at]..offsets[at + 1]] {
                    if seen[next] != stamp {
                        seen[next] = stamp;
                        n += 1;
                        stack.push(next);
                    }
                }
                steps += 1;
                if steps > BUDGET || (steps.is_multiple_of(65_536) && (self.cancelled)()) {
                    return None;
                }
            }
            total = total.checked_add(n.checked_mul(times)?)?;
        }
        Some(total)
    }

    /// The value of every aggregate, if they are all counts and `inner` is made of basic graph
    /// patterns, joins, `OPTIONAL`, `MINUS`, `UNION` and the filters `crate::idtree` takes.
    fn tree_counts(
        &self,
        inner: &GraphPattern,
        aggregates: &[(Variable, AggregateExpression)],
    ) -> Option<Vec<i64>> {
        if matches!(inner, GraphPattern::Bgp { .. }) {
            return None;
        }
        let wanted = count_aggregates(aggregates)?;
        let eval = crate::idtree::Eval {
            snapshot: self.snapshot,
            cancelled: self.cancelled,
            graph: self.graph,
        };
        // COUNT(*) without building the rows, where it can; COUNT(?v) too for a `?v` bound
        // in every row.
        let certain = crate::plan::certain(inner);
        if wanted
            .iter()
            .all(|(v, distinct)| !distinct && v.is_none_or(|v| certain.contains(v.as_str())))
            && let Some(n) = self.path_count(inner)
        {
            let n = i64::try_from(n).ok()?;
            return Some(vec![n; wanted.len()]);
        }
        if wanted
            .iter()
            .all(|(v, distinct)| !distinct && v.is_none_or(|v| certain.contains(v.as_str())))
            && let Some(n) = eval.rows(inner)
        {
            let n = i64::try_from(n).ok()?;
            return Some(vec![n; wanted.len()]);
        }
        let table = eval.eval(inner)?;
        wanted
            .into_iter()
            .map(|(v, d)| i64::try_from(crate::idtree::count(&table, v, d)?).ok())
            .collect()
    }

    /// The value of every aggregate, if `inner` is `?s ?p ?o` (optionally filtered on the term
    /// kind of one variable) and every aggregate is a count the store keeps.
    fn counts(
        &self,
        inner: &GraphPattern,
        aggregates: &[(Variable, AggregateExpression)],
    ) -> Option<Vec<i64>> {
        let (triple, filter) = match inner {
            GraphPattern::Bgp { patterns } => (patterns, None),
            GraphPattern::Filter { expr, inner } => match &**inner {
                GraphPattern::Bgp { patterns } => (patterns, Some(expr)),
                _ => return None,
            },
            _ => return None,
        };
        let [t] = triple.as_slice() else { return None };
        let (TermPattern::Variable(s), NamedNodePattern::Variable(p), TermPattern::Variable(o)) =
            (&t.subject, &t.predicate, &t.object)
        else {
            return None;
        };
        if s == p || s == o || p == o {
            return None;
        }
        let position = |v: &Variable| [s, p, o].iter().position(|x| *x == v);
        let kinds: Option<(usize, &[Kind])> = match filter {
            None => None,
            Some(Expression::FunctionCall(f, args)) => {
                let [Expression::Variable(v)] = args.as_slice() else {
                    return None;
                };
                let kinds: &[Kind] = match f {
                    Function::IsLiteral => &LITERALS,
                    Function::IsIri => &[Kind::Iri],
                    Function::IsBlank => &[Kind::Blank],
                    _ => return None,
                };
                Some((position(v)?, kinds))
            }
            Some(_) => return None,
        };
        aggregates
            .iter()
            .map(|(_, a)| {
                let count = match a {
                    // Triples of one graph are distinct, so distinct solutions are all of them.
                    AggregateExpression::CountSolutions { .. } => TripleCount::Rows,
                    AggregateExpression::FunctionCall {
                        name: AggregateFunction::Count,
                        expr: Expression::Variable(v),
                        distinct,
                    } => {
                        let at = position(v)?;
                        if *distinct {
                            TripleCount::Distinct(at)
                        } else {
                            TripleCount::Rows
                        }
                    }
                    _ => return None,
                };
                let n = self.snapshot.count_triples(self.graph, count, kinds)?;
                i64::try_from(n).ok()
            })
            .collect()
    }
}
