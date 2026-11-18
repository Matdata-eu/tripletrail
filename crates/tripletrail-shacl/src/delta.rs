//! Semi-naive passes for SPARQL rules (issue #110): after a rule's first run, its new results
//! all use at least one triple inferred since (the delta). The query is rewritten into
//! variants, each with one of its triple patterns replaced by the delta triples that match it
//! (a `VALUES` block), the rest of the pattern still reading all the data. Together the
//! variants give every result that uses the delta, and each gives only results of the query.
//! A triple pattern matching more than [`MAX_DELTA_ROWS`] delta triples reads them from the
//! delta itself instead, as the named graph [`DELTA_GRAPH`], so its variant is joined with
//! the rest of the pattern like any other triple pattern.
//!
//! [`variants`] returns `None` when the pattern has a part where that doesn't hold, or that it
//! doesn't handle: aggregates, `LIMIT` / `OFFSET`, `SERVICE`, a property path or an `EXISTS`
//! that may read the delta, or RDF 1.2 triple terms. The rule then runs on all the data, as in
//! its first pass.

use crate::graph::{DataGraph, Triple3};
use crate::sparql::ground;
use oxigraph::model::{NamedNode, Term, Triple};
use oxigraph::model::{NamedNodeRef, NamedOrBlankNode};
use rustc_hash::{FxHashMap, FxHashSet};
use spargebra::algebra::{Expression, GraphPattern, PropertyPathExpression};
use spargebra::term::{GroundTerm, NamedNodePattern, TermPattern, TriplePattern, Variable};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// The most delta triples a triple pattern takes as a `VALUES` block: with more, the block
/// costs more than reading the delta graph (it is built and hashed for each batch of focus
/// nodes).
/// Small in tests, so that they read the delta graph too.
const MAX_DELTA_ROWS: usize = if cfg!(test) { 8 } else { 16_384 };

/// The name under which a variant reads the delta triples (see the module documentation).
/// Only a `GRAPH` with this exact name reads it: it is not one of the dataset's named graphs.
pub(crate) const DELTA_GRAPH: &str = "urn:x-tripletrail-shacl:delta";

/// The triples inferred since a rule last ran, by predicate. As a [`DataGraph`], what a
/// variant reads as [`DELTA_GRAPH`]: the subject and object indexes are built on first use.
pub(crate) struct Delta<'a> {
    by_predicate: FxHashMap<&'a NamedNode, Vec<&'a Triple>>,
    all: Vec<&'a Triple>,
    /// Whether a variant reads [`DELTA_GRAPH`].
    reads_graph: AtomicBool,
    by_subject: OnceLock<FxHashMap<&'a NamedOrBlankNode, Vec<&'a Triple>>>,
    by_object: OnceLock<FxHashMap<&'a Term, Vec<&'a Triple>>>,
}

impl<'a> Delta<'a> {
    pub(crate) fn new(triples: &'a [Triple]) -> Self {
        let mut by_predicate: FxHashMap<&NamedNode, Vec<&Triple>> = FxHashMap::default();
        for t in triples {
            by_predicate.entry(&t.predicate).or_default().push(t);
        }
        Self {
            by_predicate,
            all: triples.iter().collect(),
            reads_graph: AtomicBool::new(false),
            by_subject: OnceLock::new(),
            by_object: OnceLock::new(),
        }
    }

    /// Whether the variants read the delta as [`DELTA_GRAPH`], which the query's dataset
    /// must then hold.
    pub(crate) fn reads_graph(&self) -> bool {
        self.reads_graph.load(Ordering::Relaxed)
    }

    /// The delta triples a triple pattern's predicate may match.
    fn candidates(&self, p: &NamedNodePattern) -> &[&'a Triple] {
        match p {
            NamedNodePattern::NamedNode(n) => self.by_predicate.get(n).map_or(&[], |v| v),
            NamedNodePattern::Variable(_) => &self.all,
        }
    }

    fn has_predicate(&self, p: &NamedNode) -> bool {
        self.by_predicate.contains_key(p)
    }

    /// The delta triples that may match `s p o`, from the most selective index.
    fn lookup(
        &self,
        s: Option<&Term>,
        p: Option<NamedNodeRef<'_>>,
        o: Option<&Term>,
    ) -> &[&'a Triple] {
        if let Some(s) = s {
            let Some(s) = crate::graph::subject(s) else {
                return &[];
            };
            let index = self.by_subject.get_or_init(|| {
                let mut m: FxHashMap<_, Vec<_>> = FxHashMap::default();
                for t in &self.all {
                    m.entry(&t.subject).or_default().push(*t);
                }
                m
            });
            return index.get(&s.into_owned()).map_or(&[], |v| v);
        }
        if let Some(o) = o {
            let index = self.by_object.get_or_init(|| {
                let mut m: FxHashMap<_, Vec<_>> = FxHashMap::default();
                for t in &self.all {
                    m.entry(&t.object).or_default().push(*t);
                }
                m
            });
            return index.get(o).map_or(&[], |v| v);
        }
        match p {
            Some(p) => self.by_predicate.get(&p.into_owned()).map_or(&[], |v| v),
            None => &self.all,
        }
    }
}

impl DataGraph for Delta<'_> {
    fn objects(&self, s: &Term, p: NamedNodeRef<'_>, out: &mut Vec<Term>) {
        out.extend(
            self.lookup(Some(s), Some(p), None)
                .iter()
                .filter(|t| t.predicate == p)
                .map(|t| t.object.clone()),
        );
    }

    fn subjects(&self, p: NamedNodeRef<'_>, o: &Term, out: &mut Vec<Term>) {
        out.extend(
            self.lookup(None, Some(p), Some(o))
                .iter()
                .filter(|t| t.predicate == p)
                .map(|t| t.subject.clone().into()),
        );
    }

    fn outgoing(&self, s: &Term, out: &mut Vec<(NamedNode, Term)>) {
        out.extend(
            self.lookup(Some(s), None, None)
                .iter()
                .map(|t| (t.predicate.clone(), t.object.clone())),
        );
    }

    fn with_predicate(&self, p: NamedNodeRef<'_>, out: &mut Vec<(Term, Term)>) {
        out.extend(
            self.lookup(None, Some(p), None)
                .iter()
                .map(|t| (t.subject.clone().into(), t.object.clone())),
        );
    }

    fn predicates(&self, out: &mut Vec<NamedNode>) {
        out.extend(self.by_predicate.keys().map(|p| (*p).clone()));
    }

    fn triples(
        &self,
        s: Option<&Term>,
        p: Option<NamedNodeRef<'_>>,
        o: Option<&Term>,
        out: &mut Vec<Triple3>,
    ) {
        out.extend(
            self.lookup(s, p, o)
                .iter()
                .filter(|t| {
                    s.is_none_or(|s| crate::graph::subject(s) == Some(t.subject.as_ref()))
                        && p.is_none_or(|p| t.predicate == p)
                        && o.is_none_or(|o| t.object == *o)
                })
                .map(|t| {
                    (
                        t.subject.clone().into(),
                        t.predicate.clone(),
                        t.object.clone(),
                    )
                }),
        );
    }
}

/// The variants of `pattern` (see the module documentation): empty when no triple pattern can
/// match the delta, so the rule has no new results.
pub(crate) fn variants(pattern: &GraphPattern, delta: &Delta<'_>) -> Option<Vec<GraphPattern>> {
    use GraphPattern as P;
    let map = |inner: &P, f: &dyn Fn(P) -> P| -> Option<Vec<P>> {
        Some(variants(inner, delta)?.into_iter().map(f).collect())
    };
    Some(match pattern {
        P::Bgp { patterns } => {
            let mut out = Vec::new();
            for (i, t) in patterns.iter().enumerate() {
                let Some(values) = atom(t, delta)? else {
                    continue;
                };
                // The pattern's blank nodes are variables of the whole BGP: the same ones in
                // the block and in the rest.
                let rest: Vec<TriplePattern> = patterns
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, t)| TriplePattern {
                        subject: no_blank(&t.subject),
                        predicate: t.predicate.clone(),
                        object: no_blank(&t.object),
                    })
                    .collect();
                // The rest on the left: the focus nodes' `VALUES` goes into the left-most part
                // of a join, and next to the delta block it would make a cartesian product.
                out.push(P::Join {
                    left: Box::new(P::Bgp { patterns: rest }),
                    right: Box::new(values),
                });
            }
            out
        }
        P::Path {
            subject,
            path,
            object,
        } => {
            if [subject, object]
                .iter()
                .any(|t| matches!(t, TermPattern::Triple(_)))
                || path_touches(path, delta)
            {
                return None;
            }
            Vec::new()
        }
        P::Join { left, right } => {
            let mut out: Vec<P> = variants(left, delta)?
                .into_iter()
                .map(|l| P::Join {
                    left: Box::new(l),
                    right: right.clone(),
                })
                .collect();
            out.extend(variants(right, delta)?.into_iter().map(|r| P::Join {
                left: left.clone(),
                right: Box::new(r),
            }));
            out
        }
        P::Lateral { left, right } => {
            let mut out: Vec<P> = variants(left, delta)?
                .into_iter()
                .map(|l| P::Lateral {
                    left: Box::new(l),
                    right: right.clone(),
                })
                .collect();
            out.extend(variants(right, delta)?.into_iter().map(|r| P::Lateral {
                left: left.clone(),
                right: Box::new(r),
            }));
            out
        }
        P::LeftJoin {
            left,
            right,
            expression,
        } => {
            if expression.as_ref().is_some_and(|e| expr_touches(e, delta)) {
                return None;
            }
            let mut out: Vec<P> = variants(left, delta)?
                .into_iter()
                .map(|l| P::LeftJoin {
                    left: Box::new(l),
                    right: right.clone(),
                    expression: expression.clone(),
                })
                .collect();
            // A new match of the optional side: the rows where it matches, as a join (with
            // the rows it doesn't match, it would also give old rows without its variables).
            for r in variants(right, delta)? {
                let join = P::Join {
                    left: left.clone(),
                    right: Box::new(r),
                };
                out.push(match expression {
                    Some(e) => P::Filter {
                        expr: e.clone(),
                        inner: Box::new(join),
                    },
                    None => join,
                });
            }
            out
        }
        P::Filter { expr, inner } => {
            if expr_touches(expr, delta) {
                return None;
            }
            map(inner, &|i| P::Filter {
                expr: expr.clone(),
                inner: Box::new(i),
            })?
        }
        P::Union { left, right } => {
            let mut out = variants(left, delta)?;
            out.extend(variants(right, delta)?);
            out
        }
        // The right side only takes rows away.
        P::Minus { left, right } => variants(left, delta)?
            .into_iter()
            .map(|l| P::Minus {
                left: Box::new(l),
                right: right.clone(),
            })
            .collect(),
        P::Extend {
            inner,
            variable,
            expression,
        } => {
            if expr_touches(expression, delta) {
                return None;
            }
            map(inner, &|i| P::Extend {
                inner: Box::new(i),
                variable: variable.clone(),
                expression: expression.clone(),
            })?
        }
        P::Values { .. } => Vec::new(),
        // Rules read the data graph as the default graph: a named graph (the shapes graph)
        // has no delta.
        P::Graph { .. } => Vec::new(),
        P::OrderBy { inner, expression } => map(inner, &|i| P::OrderBy {
            inner: Box::new(i),
            expression: expression.clone(),
        })?,
        P::Project { inner, variables } => map(inner, &|i| P::Project {
            inner: Box::new(i),
            variables: variables.clone(),
        })?,
        P::Distinct { inner } => map(inner, &|i| P::Distinct { inner: Box::new(i) })?,
        P::Reduced { inner } => map(inner, &|i| P::Reduced { inner: Box::new(i) })?,
        // A count or a first page can change with any new row, not only add to the results.
        P::Group { inner, .. } | P::Slice { inner, .. } => {
            if touches(inner, delta)? {
                return None;
            }
            Vec::new()
        }
        P::Service { .. } => return None,
    })
}

/// The delta triples matching `t` as a `VALUES` block of its variables (`Some(None)`: none
/// match), or `t` in [`DELTA_GRAPH`] when more than [`MAX_DELTA_ROWS`] match. `None` for a
/// triple term.
fn atom(t: &TriplePattern, delta: &Delta<'_>) -> Option<Option<GraphPattern>> {
    let positions = [&t.subject, &t.object];
    if positions
        .iter()
        .any(|p| matches!(p, TermPattern::Triple(_)))
    {
        return None;
    }
    // Variables (blank nodes of the pattern are variables too), in order of first use.
    let mut vars: Vec<Variable> = Vec::new();
    let mut slot = |name: Variable| -> usize {
        match vars.iter().position(|v| *v == name) {
            Some(i) => i,
            None => {
                vars.push(name);
                vars.len() - 1
            }
        }
    };
    #[derive(Clone)]
    enum Pos {
        Var(usize),
        Const(Term),
    }
    let term_pos = |p: &TermPattern, slot: &mut dyn FnMut(Variable) -> usize| match no_blank(p) {
        TermPattern::Variable(v) => Pos::Var(slot(v)),
        TermPattern::NamedNode(n) => Pos::Const(n.clone().into()),
        TermPattern::Literal(l) => Pos::Const(l.clone().into()),
        #[allow(unreachable_patterns)]
        _ => Pos::Const(Term::NamedNode(NamedNode::new_unchecked(
            "urn:x-tripletrail:none",
        ))),
    };
    let s = term_pos(&t.subject, &mut slot);
    let p = match &t.predicate {
        NamedNodePattern::Variable(v) => Pos::Var(slot(v.clone())),
        NamedNodePattern::NamedNode(n) => Pos::Const(n.clone().into()),
    };
    let o = term_pos(&t.object, &mut slot);
    // The constants are checked before anything is copied, and the limit applies to the
    // triples that fit: `?x a ex:C` matches few of many delta `rdf:type` triples.
    let const_fits = |d: &Triple| {
        let subject_fits = match &s {
            Pos::Const(c) => match (c, &d.subject) {
                (Term::NamedNode(a), oxigraph::model::NamedOrBlankNode::NamedNode(b)) => a == b,
                (Term::BlankNode(a), oxigraph::model::NamedOrBlankNode::BlankNode(b)) => a == b,
                _ => false,
            },
            Pos::Var(_) => true,
        };
        let predicate_fits = match &p {
            Pos::Const(Term::NamedNode(c)) => *c == d.predicate,
            Pos::Const(_) => false,
            Pos::Var(_) => true,
        };
        let object_fits = match &o {
            Pos::Const(c) => *c == d.object,
            Pos::Var(_) => true,
        };
        subject_fits && predicate_fits && object_fits
    };
    let mut seen = FxHashSet::default();
    let mut rows: Vec<Vec<Option<GroundTerm>>> = Vec::new();
    for d in delta.candidates(&t.predicate) {
        if !const_fits(d) {
            continue;
        }
        let values: [Term; 3] = [
            d.subject.clone().into(),
            d.predicate.clone().into(),
            d.object.clone(),
        ];
        let mut row: Vec<Option<Term>> = vec![None; vars.len()];
        let fits = [&s, &p, &o].iter().zip(&values).all(|(pos, v)| match pos {
            Pos::Const(c) => c == v,
            Pos::Var(i) => match &row[*i] {
                Some(bound) => bound == v,
                None => {
                    row[*i] = Some(v.clone());
                    true
                }
            },
        });
        if fits && seen.insert(row.clone()) {
            if rows.len() == MAX_DELTA_ROWS {
                delta.reads_graph.store(true, Ordering::Relaxed);
                return Some(Some(GraphPattern::Graph {
                    name: NamedNodePattern::NamedNode(NamedNode::new_unchecked(DELTA_GRAPH)),
                    inner: Box::new(GraphPattern::Bgp {
                        patterns: vec![TriplePattern {
                            subject: no_blank(&t.subject),
                            predicate: t.predicate.clone(),
                            object: no_blank(&t.object),
                        }],
                    }),
                }));
            }
            rows.push(row.iter().map(|t| t.as_ref().map(ground)).collect());
        }
    }
    if rows.is_empty() {
        return Some(None);
    }
    Some(Some(GraphPattern::Values {
        variables: vars,
        bindings: rows,
    }))
}

/// A blank node of a pattern as the variable it stands for.
fn no_blank(t: &TermPattern) -> TermPattern {
    match t {
        TermPattern::BlankNode(b) => TermPattern::Variable(Variable::new_unchecked(format!(
            "__delta_bnode_{}",
            b.as_str()
        ))),
        other => other.clone(),
    }
}

/// Whether a triple pattern of `pattern` may match the delta (`None`: one has a triple term).
fn touches(pattern: &GraphPattern, delta: &Delta<'_>) -> Option<bool> {
    Some(!variants(pattern, delta)?.is_empty())
}

fn path_touches(path: &PropertyPathExpression, delta: &Delta<'_>) -> bool {
    use PropertyPathExpression as E;
    match path {
        E::NamedNode(n) => delta.has_predicate(n),
        // Any predicate but some: any delta triple may match.
        E::NegatedPropertySet(_) => !delta.all.is_empty(),
        E::Reverse(p)
        | E::ZeroOrMore(p)
        | E::OneOrMore(p)
        | E::ZeroOrOne(p)
        | E::Bounded { path: p, .. } => path_touches(p, delta),
        E::Sequence(a, b) | E::Alternative(a, b) => {
            path_touches(a, delta) || path_touches(b, delta)
        }
    }
}

/// Whether an `EXISTS` in the expression may read the delta (then a new triple can make the
/// expression true where it was false, which the variants don't see).
fn expr_touches(e: &Expression, delta: &Delta<'_>) -> bool {
    use Expression as E;
    match e {
        E::Exists(p) => touches(p, delta) != Some(false),
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
        | E::Divide(a, b) => expr_touches(a, delta) || expr_touches(b, delta),
        E::In(a, list) => expr_touches(a, delta) || list.iter().any(|x| expr_touches(x, delta)),
        E::UnaryPlus(a) | E::UnaryMinus(a) | E::Not(a) => expr_touches(a, delta),
        E::If(a, b, c) => {
            expr_touches(a, delta) || expr_touches(b, delta) || expr_touches(c, delta)
        }
        E::Coalesce(list) | E::FunctionCall(_, list) => list.iter().any(|x| expr_touches(x, delta)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph::model::vocab::rdf;

    fn pattern(query: &str) -> GraphPattern {
        match spargebra::SparqlParser::new().parse_query(query).unwrap() {
            spargebra::Query::Construct { pattern, .. } => pattern,
            _ => unreachable!(),
        }
    }

    /// The limit counts the delta triples that fit the pattern, not all those of its
    /// predicate: `?x a ex:C` stays semi-naive when many other classes were inferred.
    #[test]
    fn the_limit_applies_to_matching_triples() {
        let node = |s: String| NamedNode::new_unchecked(s);
        let mut triples: Vec<Triple> = (0..MAX_DELTA_ROWS + 10)
            .map(|i| {
                Triple::new(
                    node(format!("http://ex/n{i}")),
                    rdf::TYPE,
                    node("http://ex/Other".to_owned()),
                )
            })
            .collect();
        triples.push(Triple::new(
            node("http://ex/a".to_owned()),
            rdf::TYPE,
            node("http://ex/C".to_owned()),
        ));
        let delta = Delta::new(&triples);
        let one = pattern(
            "CONSTRUCT { ?x <http://ex/p> 1 } WHERE { ?x a <http://ex/C> . ?x <http://ex/q> ?y }",
        );
        let v = variants(&one, &delta).expect("semi-naive");
        assert_eq!(v.len(), 1);
        assert!(!delta.reads_graph());
        // A pattern that does match them all reads them from the delta graph.
        let all =
            pattern("CONSTRUCT { ?x <http://ex/p> 1 } WHERE { ?x a ?c . ?x <http://ex/q> ?y }");
        let v = variants(&all, &delta).expect("semi-naive");
        assert_eq!(v.len(), 1);
        assert!(delta.reads_graph());
        let GraphPattern::Project { inner, .. } = &v[0] else {
            panic!("{:?}", v[0])
        };
        assert!(matches!(
            &**inner,
            GraphPattern::Join { right, .. } if matches!(&**right, GraphPattern::Graph { .. })
        ));
    }
}
