//! SHACL-SPARQL plumbing: running queries over a [`DataGraph`] (and the shapes graph), with
//! variables pre-bound for many focus nodes at once.
//!
//! Queries run on Oxigraph's SPARQL evaluator, made by an [`Evaluator`] (so a caller can
//! register custom functions, as the tripletrail engine does with its geo functions). The data graph
//! is the query's default graph and the shapes graph, when given, the named graph
//! `$shapesGraph` names. Pre-bound variables (`$this`, `$value`, `$currentShape`,
//! `$shapesGraph` and the parameters of constraint components) are bound by a `VALUES` block
//! joined in as the first element of the query's `WHERE` clause; with one row per focus node,
//! one query checks many focus nodes.

use crate::ShaclError;
use crate::graph::DataGraph;
use crate::path::Path;
use crate::vocab::sh;
use oxigraph::model::{BlankNode, NamedNode, NamedNodeRef, Term, Triple};
use oxigraph::sparql::{QueryResults, QuerySolution, SparqlEvaluator, Variable};
use spareval::{InternalQuad, QueryableDataset};
use spargebra::Query;
use spargebra::algebra::{Expression, GraphPattern};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern};
use std::convert::Infallible;
use std::sync::Arc;

/// Makes the SPARQL evaluators SHACL-SPARQL queries run on. Any `Fn() -> SparqlEvaluator`
/// is one; the tripletrail engine passes one with its geo functions registered.
pub trait Evaluator: Sync {
    fn evaluator(&self) -> SparqlEvaluator;

    /// Rewrites a query (with its focus nodes injected) before it is evaluated: the tripletrail
    /// engine plans it as its own queries. As it is by default.
    fn prepare(&self, query: Query) -> Query {
        query
    }

    /// Runs a rule's `CONSTRUCT` (its focus nodes injected) on the evaluator's own store, in
    /// which the data graph is the default graph, after adding `inferred` there: every triple
    /// inferred so far, in the order they came (earlier calls got a prefix of it). `None` when
    /// the evaluator has no such store; the query then runs on the data graph and the
    /// inferred triples with [`Evaluator::evaluator`].
    fn construct_native(
        &self,
        _query: &Query,
        _inferred: &[Triple],
    ) -> Option<Result<Vec<Triple>, String>> {
        None
    }

    /// The solutions of a `SELECT` (a rule's `CONSTRUCT` pattern, for its provenance) on the
    /// evaluator's own store, after adding `inferred` there, as for
    /// [`Evaluator::construct_native`]. `None` when the evaluator has no such store.
    fn select_native(
        &self,
        _query: &Query,
        _inferred: &[Triple],
    ) -> Option<Result<Vec<QuerySolution>, String>> {
        None
    }

    /// The `$this` values of a SPARQL target's `SELECT` on the evaluator's own store, after
    /// adding `inferred` there, as for [`Evaluator::construct_native`]. `None` when the
    /// evaluator has no such store.
    fn focus_nodes_native(
        &self,
        _query: &Query,
        _inferred: &[Triple],
    ) -> Option<Result<Vec<Term>, String>> {
        None
    }
}

impl<F: Fn() -> SparqlEvaluator + Sync + ?Sized> Evaluator for F {
    fn evaluator(&self) -> SparqlEvaluator {
        self()
    }
}

/// Oxigraph's evaluator with nothing added.
pub(crate) fn plain_evaluator() -> SparqlEvaluator {
    SparqlEvaluator::new()
}

/// Blank nodes cannot be written in `VALUES`: they are bound as IRIs with this prefix, which
/// the dataset reads back as the blank nodes.
const BNODE: &str = "urn:x-tripletrail-shacl:bnode:";

/// What a query runs on: the data graph as the default graph, and the shapes graph as a named
/// graph.
#[derive(Clone, Copy)]
pub(crate) struct QueryData<'a> {
    pub data: &'a dyn DataGraph,
    pub shapes: Option<(&'a NamedNode, &'a dyn DataGraph)>,
    /// The triples inferred since a rule last ran, read only by `GRAPH` with the name
    /// [`crate::delta::DELTA_GRAPH`] (a semi-naive variant): not one of the named graphs.
    pub delta: Option<&'a dyn DataGraph>,
}

impl<'a> QueryableDataset<'a> for QueryData<'a> {
    type InternalTerm = Term;
    type Error = Infallible;

    fn internal_quads_for_pattern(
        &self,
        subject: Option<&Term>,
        predicate: Option<&Term>,
        object: Option<&Term>,
        graph_name: Option<Option<&Term>>,
    ) -> impl Iterator<Item = Result<InternalQuad<Term>, Infallible>> + use<'a> {
        let mut quads = Vec::new();
        let predicate = match predicate {
            None => Some(None),
            Some(Term::NamedNode(p)) => Some(Some(p.as_ref())),
            Some(_) => None,
        };
        if let Some(p) = predicate {
            let mut triples = Vec::new();
            let mut add = |g: &dyn DataGraph, name: Option<Term>| {
                g.triples(subject, p, object, &mut triples);
                quads.extend(triples.drain(..).map(|(s, p, o)| {
                    Ok(InternalQuad {
                        subject: s,
                        predicate: p.into(),
                        object: o,
                        graph_name: name.clone(),
                    })
                }));
            };
            match graph_name {
                Some(None) => add(self.data, None),
                Some(Some(g)) => {
                    if let Some((name, shapes)) = self.shapes
                        && g == &Term::NamedNode(name.clone())
                    {
                        add(shapes, Some(g.clone()));
                    } else if let Some(delta) = self.delta
                        && matches!(g, Term::NamedNode(n) if n.as_str() == crate::delta::DELTA_GRAPH)
                    {
                        add(delta, Some(g.clone()));
                    }
                }
                None => {
                    if let Some((name, shapes)) = self.shapes {
                        add(shapes, Some(name.clone().into()));
                    }
                }
            }
        }
        quads.into_iter()
    }

    fn internal_named_graphs(&self) -> impl Iterator<Item = Result<Term, Infallible>> + use<'a> {
        self.shapes
            .map(|(name, _)| Ok(name.clone().into()))
            .into_iter()
    }

    fn contains_internal_graph_name(&self, graph_name: &Term) -> Result<bool, Infallible> {
        Ok(self
            .shapes
            .is_some_and(|(name, _)| graph_name == &Term::NamedNode(name.clone())))
    }

    fn internalize_term(&self, term: Term) -> Result<Term, Infallible> {
        Ok(match term {
            Term::NamedNode(n) => match n.as_str().strip_prefix(BNODE) {
                Some(id) => BlankNode::new_unchecked(id).into(),
                None => n.into(),
            },
            other => other,
        })
    }

    fn externalize_term(&self, term: Term) -> Result<Term, Infallible> {
        Ok(term)
    }
}

/// A term as it can be written in `VALUES`.
pub(crate) fn ground(t: &Term) -> GroundTerm {
    match t {
        Term::NamedNode(n) => GroundTerm::NamedNode(n.clone()),
        Term::Literal(l) => GroundTerm::Literal(l.clone()),
        Term::BlankNode(b) => {
            GroundTerm::NamedNode(NamedNode::new_unchecked(format!("{BNODE}{}", b.as_str())))
        }
        #[allow(unreachable_patterns)]
        other => GroundTerm::Literal(oxigraph::model::Literal::new_simple_literal(
            other.to_string(),
        )),
    }
}

/// Pre-bound variables: a `VALUES` table.
#[derive(Debug, Default, Clone)]
pub(crate) struct Bindings {
    pub variables: Vec<Variable>,
    pub rows: Vec<Vec<Option<Term>>>,
}

impl Bindings {
    pub fn new(variables: Vec<Variable>) -> Self {
        Self {
            variables,
            rows: Vec::new(),
        }
    }

    fn pattern(&self) -> GraphPattern {
        GraphPattern::Values {
            variables: self.variables.clone(),
            bindings: self
                .rows
                .iter()
                .map(|r| r.iter().map(|t| t.as_ref().map(ground)).collect())
                .collect(),
        }
    }
}

/// `query` with `variables` pre-bound to `values` (SHACL's pre-binding: a one-row `VALUES`
/// joined in first, in the query and in its sub-selects that select the variables). For the
/// bodies of `sh:SPARQLFunction`s (#235).
pub fn prebind(query: Query, variables: Vec<Variable>, values: Vec<Term>) -> Query {
    let bindings = Bindings {
        variables,
        rows: vec![values.into_iter().map(Some).collect()],
    };
    let values = bindings.pattern();
    let pattern = |p| inject_subselects(inject(p, values.clone(), true), &values, true);
    match query {
        Query::Select {
            dataset,
            pattern: p,
            base_iri,
        } => Query::Select {
            dataset,
            pattern: pattern(p),
            base_iri,
        },
        Query::Ask {
            dataset,
            pattern: p,
            base_iri,
        } => Query::Ask {
            dataset,
            pattern: pattern(p),
            base_iri,
        },
        other => other,
    }
}

/// Joins `values` in as the first element of the `WHERE` clause of a query's pattern
/// (below its solution modifiers, projection and grouping).
fn inject(pattern: GraphPattern, values: GraphPattern, top: bool) -> GraphPattern {
    let b = |p: Box<GraphPattern>, values, top| Box::new(inject(*p, values, top));
    match pattern {
        GraphPattern::Slice {
            inner,
            start,
            length,
        } if top => GraphPattern::Slice {
            inner: b(inner, values, true),
            start,
            length,
        },
        GraphPattern::Distinct { inner } if top => GraphPattern::Distinct {
            inner: b(inner, values, true),
        },
        GraphPattern::Reduced { inner } if top => GraphPattern::Reduced {
            inner: b(inner, values, true),
        },
        // The query's own projection; a projection below is a sub-select.
        GraphPattern::Project { inner, variables } if top => GraphPattern::Project {
            inner: b(inner, values, false),
            variables,
        },
        GraphPattern::OrderBy { inner, expression } => GraphPattern::OrderBy {
            inner: b(inner, values, false),
            expression,
        },
        GraphPattern::Group {
            inner,
            variables,
            aggregates,
        } => GraphPattern::Group {
            inner: b(inner, values, false),
            variables,
            aggregates,
        },
        GraphPattern::Filter { expr, inner } => GraphPattern::Filter {
            expr,
            inner: b(inner, values, false),
        },
        GraphPattern::Extend {
            inner,
            variable,
            expression,
        } => GraphPattern::Extend {
            inner: b(inner, values, false),
            variable,
            expression,
        },
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => GraphPattern::LeftJoin {
            left: b(left, values, false),
            right,
            expression,
        },
        GraphPattern::Join { left, right } => GraphPattern::Join {
            left: b(left, values, false),
            right,
        },
        // Pre-binding binds the variables in both branches (a `FILTER ($this = …)` alone in
        // a branch must see `$this`).
        GraphPattern::Union { left, right } => GraphPattern::Union {
            left: b(left, values.clone(), false),
            right: b(right, values, false),
        },
        GraphPattern::Minus { left, right } => GraphPattern::Minus {
            left: b(left, values, false),
            right,
        },
        GraphPattern::Lateral { left, right } => GraphPattern::Lateral {
            left: b(left, values, false),
            right,
        },
        other => GraphPattern::Join {
            left: Box::new(values),
            right: Box::new(other),
        },
    }
}

/// `pattern` with the pre-bound `variables` added to its own projection, so that a CONSTRUCT
/// template can use `$this` even when its WHERE clause does not (`WHERE { }` constructs the
/// template once per focus node).
fn project_bound(pattern: GraphPattern, variables: &[Variable]) -> GraphPattern {
    use GraphPattern as G;
    match pattern {
        G::Slice {
            inner,
            start,
            length,
        } => G::Slice {
            inner: Box::new(project_bound(*inner, variables)),
            start,
            length,
        },
        G::Distinct { inner } => G::Distinct {
            inner: Box::new(project_bound(*inner, variables)),
        },
        G::Reduced { inner } => G::Reduced {
            inner: Box::new(project_bound(*inner, variables)),
        },
        G::Project {
            inner,
            variables: mut projected,
        } => {
            for v in variables {
                if !projected.contains(v) {
                    projected.push(v.clone());
                }
            }
            G::Project {
                inner,
                variables: projected,
            }
        }
        other => other,
    }
}

/// Joins the columns of `values` (focus nodes in `$this`, constants) that a sub-select projects
/// into that sub-select too, as SHACL pre-binding does: `{ SELECT $this (MIN(?m) AS ?min)
/// WHERE { $this ex:p ?m } GROUP BY $this }` is evaluated for the focus nodes only, not for
/// every node of the data graph and then joined. Only columns the sub-select projects, and
/// groups by if it groups, restrict its rows the way the join with them would; a sub-select
/// under `LIMIT`/`OFFSET` is left alone (restricting its input changes which rows it keeps).
/// `top` is true at the query's own projection, which [`inject`] already handled.
fn inject_subselects(pattern: GraphPattern, values: &GraphPattern, top: bool) -> GraphPattern {
    use GraphPattern as G;
    let rec = |p: Box<G>, top| Box::new(inject_subselects(*p, values, top));
    match pattern {
        G::Slice { .. } if !top => pattern,
        G::Slice {
            inner,
            start,
            length,
        } => G::Slice {
            inner: rec(inner, true),
            start,
            length,
        },
        G::Distinct { inner } => G::Distinct {
            inner: rec(inner, top),
        },
        G::Reduced { inner } => G::Reduced {
            inner: rec(inner, top),
        },
        G::Project { inner, variables } if top => G::Project {
            inner: rec(inner, false),
            variables,
        },
        G::Project { inner, variables } => {
            let inner = inject_subselects(*inner, values, false);
            let inner = match columns_for(values, &variables, &inner) {
                Some(sub) => inject(inner, sub, false),
                None => inner,
            };
            G::Project {
                inner: Box::new(inner),
                variables,
            }
        }
        G::Join { left, right } => G::Join {
            left: rec(left, false),
            right: rec(right, false),
        },
        G::LeftJoin {
            left,
            right,
            expression,
        } => G::LeftJoin {
            left: rec(left, false),
            right: rec(right, false),
            expression,
        },
        G::Lateral { left, right } => G::Lateral {
            left: rec(left, false),
            right: rec(right, false),
        },
        G::Union { left, right } => G::Union {
            left: rec(left, false),
            right: rec(right, false),
        },
        G::Minus { left, right } => G::Minus {
            left: rec(left, false),
            right: rec(right, false),
        },
        G::Filter { expr, inner } => G::Filter {
            expr,
            inner: rec(inner, false),
        },
        G::Extend {
            inner,
            variable,
            expression,
        } => G::Extend {
            inner: rec(inner, false),
            variable,
            expression,
        },
        G::Graph { name, inner } => G::Graph {
            name,
            inner: rec(inner, false),
        },
        G::OrderBy { inner, expression } => G::OrderBy {
            inner: rec(inner, false),
            expression,
        },
        G::Group {
            inner,
            variables,
            aggregates,
        } => G::Group {
            inner: rec(inner, false),
            variables,
            aggregates,
        },
        other @ (G::Bgp { .. } | G::Path { .. } | G::Values { .. } | G::Service { .. }) => other,
    }
}

/// The columns of `values` a sub-select projecting `projected` over `inner` can be restricted
/// by (distinct rows), or `None`.
fn columns_for(
    values: &GraphPattern,
    projected: &[Variable],
    inner: &GraphPattern,
) -> Option<GraphPattern> {
    let GraphPattern::Values {
        variables,
        bindings,
    } = values
    else {
        return None;
    };
    // The grouping keys, if the sub-select groups (under its BINDs, FILTERs and ORDER BY).
    fn group_keys(p: &GraphPattern) -> Option<&[Variable]> {
        match p {
            GraphPattern::Group { variables, .. } => Some(variables),
            GraphPattern::OrderBy { inner, .. }
            | GraphPattern::Filter { inner, .. }
            | GraphPattern::Extend { inner, .. } => group_keys(inner),
            _ => None,
        }
    }
    let keys = group_keys(inner);
    let columns: Vec<usize> = (0..variables.len())
        .filter(|&i| {
            projected.contains(&variables[i]) && keys.is_none_or(|k| k.contains(&variables[i]))
        })
        .collect();
    if columns.is_empty() {
        return None;
    }
    let mut seen = std::collections::HashSet::new();
    let rows = bindings
        .iter()
        .map(|row| columns.iter().map(|&i| row[i].clone()).collect::<Vec<_>>())
        .filter(|row| seen.insert(row.clone()))
        .collect();
    Some(GraphPattern::Values {
        variables: columns.iter().map(|&i| variables[i].clone()).collect(),
        bindings: rows,
    })
}

/// Adds `var` to the projection of a SELECT pattern, unless the query groups by other
/// variables (then it cannot be). Returns whether `var` is projected.
pub(crate) fn project(pattern: &mut GraphPattern, var: &Variable) -> bool {
    fn grouped_without(p: &GraphPattern, var: &Variable) -> bool {
        match p {
            GraphPattern::Group { variables, .. } => !variables.contains(var),
            GraphPattern::OrderBy { inner, .. }
            | GraphPattern::Filter { inner, .. }
            | GraphPattern::Extend { inner, .. } => grouped_without(inner, var),
            _ => false,
        }
    }
    match pattern {
        GraphPattern::Slice { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner } => project(inner, var),
        GraphPattern::Project { inner, variables } => {
            if variables.contains(var) {
                return true;
            }
            if grouped_without(inner, var) {
                return false;
            }
            variables.push(var.clone());
            true
        }
        _ => false,
    }
}

/// A query of the shapes graph, parsed, with the constants to bind with it.
#[derive(Debug, Clone)]
pub(crate) struct ShapeQuery {
    pub query: Query,
    /// `$currentShape` and the parameters of a constraint component.
    pub constants: Vec<(Variable, Term)>,
}

impl ShapeQuery {
    /// Whether what a `CONSTRUCT` makes can depend on its focus node: whether `$this` is in
    /// its template, or in its pattern outside the sub-selects that do not return it (there
    /// it is another variable). A query that does not depend on it makes the same triples for
    /// every focus node: it runs once, not once per batch of focus nodes, each of them joined
    /// with all its solutions (#520). Only conservative: true when unsure.
    pub fn depends_on_focus(&self) -> bool {
        let Query::Construct {
            template, pattern, ..
        } = &self.query
        else {
            return true;
        };
        fn term(t: &TermPattern) -> bool {
            match t {
                TermPattern::Variable(v) => v.as_str() == "this",
                #[allow(unreachable_patterns)]
                TermPattern::NamedNode(_) | TermPattern::BlankNode(_) | TermPattern::Literal(_) => {
                    false
                }
                #[allow(unreachable_patterns)]
                _ => true,
            }
        }
        if template.iter().any(|t| {
            term(&t.subject)
                || term(&t.object)
                || matches!(&t.predicate, NamedNodePattern::Variable(v) if v.as_str() == "this")
        }) {
            return true;
        }
        mentions_this(&without_closed_subselects(pattern.clone(), true))
    }
}

/// `pattern` with each sub-select that does not return `$this` replaced by an empty pattern.
/// `top`: at the query's own solution modifiers and projection, which are no sub-select.
fn without_closed_subselects(pattern: GraphPattern, top: bool) -> GraphPattern {
    use GraphPattern as G;
    let rec = |p: Box<G>| Box::new(without_closed_subselects(*p, false));
    let keep = |p: Box<G>| Box::new(without_closed_subselects(*p, top));
    match pattern {
        G::Project { inner, variables } if top => G::Project {
            inner: rec(inner),
            variables,
        },
        G::Project { variables, .. } if !variables.iter().any(|v| v.as_str() == "this") => G::Bgp {
            patterns: Vec::new(),
        },
        G::Project { inner, variables } => G::Project {
            inner: rec(inner),
            variables,
        },
        G::Join { left, right } => G::Join {
            left: rec(left),
            right: rec(right),
        },
        G::LeftJoin {
            left,
            right,
            expression,
        } => G::LeftJoin {
            left: rec(left),
            right: rec(right),
            expression,
        },
        G::Lateral { left, right } => G::Lateral {
            left: rec(left),
            right: rec(right),
        },
        G::Union { left, right } => G::Union {
            left: rec(left),
            right: rec(right),
        },
        G::Minus { left, right } => G::Minus {
            left: rec(left),
            right: rec(right),
        },
        G::Filter { expr, inner } => G::Filter {
            expr,
            inner: rec(inner),
        },
        G::Extend {
            inner,
            variable,
            expression,
        } => G::Extend {
            inner: rec(inner),
            variable,
            expression,
        },
        G::Graph { name, inner } => G::Graph {
            name,
            inner: rec(inner),
        },
        G::OrderBy { inner, expression } => G::OrderBy {
            inner: keep(inner),
            expression,
        },
        G::Distinct { inner } => G::Distinct { inner: keep(inner) },
        G::Reduced { inner } => G::Reduced { inner: keep(inner) },
        G::Slice {
            inner,
            start,
            length,
        } => G::Slice {
            inner: keep(inner),
            start,
            length,
        },
        G::Group {
            inner,
            variables,
            aggregates,
        } => G::Group {
            inner: rec(inner),
            variables,
            aggregates,
        },
        G::Service {
            name,
            inner,
            silent,
        } => G::Service {
            name,
            inner: rec(inner),
            silent,
        },
        other => other,
    }
}

/// Whether `?this` (or `$this`) is anywhere in a pattern, its expressions included.
fn mentions_this(pattern: &GraphPattern) -> bool {
    let text = pattern.to_string();
    let bytes = text.as_bytes();
    text.match_indices("this").any(|(i, _)| {
        let before = i.checked_sub(1).map(|j| bytes[j]);
        let after = bytes.get(i + 4).copied();
        matches!(before, Some(b'?' | b'$'))
            && !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80)
    })
}

/// Runs queries for a validator or for rules.
#[derive(Clone, Copy)]
pub(crate) struct Runner<'a> {
    pub data: QueryData<'a>,
    pub evaluator: &'a dyn Evaluator,
}

impl Runner<'_> {
    /// `bindings` with the constants of `q` and `$shapesGraph` added to every row.
    fn bind(&self, q: &ShapeQuery, mut bindings: Bindings) -> Bindings {
        if bindings.variables.is_empty() && bindings.rows.is_empty() {
            bindings.rows.push(Vec::new());
        }
        let mut constants: Vec<(Variable, Term)> = q.constants.clone();
        if let Some((name, _)) = self.data.shapes {
            constants.push((Variable::new_unchecked("shapesGraph"), name.clone().into()));
        }
        for (var, value) in constants {
            if bindings.variables.contains(&var) {
                continue;
            }
            bindings.variables.push(var);
            for r in &mut bindings.rows {
                r.push(Some(value.clone()));
            }
        }
        bindings
    }

    /// The triples of a rule's `CONSTRUCT` on the evaluator's own store (see
    /// [`Evaluator::construct_native`]). `None` when it has none, or when a focus node or
    /// constant is a blank node, which a query cannot name.
    pub fn construct_native(
        &self,
        q: &ShapeQuery,
        bindings: Bindings,
        inferred: &[Triple],
    ) -> Option<Result<Vec<Triple>, ShaclError>> {
        let bindings = self.bind(q, bindings);
        if bindings
            .rows
            .iter()
            .flatten()
            .any(|t| matches!(t, Some(Term::BlankNode(_))))
        {
            return None;
        }
        let query = self.query(q, &bindings).ok()?;
        Some(
            self.evaluator
                .construct_native(&query, inferred)?
                .map_err(ShaclError::Query),
        )
    }

    /// The focus nodes of a SPARQL target on the evaluator's own store (see
    /// [`Evaluator::focus_nodes_native`]). `None` when it has none, or when a constant is a
    /// blank node.
    pub fn focus_nodes_native(
        &self,
        q: &ShapeQuery,
        inferred: &[Triple],
    ) -> Option<Result<Vec<Term>, ShaclError>> {
        let bindings = self.bind(q, Bindings::default());
        if bindings
            .rows
            .iter()
            .flatten()
            .any(|t| matches!(t, Some(Term::BlankNode(_))))
        {
            return None;
        }
        let query = self.query(q, &bindings).ok()?;
        Some(
            self.evaluator
                .focus_nodes_native(&query, inferred)?
                .map_err(ShaclError::Query),
        )
    }

    fn results(&self, q: &ShapeQuery, bindings: Bindings) -> Result<QueryResults<'_>, ShaclError> {
        let bindings = self.bind(q, bindings);
        let query = self.query(q, &bindings)?;
        self.evaluator
            .evaluator()
            .for_query(self.evaluator.prepare(query))
            .on_queryable_dataset(self.data)
            .execute()
            .map_err(|e| ShaclError::Query(e.to_string()))
    }

    /// The query with `bindings` (already bound) injected.
    fn query(&self, q: &ShapeQuery, bindings: &Bindings) -> Result<Query, ShaclError> {
        let values = bindings.pattern();
        let query = match q.query.clone() {
            Query::Select {
                pattern, base_iri, ..
            } => Query::Select {
                dataset: None,
                pattern: inject_subselects(inject(pattern, values.clone(), true), &values, true),
                base_iri,
            },
            Query::Construct {
                template,
                pattern,
                base_iri,
                ..
            } => Query::Construct {
                template,
                dataset: None,
                pattern: project_bound(
                    inject_subselects(inject(pattern, values.clone(), true), &values, true),
                    &bindings.variables,
                ),
                base_iri,
            },
            Query::Ask {
                pattern, base_iri, ..
            } => Query::Ask {
                dataset: None,
                pattern: inject_subselects(inject(pattern, values.clone(), true), &values, true),
                base_iri,
            },
            Query::Describe { .. } => {
                return Err(ShaclError::Shape(
                    "DESCRIBE queries are not supported".into(),
                ));
            }
        };
        Ok(query)
    }

    /// The solutions of a SELECT query.
    pub fn select(
        &self,
        q: &ShapeQuery,
        bindings: Bindings,
    ) -> Result<Vec<QuerySolution>, ShaclError> {
        match self.results(q, bindings)? {
            QueryResults::Solutions(s) => s
                .collect::<Result<_, _>>()
                .map_err(|e| ShaclError::Query(e.to_string())),
            _ => Err(ShaclError::Shape("expected a SELECT query".into())),
        }
    }

    /// The triples a CONSTRUCT query makes, each with the solution that made it: the query's
    /// pattern runs as a SELECT and the template is filled in here, as CONSTRUCT does (fresh
    /// blank nodes per solution; a triple with an unbound or misplaced term is left out).
    /// With `native` (the inferred triples so far), the SELECT runs on the evaluator's own
    /// store when it has one, as [`Runner::construct_native`] does (#455), unless a focus node
    /// or constant is a blank node, which a query cannot name.
    pub fn construct_traced(
        &self,
        q: &ShapeQuery,
        bindings: Bindings,
        native: Option<&[Triple]>,
    ) -> Result<Vec<(Triple, Arc<QuerySolution>)>, ShaclError> {
        let bindings = self.bind(q, bindings);
        let blank = bindings
            .rows
            .iter()
            .flatten()
            .any(|t| matches!(t, Some(Term::BlankNode(_))));
        let Query::Construct {
            template,
            pattern,
            base_iri,
            ..
        } = self.query(q, &bindings)?
        else {
            return Err(ShaclError::Shape("expected a CONSTRUCT query".into()));
        };
        let select = Query::Select {
            dataset: None,
            pattern,
            base_iri,
        };
        let solutions: Vec<QuerySolution> = match native
            .filter(|_| !blank)
            .and_then(|log| self.evaluator.select_native(&select, log))
        {
            Some(solutions) => solutions.map_err(ShaclError::Query)?,
            None => {
                let results = self
                    .evaluator
                    .evaluator()
                    .for_query(self.evaluator.prepare(select))
                    .on_queryable_dataset(self.data)
                    .execute()
                    .map_err(|e| ShaclError::Query(e.to_string()))?;
                let QueryResults::Solutions(solutions) = results else {
                    return Err(ShaclError::Shape("expected solutions".into()));
                };
                solutions
                    .collect::<Result<_, _>>()
                    .map_err(|e| ShaclError::Query(e.to_string()))?
            }
        };
        let mut out = Vec::new();
        for solution in solutions {
            let solution = Arc::new(solution);
            let mut blanks: Vec<(String, BlankNode)> = Vec::new();
            let mut term = |t: &TermPattern| -> Option<Term> {
                match t {
                    TermPattern::NamedNode(n) => Some(n.clone().into()),
                    TermPattern::Literal(l) => Some(l.clone().into()),
                    TermPattern::Variable(v) => solution.get(v.as_str()).cloned(),
                    TermPattern::BlankNode(b) => {
                        let id = b.as_str();
                        Some(match blanks.iter().find(|(k, _)| k == id) {
                            Some((_, n)) => n.clone().into(),
                            None => {
                                let n = BlankNode::default();
                                blanks.push((id.to_owned(), n.clone()));
                                n.into()
                            }
                        })
                    }
                    #[allow(unreachable_patterns)]
                    _ => None,
                }
            };
            for t in &template {
                let subject = match term(&t.subject) {
                    Some(Term::NamedNode(n)) => n.into(),
                    Some(Term::BlankNode(b)) => oxigraph::model::NamedOrBlankNode::from(b),
                    _ => continue,
                };
                let predicate = match &t.predicate {
                    NamedNodePattern::NamedNode(n) => n.clone(),
                    NamedNodePattern::Variable(v) => match solution.get(v.as_str()) {
                        Some(Term::NamedNode(n)) => n.clone(),
                        _ => continue,
                    },
                };
                let Some(object) = term(&t.object) else {
                    continue;
                };
                out.push((
                    Triple::new(subject, predicate, object),
                    Arc::clone(&solution),
                ));
            }
        }
        Ok(out)
    }

    /// The triples a CONSTRUCT query makes.
    pub fn construct(&self, q: &ShapeQuery, bindings: Bindings) -> Result<Vec<Triple>, ShaclError> {
        match self.results(q, bindings)? {
            QueryResults::Graph(t) => t
                .collect::<Result<_, _>>()
                .map_err(|e| ShaclError::Query(e.to_string())),
            _ => Err(ShaclError::Shape("expected a CONSTRUCT query".into())),
        }
    }
}

/// The prefix declarations of a SHACL-SPARQL node (`sh:prefixes`, then `sh:declare` with
/// `sh:prefix` and `sh:namespace`, following `owl:imports` within the shapes graph).
pub(crate) fn prefixes(
    g: &dyn DataGraph,
    node: &Term,
) -> Result<Vec<(String, String)>, ShaclError> {
    const IMPORTS: NamedNodeRef<'static> =
        NamedNodeRef::new_unchecked("http://www.w3.org/2002/07/owl#imports");
    let mut out = Vec::new();
    let mut seen = Vec::new();
    let mut todo = Vec::new();
    g.objects(node, sh::PREFIXES, &mut todo);
    while let Some(p) = todo.pop() {
        if seen.contains(&p) || seen.len() > 256 {
            continue;
        }
        g.objects(&p, IMPORTS, &mut todo);
        let mut declarations = Vec::new();
        g.objects(&p, sh::DECLARE, &mut declarations);
        for d in declarations {
            let text = |pred| match g.object(&d, pred) {
                Some(Term::Literal(l)) => Some(l.value().to_owned()),
                Some(Term::NamedNode(n)) => Some(n.into_string()),
                _ => None,
            };
            match (text(sh::PREFIX), text(sh::NAMESPACE)) {
                (Some(prefix), Some(namespace)) => out.push((prefix, namespace)),
                _ => {
                    return Err(ShaclError::Shape(format!(
                        "{d}: sh:declare needs sh:prefix and sh:namespace"
                    )));
                }
            }
        }
        seen.push(p);
    }
    Ok(out)
}

/// Parses the query text of a SHACL-SPARQL node, with `$PATH` replaced by `path`.
/// Checks the syntax rules of pre-binding (SHACL 5.2.1, 6.2.3): no `MINUS`, `SERVICE`,
/// `VALUES`, nested `SELECT` that does not return `$this`, or `AS` assigning a pre-bound
/// variable. A query that
/// breaks them is a failure: its result would depend on how the processor pre-binds.
pub(crate) fn check_prebinding(
    query: &Query,
    prebound: &[&str],
    node: &Term,
) -> Result<(), ShaclError> {
    let pattern = match query {
        Query::Select { pattern, .. }
        | Query::Ask { pattern, .. }
        | Query::Construct { pattern, .. }
        | Query::Describe { pattern, .. } => pattern,
    };
    let refuse = |what: &str| {
        Err(ShaclError::Shape(format!(
            "{node}: {what} is not allowed in a SHACL-SPARQL query with pre-bound variables"
        )))
    };
    // The query's own solution modifiers and projection come first; a projection below them
    // is a nested SELECT. Patterns of `EXISTS` and `NOT EXISTS` are checked too.
    let mut todo = vec![(pattern, true)];
    let mut exists = Vec::new();
    while let Some((p, top)) = todo.pop() {
        match p {
            GraphPattern::Filter { expr, .. }
            | GraphPattern::Extend {
                expression: expr, ..
            } => {
                exists_patterns(expr, &mut exists);
            }
            GraphPattern::LeftJoin {
                expression: Some(expr),
                ..
            } => exists_patterns(expr, &mut exists),
            _ => {}
        }
        todo.extend(exists.drain(..).map(|p| (p, false)));
        match p {
            GraphPattern::Minus { .. } => return refuse("MINUS"),
            GraphPattern::Service { .. } => return refuse("SERVICE"),
            GraphPattern::Values { .. } => return refuse("VALUES"),
            // A nested SELECT must return `$this`, so that it is pre-bound inside it too.
            GraphPattern::Project { inner, variables } => {
                if !top && !variables.iter().any(|v| v.as_str() == "this") {
                    return refuse("a nested SELECT without $this");
                }
                todo.push((inner, false));
            }
            GraphPattern::Extend {
                inner, variable, ..
            } => {
                if prebound.contains(&variable.as_str()) {
                    return refuse(&format!("AS ?{}", variable.as_str()));
                }
                todo.push((inner, top));
            }
            GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::Slice { inner, .. }
            | GraphPattern::OrderBy { inner, .. } => todo.push((inner, top)),
            GraphPattern::Group { inner, .. }
            | GraphPattern::Filter { inner, .. }
            | GraphPattern::Graph { inner, .. } => todo.push((inner, false)),
            GraphPattern::Join { left, right }
            | GraphPattern::LeftJoin { left, right, .. }
            | GraphPattern::Lateral { left, right }
            | GraphPattern::Union { left, right } => {
                todo.push((left, false));
                todo.push((right, false));
            }
            GraphPattern::Bgp { .. } | GraphPattern::Path { .. } => {}
        }
    }
    Ok(())
}

/// The patterns of the `EXISTS` in an expression.
fn exists_patterns<'a>(e: &'a Expression, out: &mut Vec<&'a GraphPattern>) {
    use Expression as E;
    match e {
        E::Exists(p) => out.push(p),
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
        | E::Divide(a, b) => {
            exists_patterns(a, out);
            exists_patterns(b, out);
        }
        E::UnaryPlus(a) | E::UnaryMinus(a) | E::Not(a) => exists_patterns(a, out),
        E::In(a, list) => {
            exists_patterns(a, out);
            list.iter().for_each(|e| exists_patterns(e, out));
        }
        E::If(a, b, c) => {
            exists_patterns(a, out);
            exists_patterns(b, out);
            exists_patterns(c, out);
        }
        E::Coalesce(list) | E::FunctionCall(_, list) => {
            list.iter().for_each(|e| exists_patterns(e, out));
        }
        E::NamedNode(_) | E::Literal(_) | E::Variable(_) | E::Bound(_) => {}
    }
}

pub(crate) fn parse(
    text: &str,
    prefixes: &[(String, String)],
    path: Option<&Path>,
    node: &Term,
) -> Result<Query, ShaclError> {
    let text = if text.contains("$PATH") {
        let Some(path) = path else {
            return Err(ShaclError::Shape(format!(
                "{node}: $PATH in a query of a shape without sh:path"
            )));
        };
        let replaced = replace_path(text, &path.to_sparql());
        match parse_text(&replaced, prefixes, node) {
            // `$PATH` in a projection (`SELECT $this $PATH WHERE`, as in ERA's RINF shapes)
            // is no path there: it stays the variable, unbound, as Jena reads it.
            Err(e) => {
                let kept = replace_path(&keep_projected_path(text), &path.to_sparql());
                return parse_text(&kept, prefixes, node).map_err(|_| e);
            }
            ok => return ok,
        }
    } else {
        text.to_owned()
    };
    parse_text(&text, prefixes, node)
}

fn parse_text(text: &str, prefixes: &[(String, String)], node: &Term) -> Result<Query, ShaclError> {
    let mut parser = spargebra::SparqlParser::new();
    for (prefix, namespace) in prefixes {
        parser = parser
            .with_prefix(prefix.clone(), namespace.clone())
            .map_err(|e| ShaclError::Shape(format!("{node}: prefix {prefix}: {e}")))?;
    }
    parser
        .parse_query(text)
        .map_err(|e| ShaclError::Shape(format!("{node}: {e}")))
}

/// `text` with each `$PATH` between a `SELECT` and its `WHERE` (or `{`) written `?PATH`, so
/// [`replace_path`] leaves it a variable. Strings, IRIs and comments are skipped.
fn keep_projected_path(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut projection) = (0, false);
    let word = |i: usize, w: &str| {
        b.len() >= i + w.len()
            && b[i..i + w.len()].eq_ignore_ascii_case(w.as_bytes())
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
            && b.get(i + w.len())
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
    };
    while i < b.len() {
        let start = i;
        match b[i] {
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'<' if b[i + 1..]
                .iter()
                .take_while(|c| **c != b'>')
                .all(|c| !c.is_ascii_whitespace()) =>
            {
                i += 1;
                while i < b.len() && b[i] != b'>' {
                    i += 1;
                }
                i += 1;
            }
            q @ (b'"' | b'\'') => {
                let long = b[i..].starts_with(&[q, q, q]);
                i += if long { 3 } else { 1 };
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                    } else if long && b[i..].starts_with(&[q, q, q]) {
                        i += 3;
                        break;
                    } else if !long && b[i] == q {
                        i += 1;
                        break;
                    } else {
                        i += 1;
                    }
                }
            }
            b'{' => {
                projection = false;
                i += 1;
            }
            b'$' if projection && word(i + 1, "PATH") => {
                out.push_str("?PATH");
                i += 5;
                continue;
            }
            _ if word(i, "SELECT") => {
                projection = true;
                i += 6;
            }
            _ if word(i, "WHERE") => {
                projection = false;
                i += 5;
            }
            _ => {
                i += text[i..].chars().next().map_or(1, char::len_utf8);
            }
        }
        out.push_str(&text[start..i.min(b.len())]);
    }
    out
}

/// `$PATH` (not followed by a name character) replaced by `path`.
fn replace_path(text: &str, path: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("$PATH") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 5..];
        if after
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            out.push_str("$PATH");
        } else {
            out.push_str(path);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

pub(crate) fn var(name: &str) -> Variable {
    Variable::new_unchecked(name)
}

#[cfg(test)]
mod tests {
    use super::{inject, inject_subselects, replace_path};
    use spargebra::algebra::GraphPattern;
    use spargebra::term::NamedNode;

    fn where_of(query: &str) -> GraphPattern {
        match spargebra::SparqlParser::new().parse_query(query).unwrap() {
            spargebra::Query::Select { pattern, .. } => pattern,
            _ => unreachable!(),
        }
    }

    fn focus(values: &[&str]) -> GraphPattern {
        GraphPattern::Values {
            variables: vec![super::var("this")],
            bindings: values
                .iter()
                .map(|v| vec![Some(NamedNode::new_unchecked(*v).into())])
                .collect(),
        }
    }

    fn injected(query: &str) -> String {
        let values = focus(&["http://e/a", "http://e/b"]);
        let p = inject(where_of(query), values.clone(), true);
        inject_subselects(p, &values, true).to_string()
    }

    #[test]
    fn focus_nodes_go_into_subselects_grouped_on_this() {
        let q = injected(
            "SELECT $this ?min WHERE { $this <http://e/q> ?x . \
             { SELECT $this (MIN(?m) AS ?min) WHERE { $this <http://e/p> ?m } GROUP BY $this } }",
        );
        assert_eq!(q.matches("VALUES ( ?this )").count(), 2, "{q}");
    }

    #[test]
    fn subselects_not_grouped_on_this_or_sliced_are_left_alone() {
        for sub in [
            "SELECT (MIN(?m) AS ?min) WHERE { $this <http://e/p> ?m }",
            "SELECT $this (COUNT(*) AS ?n) WHERE { $this <http://e/p> ?m } GROUP BY $this LIMIT 1",
            "SELECT ?y WHERE { $this <http://e/p> ?y }",
        ] {
            let q = injected(&format!(
                "SELECT $this WHERE {{ $this <http://e/q> ?x . {{ {sub} }} }}"
            ));
            assert_eq!(q.matches("VALUES ( ?this )").count(), 1, "{q}");
        }
    }

    #[test]
    fn projected_path_stays_a_variable() {
        use super::keep_projected_path as k;
        assert_eq!(
            k("SELECT $this $PATH WHERE { $this $PATH ?v }"),
            "SELECT $this ?PATH WHERE { $this $PATH ?v }"
        );
        // Subqueries, case, strings, IRIs and comments.
        assert_eq!(
            k(
                "select $PATH { { SELECT $PATH ?x { $this $PATH ?x } } # SELECT $PATH\n \
               FILTER(?x != \"SELECT $PATH\" && ?x != <urn:$PATH>) }"
            ),
            "select ?PATH { { SELECT ?PATH ?x { $this $PATH ?x } } # SELECT $PATH\n \
               FILTER(?x != \"SELECT $PATH\" && ?x != <urn:$PATH>) }"
        );
        assert_eq!(k("SELECT $PATHS WHERE {}"), "SELECT $PATHS WHERE {}");
    }

    #[test]
    fn construct_depends_on_focus_only_where_this_is_in_scope() {
        let depends = |q: &str| {
            super::ShapeQuery {
                query: spargebra::SparqlParser::new().parse_query(q).unwrap(),
                constants: Vec::new(),
            }
            .depends_on_focus()
        };
        // `$this` only in a sub-select that does not return it (#520).
        assert!(!depends(
            "CONSTRUCT { ?l <http://e/c> ?m } WHERE { \
             { SELECT ?l (MIN(?x) AS ?m) WHERE { ?e <http://e/s> ?l . \
               BIND(IRI(STR(?e)) AS $this) $this <http://e/k> ?x } GROUP BY ?l } }"
        ));
        assert!(!depends(
            "CONSTRUCT { ?s <http://e/c> ?o } WHERE { ?s <http://e/p> ?o . ?other <http://e/thisx> ?o }"
        ));
        for q in [
            "CONSTRUCT { ?s <http://e/c> ?o } WHERE { $this <http://e/p> ?o BIND($this AS ?s) }",
            "CONSTRUCT { $this <http://e/c> ?o } WHERE { ?s <http://e/p> ?o }",
            "CONSTRUCT { ?s <http://e/c> ?o } WHERE { ?s <http://e/p> ?o FILTER(?o != $this) }",
            "CONSTRUCT { ?s <http://e/c> ?o } WHERE { ?s <http://e/p> ?o FILTER EXISTS { ?o <http://e/q> $this } }",
            "CONSTRUCT { ?s <http://e/c> ?m } WHERE { ?s <http://e/p> ?o . \
             { SELECT $this (MIN(?x) AS ?m) WHERE { $this <http://e/k> ?x } GROUP BY $this } }",
        ] {
            assert!(depends(q), "{q}");
        }
    }

    #[test]
    fn path_replacement() {
        assert_eq!(
            replace_path("$this $PATH ?v . $PATHS", "<p>"),
            "$this <p> ?v . $PATHS"
        );
    }
}
