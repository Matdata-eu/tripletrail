//! SHACL property paths: parsing from a shapes graph, evaluation against a data graph, and
//! writing back as triples.

use crate::ShaclError;
use crate::graph::DataGraph;
use crate::vocab::{rdf, sh};
use oxigraph::model::{BlankNode, Graph, NamedNode, Term, Triple};
use rustc_hash::FxHashSet;
use std::collections::BTreeSet;

/// A property path (SHACL 2.3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Path {
    Predicate(NamedNode),
    Inverse(Box<Path>),
    Sequence(Vec<Path>),
    Alternative(Vec<Path>),
    ZeroOrMore(Box<Path>),
    OneOrMore(Box<Path>),
    ZeroOrOne(Box<Path>),
}

/// The members of an RDF list.
pub(crate) fn list(g: &dyn DataGraph, head: &Term) -> Result<Vec<Term>, ShaclError> {
    let mut out = Vec::new();
    let mut node = head.clone();
    let mut seen = FxHashSet::default();
    while node.as_ref() != rdf::NIL.into() {
        if !seen.insert(node.clone()) {
            return Err(ShaclError::Shape(format!("cyclic RDF list at {head}")));
        }
        let first = g
            .object(&node, rdf::FIRST)
            .ok_or_else(|| ShaclError::Shape(format!("{head} is not a well-formed RDF list")))?;
        out.push(first);
        node = g
            .object(&node, rdf::REST)
            .ok_or_else(|| ShaclError::Shape(format!("{head} is not a well-formed RDF list")))?;
    }
    Ok(out)
}

impl Path {
    /// Reads the path whose node is `node` in the shapes graph.
    pub fn parse(g: &dyn DataGraph, node: &Term) -> Result<Self, ShaclError> {
        Self::parse_depth(g, node, 0)
    }

    fn parse_depth(g: &dyn DataGraph, node: &Term, depth: usize) -> Result<Self, ShaclError> {
        if depth > 64 {
            return Err(ShaclError::Shape(format!("path {node} is nested too deep")));
        }
        let sub = |n: &Term| Self::parse_depth(g, n, depth + 1).map(Box::new);
        match node {
            Term::NamedNode(n) => return Ok(Self::Predicate(n.clone())),
            Term::BlankNode(_) => {}
            _ => return Err(ShaclError::Shape(format!("{node} is not a path"))),
        }
        if g.has_object(node, rdf::FIRST) {
            let mut members = list(g, node)?
                .iter()
                .map(|m| Self::parse_depth(g, m, depth + 1))
                .collect::<Result<Vec<_>, _>>()?;
            return match members.len() {
                0 => Err(ShaclError::Shape("an empty sequence path".into())),
                1 => Ok(members.pop().unwrap()),
                _ => Ok(Self::Sequence(members)),
            };
        }
        if let Some(n) = g.object(node, sh::INVERSE_PATH) {
            return Ok(Self::Inverse(sub(&n)?));
        }
        if let Some(n) = g.object(node, sh::ALTERNATIVE_PATH) {
            let members = list(g, &n)?
                .iter()
                .map(|m| Self::parse_depth(g, m, depth + 1))
                .collect::<Result<Vec<_>, _>>()?;
            if members.is_empty() {
                return Err(ShaclError::Shape("an empty alternative path".into()));
            }
            return Ok(Self::Alternative(members));
        }
        if let Some(n) = g.object(node, sh::ZERO_OR_MORE_PATH) {
            return Ok(Self::ZeroOrMore(sub(&n)?));
        }
        if let Some(n) = g.object(node, sh::ONE_OR_MORE_PATH) {
            return Ok(Self::OneOrMore(sub(&n)?));
        }
        if let Some(n) = g.object(node, sh::ZERO_OR_ONE_PATH) {
            return Ok(Self::ZeroOrOne(sub(&n)?));
        }
        Err(ShaclError::Shape(format!("{node} is not a path")))
    }

    /// The predicate, when the path is one.
    pub fn as_predicate(&self) -> Option<&NamedNode> {
        match self {
            Self::Predicate(p) => Some(p),
            _ => None,
        }
    }

    /// The path in SPARQL property path syntax (what `$PATH` stands for in SHACL-SPARQL).
    pub fn to_sparql(&self) -> String {
        let join = |list: &[Path], sep: &str| {
            let parts: Vec<String> = list.iter().map(Path::to_sparql).collect();
            format!("({})", parts.join(sep))
        };
        match self {
            Self::Predicate(p) => p.to_string(),
            Self::Inverse(p) => format!("^({})", p.to_sparql()),
            Self::Sequence(list) => join(list, "/"),
            Self::Alternative(list) => join(list, "|"),
            Self::ZeroOrMore(p) => format!("({})*", p.to_sparql()),
            Self::OneOrMore(p) => format!("({})+", p.to_sparql()),
            Self::ZeroOrOne(p) => format!("({})?", p.to_sparql()),
        }
    }

    /// The predicates the path reads.
    pub fn predicates(&self, out: &mut BTreeSet<String>) {
        match self {
            Self::Predicate(p) => {
                out.insert(p.as_str().to_owned());
            }
            Self::Inverse(p) | Self::ZeroOrMore(p) | Self::OneOrMore(p) | Self::ZeroOrOne(p) => {
                p.predicates(out)
            }
            Self::Sequence(list) | Self::Alternative(list) => {
                for p in list {
                    p.predicates(out);
                }
            }
        }
    }

    /// The value nodes of `focus` through the path, without repeats.
    pub fn values(&self, g: &dyn DataGraph, focus: &Term) -> Vec<Term> {
        let mut out = Vec::new();
        if let Self::Predicate(p) = self {
            g.objects(focus, p.as_ref(), &mut out);
            dedup(&mut out);
            return out;
        }
        self.eval(g, std::slice::from_ref(focus), true, &mut out);
        out
    }

    /// Appends the nodes reached from `from` (forwards, or against the path when `!forward`)
    /// to `out`, without repeats.
    fn eval(&self, g: &dyn DataGraph, from: &[Term], forward: bool, out: &mut Vec<Term>) {
        let start = out.len();
        match self {
            Self::Predicate(p) => {
                for n in from {
                    if forward {
                        g.objects(n, p.as_ref(), out);
                    } else {
                        g.subjects(p.as_ref(), n, out);
                    }
                }
            }
            Self::Inverse(p) => p.eval(g, from, !forward, out),
            Self::Sequence(list) => {
                let mut current = from.to_vec();
                let mut steps: Box<dyn Iterator<Item = &Path>> = if forward {
                    Box::new(list.iter())
                } else {
                    Box::new(list.iter().rev())
                };
                let mut next = Vec::new();
                for step in &mut steps {
                    next.clear();
                    step.eval(g, &current, forward, &mut next);
                    std::mem::swap(&mut current, &mut next);
                    if current.is_empty() {
                        break;
                    }
                }
                out.extend(current);
            }
            Self::Alternative(list) => {
                for p in list {
                    p.eval(g, from, forward, out);
                }
            }
            Self::ZeroOrOne(p) => {
                out.extend(from.iter().cloned());
                p.eval(g, from, forward, out);
            }
            Self::ZeroOrMore(p) | Self::OneOrMore(p) => {
                let mut seen: FxHashSet<Term> = FxHashSet::default();
                let mut frontier = Vec::new();
                if matches!(self, Self::ZeroOrMore(_)) {
                    for n in from {
                        if seen.insert(n.clone()) {
                            frontier.push(n.clone());
                        }
                    }
                    out.extend(frontier.iter().cloned());
                } else {
                    let mut first = Vec::new();
                    p.eval(g, from, forward, &mut first);
                    for n in first {
                        if seen.insert(n.clone()) {
                            out.push(n.clone());
                            frontier.push(n);
                        }
                    }
                }
                let mut next = Vec::new();
                while !frontier.is_empty() {
                    next.clear();
                    p.eval(g, &frontier, forward, &mut next);
                    frontier.clear();
                    for n in next.drain(..) {
                        if seen.insert(n.clone()) {
                            out.push(n.clone());
                            frontier.push(n);
                        }
                    }
                }
            }
        }
        let mut tail = out.split_off(start);
        dedup(&mut tail);
        out.extend(tail);
    }

    /// Writes the path as SHACL triples into `g` and returns its node.
    pub fn write(&self, g: &mut Graph) -> Term {
        let node = |g: &mut Graph, p: NamedNode, o: Term| {
            let b = BlankNode::default();
            g.insert(&Triple::new(b.clone(), p, o));
            Term::from(b)
        };
        match self {
            Self::Predicate(p) => p.clone().into(),
            Self::Inverse(p) => {
                let o = p.write(g);
                node(g, sh::INVERSE_PATH.into_owned(), o)
            }
            Self::Sequence(list) => {
                let members: Vec<Term> = list.iter().map(|p| p.write(g)).collect();
                write_list(g, members)
            }
            Self::Alternative(list) => {
                let members: Vec<Term> = list.iter().map(|p| p.write(g)).collect();
                let l = write_list(g, members);
                node(g, sh::ALTERNATIVE_PATH.into_owned(), l)
            }
            Self::ZeroOrMore(p) => {
                let o = p.write(g);
                node(g, sh::ZERO_OR_MORE_PATH.into_owned(), o)
            }
            Self::OneOrMore(p) => {
                let o = p.write(g);
                node(g, sh::ONE_OR_MORE_PATH.into_owned(), o)
            }
            Self::ZeroOrOne(p) => {
                let o = p.write(g);
                node(g, sh::ZERO_OR_ONE_PATH.into_owned(), o)
            }
        }
    }
}

/// Writes an RDF list and returns its head.
pub(crate) fn write_list(g: &mut Graph, members: Vec<Term>) -> Term {
    let mut head: Term = rdf::NIL.into_owned().into();
    for m in members.into_iter().rev() {
        let b = BlankNode::default();
        g.insert(&Triple::new(b.clone(), rdf::FIRST.into_owned(), m));
        g.insert(&Triple::new(b.clone(), rdf::REST.into_owned(), head));
        head = b.into();
    }
    head
}

/// Removes repeats, keeping the first of each.
pub(crate) fn dedup(v: &mut Vec<Term>) {
    if v.len() < 2 {
        return;
    }
    if v.len() <= 8 {
        let mut i = 1;
        while i < v.len() {
            if v[..i].contains(&v[i]) {
                v.remove(i);
            } else {
                i += 1;
            }
        }
        return;
    }
    let mut seen = FxHashSet::default();
    v.retain(|t| seen.insert(t.clone()));
}
