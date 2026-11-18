//! Turtle for shape files that diff well: subjects in order, `a` first, then the predicates
//! and objects in order, lists as `( … )` and blank nodes used once inlined as `[ … ]`.

use super::{SH, TTS};
use oxigraph::model::vocab::{rdf, xsd};
use oxigraph::model::{Graph, NamedOrBlankNodeRef, TermRef};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

const PREFIXES: [(&str, &str); 8] = [
    ("sh", SH),
    ("tts", TTS),
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("skos", "http://www.w3.org/2004/02/skos/core#"),
    ("geo", "http://www.opengis.net/ont/geosparql#"),
    ("dcterms", "http://purl.org/dc/terms/"),
];

struct Writer<'a> {
    graph: &'a Graph,
    /// Blank nodes used once as an object, written where they are used.
    inline: HashSet<NamedOrBlankNodeRef<'a>>,
    /// Labels of the other blank nodes.
    labels: HashMap<NamedOrBlankNodeRef<'a>, usize>,
}

fn iri(iri: &str) -> String {
    for (prefix, namespace) in PREFIXES {
        if let Some(local) = iri.strip_prefix(namespace)
            && !local.is_empty()
            && local
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && local.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        {
            return format!("{prefix}:{local}");
        }
    }
    format!("<{iri}>")
}

impl<'a> Writer<'a> {
    fn term(&self, term: TermRef<'a>, depth: usize) -> String {
        match term {
            TermRef::NamedNode(n) => iri(n.as_str()),
            TermRef::BlankNode(b) => {
                let node = NamedOrBlankNodeRef::from(b);
                if self.inline.contains(&node) {
                    if let Some(items) = self.list(node) {
                        let items: Vec<String> =
                            items.into_iter().map(|i| self.term(i, depth + 1)).collect();
                        format!("( {} )", items.join(" "))
                    } else {
                        let pad = "    ".repeat(depth + 1);
                        format!(
                            "[\n{pad}{}\n{}]",
                            self.predicates(node, depth + 1),
                            "    ".repeat(depth)
                        )
                    }
                } else {
                    format!("_:b{}", self.labels.get(&node).copied().unwrap_or(0))
                }
            }
            TermRef::Literal(l) => {
                let datatype = l.datatype();
                if (datatype == xsd::INTEGER
                    && l.value().parse::<i64>().is_ok()
                    && !l.value().starts_with('+'))
                    || (datatype == xsd::BOOLEAN && matches!(l.value(), "true" | "false"))
                {
                    l.value().to_owned()
                } else if datatype == xsd::STRING || l.language().is_some() {
                    l.to_string()
                } else {
                    let quoted = oxigraph::model::Literal::new_simple_literal(l.value());
                    format!("{quoted}^^{}", iri(datatype.as_str()))
                }
            }
            #[allow(unreachable_patterns)]
            other => other.to_string(),
        }
    }

    /// The items of a well-formed list whose cells are all inlined.
    fn list(&self, head: NamedOrBlankNodeRef<'a>) -> Option<Vec<TermRef<'a>>> {
        let mut items = Vec::new();
        let mut cell = head;
        loop {
            let triples: Vec<_> = self.graph.triples_for_subject(cell).collect();
            let first = triples.iter().find(|t| t.predicate == rdf::FIRST)?;
            let rest = triples.iter().find(|t| t.predicate == rdf::REST)?;
            if triples.len() != 2 {
                return None;
            }
            items.push(first.object);
            match rest.object {
                TermRef::NamedNode(n) if n == rdf::NIL => return Some(items),
                TermRef::BlankNode(b) if self.inline.contains(&b.into()) => cell = b.into(),
                _ => return None,
            }
        }
    }

    /// `p o, o ; p o` of a subject, `a` first, in order.
    fn predicates(&self, subject: NamedOrBlankNodeRef<'a>, depth: usize) -> String {
        let mut by_predicate: Vec<(String, Vec<String>)> = Vec::new();
        for t in self.graph.triples_for_subject(subject) {
            let p = if t.predicate == rdf::TYPE {
                "a".to_owned()
            } else {
                iri(t.predicate.as_str())
            };
            let o = self.term(t.object, depth);
            match by_predicate.iter_mut().find(|(q, _)| *q == p) {
                Some((_, objects)) => objects.push(o),
                None => by_predicate.push((p, vec![o])),
            }
        }
        by_predicate.sort_by(|(a, _), (b, _)| (a != "a", a).cmp(&(b != "a", b)));
        let pad = "    ".repeat(depth);
        by_predicate
            .into_iter()
            .map(|(p, mut objects)| {
                objects.sort();
                let one_line = objects.join(", ");
                if objects.len() > 1 && one_line.len() + pad.len() + p.len() > 96 {
                    format!("{p}\n{pad}    {}", objects.join(&format!(",\n{pad}    ")))
                } else {
                    format!("{p} {one_line}")
                }
            })
            .collect::<Vec<_>>()
            .join(&format!(" ;\n{pad}"))
    }
}

/// `graph` as Turtle that diffs well.
pub fn to_turtle(graph: &Graph) -> String {
    let mut uses: HashMap<NamedOrBlankNodeRef<'_>, usize> = HashMap::new();
    for t in graph.iter() {
        if let TermRef::BlankNode(b) = t.object {
            *uses.entry(b.into()).or_default() += 1;
        }
    }
    let mut subjects: Vec<NamedOrBlankNodeRef<'_>> = Vec::new();
    let mut seen = HashSet::new();
    for t in graph.iter() {
        if seen.insert(t.subject) {
            subjects.push(t.subject);
        }
    }
    let inline: HashSet<_> = subjects
        .iter()
        .copied()
        .filter(|s| matches!(s, NamedOrBlankNodeRef::BlankNode(_)) && uses.get(s) == Some(&1))
        .collect();
    let mut writer = Writer {
        graph,
        inline,
        labels: HashMap::new(),
    };
    // Blank nodes used once but in a cycle would never be written: label those too.
    let mut reached = HashSet::new();
    let mut todo: Vec<NamedOrBlankNodeRef<'_>> = subjects
        .iter()
        .copied()
        .filter(|s| !writer.inline.contains(s))
        .collect();
    while let Some(s) = todo.pop() {
        for t in graph.triples_for_subject(s) {
            if let TermRef::BlankNode(b) = t.object
                && writer.inline.contains(&b.into())
                && reached.insert(NamedOrBlankNodeRef::from(b))
            {
                todo.push(b.into());
            }
        }
    }
    writer.inline.retain(|s| reached.contains(s));
    let mut top: Vec<(String, NamedOrBlankNodeRef<'_>)> = Vec::new();
    for s in &subjects {
        if !writer.inline.contains(s) {
            let key = match s {
                NamedOrBlankNodeRef::NamedNode(n) => format!("0{}", n.as_str()),
                NamedOrBlankNodeRef::BlankNode(_) => {
                    format!("1{}", writer.predicates(*s, 1))
                }
            };
            top.push((key, *s));
        }
    }
    top.sort_by(|(a, _), (b, _)| a.cmp(b));
    let mut n = 0;
    for (_, s) in &top {
        if matches!(s, NamedOrBlankNodeRef::BlankNode(_)) {
            n += 1;
            writer.labels.insert(*s, n);
        }
    }
    // Blank-node objects neither inlined nor subjects.
    for t in graph.iter() {
        if let TermRef::BlankNode(b) = t.object
            && !writer.inline.contains(&b.into())
            && !writer.labels.contains_key(&b.into())
        {
            n += 1;
            writer.labels.insert(b.into(), n);
        }
    }
    let mut out = String::new();
    for (prefix, namespace) in PREFIXES {
        let _ = writeln!(out, "@prefix {prefix}: <{namespace}> .");
    }
    for (_, s) in top {
        let subject = match s {
            NamedOrBlankNodeRef::NamedNode(n) => iri(n.as_str()),
            NamedOrBlankNodeRef::BlankNode(_) => format!("_:b{}", writer.labels[&s]),
        };
        let _ = write!(out, "\n{subject}\n    {} .\n", writer.predicates(s, 1));
    }
    out
}
