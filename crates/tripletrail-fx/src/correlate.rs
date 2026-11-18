//! Option triples whose value comes from outside the SERVICE, as SPARQL Anything allows:
//!
//! ```sparql
//! ?row xyz:attribute_values ?xml .
//! OPTIONAL { SERVICE <x-sparql-anything:> { fx:properties fx:content ?xml . ... } }
//! ```
//!
//! A SERVICE handler only sees its pattern, so the query is rewritten to give it the value in
//! its IRI: the SERVICE gets a variable name, bound per row by a `BIND` of
//! `IRI(CONCAT("x-sparql-anything:…", ",__fx_v.content=", ENCODE_FOR_URI(STR(?xml))))`, and
//! the join or OPTIONAL that binds `?xml` becomes a `LATERAL`, so the SERVICE runs once per
//! row with the row's value. [`crate::options::prepare`] decodes the option again.

use crate::options::{HIDDEN, SCHEME, visit};
use crate::{FX, FXE};
use oxigraph::model::Variable;
use spargebra::algebra::{Expression, Function, GraphPattern};
use spargebra::term::{Literal, NamedNodePattern, TermPattern};
use std::collections::HashSet;

/// The prefix, in a SERVICE IRI, of an option the rewrite put there (percent-encoded).
pub(crate) const VALUE: &str = "__fx_v.";

/// `pattern` with every Facade-X SERVICE whose option triples take a variable bound outside
/// it rewritten to run once per binding.
pub fn correlate(pattern: GraphPattern) -> GraphPattern {
    let mut counter = 0;
    rewrite(pattern, &mut counter).0
}

/// The rewritten pattern, and the outside variables its SERVICE calls still wait for.
fn rewrite(p: GraphPattern, counter: &mut usize) -> (GraphPattern, HashSet<Variable>) {
    use GraphPattern as G;
    let mut go = |p: Box<G>| rewrite(*p, counter);
    match p {
        G::Service {
            name: NamedNodePattern::NamedNode(name),
            inner,
            silent,
        } if name.as_str().starts_with(SCHEME) => {
            let outside = outside_options(&inner);
            if outside.is_empty() {
                return (
                    G::Service {
                        name: NamedNodePattern::NamedNode(name),
                        inner,
                        silent,
                    },
                    HashSet::new(),
                );
            }
            let service = Variable::new_unchecked(format!("{HIDDEN}service{counter}"));
            *counter += 1;
            let mut inner = *inner;
            remove_options(&mut inner, &outside);
            let mut variables: Vec<Variable> = Vec::new();
            inner.on_in_scope_variable(|v| {
                if !variables.contains(v) {
                    variables.push(v.clone());
                }
            });
            let mut pending: Vec<&(String, Variable)> = outside.iter().collect();
            pending.sort();
            let mut parts = vec![Expression::Literal(Literal::new_simple_literal(
                name.as_str(),
            ))];
            for (option, v) in &pending {
                parts.push(Expression::Coalesce(vec![
                    Expression::FunctionCall(
                        Function::Concat,
                        vec![
                            Expression::Literal(Literal::new_simple_literal(format!(
                                ",{VALUE}{option}="
                            ))),
                            Expression::FunctionCall(
                                Function::EncodeForUri,
                                vec![Expression::FunctionCall(
                                    Function::Str,
                                    vec![Expression::Variable(v.clone())],
                                )],
                            ),
                        ],
                    ),
                    Expression::Literal(Literal::new_simple_literal("")),
                ]));
            }
            for (_, v) in &pending {
                if !variables.contains(v) {
                    variables.push(v.clone());
                }
            }
            let bind = G::Extend {
                inner: Box::new(G::Bgp { patterns: vec![] }),
                variable: service.clone(),
                expression: Expression::FunctionCall(
                    Function::Iri,
                    vec![Expression::FunctionCall(Function::Concat, parts)],
                ),
            };
            let call = G::Project {
                inner: Box::new(G::Lateral {
                    left: Box::new(bind),
                    right: Box::new(G::Service {
                        name: NamedNodePattern::Variable(service),
                        inner: Box::new(inner),
                        silent,
                    }),
                }),
                variables,
            };
            let waits = pending.into_iter().map(|(_, v)| v.clone()).collect();
            (call, waits)
        }
        G::Join { left, right } => {
            let (left, wl) = go(left);
            let (right, wr) = go(right);
            let (nl, nr) = (names(&left), names(&right));
            if !wr.is_disjoint(&nl) {
                let waits = wl.into_iter().chain(&wr - &nl).collect();
                (lateral(left, right), waits)
            } else if !wl.is_disjoint(&nr) {
                let waits = wr.into_iter().chain(&wl - &nr).collect();
                (lateral(right, left), waits)
            } else {
                let waits = wl.into_iter().chain(wr).collect();
                (
                    G::Join {
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    waits,
                )
            }
        }
        G::LeftJoin {
            left,
            right,
            expression,
        } => {
            let (left, wl) = go(left);
            let (right, wr) = go(right);
            let nl = names(&left);
            if !wr.is_disjoint(&nl) {
                // `LATERAL` over an OPTIONAL with an empty left side runs the right side once
                // per row and keeps the rows it does not extend.
                let waits = wl.into_iter().chain(&wr - &nl).collect();
                let optional = G::LeftJoin {
                    left: Box::new(G::Bgp { patterns: vec![] }),
                    right: Box::new(right),
                    expression,
                };
                (lateral(left, optional), waits)
            } else {
                let waits = wl.into_iter().chain(wr).collect();
                (
                    G::LeftJoin {
                        left: Box::new(left),
                        right: Box::new(right),
                        expression,
                    },
                    waits,
                )
            }
        }
        G::Lateral { left, right } => {
            let (left, wl) = go(left);
            let (right, wr) = go(right);
            let nl = names(&left);
            let waits = wl.into_iter().chain(&wr - &nl).collect();
            (lateral(left, right), waits)
        }
        G::Union { left, right } => {
            let (left, wl) = go(left);
            let (right, wr) = go(right);
            (
                G::Union {
                    left: Box::new(left),
                    right: Box::new(right),
                },
                wl.into_iter().chain(wr).collect(),
            )
        }
        G::Minus { left, right } => {
            let (left, wl) = go(left);
            let (right, wr) = go(right);
            (
                G::Minus {
                    left: Box::new(left),
                    right: Box::new(right),
                },
                wl.into_iter().chain(wr).collect(),
            )
        }
        G::Filter { expr, inner } => {
            let (inner, w) = go(inner);
            (
                G::Filter {
                    expr,
                    inner: Box::new(inner),
                },
                w,
            )
        }
        G::Graph { name, inner } => {
            let (inner, w) = go(inner);
            (
                G::Graph {
                    name,
                    inner: Box::new(inner),
                },
                w,
            )
        }
        G::Extend {
            inner,
            variable,
            expression,
        } => {
            let (inner, w) = go(inner);
            (
                G::Extend {
                    inner: Box::new(inner),
                    variable,
                    expression,
                },
                w,
            )
        }
        G::OrderBy { inner, expression } => {
            let (inner, w) = go(inner);
            (
                G::OrderBy {
                    inner: Box::new(inner),
                    expression,
                },
                w,
            )
        }
        G::Distinct { inner } => {
            let (inner, w) = go(inner);
            (
                G::Distinct {
                    inner: Box::new(inner),
                },
                w,
            )
        }
        G::Reduced { inner } => {
            let (inner, w) = go(inner);
            (
                G::Reduced {
                    inner: Box::new(inner),
                },
                w,
            )
        }
        G::Slice {
            inner,
            start,
            length,
        } => {
            let (inner, w) = go(inner);
            (
                G::Slice {
                    inner: Box::new(inner),
                    start,
                    length,
                },
                w,
            )
        }
        // A sub-select sees only the outside variables it projects.
        G::Project { inner, variables } => {
            let (inner, w) = go(inner);
            let w = w.into_iter().filter(|v| variables.contains(v)).collect();
            (
                G::Project {
                    inner: Box::new(inner),
                    variables,
                },
                w,
            )
        }
        G::Group {
            inner,
            variables,
            aggregates,
        } => {
            let (inner, w) = go(inner);
            let w = w.into_iter().filter(|v| variables.contains(v)).collect();
            (
                G::Group {
                    inner: Box::new(inner),
                    variables,
                    aggregates,
                },
                w,
            )
        }
        other @ (G::Bgp { .. } | G::Path { .. } | G::Values { .. } | G::Service { .. }) => {
            (other, HashSet::new())
        }
    }
}

fn lateral(left: GraphPattern, right: GraphPattern) -> GraphPattern {
    GraphPattern::Lateral {
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn names(p: &GraphPattern) -> HashSet<Variable> {
    let mut out = HashSet::new();
    p.on_in_scope_variable(|v| {
        out.insert(v.clone());
    });
    out
}

/// The option namespace of an option triple's subject.
fn option_namespace(subject: &TermPattern) -> Option<&'static str> {
    match subject {
        TermPattern::NamedNode(s) if s.as_str() == format!("{FXE}properties") => Some(FXE),
        TermPattern::NamedNode(s) if s.as_str() == format!("{FX}properties") => Some(FX),
        _ => None,
    }
}

/// The options set by a variable that no BIND inside the SERVICE binds: (option, variable).
fn outside_options(pattern: &GraphPattern) -> HashSet<(String, Variable)> {
    let mut bound = HashSet::new();
    visit(pattern, &mut |p| {
        if let GraphPattern::Extend { variable, .. } = p {
            bound.insert(variable.clone());
        }
    });
    let mut out = HashSet::new();
    visit(pattern, &mut |p| {
        let GraphPattern::Bgp { patterns } = p else {
            return;
        };
        for t in patterns {
            let Some(ns) = option_namespace(&t.subject) else {
                continue;
            };
            if let (NamedNodePattern::NamedNode(n), TermPattern::Variable(v)) =
                (&t.predicate, &t.object)
                && let Some(option) = n.as_str().strip_prefix(ns)
                && !bound.contains(v)
            {
                out.insert((option.to_owned(), v.clone()));
            }
        }
    });
    out
}

/// `pattern` without the option triples of `outside`.
fn remove_options(pattern: &mut GraphPattern, outside: &HashSet<(String, Variable)>) {
    crate::options::visit_mut(pattern, &mut |p| {
        let GraphPattern::Bgp { patterns } = p else {
            return;
        };
        patterns.retain(|t| {
            let Some(ns) = option_namespace(&t.subject) else {
                return true;
            };
            match (&t.predicate, &t.object) {
                (NamedNodePattern::NamedNode(n), TermPattern::Variable(v)) => !n
                    .as_str()
                    .strip_prefix(ns)
                    .is_some_and(|option| outside.contains(&(option.to_owned(), v.clone()))),
                _ => true,
            }
        });
    });
}
