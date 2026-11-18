//! Planning of correlated sub-patterns (`EXISTS` / `NOT EXISTS`).
//!
//! Oxigraph's optimizer reorders the joins of the main pattern but not those inside an
//! `EXISTS` expression: the pattern keeps its written order and is evaluated as hash joins for
//! every outer row. When the pattern is connected to the outer row only through its last triple
//! (`FILTER NOT EXISTS { ?s a ex:C ; ex:p/ex:q ?outer }`), every outer row scans all of `ex:C`
//! and joins it with the rest, so the cost grows with outer rows × inner pattern size.
//!
//! This pass orders the triple patterns of such a sub-pattern starting from the variables the
//! outer row binds, and chains them with `LATERAL` joins, which evaluate each next pattern once
//! per binding of the previous ones: every step becomes an index lookup on a bound term.

use spargebra::algebra::{Expression, Function, GraphPattern};
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};
use spargebra::{GraphUpdateOperation, Query, Update};
use std::collections::HashSet;

/// A name standing for a variable or a blank node (blank nodes act as variables in patterns).
type Name = String;

pub(crate) fn plan_query(query: Query) -> Query {
    match crate::xfn::alias_query(query) {
        Query::Select {
            dataset,
            pattern,
            base_iri,
        } => Query::Select {
            dataset,
            pattern: walk(tripletrail_fx::correlate(service_names(pattern))),
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
            pattern: walk(tripletrail_fx::correlate(service_names(pattern))),
            base_iri,
        },
        Query::Describe {
            dataset,
            pattern,
            base_iri,
        } => Query::Describe {
            dataset,
            pattern: walk(tripletrail_fx::correlate(service_names(pattern))),
            base_iri,
        },
        Query::Ask {
            dataset,
            pattern,
            base_iri,
        } => Query::Ask {
            dataset,
            pattern: walk(tripletrail_fx::correlate(service_names(pattern))),
            base_iri,
        },
    }
}

pub(crate) fn plan_update(update: Update) -> Update {
    let update = crate::xfn::alias_update(update);
    Update {
        operations: update
            .operations
            .into_iter()
            .map(|op| match op {
                GraphUpdateOperation::DeleteInsert {
                    delete,
                    insert,
                    using,
                    pattern,
                } => GraphUpdateOperation::DeleteInsert {
                    delete,
                    insert,
                    using,
                    pattern: Box::new(walk(tripletrail_fx::correlate(service_names(*pattern)))),
                },
                other => other,
            })
            .collect(),
        base_iri: update.base_iri,
    }
}

/// `pattern` with each `SERVICE ?endpoint` joined to the patterns before it evaluated once per
/// row of them (a `LATERAL`), as Fuseki does: evaluated on its own, as the join of the algebra
/// has it, the endpoint variable would never be bound (W3C service05). Done with the
/// optimizer off too, since without it the query cannot be answered at all. The same for
/// `OPTIONAL { SERVICE ?endpoint … }`, and for a group whose SERVICE waits for an endpoint
/// from outside it (`OPTIONAL { SERVICE ?endpoint { … } ?s ex:p ?o }`).
fn service_names(p: GraphPattern) -> GraphPattern {
    use GraphPattern as G;
    let b = |p: Box<G>| Box::new(service_names(*p));
    match p {
        G::Join { left, right } => {
            let (left, right) = (b(left), b(right));
            if waits_for_endpoint(&right) {
                G::Lateral { left, right }
            } else {
                G::Join { left, right }
            }
        }
        G::LeftJoin {
            left,
            right,
            expression,
        } => {
            let (left, right) = (b(left), b(right));
            if waits_for_endpoint(&right) {
                // `OPTIONAL { SERVICE ?endpoint … }`: per row, the SERVICE's rows or the row
                // alone, which is what the left join gives.
                G::Lateral {
                    left,
                    right: Box::new(G::LeftJoin {
                        left: Box::new(G::Bgp {
                            patterns: Vec::new(),
                        }),
                        right,
                        expression,
                    }),
                }
            } else {
                G::LeftJoin {
                    left,
                    right,
                    expression,
                }
            }
        }
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
            expr,
            inner: b(inner),
        },
        G::Graph { name, inner } => G::Graph {
            name,
            inner: b(inner),
        },
        G::Extend {
            inner,
            variable,
            expression,
        } => G::Extend {
            inner: b(inner),
            variable,
            expression,
        },
        G::OrderBy { inner, expression } => G::OrderBy {
            inner: b(inner),
            expression,
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
        G::Service {
            name,
            inner,
            silent,
        } => G::Service {
            name,
            inner: b(inner),
            silent,
        },
        other => other,
    }
}

/// Whether `p` has a `SERVICE ?endpoint` whose endpoint nothing in `p` binds: it needs the
/// endpoint from the patterns before it.
fn waits_for_endpoint(p: &GraphPattern) -> bool {
    let mut names = Vec::new();
    endpoint_variables(p, &mut names);
    names.iter().any(|v| !binds(p, v))
}

/// The variables naming the endpoint of a SERVICE in `p`, in the group or the groups joined
/// with it (not in sub-selects or in a SERVICE's own pattern).
fn endpoint_variables<'a>(p: &'a GraphPattern, out: &mut Vec<&'a spargebra::term::Variable>) {
    use GraphPattern as G;
    match p {
        G::Service {
            name: NamedNodePattern::Variable(v),
            ..
        } => out.push(v),
        G::Join { left, right }
        | G::LeftJoin { left, right, .. }
        | G::Lateral { left, right }
        | G::Union { left, right } => {
            endpoint_variables(left, out);
            endpoint_variables(right, out);
        }
        G::Minus { left, .. } => endpoint_variables(left, out),
        G::Filter { inner, .. } | G::Extend { inner, .. } | G::Graph { inner, .. } => {
            endpoint_variables(inner, out)
        }
        _ => {}
    }
}

/// Whether some pattern of `p` can bind `v` (a SERVICE's endpoint is not one).
fn binds(p: &GraphPattern, v: &spargebra::term::Variable) -> bool {
    use GraphPattern as G;
    let term = |t: &TermPattern| matches!(t, TermPattern::Variable(x) if x == v);
    match p {
        G::Bgp { patterns } => patterns.iter().any(|t| {
            term(&t.subject)
                || term(&t.object)
                || matches!(&t.predicate, NamedNodePattern::Variable(x) if x == v)
        }),
        G::Path {
            subject, object, ..
        } => term(subject) || term(object),
        G::Values { variables, .. } => variables.contains(v),
        G::Extend {
            inner, variable, ..
        } => variable == v || binds(inner, v),
        G::Graph { name, inner } => {
            matches!(name, NamedNodePattern::Variable(x) if x == v) || binds(inner, v)
        }
        G::Project { inner, variables } => variables.contains(v) && binds(inner, v),
        G::Group {
            variables,
            aggregates,
            ..
        } => variables.contains(v) || aggregates.iter().any(|(a, _)| a == v),
        G::Service { inner, .. } => binds(inner, v),
        G::Join { left, right }
        | G::LeftJoin { left, right, .. }
        | G::Lateral { left, right }
        | G::Union { left, right } => binds(left, v) || binds(right, v),
        G::Minus { left, .. } => binds(left, v),
        G::Filter { inner, .. }
        | G::OrderBy { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::Slice { inner, .. } => binds(inner, v),
    }
}

fn walk(p: GraphPattern) -> GraphPattern {
    use GraphPattern as G;
    // `#pragma tt.optimizer off` (#238): the patterns as written.
    if crate::Hints::current().optimizer_off {
        return p;
    }
    let b = |p: Box<G>| Box::new(walk(*p));
    match p {
        G::Filter { expr, inner } => {
            let inner = walk(*inner);
            let bound = names_of_pattern(&inner);
            G::Filter {
                expr: expression(expr, &bound),
                inner: Box::new(inner),
            }
        }
        G::Extend {
            inner,
            variable,
            expression: e,
        } => {
            let inner = walk(*inner);
            let bound = names_of_pattern(&inner);
            G::Extend {
                expression: expression(e, &bound),
                inner: Box::new(inner),
                variable,
            }
        }
        G::LeftJoin {
            left,
            right,
            expression: e,
        } => {
            let (left, right) = (walk(*left), walk(*right));
            let e = e.map(|e| {
                let mut bound = names_of_pattern(&left);
                bound.extend(names_of_pattern(&right));
                expression(e, &bound)
            });
            G::LeftJoin {
                left: Box::new(left),
                right: Box::new(right),
                expression: e,
            }
        }
        G::Join { left, right } => service_first(hoist_extends(walk(*left), walk(*right))),
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
        G::Graph { name, inner } => G::Graph {
            name,
            inner: b(inner),
        },
        G::OrderBy { inner, expression } => G::OrderBy {
            inner: b(inner),
            expression,
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
        } => component_sizes(G::Group {
            inner: b(inner),
            variables,
            aggregates,
        }),
        other @ (G::Bgp { .. } | G::Path { .. } | G::Values { .. } | G::Service { .. }) => other,
    }
}

/// Joins `left` and `right`, moving the `BIND`s that end either operand above the join:
///
/// ```sparql
/// ?t a :Track ; :loc/:on ?ne .
/// BIND(REPLACE(STR(?ne), "...", "") AS ?id)
/// ?ne time:hasBeginning ?b .
/// ```
///
/// is `Join(Extend(P1, ?id), P2)`: Oxigraph's optimizer does not reorder the joins of `P1`
/// with `P2` through the `BIND`, and joins a block of spatial candidates (a `VALUES` the
/// spatial rewrite puts beside a `FILTER(geof:sfContains(...))` after `BIND`s) with the
/// cartesian product of the patterns under the `BIND`s. As `Extend(Join(P1, P2), ?id)`, the
/// triple patterns of both sides are ordered together.
///
/// The rows are the same when the `BIND` variable is not used by the other side and the
/// variables of its expression that the other side uses are always bound by `P1` (bound to the
/// same values in every joined row). Expressions that give a new value at each call (`BNODE()`,
/// `RAND()`, `UUID()`, `STRUUID()`) or hold an `EXISTS` stay where they are.
fn hoist_extends(left: GraphPattern, right: GraphPattern) -> GraphPattern {
    fn peel(
        mut p: GraphPattern,
        other: &HashSet<Name>,
        lifted: &mut Vec<(spargebra::term::Variable, Expression)>,
    ) -> GraphPattern {
        loop {
            match p {
                GraphPattern::Extend {
                    inner,
                    variable,
                    expression,
                } if !other.contains(variable.as_str()) && {
                    let mut vars = HashSet::new();
                    expression_variables(&expression, &mut vars).is_some_and(|()| {
                        let certain = certain(&inner);
                        vars.iter()
                            .all(|v| !other.contains(v) || certain.contains(v))
                    })
                } =>
                {
                    lifted.push((variable, expression));
                    p = *inner;
                }
                p => return p,
            }
        }
    }
    let mut lifted = Vec::new();
    let left = peel(left, &names_of_pattern(&right), &mut lifted);
    // The right side's BINDs must not see the variables of the left side's either.
    let mut left_names = names_of_pattern(&left);
    left_names.extend(lifted.iter().map(|(v, _)| v.as_str().to_owned()));
    let right = peel(right, &left_names, &mut lifted);
    if lifted.is_empty() {
        return GraphPattern::Join {
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    // The join of what is left, with nested joins merged so the optimizer sees all operands.
    let mut out = GraphPattern::Join {
        left: Box::new(left),
        right: Box::new(right),
    };
    // Peeled outermost first, per side: re-applied innermost first. The two sides bind
    // different variables, so their relative order does not matter.
    for (variable, expression) in lifted.into_iter().rev() {
        out = GraphPattern::Extend {
            inner: Box::new(out),
            variable,
            expression,
        };
    }
    out
}

/// Adds the variables of `e` to `out`; `None` when `e` holds an `EXISTS` or a function that
/// gives a new value at each call.
fn expression_variables(e: &Expression, out: &mut HashSet<Name>) -> Option<()> {
    use spargebra::algebra::Function as F;
    match e {
        Expression::Variable(v) | Expression::Bound(v) => {
            out.insert(v.as_str().to_owned());
        }
        Expression::NamedNode(_) | Expression::Literal(_) => {}
        Expression::Exists(_) => return None,
        Expression::FunctionCall(F::BNode | F::Rand | F::Uuid | F::StrUuid, _) => return None,
        Expression::FunctionCall(_, args) | Expression::Coalesce(args) => {
            for a in args {
                expression_variables(a, out)?;
            }
        }
        Expression::In(a, list) => {
            expression_variables(a, out)?;
            for l in list {
                expression_variables(l, out)?;
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
            expression_variables(a, out)?;
            expression_variables(b, out)?;
        }
        Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
            expression_variables(a, out)?;
        }
        Expression::If(a, b, c) => {
            expression_variables(a, out)?;
            expression_variables(b, out)?;
            expression_variables(c, out)?;
        }
    }
    Some(())
}

/// The variables (and blank nodes) bound in every row of `p`. Under-approximates.
pub(crate) fn certain(p: &GraphPattern) -> HashSet<Name> {
    use GraphPattern as G;
    match p {
        G::Bgp { .. } | G::Path { .. } => names_of_pattern(p),
        G::Join { left, right } | G::Lateral { left, right } => {
            let mut out = certain(left);
            out.extend(certain(right));
            out
        }
        G::LeftJoin { left, .. } | G::Minus { left, .. } => certain(left),
        G::Union { left, right } => {
            let right = certain(right);
            certain(left)
                .into_iter()
                .filter(|n| right.contains(n))
                .collect()
        }
        G::Graph { name, inner } => {
            let mut out = certain(inner);
            if let NamedNodePattern::Variable(v) = name {
                out.insert(v.as_str().to_owned());
            }
            out
        }
        G::Values {
            variables,
            bindings,
        } => variables
            .iter()
            .enumerate()
            .filter(|(i, _)| bindings.iter().all(|row| row[*i].is_some()))
            .map(|(_, v)| v.as_str().to_owned())
            .collect(),
        G::Project { inner, variables } => {
            let inner = certain(inner);
            variables
                .iter()
                .map(|v| v.as_str().to_owned())
                .filter(|v| inner.contains(v))
                .collect()
        }
        G::Filter { inner, .. }
        | G::Extend { inner, .. }
        | G::OrderBy { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::Slice { inner, .. } => certain(inner),
        G::Group {
            inner, variables, ..
        } => {
            let inner = certain(inner);
            variables
                .iter()
                .map(|v| v.as_str().to_owned())
                .filter(|v| inner.contains(v))
                .collect()
        }
        G::Service { .. } => HashSet::new(),
    }
}

/// Whether `p` is a `SERVICE` call, possibly under `BIND`s and `FILTER`s on its rows. Not a
/// `SERVICE SILENT`: when it fails it gives one empty row, which must join every row of the
/// rest, not be dropped for leaving the linking variables unbound.
fn is_service(p: &GraphPattern) -> bool {
    match p {
        GraphPattern::Service { silent, .. } => !silent,
        GraphPattern::Extend { inner, .. } | GraphPattern::Filter { inner, .. } => {
            is_service(inner)
        }
        _ => false,
    }
}

/// Starts a join from its `SERVICE` operands (a route from the in-process `pgr` service, say),
/// and looks up the triples connected to their rows once per row, chained with `LATERAL`
/// joins, instead of joining the rows with a full scan of those triples:
///
/// ```sparql
/// SERVICE <http://w3id.org/tripletrail/pgr> { ... pgr:node ?node }
/// BIND(IRI(STRBEFORE(?node, "#")) AS ?e)
/// ?e geo:hasGeometry/geo:asWKT ?ew
/// ```
///
/// Oxigraph's optimizer has no estimate for a `SERVICE`, and a variable bound by `BIND` is no
/// join key for it, so it would read every `geo:hasGeometry` and `geo:asWKT` triple first. A
/// `LATERAL` join of triple patterns gives the same rows as the join: the right side only
/// sees the left row's bindings as constants.
fn service_first(p: GraphPattern) -> GraphPattern {
    fn has_service(p: &GraphPattern) -> bool {
        match p {
            GraphPattern::Join { left, right } => has_service(left) || has_service(right),
            p => is_service(p),
        }
    }
    if !has_service(&p) {
        return p;
    }
    let mut parts = Vec::new();
    split(p, &mut parts);
    let (services, rest): (Vec<Part>, Vec<Part>) = parts
        .into_iter()
        .partition(|x| matches!(x, Part::Other(p) if is_service(p)));
    if services.is_empty() {
        return rebuild_plain(rest);
    }
    let mut bound = HashSet::new();
    for s in &services {
        bound.extend(s.names());
    }
    // The triples and paths reached from the service rows through shared names.
    let (mut chained, mut others) = (Vec::new(), Vec::new());
    let mut known = bound.clone();
    let mut pending = rest;
    loop {
        let before = chained.len();
        let mut next = Vec::new();
        for part in pending {
            let lookup = !matches!(part, Part::Other(_));
            if lookup && part.names().iter().any(|n| known.contains(n)) {
                known.extend(part.names());
                chained.push(part);
            } else {
                next.push(part);
            }
        }
        pending = next;
        if chained.len() == before {
            break;
        }
    }
    others.extend(pending);
    let mut out = services
        .into_iter()
        .map(Part::pattern)
        .reduce(|left, right| GraphPattern::Join {
            left: Box::new(left),
            right: Box::new(right),
        })
        .expect("at least one service");
    if !chained.is_empty() {
        // A service row that leaves a linking variable unbound (the last row of a route has
        // no edge) joined no triple in the join this replaces: such rows are dropped first,
        // since in the LATERAL lookup the triple itself would bind the variable.
        let mut keys: Vec<Name> = chained
            .iter()
            .flat_map(Part::names)
            .filter(|n| bound.contains(n) && !n.starts_with("_:"))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        keys.sort();
        if let Some(expr) = keys
            .into_iter()
            .map(|k| {
                // Not BOUND(?k): Oxigraph's optimizer takes a variable of a SERVICE pattern
                // as always bound and drops that test. sameTerm on an unbound variable is an
                // error, so the row is dropped all the same.
                let v = Expression::Variable(spargebra::term::Variable::new_unchecked(k));
                Expression::SameTerm(Box::new(v.clone()), Box::new(v))
            })
            .reduce(|a, b| Expression::And(Box::new(a), Box::new(b)))
        {
            out = GraphPattern::Filter {
                expr,
                inner: Box::new(out),
            };
        }
        // A blank node joins triples of one BGP only: chained in separate patterns, it would
        // no longer link them (the inner node of `?e geo:hasGeometry/geo:asWKT ?w`). As a
        // variable it does; the projection was fixed by the parser, so it is not returned.
        let chained = chained.into_iter().map(blank_to_variable).collect();
        out = GraphPattern::Lateral {
            left: Box::new(out),
            right: Box::new(correlated(rebuild_plain(chained), &bound)),
        };
    }
    if others.is_empty() {
        return out;
    }
    GraphPattern::Join {
        left: Box::new(out),
        right: Box::new(rebuild_plain(others)),
    }
}

fn blank_to_variable(part: Part) -> Part {
    let term = |t: TermPattern| match t {
        TermPattern::BlankNode(b) => TermPattern::Variable(
            spargebra::term::Variable::new_unchecked(format!("_tripletrail_bn_{}", b.as_str())),
        ),
        t => t,
    };
    match part {
        Part::Triple(t) => Part::Triple(TriplePattern {
            subject: term(t.subject),
            predicate: t.predicate,
            object: term(t.object),
        }),
        Part::Path(GraphPattern::Path {
            subject,
            path,
            object,
        }) => Part::Path(GraphPattern::Path {
            subject: term(subject),
            path,
            object: term(object),
        }),
        other => other,
    }
}

/// Plans the `EXISTS` sub-patterns of an expression evaluated where `bound` may be bound.
fn expression(e: Expression, bound: &HashSet<Name>) -> Expression {
    use Expression as E;
    let b = |e: Box<E>| Box::new(expression(*e, bound));
    let e = match e {
        E::Exists(p) => {
            // Nested EXISTS see the variables of this pattern too.
            let p = walk(*p);
            E::Exists(Box::new(correlated(p, bound)))
        }
        E::Or(l, r) => E::Or(b(l), b(r)),
        E::And(l, r) => E::And(b(l), b(r)),
        E::Equal(l, r) => E::Equal(b(l), b(r)),
        E::SameTerm(l, r) => E::SameTerm(b(l), b(r)),
        E::Greater(l, r) => E::Greater(b(l), b(r)),
        E::GreaterOrEqual(l, r) => E::GreaterOrEqual(b(l), b(r)),
        E::Less(l, r) => E::Less(b(l), b(r)),
        E::LessOrEqual(l, r) => E::LessOrEqual(b(l), b(r)),
        E::In(l, r) => E::In(b(l), r.into_iter().map(|e| expression(e, bound)).collect()),
        E::Add(l, r) => E::Add(b(l), b(r)),
        E::Subtract(l, r) => E::Subtract(b(l), b(r)),
        E::Multiply(l, r) => E::Multiply(b(l), b(r)),
        E::Divide(l, r) => E::Divide(b(l), b(r)),
        E::UnaryPlus(i) => E::UnaryPlus(b(i)),
        E::UnaryMinus(i) => E::UnaryMinus(b(i)),
        E::Not(i) => E::Not(b(i)),
        E::If(c, t, f) => E::If(b(c), b(t), b(f)),
        E::Coalesce(v) => E::Coalesce(v.into_iter().map(|e| expression(e, bound)).collect()),
        E::FunctionCall(f, v) => {
            E::FunctionCall(f, v.into_iter().map(|e| expression(e, bound)).collect())
        }
        other @ (E::NamedNode(_) | E::Literal(_) | E::Variable(_) | E::Bound(_)) => return other,
    };
    fold(e)
}

/// A constant expression as the constant it evaluates to, once, when the plan is made. Oxigraph
/// compiles the pattern of `REGEX` and `REPLACE` once only when it is a literal:
/// `REPLACE(STR(?x), STR(ex:), "")` (#260) or `REPLACE(STR(?x), CONCAT(STR(ex:), "_"), "")`
/// (#375), common ways to strip a namespace, otherwise compiled a regular expression for every
/// row (60 µs each). Called bottom-up, so an expression is constant when its direct operands
/// are constants. Functions that give a new value per call (`RAND`, `NOW`, `UUID`, `BNODE`),
/// `IRI` (relative to the base IRI), custom functions and expressions that raise an error are
/// left as they are.
fn fold(e: Expression) -> Expression {
    use Expression as E;
    let constant = |e: &E| matches!(e, E::NamedNode(_) | E::Literal(_));
    let foldable = match &e {
        E::Or(l, r)
        | E::And(l, r)
        | E::Equal(l, r)
        | E::SameTerm(l, r)
        | E::Greater(l, r)
        | E::GreaterOrEqual(l, r)
        | E::Less(l, r)
        | E::LessOrEqual(l, r)
        | E::Add(l, r)
        | E::Subtract(l, r)
        | E::Multiply(l, r)
        | E::Divide(l, r) => constant(l) && constant(r),
        E::UnaryPlus(i) | E::UnaryMinus(i) | E::Not(i) => constant(i),
        E::In(l, r) => constant(l) && r.iter().all(constant),
        E::If(c, t, f) => constant(c) && constant(t) && constant(f),
        E::Coalesce(v) => !v.is_empty() && v.iter().all(constant),
        E::FunctionCall(f, args) => {
            !matches!(
                f,
                Function::Rand
                    | Function::Now
                    | Function::Uuid
                    | Function::StrUuid
                    | Function::BNode
                    | Function::Iri
                    | Function::Custom(_)
            ) && args.iter().all(constant)
        }
        _ => false,
    };
    if !foldable {
        return e;
    }
    evaluate_constant(&e).unwrap_or(e)
}

/// The value of an expression without variables, on an empty dataset: `None` when it raises
/// an error or gives a blank node.
fn evaluate_constant(e: &Expression) -> Option<Expression> {
    use oxigraph::sparql::{QueryResults, SparqlEvaluator};
    let v = spargebra::term::Variable::new_unchecked("__tripletrail_constant");
    let query = Query::Select {
        dataset: None,
        pattern: GraphPattern::Extend {
            inner: Box::new(GraphPattern::Bgp {
                patterns: Vec::new(),
            }),
            variable: v.clone(),
            expression: e.clone(),
        },
        base_iri: None,
    };
    let empty = oxigraph::model::Dataset::new();
    let QueryResults::Solutions(mut solutions) = SparqlEvaluator::new()
        .for_query(query)
        .on_queryable_dataset(&empty)
        .execute()
        .ok()?
    else {
        return None;
    };
    match solutions.next()?.ok()?.get(&v)? {
        oxigraph::model::Term::NamedNode(n) => Some(Expression::NamedNode(n.clone())),
        oxigraph::model::Term::Literal(l) => Some(Expression::Literal(l.clone())),
        _ => None,
    }
}

/// One operand of the group being ordered.
enum Part {
    Triple(TriplePattern),
    Path(GraphPattern),
    /// Anything else (OPTIONAL, UNION, sub-select...): joined after the ordered chain.
    Other(GraphPattern),
}

impl Part {
    fn names(&self) -> HashSet<Name> {
        match self {
            Part::Triple(t) => triple_names(t),
            Part::Path(p) | Part::Other(p) => names_of_pattern(p),
        }
    }

    fn pattern(self) -> GraphPattern {
        match self {
            Part::Triple(t) => GraphPattern::Bgp { patterns: vec![t] },
            Part::Path(p) | Part::Other(p) => p,
        }
    }

    /// How selective the part is once `known` are bound: higher is better.
    fn score(&self, known: &HashSet<Name>) -> Option<(u8, u8)> {
        let bound = |t: &TermPattern| match term_name(t) {
            Some(n) => known.contains(&n),
            None => true,
        };
        let (s, p, o) = match self {
            Part::Triple(t) => (
                bound(&t.subject),
                match &t.predicate {
                    NamedNodePattern::NamedNode(_) => true,
                    NamedNodePattern::Variable(v) => known.contains(v.as_str()),
                },
                bound(&t.object),
            ),
            Part::Path(GraphPattern::Path {
                subject, object, ..
            }) => (bound(subject), true, bound(object)),
            _ => return None,
        };
        // Connected to what is known through a variable (not only through constants).
        let connected = self.names().iter().any(|n| known.contains(n));
        // A bound subject or object narrows the lookup most; the predicate alone barely does.
        let ends = u8::from(s) * 2 + u8::from(o) * 2 + u8::from(p);
        Some((u8::from(connected), ends))
    }
}

/// Orders the triple patterns of `p` starting from the names in `bound`, chained with LATERAL
/// joins. Left unchanged when it does not share a name with `bound`.
fn correlated(p: GraphPattern, bound: &HashSet<Name>) -> GraphPattern {
    match p {
        GraphPattern::Filter { expr, inner } => GraphPattern::Filter {
            expr,
            inner: Box::new(correlated(*inner, bound)),
        },
        GraphPattern::Graph { name, inner } => {
            let mut inner_bound = bound.clone();
            if let NamedNodePattern::Variable(v) = &name
                && bound.contains(v.as_str())
            {
                inner_bound.insert(v.as_str().to_owned());
            }
            GraphPattern::Graph {
                name,
                inner: Box::new(correlated(*inner, &inner_bound)),
            }
        }
        p @ (GraphPattern::Bgp { .. } | GraphPattern::Join { .. }) => {
            let mut parts = Vec::new();
            split(p, &mut parts);
            if parts.len() < 2
                || !parts
                    .iter()
                    .any(|x| x.names().iter().any(|n| bound.contains(n)))
            {
                return rebuild_plain(parts);
            }
            let (mut chain_parts, others): (Vec<Part>, Vec<Part>) = parts
                .into_iter()
                .partition(|x| !matches!(x, Part::Other(_)));
            let mut known = bound.clone();
            let mut chain: Option<GraphPattern> = None;
            while !chain_parts.is_empty() {
                // The best next part; ties keep the written order.
                let mut best = 0;
                let mut best_score = None;
                for (i, part) in chain_parts.iter().enumerate() {
                    let s = part.score(&known);
                    if s > best_score {
                        best = i;
                        best_score = s;
                    }
                }
                let next = chain_parts.remove(best);
                known.extend(next.names());
                let next = next.pattern();
                chain = Some(match chain {
                    None => next,
                    Some(left) => GraphPattern::Lateral {
                        left: Box::new(left),
                        right: Box::new(next),
                    },
                });
            }
            let mut out = chain.unwrap_or(GraphPattern::Bgp {
                patterns: Vec::new(),
            });
            for other in others {
                out = GraphPattern::Join {
                    left: Box::new(out),
                    right: Box::new(other.pattern()),
                };
            }
            out
        }
        other => other,
    }
}

fn split(p: GraphPattern, out: &mut Vec<Part>) {
    match p {
        GraphPattern::Join { left, right } => {
            split(*left, out);
            split(*right, out);
        }
        GraphPattern::Bgp { patterns } => out.extend(patterns.into_iter().map(Part::Triple)),
        p @ GraphPattern::Path { .. } => out.push(Part::Path(p)),
        other => out.push(Part::Other(other)),
    }
}

/// The parts joined back in their written order, BGPs merged.
fn rebuild_plain(parts: Vec<Part>) -> GraphPattern {
    let mut out: Option<GraphPattern> = None;
    let mut triples: Vec<TriplePattern> = Vec::new();
    let flush = |out: &mut Option<GraphPattern>, triples: &mut Vec<TriplePattern>| {
        if triples.is_empty() {
            return;
        }
        let bgp = GraphPattern::Bgp {
            patterns: std::mem::take(triples),
        };
        *out = Some(match out.take() {
            None => bgp,
            Some(left) => GraphPattern::Join {
                left: Box::new(left),
                right: Box::new(bgp),
            },
        });
    };
    for part in parts {
        match part {
            Part::Triple(t) => triples.push(t),
            other => {
                flush(&mut out, &mut triples);
                let p = other.pattern();
                out = Some(match out.take() {
                    None => p,
                    Some(left) => GraphPattern::Join {
                        left: Box::new(left),
                        right: Box::new(p),
                    },
                });
            }
        }
    }
    flush(&mut out, &mut triples);
    out.unwrap_or(GraphPattern::Bgp {
        patterns: Vec::new(),
    })
}

fn term_name(t: &TermPattern) -> Option<Name> {
    match t {
        TermPattern::Variable(v) => Some(v.as_str().to_owned()),
        TermPattern::BlankNode(b) => Some(format!("_:{}", b.as_str())),
        _ => None,
    }
}

fn triple_names(t: &TriplePattern) -> HashSet<Name> {
    let mut out = HashSet::new();
    out.extend(term_name(&t.subject));
    if let NamedNodePattern::Variable(v) = &t.predicate {
        out.insert(v.as_str().to_owned());
    }
    out.extend(term_name(&t.object));
    out
}

/// The variables (and blank nodes) a pattern mentions. Over-approximates the variables bound
/// for an expression evaluated on its rows, which only affects ordering, never results.
fn names_of_pattern(p: &GraphPattern) -> HashSet<Name> {
    let mut out = HashSet::new();
    collect(p, &mut out);
    out
}

fn collect(p: &GraphPattern, out: &mut HashSet<Name>) {
    use GraphPattern as G;
    match p {
        G::Bgp { patterns } => patterns.iter().for_each(|t| out.extend(triple_names(t))),
        G::Path {
            subject, object, ..
        } => {
            out.extend(term_name(subject));
            out.extend(term_name(object));
        }
        G::Join { left, right }
        | G::LeftJoin { left, right, .. }
        | G::Lateral { left, right }
        | G::Union { left, right } => {
            collect(left, out);
            collect(right, out);
        }
        // MINUS binds nothing from its right side; a sub-select only its projection.
        G::Minus { left, .. } => collect(left, out),
        G::Project { variables, .. } => {
            out.extend(variables.iter().map(|v| v.as_str().to_owned()));
        }
        G::Graph { name, inner } | G::Service { name, inner, .. } => {
            if let NamedNodePattern::Variable(v) = name {
                out.insert(v.as_str().to_owned());
            }
            collect(inner, out);
        }
        G::Extend {
            inner, variable, ..
        } => {
            out.insert(variable.as_str().to_owned());
            collect(inner, out);
        }
        G::Values { variables, .. } => {
            out.extend(variables.iter().map(|v| v.as_str().to_owned()));
        }
        G::Group {
            variables,
            aggregates,
            ..
        } => {
            out.extend(variables.iter().map(|v| v.as_str().to_owned()));
            out.extend(aggregates.iter().map(|(v, _)| v.as_str().to_owned()));
        }
        G::Filter { inner, .. }
        | G::OrderBy { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::Slice { inner, .. } => collect(inner, out),
    }
}

/// `GROUP BY ?component` with `COUNT(?node)` (or `COUNT(*)`) over the rows of
/// `pgr:pgr_connectedComponents` or `pgr:pgr_strongComponents`: the routing SERVICE gives the
/// sizes itself with `pgr:size`, one row per component, rather than one row per vertex to be
/// grouped here.
fn component_sizes(p: GraphPattern) -> GraphPattern {
    use spargebra::algebra::{AggregateExpression, AggregateFunction};
    let GraphPattern::Group {
        inner,
        variables,
        aggregates,
    } = &p
    else {
        return p;
    };
    let ([component], [(size, aggregate)]) = (variables.as_slice(), aggregates.as_slice()) else {
        return p;
    };
    let GraphPattern::Service {
        name: NamedNodePattern::NamedNode(service),
        inner: service_pattern,
        silent,
    } = &**inner
    else {
        return p;
    };
    // The request's triples, and the rest of the SERVICE pattern (the edge set's sub-select)
    // with the side it is on.
    let (patterns, rest) = match &**service_pattern {
        GraphPattern::Bgp { patterns } => (patterns, None),
        GraphPattern::Join { left, right } => match (&**left, &**right) {
            (GraphPattern::Bgp { patterns }, other) => (patterns, Some((other, false))),
            (other, GraphPattern::Bgp { patterns }) => (patterns, Some((other, true))),
            _ => return p,
        },
        _ => return p,
    };
    if service.as_str() != tripletrail_pgr::PGR_SERVICE {
        return p;
    }
    let pgr = |local: &str| format!("{}{local}", tripletrail_pgr::PGR);
    let predicate = |t: &TriplePattern, local: &str| matches!(&t.predicate, NamedNodePattern::NamedNode(n) if n.as_str() == pgr(local));
    let components = patterns.iter().any(|t| {
        predicate(t, "function")
            && matches!(&t.object, TermPattern::NamedNode(f)
            if f.as_str().strip_prefix(tripletrail_pgr::PGR).is_some_and(|l| {
                matches!(
                    l.strip_prefix("pgr_").unwrap_or(l),
                    "connectedComponents" | "strongComponents"
                )
            }))
    });
    let grouped_by_component = patterns.iter().any(|t| {
        predicate(t, "component") && matches!(&t.object, TermPattern::Variable(v) if v == component)
    });
    let nodes: Vec<usize> = (0..patterns.len())
        .filter(|&i| predicate(&patterns[i], "node"))
        .collect();
    let [node_triple] = nodes.as_slice() else {
        return p;
    };
    let TermPattern::Variable(node) = &patterns[*node_triple].object else {
        return p;
    };
    let counts_nodes = match aggregate {
        AggregateExpression::CountSolutions { .. } => true,
        AggregateExpression::FunctionCall {
            name: AggregateFunction::Count,
            expr: Expression::Variable(v),
            ..
        } => v == node,
        _ => false,
    };
    // `?node` must not be used by any other pattern of the SERVICE.
    let node_once = patterns
        .iter()
        .enumerate()
        .all(|(i, t)| i == *node_triple || !triple_names(t).contains(node.as_str()))
        && rest.is_none_or(|(r, _)| !names_of_pattern(r).contains(node.as_str()));
    let asks_size = patterns
        .iter()
        .any(|t| predicate(t, "size") || predicate(t, "component_size"));
    if !(components && grouped_by_component && counts_nodes && node_once && !asks_size)
        || size == component
    {
        return p;
    }
    let mut patterns = patterns.clone();
    patterns[*node_triple] = TriplePattern {
        subject: patterns[*node_triple].subject.clone(),
        predicate: NamedNodePattern::NamedNode(spargebra::term::NamedNode::new_unchecked(pgr(
            "size",
        ))),
        object: TermPattern::Variable(size.clone()),
    };
    let bgp = Box::new(GraphPattern::Bgp { patterns });
    let service_pattern = match rest {
        None => bgp,
        Some((r, false)) => Box::new(GraphPattern::Join {
            left: bgp,
            right: Box::new(r.clone()),
        }),
        Some((r, true)) => Box::new(GraphPattern::Join {
            left: Box::new(r.clone()),
            right: bgp,
        }),
    };
    GraphPattern::Project {
        inner: Box::new(GraphPattern::Service {
            name: NamedNodePattern::NamedNode(service.clone()),
            inner: service_pattern,
            silent: *silent,
        }),
        variables: vec![component.clone(), size.clone()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(q: &str) -> String {
        let q = spargebra::SparqlParser::new().parse_query(q).unwrap();
        plan_query(q).to_sse()
    }

    #[test]
    fn a_service_named_by_a_variable_runs_per_row_before_it() {
        let q = "SELECT * WHERE { ?p <http://e/endpoint> ?s SERVICE ?s { ?x ?y ?z } }";
        let sse = plan(q);
        assert!(sse.contains("(lateral (bgp"), "{sse}");
        crate::hints::scope(
            crate::Hints {
                optimizer_off: true,
                ..Default::default()
            },
            || {
                assert!(plan(q).contains("(lateral (bgp"));
            },
        );
        let sse = plan(
            "SELECT * WHERE { ?p <http://e/endpoint> ?s OPTIONAL { SERVICE ?s { ?x ?y ?z } } }",
        );
        assert!(sse.contains("(lateral (bgp"), "{sse}");
        assert!(sse.contains("(leftjoin (bgp) (service ?s"), "{sse}");
        // A SERVICE with more patterns in its OPTIONAL group, and a group that binds its own
        // endpoint, which stays a left join.
        let sse = plan(
            "SELECT * WHERE { ?p <http://e/endpoint> ?s \
             OPTIONAL { SERVICE ?s { ?x ?y ?z } ?x <http://e/n> ?m } }",
        );
        assert!(sse.contains("(lateral (bgp"), "{sse}");
        let sse = plan(
            "SELECT * WHERE { ?p ?q ?r OPTIONAL { ?r <http://e/endpoint> ?s SERVICE ?s { ?x ?y ?z } } }",
        );
        assert!(sse.contains("(leftjoin (bgp"), "{sse}");
        // A named endpoint is left as a join (or reordered by the planner).
        assert!(
            !plan("SELECT * WHERE { ?p ?q ?s SERVICE <http://e/sparql> { ?x ?y ?z } }")
                .contains("lateral")
        );
    }

    #[test]
    fn str_of_a_constant_is_folded() {
        let sse = plan(
            r#"PREFIX ex: <http://example.com/> SELECT * WHERE {
                ?s ?p ?o BIND(REPLACE(STR(?s), STR(ex:), "") AS ?id) FILTER(STR("a"@en) = ?o)
            }"#,
        );
        assert!(!sse.contains("<http://example.com/>"), "{sse}");
        assert!(sse.contains("\"http://example.com/\""), "{sse}");
        assert!(sse.contains("( str ?s)"), "{sse}");
        assert!(!sse.contains("@en"), "{sse}");
    }

    #[test]
    fn not_exists_starts_from_the_outer_variable() {
        let sse = plan(
            "PREFIX ex: <http://example.com/> SELECT * WHERE {
                ?t ex:on ?ne .
                FILTER NOT EXISTS { ?s a ex:S ; ex:loc ?l . ?l ex:on ?ne }
            }",
        );
        // The pattern on ?ne comes first, the type check last.
        let on = sse.find("?l <http://example.com/on> ?ne").unwrap();
        let loc = sse.find("?s <http://example.com/loc> ?l").unwrap();
        let ty = sse
            .find("?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type>")
            .unwrap();
        assert!(on < loc && loc < ty, "{sse}");
        assert!(sse.contains("lateral"), "{sse}");
    }

    #[test]
    fn uncorrelated_exists_is_left_alone() {
        let sse = plan(
            "PREFIX ex: <http://example.com/> SELECT * WHERE {
                ?t ex:on ?ne .
                FILTER EXISTS { ?s a ex:S ; ex:loc ?l }
            }",
        );
        assert!(!sse.contains("lateral"), "{sse}");
    }

    #[test]
    fn service_rows_look_up_their_triples() {
        let sse = plan(
            "PREFIX ex: <http://example.com/> SELECT * WHERE {
                SERVICE <http://w3id.org/tripletrail/pgr> { [] ex:node ?node }
                BIND(IRI(STRBEFORE(?node, \"#\")) AS ?e)
                ?e ex:geom/ex:wkt ?ew .
                ?s a ex:Signal ; ex:geom/ex:wkt ?sw .
            }",
        );
        // The service, then a lateral lookup of ?e's geometry; the signals are joined after.
        let service = sse.find("(service").unwrap();
        let lateral = sse.find("(lateral").unwrap();
        let geom = sse.find("?e <http://example.com/geom>").unwrap();
        let signal = sse.find("<http://example.com/Signal>").unwrap();
        assert!(
            lateral < service && service < geom && geom < signal,
            "{sse}"
        );
    }

    #[test]
    fn binds_move_above_the_join() {
        let sse = plan(
            "PREFIX ex: <http://example.com/> SELECT * WHERE {
                ?t a ex:Track ; ex:on ?ne . BIND(STR(?ne) AS ?id) ?ne ex:begin ?b }",
        );
        // One join of the triple patterns, under the BIND.
        assert!(
            sse.contains("(extend ((?id ( str ?ne))) (join (bgp"),
            "{sse}"
        );
    }

    #[test]
    fn binds_reading_the_other_side_stay() {
        // ?b is bound by the other side only: BIND(?b...) would see it bound above the join.
        for q in [
            "PREFIX ex: <http://example.com/> SELECT * WHERE {
                ?t ex:on ?ne . BIND(COALESCE(?b, 0) AS ?x) ?ne ex:begin ?b }",
            "PREFIX ex: <http://example.com/> SELECT * WHERE {
                ?t ex:on ?ne . BIND(BNODE() AS ?x) ?ne ex:begin ?b }",
            "PREFIX ex: <http://example.com/> SELECT * WHERE {
                ?t ex:on ?ne . BIND(1 AS ?b) ?ne ex:begin ?b }",
        ] {
            let parsed = spargebra::SparqlParser::new().parse_query(q).unwrap();
            assert_eq!(plan(q), parsed.to_sse(), "{q}");
        }
    }

    #[test]
    fn joins_without_a_service_are_left_alone() {
        let q = "PREFIX ex: <http://example.com/> SELECT * WHERE {
            ?a ex:p ?b . { SELECT ?b WHERE { ?b ex:q ?c } } ?b ex:r ?d }";
        let parsed = spargebra::SparqlParser::new().parse_query(q).unwrap();
        assert_eq!(plan(q), parsed.to_sse());
    }
}
