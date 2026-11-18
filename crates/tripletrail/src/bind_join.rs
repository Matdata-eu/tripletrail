//! Bind joins: a SERVICE joined to local data is called with the values it can join on, a
//! batch at a time, instead of once for everything. Used for remote SPARQL endpoints (#192)
//! and Facade-X database sources (#216).

use oxigraph::model::Variable;
use spargebra::algebra::GraphPattern;

/// Most distinct left-side rows a bind join sends; with more, the SERVICE runs as written.
pub const BIND_JOIN_MAX_ROWS: usize = 10_000;
/// Rows per `VALUES` block for a remote endpoint, so one call per batch.
pub const BIND_JOIN_BATCH: usize = 100;

/// Rows of a pre-evaluated pattern, or `None` when there were too many.
pub(crate) type Rows = Option<Vec<Vec<Option<oxigraph::model::Term>>>>;

/// Evaluates a pattern for its distinct rows of the given variables.
pub(crate) type Run<'a, E> = dyn FnMut(GraphPattern, &[Variable]) -> Result<Rows, E> + 'a;

/// Whether `SERVICE <iri> { inner }` takes a bind join on the `shared` variables, and with
/// how many rows per call.
pub(crate) type Binds<'a> = dyn Fn(&str, &GraphPattern, &[Variable]) -> Option<usize> + 'a;

/// Rewrites `Join(left, SERVICE <iri> { inner })` (and `OPTIONAL { SERVICE … }`) into a bind
/// join when `binds` says so: `left` is evaluated first (by `run`, for its distinct values of
/// the variables it shares with `inner`), and the SERVICE becomes one call per batch of those
/// values, each with a `VALUES` block in front of `inner`. The answers are the same; the
/// service only gets the rows that can join. `SERVICE SILENT`, patterns under `GRAPH`, the
/// right side of a `LATERAL`, and left sides with unbound or blank shared values are left as
/// they are.
pub(crate) fn bind_join<E>(
    pattern: &mut GraphPattern,
    binds: &Binds<'_>,
    run: &mut Run<'_, E>,
) -> Result<(), E> {
    use GraphPattern as P;
    match pattern {
        // Under GRAPH the left side would read another graph on its own; a LATERAL right side
        // sees bindings from its left.
        P::Graph { .. } => return Ok(()),
        P::Lateral { left, .. } => return bind_join(left, binds, run),
        P::Join { left, right } | P::Union { left, right } | P::Minus { left, right } => {
            bind_join(left, binds, run)?;
            bind_join(right, binds, run)?;
        }
        P::LeftJoin { left, right, .. } => {
            bind_join(left, binds, run)?;
            bind_join(right, binds, run)?;
        }
        P::Filter { inner, .. }
        | P::Extend { inner, .. }
        | P::OrderBy { inner, .. }
        | P::Project { inner, .. }
        | P::Distinct { inner }
        | P::Reduced { inner }
        | P::Slice { inner, .. }
        | P::Group { inner, .. } => bind_join(inner, binds, run)?,
        _ => return Ok(()),
    }
    // The SERVICE may sit on either side of a join; an OPTIONAL's must be on the right.
    let (left, right, either) = match pattern {
        P::Join { left, right } => (left, right, true),
        P::LeftJoin {
            left,
            right,
            expression: None,
        } => (left, right, false),
        _ => return Ok(()),
    };
    let sides: &[bool] = if either { &[true, false] } else { &[true] };
    for &service_right in sides {
        let (outer, service) = if service_right {
            (&**left, &mut **right)
        } else {
            (&**right, &mut **left)
        };
        let P::Service {
            name: spargebra::term::NamedNodePattern::NamedNode(name),
            inner,
            silent: false,
        } = &*service
        else {
            // A failed SILENT call is one empty solution for the whole SERVICE; split into
            // batches, each failed batch would add its own and repeat the outer rows.
            continue;
        };
        let shared = shared_variables(outer, inner);
        if shared.is_empty() {
            continue;
        }
        let Some(batch) = binds(name.as_str(), inner, &shared) else {
            continue;
        };
        let Some(rows) = run(outer.clone(), &shared)? else {
            return Ok(());
        };
        let Some(values) = ground(rows) else {
            return Ok(());
        };
        let (name, inner) = (name.clone(), inner.clone());
        *service = values
            .chunks(batch.max(1))
            .map(|batch| P::Service {
                name: spargebra::term::NamedNodePattern::NamedNode(name.clone()),
                inner: Box::new(P::Join {
                    left: Box::new(P::Values {
                        variables: shared.clone(),
                        bindings: batch.to_vec(),
                    }),
                    right: inner.clone(),
                }),
                silent: false,
            })
            .reduce(|a, b| P::Union {
                left: Box::new(a),
                right: Box::new(b),
            })
            // No rows on the outer side: nothing can join, and nothing is called.
            .unwrap_or(P::Values {
                variables: shared,
                bindings: Vec::new(),
            });
        return Ok(());
    }
    Ok(())
}

/// Passes the `LIMIT` of a query that is one SERVICE (`SELECT … WHERE { SERVICE <iri> { … } }
/// LIMIT n`, also with `OFFSET`) into the SERVICE when `takes` says so: any `OFFSET + LIMIT`
/// of the SERVICE's answers give the query's, so the service may make fewer.
pub(crate) fn push_limit(pattern: &mut GraphPattern, takes: &dyn Fn(&str, &GraphPattern) -> bool) {
    use GraphPattern as P;
    let P::Slice {
        inner,
        start,
        length: Some(length),
    } = pattern
    else {
        return;
    };
    let wanted = start.saturating_add(*length);
    let service = match &mut **inner {
        P::Project { inner, .. } => &mut **inner,
        other => other,
    };
    let P::Service {
        name: spargebra::term::NamedNodePattern::NamedNode(name),
        inner,
        silent: false,
    } = service
    else {
        return;
    };
    if matches!(**inner, P::Slice { .. }) || !takes(name.as_str(), inner) {
        return;
    }
    let old = std::mem::replace(
        &mut **inner,
        P::Bgp {
            patterns: Vec::new(),
        },
    );
    **inner = P::Slice {
        inner: Box::new(old),
        start: 0,
        length: Some(wanted),
    };
}

/// Replaces `GRAPH <iri> { inner }` by `SERVICE <service> { inner }` for each named virtual
/// graph `iri` of `graphs` (#216), at any depth (not inside `EXISTS`, whose patterns are
/// expressions).
pub(crate) fn virtual_graphs(
    pattern: &mut GraphPattern,
    graphs: &std::collections::HashMap<oxigraph::model::NamedNode, oxigraph::model::NamedNode>,
) {
    use GraphPattern as P;
    use spargebra::term::NamedNodePattern;
    if let P::Graph {
        name: NamedNodePattern::NamedNode(name),
        inner,
    } = pattern
        && let Some(service) = graphs.get(name)
    {
        let inner = std::mem::replace(
            &mut **inner,
            P::Bgp {
                patterns: Vec::new(),
            },
        );
        *pattern = P::Service {
            name: NamedNodePattern::NamedNode(service.clone()),
            inner: Box::new(inner),
            silent: false,
        };
    }
    match pattern {
        P::Join { left, right }
        | P::Union { left, right }
        | P::Minus { left, right }
        | P::Lateral { left, right }
        | P::LeftJoin { left, right, .. } => {
            virtual_graphs(left, graphs);
            virtual_graphs(right, graphs);
        }
        P::Filter { inner, .. }
        | P::Extend { inner, .. }
        | P::OrderBy { inner, .. }
        | P::Project { inner, .. }
        | P::Distinct { inner }
        | P::Reduced { inner }
        | P::Slice { inner, .. }
        | P::Group { inner, .. }
        | P::Graph { inner, .. } => virtual_graphs(inner, graphs),
        // A SERVICE's pattern is the service's to read.
        P::Service { .. } | P::Bgp { .. } | P::Path { .. } | P::Values { .. } => {}
    }
}

/// The variables of `inner` that `outer` binds too.
fn shared_variables(outer: &GraphPattern, inner: &GraphPattern) -> Vec<Variable> {
    let mut outer_vars = Vec::new();
    outer.on_in_scope_variable(|v| {
        if !outer_vars.contains(v) {
            outer_vars.push(v.clone());
        }
    });
    let mut shared = Vec::new();
    inner.on_in_scope_variable(|v| {
        if outer_vars.contains(v) && !shared.contains(v) {
            shared.push(v.clone());
        }
    });
    shared
}

/// The rows as `VALUES` rows, or `None` when one holds an unbound or blank value.
fn ground(
    rows: Vec<Vec<Option<oxigraph::model::Term>>>,
) -> Option<Vec<Vec<Option<spargebra::term::GroundTerm>>>> {
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|term| match term {
                    Some(oxigraph::model::Term::NamedNode(n)) => {
                        Some(Some(spargebra::term::GroundTerm::NamedNode(n)))
                    }
                    Some(oxigraph::model::Term::Literal(l)) => {
                        Some(Some(spargebra::term::GroundTerm::Literal(l)))
                    }
                    _ => None,
                })
                .collect()
        })
        .collect()
}
