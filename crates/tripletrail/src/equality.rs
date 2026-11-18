//! Joins written as filters (#185): `FILTER(?a = ?b)` between two parts of a pattern that
//! share no variable. The evaluator joins the parts as a cross product and filters every pair:
//! in the railway knowledge graph's "parent line of a line section" update, lines × line
//! sections on the same net element took 11 s, against 0.04 s with the net element as one
//! variable.
//!
//! `?a = ?b` is `sameTerm(?a, ?b)` as soon as one side is an IRI or a blank node: two
//! different terms are only `=` when both are literals of equal value (`1` and `1.0`). So
//! `FILTER(?a = ?b && rest)` over a plain pattern (triple patterns and paths, which bind every
//! variable they hold) becomes the pattern with `?b` written as `?a`, `?b` bound to `?a` after
//! it, and `FILTER(rest)`, when the pattern itself shows that `?a` or `?b` is never a
//! literal: it is a subject or a predicate of one of its triple patterns, or the object of a
//! predicate whose objects are all IRIs or blank nodes (from the counts the store keeps per
//! predicate). `sameTerm(?a, ?b)` needs no such proof.

use spargebra::algebra::{Expression, GraphPattern};
use spargebra::term::NamedNode;
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern, Variable};

/// Rewrites the filters of `pattern` that join two of its variables. `resource(p)`: whether
/// every object of `p` is surely an IRI or a blank node.
pub(crate) fn rewrite(
    pattern: GraphPattern,
    resource: &dyn Fn(&NamedNode) -> bool,
) -> GraphPattern {
    walk(pattern, resource)
}

/// [`rewrite`] on the pattern of a query.
pub(crate) fn rewrite_query(
    query: spargebra::Query,
    resource: &dyn Fn(&NamedNode) -> bool,
) -> spargebra::Query {
    use spargebra::Query as Q;
    match query {
        Q::Select {
            dataset,
            pattern,
            base_iri,
        } => Q::Select {
            dataset,
            pattern: walk(pattern, resource),
            base_iri,
        },
        Q::Construct {
            template,
            dataset,
            pattern,
            base_iri,
        } => Q::Construct {
            template,
            dataset,
            pattern: walk(pattern, resource),
            base_iri,
        },
        Q::Describe {
            dataset,
            pattern,
            base_iri,
        } => Q::Describe {
            dataset,
            pattern: walk(pattern, resource),
            base_iri,
        },
        Q::Ask {
            dataset,
            pattern,
            base_iri,
        } => Q::Ask {
            dataset,
            pattern: walk(pattern, resource),
            base_iri,
        },
    }
}

/// The predicates an update may write, as far as its text tells: `None` when an operation
/// may write any (`LOAD`, `ADD`, `COPY`, `MOVE`, a variable predicate in an `INSERT`).
pub(crate) fn written_predicates(update: &spargebra::Update) -> Option<Vec<NamedNode>> {
    use spargebra::GraphUpdateOperation as Op;
    let mut out = Vec::new();
    for op in &update.operations {
        match op {
            Op::InsertData { data } => out.extend(data.iter().map(|q| q.predicate.clone())),
            Op::DeleteInsert { insert, .. } => {
                for q in insert {
                    match &q.predicate {
                        NamedNodePattern::NamedNode(n) => out.push(n.clone()),
                        NamedNodePattern::Variable(_) => return None,
                    }
                }
            }
            Op::DeleteData { .. } | Op::Clear { .. } | Op::Create { .. } | Op::Drop { .. } => {}
            Op::Load { .. } => return None,
        }
    }
    Some(out)
}

fn walk(p: GraphPattern, resource: &dyn Fn(&NamedNode) -> bool) -> GraphPattern {
    use GraphPattern as G;
    let b = |p: Box<G>| Box::new(walk(*p, resource));
    match p {
        G::Filter { expr, inner } => {
            let inner = walk(*inner, resource);
            join_equalities(expr, inner, resource)
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
            expression: e,
        },
        // The right of LATERAL is evaluated with the left's bindings: a variable it shares
        // with them must keep being matched, so it is left as written.
        G::Lateral { left, right } => G::Lateral {
            left: b(left),
            right,
        },
        G::Union { left, right } => G::Union {
            left: b(left),
            right: b(right),
        },
        G::Minus { left, right } => G::Minus {
            left: b(left),
            right: b(right),
        },
        G::Extend {
            inner,
            variable,
            expression: e,
        } => G::Extend {
            inner: b(inner),
            variable,
            expression: e,
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
            expression: e,
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

/// The conjuncts of `e`, in order.
fn conjuncts(e: Expression, out: &mut Vec<Expression>) {
    match e {
        Expression::And(l, r) => {
            conjuncts(*l, out);
            conjuncts(*r, out);
        }
        other => out.push(other),
    }
}

/// The parts of a plain pattern (triple patterns and paths, joined): `None` if it has others.
fn plain(
    p: &GraphPattern,
    triples: &mut Vec<TriplePattern>,
    paths: &mut Vec<(TermPattern, TermPattern)>,
) -> bool {
    match p {
        GraphPattern::Bgp { patterns } => {
            triples.extend(patterns.iter().cloned());
            true
        }
        GraphPattern::Path {
            subject, object, ..
        } => {
            paths.push((subject.clone(), object.clone()));
            true
        }
        GraphPattern::Join { left, right } => {
            plain(left, triples, paths) && plain(right, triples, paths)
        }
        _ => false,
    }
}

fn is(t: &TermPattern, v: &Variable) -> bool {
    matches!(t, TermPattern::Variable(x) if x == v)
}

/// `FILTER(expr)` over `inner`, with the equalities of two variables of a plain `inner` made
/// joins where that is exact (see the module).
fn join_equalities(
    expr: Expression,
    mut inner: GraphPattern,
    resource: &dyn Fn(&NamedNode) -> bool,
) -> GraphPattern {
    let (mut triples, mut paths) = (Vec::new(), Vec::new());
    if !plain(&inner, &mut triples, &mut paths) {
        return GraphPattern::Filter {
            expr,
            inner: Box::new(inner),
        };
    }
    let mut parts = Vec::new();
    conjuncts(expr, &mut parts);
    let mut kept = Vec::new();
    let mut bound_after = Vec::new();
    for part in parts {
        let pair = match &part {
            Expression::Equal(l, r) | Expression::SameTerm(l, r) => match (&**l, &**r) {
                (Expression::Variable(a), Expression::Variable(b)) if a != b => Some((
                    a.clone(),
                    b.clone(),
                    matches!(part, Expression::SameTerm(..)),
                )),
                _ => None,
            },
            _ => None,
        };
        let Some((a, b, same_term)) = pair else {
            kept.push(part);
            continue;
        };
        let held = |v: &Variable| {
            triples.iter().any(|t| {
                is(&t.subject, v)
                    || is(&t.object, v)
                    || matches!(&t.predicate, NamedNodePattern::Variable(x) if x == v)
            }) || paths.iter().any(|(s, o)| is(s, v) || is(o, v))
        };
        // Never a literal: a subject or a predicate, or the object of a predicate whose objects
        // are all resources.
        let never_literal = |v: &Variable| {
            triples.iter().any(|t| {
                is(&t.subject, v)
                    || matches!(&t.predicate, NamedNodePattern::Variable(x) if x == v)
                    || (is(&t.object, v)
                        && matches!(&t.predicate, NamedNodePattern::NamedNode(p) if resource(p)))
            })
        };
        if !(held(&a)
            && held(&b)
            && !connected(&triples, &paths, &a, &b)
            && (same_term || never_literal(&a) || never_literal(&b)))
        {
            kept.push(part);
            continue;
        }
        let rename = |t: TermPattern| {
            if is(&t, &b) {
                TermPattern::Variable(a.clone())
            } else {
                t
            }
        };
        inner = rename_in(inner, &b, &a);
        for t in &mut triples {
            t.subject = rename(t.subject.clone());
            t.object = rename(t.object.clone());
            if matches!(&t.predicate, NamedNodePattern::Variable(x) if *x == b) {
                t.predicate = NamedNodePattern::Variable(a.clone());
            }
        }
        for (s, o) in &mut paths {
            *s = rename(s.clone());
            *o = rename(o.clone());
        }
        bound_after.push((b, a));
    }
    // Each renamed variable is bound to the one it became, before the rest of the filter.
    // A variable renamed to one renamed later points at the last.
    let last = |v: &Variable| {
        let mut v = v.clone();
        while let Some((_, to)) = bound_after.iter().find(|(from, _)| *from == v) {
            v = to.clone();
        }
        v
    };
    for (from, to) in bound_after.iter() {
        inner = GraphPattern::Extend {
            inner: Box::new(inner),
            variable: from.clone(),
            expression: Expression::Variable(last(to)),
        };
    }
    match kept
        .into_iter()
        .reduce(|l, r| Expression::And(Box::new(l), Box::new(r)))
    {
        Some(expr) => GraphPattern::Filter {
            expr,
            inner: Box::new(inner),
        },
        None => inner,
    }
}

/// Whether `a` and `b` are joined by the pattern already: through a chain of triple patterns
/// and paths that share variables. Then the evaluator, and the count shortcuts, take the
/// filter as it is; only parts the pattern leaves unjoined are a cross product.
fn connected(
    triples: &[TriplePattern],
    paths: &[(TermPattern, TermPattern)],
    a: &Variable,
    b: &Variable,
) -> bool {
    let mut edges: Vec<Vec<&Variable>> = Vec::new();
    for t in triples {
        let mut vars = Vec::new();
        if let TermPattern::Variable(v) = &t.subject {
            vars.push(v);
        }
        if let NamedNodePattern::Variable(v) = &t.predicate {
            vars.push(v);
        }
        if let TermPattern::Variable(v) = &t.object {
            vars.push(v);
        }
        edges.push(vars);
    }
    for (s, o) in paths {
        let vars = [s, o]
            .into_iter()
            .filter_map(|t| match t {
                TermPattern::Variable(v) => Some(v),
                _ => None,
            })
            .collect();
        edges.push(vars);
    }
    let mut reached = vec![a];
    let mut grew = true;
    while grew {
        grew = false;
        for vars in &edges {
            if vars.iter().any(|v| reached.contains(v)) {
                for v in vars {
                    if !reached.contains(v) {
                        reached.push(v);
                        grew = true;
                    }
                }
            }
        }
    }
    reached.contains(&b)
}

/// A plain pattern with `from` written as `to`.
fn rename_in(p: GraphPattern, from: &Variable, to: &Variable) -> GraphPattern {
    let term = |t: TermPattern| {
        if is(&t, from) {
            TermPattern::Variable(to.clone())
        } else {
            t
        }
    };
    match p {
        GraphPattern::Bgp { patterns } => GraphPattern::Bgp {
            patterns: patterns
                .into_iter()
                .map(|t| TriplePattern {
                    subject: term(t.subject),
                    predicate: match t.predicate {
                        NamedNodePattern::Variable(x) if x == *from => {
                            NamedNodePattern::Variable(to.clone())
                        }
                        other => other,
                    },
                    object: term(t.object),
                })
                .collect(),
        },
        GraphPattern::Path {
            subject,
            path,
            object,
        } => GraphPattern::Path {
            subject: term(subject),
            path,
            object: term(object),
        },
        GraphPattern::Join { left, right } => GraphPattern::Join {
            left: Box::new(rename_in(*left, from, to)),
            right: Box::new(rename_in(*right, from, to)),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spargebra::SparqlParser;

    fn rewritten(query: &str, resources: &[&str]) -> String {
        let q = SparqlParser::new().parse_query(query).unwrap();
        let resource = |p: &NamedNode| resources.contains(&p.as_str());
        rewrite_query(q, &resource).to_string()
    }

    #[test]
    fn an_equality_with_a_subject_becomes_a_join() {
        let out = rewritten(
            "SELECT * WHERE { ?a <http://e/p> ?x . ?y <http://e/q> ?b FILTER(?a = ?b && ?x < 3) }",
            &[],
        );
        assert!(!out.contains("?a = ?b"), "{out}");
        assert!(out.contains("<http://e/q> ?a"), "{out}");
    }

    #[test]
    fn an_equality_of_two_objects_needs_resource_objects() {
        let q = "SELECT * WHERE { ?x <http://e/p> ?a . ?y <http://e/q> ?b FILTER(?a = ?b) }";
        assert!(rewritten(q, &[]).contains("?a = ?b"));
        let out = rewritten(q, &["http://e/q"]);
        assert!(
            !out.contains("?a = ?b") && out.contains("<http://e/q> ?a"),
            "{out}"
        );
    }

    #[test]
    fn same_term_needs_no_proof_and_optional_parts_are_left_alone() {
        let q =
            "SELECT * WHERE { ?x <http://e/p> ?a . ?y <http://e/q> ?b FILTER(sameTerm(?a, ?b)) }";
        assert!(!rewritten(q, &[]).contains("sameTerm"));
        let q =
            "SELECT * WHERE { ?x <http://e/p> ?a OPTIONAL { ?y <http://e/q> ?b } FILTER(?a = ?b) }";
        assert!(rewritten(q, &["http://e/q", "http://e/p"]).contains("?a = ?b"));
    }

    #[test]
    fn parts_the_pattern_joins_already_are_left_alone() {
        let q =
            "SELECT * WHERE { ?s <http://e/p> ?a . ?s <http://e/p> ?b FILTER(sameTerm(?a, ?b)) }";
        assert!(rewritten(q, &[]).contains("sameTerm"));
    }
}
