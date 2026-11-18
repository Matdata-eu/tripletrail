//! Zero-length paths from a term that is not in the store (W3C property-path tests
//! `zero_or_more_set_start` and the like). `?s :p* :o` matches `?s = :o` through the
//! zero-length path even when `:o` appears in no triple, but Oxigraph's evaluator only follows
//! paths from nodes of the dataset and gives no row. With `:o` the subject or object of no
//! quad of the store, the paths of one or more steps match nothing, so the pattern is exactly
//! `VALUES ?s { :o }`. That holds for a term the store knows only as a predicate or a graph
//! name, or not at all, for `^(p*)` as for `p*`, and inside `EXISTS` too.

use oxigraph::model::GraphName;
use oxigraph::sparql::QueryDataset;
use spargebra::Query;
use spargebra::algebra::{Expression, GraphPattern, OrderExpression, PropertyPathExpression};
use spargebra::term::{GroundTerm, TermPattern};
use tripletrail_store::{GraphSelector, Snapshot};

/// The graphs of the dataset `query` is evaluated on: `dataset` if given, else the query's
/// own `FROM` / `FROM NAMED`, else every graph. A term is a node of the dataset when it is the
/// subject or object of a quad of one of them; one that is a node only of other graphs is as
/// absent (#526: a caller who may read some graphs only must not learn otherwise).
fn dataset_graphs(
    query: &Query,
    dataset: Option<&QueryDataset>,
    snapshot: &Snapshot,
) -> Vec<GraphSelector> {
    let id = |g: oxigraph::model::NamedOrBlankNodeRef<'_>| snapshot.term_id(g.into());
    if let Some(d) = dataset {
        let mut graphs = Vec::new();
        match d.default_graph_graphs() {
            None => return vec![GraphSelector::All],
            Some(list) => {
                for g in list {
                    match g {
                        GraphName::DefaultGraph => graphs.push(GraphSelector::Default),
                        GraphName::NamedNode(n) => {
                            graphs.extend(id(n.as_ref().into()).map(GraphSelector::Graph))
                        }
                        GraphName::BlankNode(b) => {
                            graphs.extend(id(b.as_ref().into()).map(GraphSelector::Graph))
                        }
                    }
                }
            }
        }
        match d.available_named_graphs() {
            None => graphs.push(GraphSelector::AnyNamed),
            Some(list) => graphs.extend(
                list.iter()
                    .filter_map(|g| id(g.as_ref()))
                    .map(GraphSelector::Graph),
            ),
        }
        return graphs;
    }
    let own = match query {
        Query::Select { dataset, .. }
        | Query::Construct { dataset, .. }
        | Query::Describe { dataset, .. }
        | Query::Ask { dataset, .. } => dataset.as_ref(),
    };
    match own {
        None => vec![GraphSelector::All],
        Some(own) => {
            let mut graphs: Vec<GraphSelector> = own
                .default
                .iter()
                .chain(own.named.iter().flatten())
                .filter_map(|n| id(n.as_ref().into()))
                .map(GraphSelector::Graph)
                .collect();
            if own.named.is_none() {
                graphs.push(GraphSelector::AnyNamed);
            }
            graphs
        }
    }
}

/// Rewrites the `*` and `?` paths of `query` with a constant end that is no node of the
/// dataset it is evaluated on (see [`dataset_graphs`]).
pub(crate) fn rewrite(query: Query, snapshot: &Snapshot, dataset: Option<&QueryDataset>) -> Query {
    let graphs = dataset_graphs(&query, dataset, snapshot);
    let absent = |t: &TermPattern| -> Option<GroundTerm> {
        let term: GroundTerm = match t {
            TermPattern::NamedNode(n) => n.clone().into(),
            TermPattern::Literal(l) => l.clone().into(),
            _ => return None,
        };
        let id = match &term {
            GroundTerm::NamedNode(n) => snapshot.term_id(n.as_ref().into()),
            GroundTerm::Literal(l) => snapshot.term_id(l.as_ref().into()),
            #[allow(unreachable_patterns)]
            _ => return None,
        };
        // A node of the graph is the subject or the object of a quad.
        let node = id.is_some_and(|id| {
            graphs.iter().any(|&g| {
                snapshot
                    .quad_ids_for_pattern(Some(id), None, None, g)
                    .next()
                    .is_some()
                    || snapshot
                        .quad_ids_for_pattern(None, None, Some(id), g)
                        .next()
                        .is_some()
            })
        });
        (!node).then_some(term)
    };
    let pass = |p: GraphPattern| walk(p, &absent);
    match query {
        Query::Select {
            dataset,
            pattern,
            base_iri,
        } => Query::Select {
            dataset,
            pattern: pass(pattern),
            base_iri,
        },
        Query::Construct {
            template,
            dataset,
            pattern,
            base_iri,
        } => Query::Construct {
            template,
            dataset,
            pattern: pass(pattern),
            base_iri,
        },
        Query::Describe {
            dataset,
            pattern,
            base_iri,
        } => Query::Describe {
            dataset,
            pattern: pass(pattern),
            base_iri,
        },
        Query::Ask {
            dataset,
            pattern,
            base_iri,
        } => Query::Ask {
            dataset,
            pattern: pass(pattern),
            base_iri,
        },
    }
}

fn walk(p: GraphPattern, absent: &dyn Fn(&TermPattern) -> Option<GroundTerm>) -> GraphPattern {
    use GraphPattern as G;
    let b = |p: Box<G>| Box::new(walk(*p, absent));
    match p {
        G::Path {
            subject,
            path,
            object,
        } if zero_length(&path) => {
            let one = |variable: &spargebra::term::Variable, term: GroundTerm| G::Values {
                variables: vec![variable.clone()],
                bindings: vec![vec![Some(term)]],
            };
            match (&subject, &object) {
                (TermPattern::Variable(s), o) if let Some(term) = absent(o) => one(s, term),
                (s, TermPattern::Variable(o)) if let Some(term) = absent(s) => one(o, term),
                (s, o) if s == o && absent(s).is_some() => G::Bgp {
                    patterns: Vec::new(),
                },
                _ => G::Path {
                    subject,
                    path,
                    object,
                },
            }
        }
        G::Join { left, right } => G::Join {
            left: b(left),
            right: b(right),
        },
        G::LeftJoin {
            left,
            right,
            expression: e,
        } => G::LeftJoin {
            left: b(left),
            right: b(right),
            expression: e.map(|e| expression(e, absent)),
        },
        G::Lateral { left, right } => G::Lateral {
            left: b(left),
            right: b(right),
        },
        G::Union { left, right } => G::Union {
            left: b(left),
            right: b(right),
        },
        G::Minus { left, right } => G::Minus {
            left: b(left),
            right: b(right),
        },
        G::Filter { expr, inner } => G::Filter {
            expr: expression(expr, absent),
            inner: b(inner),
        },
        G::Extend {
            inner,
            variable,
            expression: e,
        } => G::Extend {
            inner: b(inner),
            variable,
            expression: expression(e, absent),
        },
        G::Graph { name, inner } => G::Graph {
            name,
            inner: b(inner),
        },
        G::OrderBy {
            inner,
            expression: e,
        } => G::OrderBy {
            inner: b(inner),
            expression: e
                .into_iter()
                .map(|o| match o {
                    OrderExpression::Asc(e) => OrderExpression::Asc(expression(e, absent)),
                    OrderExpression::Desc(e) => OrderExpression::Desc(expression(e, absent)),
                })
                .collect(),
        },
        G::Project { inner, variables } => G::Project {
            inner: b(inner),
            variables,
        },
        G::Distinct { inner } => G::Distinct { inner: b(inner) },
        G::Reduced { inner } => G::Reduced { inner: b(inner) },
        G::Slice {
            inner,
            start,
            length,
        } => G::Slice {
            inner: b(inner),
            start,
            length,
        },
        G::Group {
            inner,
            variables,
            aggregates,
        } => G::Group {
            inner: b(inner),
            variables,
            aggregates,
        },
        other => other,
    }
}

/// Whether a path matches zero steps: `p*`, `p?`, `p{0,m}`, and those reversed.
fn zero_length(path: &PropertyPathExpression) -> bool {
    match path {
        PropertyPathExpression::ZeroOrMore(_)
        | PropertyPathExpression::ZeroOrOne(_)
        | PropertyPathExpression::Bounded { min: 0, .. } => true,
        PropertyPathExpression::Reverse(inner) => zero_length(inner),
        _ => false,
    }
}

/// The patterns of the `EXISTS` in an expression, rewritten.
fn expression(e: Expression, absent: &dyn Fn(&TermPattern) -> Option<GroundTerm>) -> Expression {
    use Expression as E;
    let b = |e: Box<E>| Box::new(expression(*e, absent));
    let all = |v: Vec<E>| v.into_iter().map(|e| expression(e, absent)).collect();
    match e {
        E::Exists(p) => E::Exists(Box::new(walk(*p, absent))),
        E::Or(l, r) => E::Or(b(l), b(r)),
        E::And(l, r) => E::And(b(l), b(r)),
        E::Equal(l, r) => E::Equal(b(l), b(r)),
        E::SameTerm(l, r) => E::SameTerm(b(l), b(r)),
        E::Greater(l, r) => E::Greater(b(l), b(r)),
        E::GreaterOrEqual(l, r) => E::GreaterOrEqual(b(l), b(r)),
        E::Less(l, r) => E::Less(b(l), b(r)),
        E::LessOrEqual(l, r) => E::LessOrEqual(b(l), b(r)),
        E::In(l, r) => E::In(b(l), all(r)),
        E::Add(l, r) => E::Add(b(l), b(r)),
        E::Subtract(l, r) => E::Subtract(b(l), b(r)),
        E::Multiply(l, r) => E::Multiply(b(l), b(r)),
        E::Divide(l, r) => E::Divide(b(l), b(r)),
        E::UnaryPlus(i) => E::UnaryPlus(b(i)),
        E::UnaryMinus(i) => E::UnaryMinus(b(i)),
        E::Not(i) => E::Not(b(i)),
        E::If(c, t, f) => E::If(b(c), b(t), b(f)),
        E::Coalesce(v) => E::Coalesce(all(v)),
        E::FunctionCall(f, v) => E::FunctionCall(f, all(v)),
        other => other,
    }
}
