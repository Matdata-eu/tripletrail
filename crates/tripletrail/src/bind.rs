//! `BIND` expressions of update `WHERE`s computed on every core (#292).
//!
//! The evaluator computes a `BIND` one row at a time on one thread. A pattern such as the rail KG's
//! GPS update (two `spatialf:transformSRS` and six `REPLACE` per row, over 315k rows) spends
//! nearly all its time there. This pass finds such chains of `BIND`s in the `WHERE` of an
//! `INSERT`/`DELETE`, evaluates the pattern under them on its own, splits its rows into chunks,
//! and evaluates the chain over each chunk (as a `VALUES` block) on every core with the usual
//! evaluator, so every expression gives exactly what it gives in the evaluator. The rows come
//! back as one `VALUES` block in place of the chain, holding only the variables used above it.
//!
//! Only updates: an update holds what it writes anyway, while a query streams its rows (its
//! first row, and the memory it takes, would wait on all of them). The rows under a chain are
//! read up to [`max_values`] values; past that, the chain is left to the evaluator.
//!
//! Only chains whose every row is needed are taken: from the top of the pattern down through
//! `SELECT`, `DISTINCT`, `REDUCED`, `UNION`, `ORDER BY` and `GROUP BY`, and through a `FILTER`
//! that reads nothing the chain binds, even inside an `EXISTS` (it moves under the chain). A
//! chain inside a join, `OPTIONAL`, `MINUS`, `GRAPH`, `SERVICE` or under a `LIMIT` is left
//! alone, as are chains with `EXISTS` (each chunk would read the data again), `NOW()` (one value
//! per query) or `BNODE()` (blank nodes can't go in `VALUES`), and chains with fewer than
//! [`MIN_COSTLY`] costly calls (`REPLACE`, `REGEX` or an extension function) or without an
//! extension function (such as `spatialf:transformSRS`; casts don't count): the round trip
//! through `VALUES` costs more than cheap expressions do. Three `REPLACE`s and a `CONCAT` over
//! 420,000 rows took 9.7 s in chunks against 7.0 s in the evaluator.

use spargebra::algebra::{Expression, Function, GraphPattern, OrderExpression};
use spargebra::term::Variable;
use std::sync::atomic::{AtomicU64, Ordering};

use tripletrail_store::WhereRead;

/// Evaluates a pattern as `SELECT <variables> WHERE { <pattern> }`, with the dataset and base
/// IRI of the update operation it comes from, reading at most the given number of rows.
pub(crate) type Run<'a, E> =
    dyn Fn(GraphPattern, &[Variable], usize) -> Result<WhereRead, E> + Sync + 'a;

/// What became of the chains that were worth evaluating in parallel, since the process
/// started (see [`parallel_bind_stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParallelBindStats {
    /// Chains evaluated in parallel chunks (or, under 4096 rows, over their rows read once).
    pub taken: u64,
    /// Chains left to the evaluator because the rows under them held more values (rows ×
    /// variables) than a 256th of the join memory, or 2M if that is more.
    pub too_many_values: u64,
    /// Chains left to the evaluator because a row under them, or a row they computed, held a
    /// blank node or a triple term.
    pub blank_nodes: u64,
}

static TAKEN: AtomicU64 = AtomicU64::new(0);
static TOO_MANY: AtomicU64 = AtomicU64::new(0);
static BLANK: AtomicU64 = AtomicU64::new(0);

/// What became of the BIND chains of update `WHERE`s worth evaluating in parallel, in this
/// process so far.
pub fn parallel_bind_stats() -> ParallelBindStats {
    ParallelBindStats {
        taken: TAKEN.load(Ordering::Relaxed),
        too_many_values: TOO_MANY.load(Ordering::Relaxed),
        blank_nodes: BLANK.load(Ordering::Relaxed),
    }
}

/// The values (rows × variables) the rows under a chain may hold: a 256th of the join memory
/// ([`tripletrail_store::set_join_memory`]), about the bytes a value and what is computed from it
/// take, and at least 2M.
fn max_values() -> usize {
    (tripletrail_store::join_memory() / 256).max(MIN_VALUES)
}

/// Below this many rows, the chain is evaluated as usual (on the `VALUES` of the rows).
const PARALLEL_ROWS: usize = 4096;
/// Chains with fewer costly calls are left to the evaluator: with one `REPLACE` per row,
/// reading the rows into `VALUES` and back costs more than the cores save.
const MIN_COSTLY: usize = 3;
/// Rows per chunk at least.
const CHUNK_ROWS: usize = 1024;
/// Longer chains, and expressions nested deeper, are left alone: the chunks are evaluated on
/// the query pool's threads, whose stacks are not as deep as the query threads'.
const MAX_CHAIN: usize = 64;
const MAX_DEPTH: usize = 32;
/// The rows under a chain are read until they hold more than [`max_values`] values (rows ×
/// variables), and the chain is then left to the evaluator, which streams; at least this many.
const MIN_VALUES: usize = 1 << 21;

/// Replaces the chains of `pattern` it evaluates; `needed` is what is read of its variables
/// (all of them when `None`). Says how many chains it took.
pub(crate) fn rewrite<E: Send>(
    pattern: GraphPattern,
    needed: Option<&[Variable]>,
    run: &Run<'_, E>,
) -> Result<(GraphPattern, usize), E> {
    let mut pass = Pass { run, done: 0 };
    let pattern = pass.walk(pattern, needed.map(<[Variable]>::to_vec))?;
    Ok((pattern, pass.done))
}

enum Op {
    Extend(Variable, Expression),
    Filter(Expression),
}

struct Pass<'r, 'a, E> {
    run: &'r Run<'a, E>,
    done: usize,
}

impl<E: Send> Pass<'_, '_, E> {
    fn walk(&mut self, p: GraphPattern, needed: Option<Vec<Variable>>) -> Result<GraphPattern, E> {
        use GraphPattern as G;
        Ok(match p {
            G::Project { inner, variables } => G::Project {
                inner: Box::new(self.walk(*inner, Some(variables.clone()))?),
                variables,
            },
            G::Distinct { inner } => G::Distinct {
                inner: Box::new(self.walk(*inner, needed)?),
            },
            G::Reduced { inner } => G::Reduced {
                inner: Box::new(self.walk(*inner, needed)?),
            },
            G::Union { left, right } => G::Union {
                left: Box::new(self.walk(*left, needed.clone())?),
                right: Box::new(self.walk(*right, needed)?),
            },
            G::OrderBy { inner, expression } => {
                let mut used = Used::default();
                for e in &expression {
                    let (OrderExpression::Asc(e) | OrderExpression::Desc(e)) = e;
                    used.expression(e);
                }
                // An `EXISTS` may read any variable.
                let needed = needed.filter(|_| !used.exists).map(|mut n| {
                    used.add_to(&mut n);
                    n
                });
                G::OrderBy {
                    inner: Box::new(self.walk(*inner, needed)?),
                    expression,
                }
            }
            G::Group {
                inner,
                variables,
                aggregates,
            } => {
                let mut used = Used::default();
                for (_, a) in &aggregates {
                    used.aggregate(a);
                }
                let mut n = variables.clone();
                used.add_to(&mut n);
                // An aggregate over `*` (COUNT(*)) reads every variable.
                let needed = (!used.exists).then_some(n);
                G::Group {
                    inner: Box::new(self.walk(*inner, needed)?),
                    variables,
                    aggregates,
                }
            }
            p @ (G::Extend { .. } | G::Filter { .. }) => self.chain(p, needed)?,
            other => other,
        })
    }

    /// `p`, an `Extend` or `Filter`: its chain evaluated here if it is worth it.
    fn chain(&mut self, p: GraphPattern, needed: Option<Vec<Variable>>) -> Result<GraphPattern, E> {
        use GraphPattern as G;
        // Outermost first.
        let mut ops = Vec::new();
        let mut inner = p;
        loop {
            inner = match inner {
                G::Extend {
                    inner,
                    variable,
                    expression,
                } => {
                    ops.push(Op::Extend(variable, expression));
                    *inner
                }
                G::Filter { expr, inner } => {
                    ops.push(Op::Filter(expr));
                    *inner
                }
                other => break inner = other,
            };
        }
        // Innermost first from here on.
        ops.reverse();
        let inner = self.walk(inner, None)?;
        // Filters that read nothing the chain binds move under it, into the rows evaluated
        // first; a filter that reads what it binds leaves the chain to the evaluator.
        let mut bound: Vec<Variable> = Vec::new();
        let mut moved = Vec::new();
        let mut used = Used::default();
        let mut costly = Costly::default();
        let mut take = ops.len() <= MAX_CHAIN;
        for (k, op) in ops.iter().enumerate() {
            match op {
                Op::Extend(v, e) => {
                    bound.push(v.clone());
                    let mut own = Used::default();
                    own.expression(e);
                    // An EXISTS in the chain would read the data again, in each chunk.
                    take &= !own.exists && !own.forbidden && depth(e) <= MAX_DEPTH;
                    costly.add(e);
                    used.merge(own);
                }
                Op::Filter(e) => {
                    let mut own = Used::default();
                    own.expression(e);
                    if own.vars.iter().any(|v| bound.contains(v)) {
                        take = false;
                    } else {
                        moved.push(k);
                    }
                }
            }
        }
        if !take || costly.calls < MIN_COSTLY || costly.extensions == 0 {
            return Ok(wrap(ops, inner));
        }
        let mut kept = Vec::with_capacity(ops.len());
        let mut inner = inner;
        for op in ops {
            match op {
                Op::Filter(expr) => {
                    inner = G::Filter {
                        expr,
                        inner: Box::new(inner),
                    }
                }
                extend => kept.push(extend),
            }
        }
        let mut in_scope = Vec::new();
        inner.on_in_scope_variable(|v| {
            if !in_scope.contains(v) {
                in_scope.push(v.clone());
            }
        });
        let out: Vec<Variable> = match &needed {
            Some(n) => n
                .iter()
                .filter(|v| in_scope.contains(v) || bound.contains(v))
                .cloned()
                .collect(),
            None => in_scope.iter().chain(&bound).cloned().collect(),
        };
        // The chain reads these of the rows below it.
        let read: Vec<Variable> = in_scope
            .iter()
            .filter(|v| used.vars.contains(v) || out.contains(v))
            .cloned()
            .collect();
        let limit = max_values() / read.len().max(1);
        let bindings = match (self.run)(inner.clone(), &read, limit)? {
            WhereRead::Rows(rows) => rows,
            // Left to the evaluator.
            WhereRead::TooMany => {
                TOO_MANY.fetch_add(1, Ordering::Relaxed);
                return Ok(wrap(kept, inner));
            }
            WhereRead::BlankNode => {
                BLANK.fetch_add(1, Ordering::Relaxed);
                return Ok(wrap(kept, inner));
            }
        };
        let count = bindings.len();
        let values = G::Values {
            variables: read.clone(),
            bindings,
        };
        if count < PARALLEL_ROWS {
            self.done += 1;
            TAKEN.fetch_add(1, Ordering::Relaxed);
            return Ok(G::Project {
                inner: Box::new(wrap(kept, values)),
                variables: out,
            });
        }
        let G::Values { bindings, .. } = values else {
            unreachable!()
        };
        // On the query pool, the calling thread working too.
        let parts = tripletrail_store::par::chunks_sized(count, 0, CHUNK_ROWS, |range| {
            let pattern = wrap_ref(
                &kept,
                G::Values {
                    variables: read.clone(),
                    bindings: bindings[range.clone()].to_vec(),
                },
            );
            (self.run)(pattern, &out, range.len())
        })
        .into_iter()
        .collect::<Result<Vec<_>, E>>()?;
        let mut computed = Vec::with_capacity(count);
        for part in parts {
            let WhereRead::Rows(part) = part else {
                // A blank node was computed: the chain is evaluated as usual, over the rows.
                BLANK.fetch_add(1, Ordering::Relaxed);
                return Ok(wrap(
                    kept,
                    G::Values {
                        variables: read,
                        bindings,
                    },
                ));
            };
            computed.extend(part);
        }
        self.done += 1;
        TAKEN.fetch_add(1, Ordering::Relaxed);
        Ok(G::Values {
            variables: out,
            bindings: computed,
        })
    }
}

/// `inner` under the chain `ops` (innermost first).
fn wrap(ops: Vec<Op>, inner: GraphPattern) -> GraphPattern {
    ops.into_iter().fold(inner, |inner, op| match op {
        Op::Extend(variable, expression) => GraphPattern::Extend {
            inner: Box::new(inner),
            variable,
            expression,
        },
        Op::Filter(expr) => GraphPattern::Filter {
            expr,
            inner: Box::new(inner),
        },
    })
}

fn wrap_ref(ops: &[Op], inner: GraphPattern) -> GraphPattern {
    ops.iter().fold(inner, |inner, op| match op {
        Op::Extend(variable, expression) => GraphPattern::Extend {
            inner: Box::new(inner),
            variable: variable.clone(),
            expression: expression.clone(),
        },
        Op::Filter(expr) => GraphPattern::Filter {
            expr: expr.clone(),
            inner: Box::new(inner),
        },
    })
}

/// How many calls in `e` cost much more than reading a value: regular expressions and
/// extension functions (geometry functions, `spatialf:transformSRS`...).
#[derive(Default)]
struct Costly {
    /// `REPLACE`, `REGEX` and extension function calls.
    calls: usize,
    /// Extension function calls other than casts.
    extensions: usize,
}

impl Costly {
    fn add(&mut self, e: &Expression) {
        visit(e, &mut |e| {
            if let Expression::FunctionCall(f, _) = e {
                match f {
                    Function::Replace | Function::Regex => self.calls += 1,
                    Function::Custom(iri) => {
                        self.calls += 1;
                        if !iri
                            .as_str()
                            .starts_with("http://www.w3.org/2001/XMLSchema#")
                        {
                            self.extensions += 1;
                        }
                    }
                    _ => {}
                }
            }
        });
    }
}

/// How deep `e` nests (an `EXISTS` counts as deep).
fn depth(e: &Expression) -> usize {
    match e {
        Expression::NamedNode(_)
        | Expression::Literal(_)
        | Expression::Variable(_)
        | Expression::Bound(_) => 1,
        Expression::Exists(_) => MAX_DEPTH,
        Expression::FunctionCall(_, args) | Expression::Coalesce(args) => {
            1 + args.iter().map(depth).max().unwrap_or(0)
        }
        Expression::In(a, list) => 1 + list.iter().map(depth).max().unwrap_or(0).max(depth(a)),
        Expression::Or(a, b)
        | Expression::And(a, b)
        | Expression::Equal(a, b)
        | Expression::SameTerm(a, b)
        | Expression::Greater(a, b)
        | Expression::GreaterOrEqual(a, b)
        | Expression::Less(a, b)
        | Expression::LessOrEqual(a, b)
        | Expression::Add(a, b)
        | Expression::Subtract(a, b)
        | Expression::Multiply(a, b)
        | Expression::Divide(a, b) => 1 + depth(a).max(depth(b)),
        Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => 1 + depth(a),
        Expression::If(a, b, c) => 1 + depth(a).max(depth(b)).max(depth(c)),
    }
}

fn visit(e: &Expression, f: &mut impl FnMut(&Expression)) {
    f(e);
    match e {
        Expression::NamedNode(_)
        | Expression::Literal(_)
        | Expression::Variable(_)
        | Expression::Bound(_)
        | Expression::Exists(_) => {}
        Expression::FunctionCall(_, args) | Expression::Coalesce(args) => {
            for a in args {
                visit(a, f);
            }
        }
        Expression::In(a, list) => {
            visit(a, f);
            for l in list {
                visit(l, f);
            }
        }
        Expression::Or(a, b)
        | Expression::And(a, b)
        | Expression::Equal(a, b)
        | Expression::SameTerm(a, b)
        | Expression::Greater(a, b)
        | Expression::GreaterOrEqual(a, b)
        | Expression::Less(a, b)
        | Expression::LessOrEqual(a, b)
        | Expression::Add(a, b)
        | Expression::Subtract(a, b)
        | Expression::Multiply(a, b)
        | Expression::Divide(a, b) => {
            visit(a, f);
            visit(b, f);
        }
        Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => visit(a, f),
        Expression::If(a, b, c) => {
            visit(a, f);
            visit(b, f);
            visit(c, f);
        }
    }
}

/// Adds every variable `p` mentions, anywhere in it (in its filters, `BIND`s and nested
/// `EXISTS` too), to `out`.
fn pattern_variables(p: &GraphPattern, out: &mut Vec<Variable>) {
    use GraphPattern as G;
    use spargebra::term::{NamedNodePattern, TermPattern};
    let add = |v: &Variable, out: &mut Vec<Variable>| {
        if !out.contains(v) {
            out.push(v.clone());
        }
    };
    let expression = |e: &Expression, out: &mut Vec<Variable>| {
        let mut used = Used::default();
        used.expression(e);
        used.add_to(out);
    };
    match p {
        G::Bgp { patterns } => {
            for t in patterns {
                for term in [&t.subject, &t.object] {
                    if let TermPattern::Variable(v) = term {
                        add(v, out);
                    }
                }
                if let NamedNodePattern::Variable(v) = &t.predicate {
                    add(v, out);
                }
            }
        }
        G::Path {
            subject, object, ..
        } => {
            for term in [subject, object] {
                if let TermPattern::Variable(v) = term {
                    add(v, out);
                }
            }
        }
        G::Join { left, right }
        | G::Union { left, right }
        | G::Minus { left, right }
        | G::Lateral { left, right } => {
            pattern_variables(left, out);
            pattern_variables(right, out);
        }
        G::LeftJoin {
            left,
            right,
            expression: e,
        } => {
            pattern_variables(left, out);
            pattern_variables(right, out);
            if let Some(e) = e {
                expression(e, out);
            }
        }
        G::Filter { expr, inner } => {
            expression(expr, out);
            pattern_variables(inner, out);
        }
        G::Graph { name, inner } => {
            if let NamedNodePattern::Variable(v) = name {
                add(v, out);
            }
            pattern_variables(inner, out);
        }
        G::Extend {
            inner,
            variable,
            expression: e,
        } => {
            add(variable, out);
            expression(e, out);
            pattern_variables(inner, out);
        }
        G::Values { variables, .. } | G::Project { variables, .. } => {
            for v in variables {
                add(v, out);
            }
            if let G::Project { inner, .. } = p {
                pattern_variables(inner, out);
            }
        }
        G::OrderBy {
            inner,
            expression: es,
        } => {
            for e in es {
                let (OrderExpression::Asc(e) | OrderExpression::Desc(e)) = e;
                expression(e, out);
            }
            pattern_variables(inner, out);
        }
        G::Distinct { inner } | G::Reduced { inner } | G::Slice { inner, .. } => {
            pattern_variables(inner, out);
        }
        G::Group {
            inner,
            variables,
            aggregates,
        } => {
            for v in variables {
                add(v, out);
            }
            for (v, a) in aggregates {
                add(v, out);
                let mut used = Used::default();
                used.aggregate(a);
                used.add_to(out);
            }
            pattern_variables(inner, out);
        }
        G::Service { name, inner, .. } => {
            if let NamedNodePattern::Variable(v) = name {
                add(v, out);
            }
            pattern_variables(inner, out);
        }
    }
}

/// What expressions read.
#[derive(Default)]
struct Used {
    /// With the variables of `EXISTS` patterns.
    vars: Vec<Variable>,
    /// An `EXISTS`, which reads any variable of the row (or `COUNT(*)`, which reads them all).
    exists: bool,
    /// `NOW()` (the same value in a whole query) or `BNODE()`.
    forbidden: bool,
}

impl Used {
    fn expression(&mut self, e: &Expression) {
        visit(e, &mut |e| match e {
            Expression::Variable(v) | Expression::Bound(v) => {
                if !self.vars.contains(v) {
                    self.vars.push(v.clone());
                }
            }
            Expression::Exists(p) => {
                self.exists = true;
                pattern_variables(p, &mut self.vars);
            }
            Expression::FunctionCall(Function::Now | Function::BNode, _) => self.forbidden = true,
            _ => {}
        });
    }

    fn aggregate(&mut self, a: &spargebra::algebra::AggregateExpression) {
        match a {
            spargebra::algebra::AggregateExpression::CountSolutions { .. } => self.exists = true,
            spargebra::algebra::AggregateExpression::FunctionCall { expr, .. } => {
                self.expression(expr);
            }
        }
    }

    fn merge(&mut self, other: Self) {
        other.add_to(&mut self.vars);
        self.exists |= other.exists;
        self.forbidden |= other.forbidden;
    }

    fn add_to(&self, out: &mut Vec<Variable>) {
        for v in &self.vars {
            if !out.contains(v) {
                out.push(v.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::Literal;
    use std::sync::Mutex;

    fn pattern(query: &str) -> GraphPattern {
        let q = spargebra::SparqlParser::new()
            .with_prefix("ex", "http://example.org/")
            .unwrap()
            .with_prefix("xsd", "http://www.w3.org/2001/XMLSchema#")
            .unwrap()
            .parse_query(query)
            .unwrap();
        let spargebra::Query::Select { pattern, .. } = q else {
            panic!()
        };
        pattern
    }

    /// Rewrites `query` with a `run` that gives `n` rows for the pattern under the chain and
    /// evaluates nothing else; returns the result and the patterns `run` was asked for.
    fn rewritten(query: &str, n: usize) -> (GraphPattern, usize, Vec<GraphPattern>) {
        let calls = Mutex::new(Vec::new());
        let run = |p: GraphPattern, vars: &[Variable], limit: usize| -> Result<WhereRead, ()> {
            calls.lock().unwrap().push(p.clone());
            let rows = match &p {
                GraphPattern::Project { .. } | GraphPattern::Extend { .. } => {
                    // A chunk: one row per row of its VALUES.
                    let mut len = 0;
                    let mut q = &p;
                    loop {
                        match q {
                            GraphPattern::Extend { inner, .. }
                            | GraphPattern::Project { inner, .. } => q = inner,
                            GraphPattern::Values { bindings, .. } => {
                                len = bindings.len();
                                break;
                            }
                            _ => break,
                        }
                    }
                    len
                }
                _ => n,
            };
            if rows > limit {
                return Ok(WhereRead::TooMany);
            }
            Ok(WhereRead::Rows(
                (0..rows)
                    .map(|i| {
                        vars.iter()
                            .map(|_| Some(Literal::from(i as i64).into()))
                            .collect()
                    })
                    .collect(),
            ))
        };
        let (p, done) = rewrite(pattern(query), None, &run).unwrap();
        (p, done, calls.into_inner().unwrap())
    }

    #[test]
    fn a_costly_chain_is_computed_in_chunks_with_its_filters_moved_under_it() {
        let q = r#"SELECT ?s ?b WHERE { ?s ex:p ?x FILTER NOT EXISTS { ?s a ex:Skip }
            BIND(REPLACE(REPLACE(<urn:f>(STR(?x)), "c", "d"), "e", "f") AS ?a) BIND(CONCAT(?a, "!") AS ?b) }"#;
        let (p, done, calls) = rewritten(q, 10_000);
        assert_eq!(done, 1);
        // The rows under the chain, with the filter, then one call per chunk.
        assert!(
            matches!(&calls[0], GraphPattern::Filter { .. }),
            "{}",
            calls[0]
        );
        assert!(calls.len() > 2, "{}", calls.len());
        let GraphPattern::Project { inner, variables } = p else {
            panic!("{p}")
        };
        assert_eq!(variables.len(), 2);
        let GraphPattern::Values {
            variables,
            bindings,
        } = *inner
        else {
            panic!()
        };
        // Only what the projection reads.
        assert_eq!(
            variables.iter().map(Variable::as_str).collect::<Vec<_>>(),
            ["s", "b"]
        );
        assert_eq!(bindings.len(), 10_000);
    }

    #[test]
    fn few_rows_are_evaluated_without_chunks() {
        let q = r#"SELECT ?s ?a WHERE { ?s ex:p ?x BIND(REPLACE(REPLACE(<urn:f>(STR(?x)), "c", "d"), "e", "f") AS ?a) }"#;
        let (p, done, calls) = rewritten(q, 10);
        assert_eq!((done, calls.len()), (1, 1));
        // The chain stays, over the rows.
        assert!(format!("{p:?}").contains("Extend"), "{p}");
    }

    #[test]
    fn chains_left_to_the_evaluator() {
        for q in [
            // Nothing costly, or too little.
            r#"SELECT * WHERE { ?s ex:p ?x BIND(STR(?x) AS ?a) }"#,
            r#"SELECT * WHERE { ?s ex:p ?x BIND(REPLACE(STR(?x), "a", "b") AS ?a) }"#,
            // Costly but no extension function: casts and REPLACE are cheaper than the chunks.
            r#"SELECT * WHERE { ?s ex:p ?x BIND(REPLACE(REPLACE(REPLACE(STR(?x), "a", "b"), "c", "d"), "e", "f") AS ?a) }"#,
            r#"SELECT * WHERE { ?s ex:p ?x BIND(xsd:string(REPLACE(REPLACE(STR(?x), "a", "b"), "c", "d")) AS ?a) }"#,
            // A filter on what the chain binds.
            r#"SELECT * WHERE { ?s ex:p ?x BIND(REPLACE(<urn:f>(<urn:f>(STR(?x))), "a", "b") AS ?a) FILTER(?a != "") }"#,
            // Under a LIMIT, in an OPTIONAL, with NOW() or BNODE().
            r#"SELECT * WHERE { ?s ex:p ?x BIND(REPLACE(<urn:f>(<urn:f>(STR(?x))), "a", "b") AS ?a) } LIMIT 5"#,
            r#"SELECT * WHERE { ?s a ex:C OPTIONAL { ?s ex:p ?x BIND(REPLACE(<urn:f>(<urn:f>(STR(?x))), "a", "b") AS ?a) } }"#,
            r#"SELECT * WHERE { ?s ex:p ?x BIND(REPLACE(<urn:f>(<urn:f>(STR(?x))), "a", "b") AS ?a) BIND(NOW() AS ?t) }"#,
            r#"SELECT * WHERE { ?s ex:p ?x BIND(REPLACE(<urn:f>(<urn:f>(STR(?x))), "a", "b") AS ?a) BIND(BNODE() AS ?t) }"#,
            // A filter whose EXISTS reads what the chain binds, inside a filter of its own.
            r#"SELECT * WHERE { ?s ex:p ?x BIND(REPLACE(<urn:f>(<urn:f>(STR(?x))), "a", "b") AS ?a) FILTER NOT EXISTS { ?s ex:q ?o FILTER(?o = ?a) } }"#,
            // An EXISTS in the chain.
            r#"SELECT * WHERE { ?s ex:p ?x BIND(IF(EXISTS { ?s ex:q ?x }, REPLACE(<urn:f>(<urn:f>(?x)), "a", "b"), "") AS ?a) }"#,
        ] {
            let (_, done, calls) = rewritten(q, 10_000);
            assert_eq!((done, calls.len()), (0, 0), "{q}");
        }
    }
}
