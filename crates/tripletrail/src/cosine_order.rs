//! `ORDER BY DESC(tt:cosine(?v, "[…]"^^tt:vector)) LIMIT k` through the vector index (#236).
//!
//! The query is evaluated again with `?v` restricted to the vectors the index finds most
//! similar, comparing every one of them (exact, unlike `tt:nearest` past its graph size),
//! and kept to the rows whose cosine is above what any vector left out can reach. Those rows
//! are the first ones of the whole query, so once there are `k` of them they are its answer;
//! until then the restriction takes in more vectors, and past them all the query runs as
//! written.

use crate::ttfn::MG;
use crate::vector;
use oxigraph::model::NamedNode;
use spargebra::Query;
use spargebra::algebra::{Expression, Function, GraphPattern, OrderExpression};
use spargebra::term::{NamedNodePattern, TermPattern, Variable};

/// A query whose first ordering is the cosine of a variable and a constant vector, under a
/// `LIMIT`.
#[derive(Debug)]
pub struct CosineOrder {
    /// The `LIMIT`: the restricted query is the answer once it returns this many rows.
    pub limit: usize,
    /// The `OFFSET`: the rows skipped before the `LIMIT`.
    pub offset: usize,
    /// The variable holding the vectors.
    pub var: Variable,
    /// The constant vector.
    pub vector: Vec<f64>,
    /// The only property whose objects the variable holds.
    pub property: NamedNode,
    /// The cosine, as the query computes it.
    cosine: Expression,
}

/// The cosine ordering of `query`, if it has one: a `SELECT` whose pattern is `LIMIT` over (`DISTINCT`, `REDUCED`, projection and) `ORDER BY` with
/// `DESC(tt:cosine(?v, vector))` first, or `DESC(?score)` with `?score` bound to that cosine,
/// and `?v` the object of one property in every row.
pub fn cosine_order(query: &Query) -> Option<CosineOrder> {
    let Query::Select { pattern, .. } = query else {
        return None;
    };
    let GraphPattern::Slice {
        inner,
        start,
        length: Some(limit),
    } = pattern
    else {
        return None;
    };
    let mut p = &**inner;
    while let GraphPattern::Distinct { inner }
    | GraphPattern::Reduced { inner }
    | GraphPattern::Project { inner, .. } = p
    {
        p = inner;
    }
    let GraphPattern::OrderBy { inner, expression } = p else {
        return None;
    };
    let Some(OrderExpression::Desc(first)) = expression.first() else {
        return None;
    };
    let cosine = match first {
        Expression::Variable(v) => bound_to(inner, v)?,
        e => e.clone(),
    };
    let (var, vector) = cosine_of(&cosine)?;
    let property = only_property(inner, &var)?;
    let covered =
        |p: &NamedNodePattern| matches!(p, NamedNodePattern::NamedNode(n) if *n == property);
    if !tripletrail_spatial::binds_through(inner, &var, &covered, false) {
        return None;
    }
    Some(CosineOrder {
        limit: *limit,
        offset: *start,
        var,
        vector,
        property,
        cosine,
    })
}

/// The expression `v` is bound to by a `BIND` under `pattern`, through other `BIND`s and
/// `FILTER`s only.
fn bound_to(mut pattern: &GraphPattern, v: &Variable) -> Option<Expression> {
    loop {
        match pattern {
            GraphPattern::Extend {
                variable,
                expression,
                ..
            } if variable == v => return Some(expression.clone()),
            GraphPattern::Extend { inner, .. } | GraphPattern::Filter { inner, .. } => {
                pattern = inner
            }
            _ => return None,
        }
    }
}

/// The variable and the constant vector of `tt:cosine(?v, vector)`, in either order.
fn cosine_of(e: &Expression) -> Option<(Variable, Vec<f64>)> {
    let Expression::FunctionCall(Function::Custom(f), args) = e else {
        return None;
    };
    if f.as_str().strip_prefix(MG) != Some("cosine") {
        return None;
    }
    let (var, constant) = match args.as_slice() {
        [Expression::Variable(v), Expression::Literal(l)]
        | [Expression::Literal(l), Expression::Variable(v)] => (v, l),
        _ => return None,
    };
    let vector = vector::parse(&oxigraph::model::Term::Literal(constant.clone()))?;
    Some((var.clone(), vector))
}

/// The property of the triples with `var` for object under `pattern`, if there is only one.
fn only_property(pattern: &GraphPattern, var: &Variable) -> Option<NamedNode> {
    let mut found: Option<NamedNode> = None;
    let mut one = true;
    walk(pattern, &mut |t| {
        if matches!(&t.object, TermPattern::Variable(v) if v == var) {
            match (&t.predicate, &found) {
                (NamedNodePattern::NamedNode(p), None) => found = Some(p.clone()),
                (NamedNodePattern::NamedNode(p), Some(f)) if p == f => {}
                _ => one = false,
            }
        }
    });
    found.filter(|_| one)
}

fn walk(pattern: &GraphPattern, f: &mut dyn FnMut(&spargebra::term::TriplePattern)) {
    match pattern {
        GraphPattern::Bgp { patterns } => patterns.iter().for_each(&mut *f),
        GraphPattern::Join { left, right }
        | GraphPattern::LeftJoin { left, right, .. }
        | GraphPattern::Lateral { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right } => {
            walk(left, f);
            walk(right, f);
        }
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Graph { inner, .. }
        | GraphPattern::Extend { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::Group { inner, .. }
        | GraphPattern::Service { inner, .. } => walk(inner, f),
        GraphPattern::Path { .. } | GraphPattern::Values { .. } => {}
    }
}

impl CosineOrder {
    /// `query` with the rows under its `ORDER BY` joined to `table` (the vectors kept) and,
    /// with `above`, kept to those whose cosine is more than it.
    pub fn restricted(&self, query: &Query, table: GraphPattern, above: Option<f64>) -> Query {
        fn add(p: &GraphPattern, f: &dyn Fn(&GraphPattern) -> GraphPattern) -> GraphPattern {
            let rec = |inner: &GraphPattern| Box::new(add(inner, f));
            match p {
                GraphPattern::Slice {
                    inner,
                    start,
                    length,
                } => GraphPattern::Slice {
                    inner: rec(inner),
                    start: *start,
                    length: *length,
                },
                GraphPattern::Distinct { inner } => GraphPattern::Distinct { inner: rec(inner) },
                GraphPattern::Reduced { inner } => GraphPattern::Reduced { inner: rec(inner) },
                GraphPattern::Project { inner, variables } => GraphPattern::Project {
                    inner: rec(inner),
                    variables: variables.clone(),
                },
                GraphPattern::OrderBy { inner, expression } => GraphPattern::OrderBy {
                    inner: Box::new(f(inner)),
                    expression: expression.clone(),
                },
                other => other.clone(),
            }
        }
        let restrict = |inner: &GraphPattern| {
            let joined = GraphPattern::Join {
                left: Box::new(table.clone()),
                right: Box::new(inner.clone()),
            };
            match above {
                Some(t) => GraphPattern::Filter {
                    expr: Expression::Greater(
                        Box::new(self.cosine.clone()),
                        Box::new(Expression::Literal(
                            spargebra::term::Literal::new_typed_literal(
                                format!("{t:e}"),
                                oxigraph::model::vocab::xsd::DOUBLE,
                            ),
                        )),
                    ),
                    inner: Box::new(joined),
                },
                None => joined,
            }
        };
        let Query::Select {
            dataset,
            pattern,
            base_iri,
        } = query
        else {
            return query.clone();
        };
        Query::Select {
            dataset: dataset.clone(),
            pattern: add(pattern, &restrict),
            base_iri: base_iri.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(q: &str) -> Option<CosineOrder> {
        cosine_order(&spargebra::SparqlParser::new().parse_query(q).unwrap())
    }

    const P: &str = "PREFIX tt: <http://w3id.org/tripletrail/fn#> PREFIX ex: <http://x/> ";

    #[test]
    fn finds_the_cosine_ordering() {
        let o = order(&format!(
            "{P} SELECT ?s WHERE {{ ?s ex:e ?v ; ex:l ?l }} \
             ORDER BY DESC(tt:cosine(?v, \"[1, 0]\"^^tt:vector)) LIMIT 3"
        ))
        .unwrap();
        assert_eq!(
            (o.limit, o.var.as_str(), o.vector.as_slice()),
            (3, "v", &[1.0, 0.0][..])
        );
        assert_eq!(o.property.as_str(), "http://x/e");
        let o = order(&format!(
            "{P} SELECT ?s ?c WHERE {{ ?s ex:e ?v BIND(tt:cosine(\"[1, 0]\", ?v) AS ?c) }} \
             ORDER BY DESC(?c) ?s LIMIT 3"
        ));
        assert!(o.is_some());
    }

    #[test]
    fn leaves_other_orderings_alone() {
        for q in [
            // Ascending: the least similar first.
            "SELECT ?s WHERE { ?s ex:e ?v } ORDER BY tt:cosine(?v, \"[1]\") LIMIT 3",
            // No limit.
            "SELECT ?s WHERE { ?s ex:e ?v } ORDER BY DESC(tt:cosine(?v, \"[1]\"))",
            // Another function.
            "SELECT ?s WHERE { ?s ex:e ?v } ORDER BY DESC(tt:dot(?v, \"[1]\")) LIMIT 3",
            // Two properties hold ?v.
            "SELECT ?s WHERE { { ?s ex:e ?v } UNION { ?s ex:f ?v } } \
             ORDER BY DESC(tt:cosine(?v, \"[1]\")) LIMIT 3",
            // ?v is optional.
            "SELECT ?s WHERE { ?s ex:l ?l OPTIONAL { ?s ex:e ?v } } \
             ORDER BY DESC(tt:cosine(?v, \"[1]\")) LIMIT 3",
            // Not a vector.
            "SELECT ?s WHERE { ?s ex:e ?v } ORDER BY DESC(tt:cosine(?v, \"one\")) LIMIT 3",
        ] {
            assert!(order(&format!("{P} {q}")).is_none(), "{q}");
        }
    }
}
