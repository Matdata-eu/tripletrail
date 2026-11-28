//! A Facade-X lift generated from a schema's shapes (#277): the shapes and provenance
//! `tripletrail schema xsd` writes (from `xsd2rdf --source-provenance`) say which XML element
//! or attribute each property shape stands for, so they give the `CONSTRUCT` that turns an XML
//! file of that schema into data the same shapes accept, with no hand-written mapping.
//!
//! The query starts at the schema's top-level element and walks the node shapes down through
//! the property shapes that have an XML name (`xsd2rdf:xmlName`): each element whose shape has
//! a node shape of its own becomes an entity, typed by its class, with an IRI made of the
//! namespace, the class's local name and an MD5 of its place in the document; attributes,
//! simple elements and text become values cast to the shape's `sh:datatype`; enumeration
//! values become the SKOS concepts whose `skos:notation` they are. Each value is its own
//! `UNION` branch, so that elements that repeat do not multiply each other.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::{Graph, NamedNodeRef, NamedOrBlankNodeRef, Term, TermRef};

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const SH: &str = "http://www.w3.org/ns/shacl#";
const XSD2RDF: &str = "https://w3id.org/xsd2rdf#";
const SKOS: &str = "http://www.w3.org/2004/02/skos/core#";
const DCT_SOURCE: &str = "http://purl.org/dc/terms/source";
const XYZ: &str = "http://sparql.xyz/facade-x/data/";
/// How deep the walk goes: a type that contains itself stops at its first repeat anyway.
const MAX_DEPTH: usize = 12;

/// The artefacts of `tripletrail schema xsd --out DIR`, as Turtle.
pub struct Schema<'a> {
    pub shapes: &'a str,
    pub provenance: &'a str,
    pub skos: &'a str,
}

/// The `CONSTRUCT` lifting an XML file of the schema, which reads the file named by the
/// template parameter `?_location` (`tripletrail lift -v location=FILE`). `root` picks the
/// top-level element when the schema has more than one.
pub fn xml_construct(
    schema: &Schema,
    namespace: &str,
    root: Option<&str>,
) -> Result<String, String> {
    let mut g = Graph::new();
    for (name, text) in [
        ("shapes", schema.shapes),
        ("provenance", schema.provenance),
        ("skos", schema.skos),
    ] {
        for t in RdfParser::from_format(RdfFormat::Turtle)
            .rename_blank_nodes()
            .for_slice(text.as_bytes())
        {
            g.insert(&t.map_err(|e| format!("{name}: {e}"))?.into());
        }
    }
    let m = Model { g };
    let roots = m.roots();
    let (shape, element) = match root {
        Some(r) => roots
            .into_iter()
            .find(|(_, (local, _))| local == r)
            .ok_or_else(|| format!("the schema has no top-level element {r}"))?,
        None => match roots.len() {
            0 => return Err("the shapes have no top-level element with its provenance: make them with `tripletrail schema xsd`".into()),
            1 => roots.into_iter().next().unwrap(),
            _ => {
                let names: Vec<_> = roots.iter().map(|(_, (l, _))| l.as_str()).collect();
                return Err(format!(
                    "the schema has {} top-level elements ({}): pick one with --root",
                    names.len(),
                    names.join(", ")
                ));
            }
        },
    };
    let mut q = Query {
        ns: namespace.to_owned(),
        branches: Vec::new(),
    };
    let start = Position {
        pattern: format!(
            "?n0 a <http://sparql.xyz/facade-x/ns/Root> , <{}> .",
            xml_iri(&element.0, element.1.as_deref())
        ),
        var: "?n0".into(),
        depth: 0,
    };
    q.walk(
        &m,
        &shape,
        &start,
        &m.classes(None, &shape),
        &mut vec![shape.clone()],
    );
    Ok(q.text())
}

struct Model {
    g: Graph,
}

fn subject(t: &Term) -> Option<NamedOrBlankNodeRef<'_>> {
    match t {
        Term::NamedNode(n) => Some(n.into()),
        Term::BlankNode(b) => Some(b.into()),
        _ => None,
    }
}

impl Model {
    fn objects(&self, s: &Term, p: &str) -> Vec<Term> {
        let Some(s) = subject(s) else {
            return Vec::new();
        };
        // In the same order in every run, so that the query is too.
        let mut out: Vec<Term> = self
            .g
            .objects_for_subject_predicate(s, NamedNodeRef::new_unchecked(p))
            .map(TermRef::into_owned)
            .collect();
        out.sort_by_cached_key(Term::to_string);
        out
    }

    fn object(&self, s: &Term, p: &str) -> Option<Term> {
        self.objects(s, p).into_iter().next()
    }

    fn text(&self, s: &Term, p: &str) -> Option<String> {
        match self.object(s, p)? {
            Term::Literal(l) => Some(l.value().to_owned()),
            _ => None,
        }
    }

    fn list(&self, head: &Term) -> Vec<Term> {
        let mut out = Vec::new();
        let mut at = head.clone();
        while let Some(first) = self.object(&at, &format!("{RDF}first")) {
            out.push(first);
            match self.object(&at, &format!("{RDF}rest")) {
                Some(rest) if out.len() < 10_000 => at = rest,
                _ => break,
            }
        }
        out
    }

    /// The node shapes made from a top-level element, with its XML name and namespace, read
    /// from their XSD component designator: `…xscd(/schemaElement::t:name)`.
    fn roots(&self) -> Vec<(Term, (String, Option<String>))> {
        let mut out = Vec::new();
        for t in self
            .g
            .triples_for_predicate(NamedNodeRef::new_unchecked(DCT_SOURCE))
        {
            let TermRef::Literal(scd) = t.object else {
                continue;
            };
            let s: Term = t.subject.into_owned().into();
            if !self
                .objects(&s, &format!("{RDF}type"))
                .iter()
                .any(|k| is(k, &format!("{SH}NodeShape")))
            {
                continue;
            }
            if let Some(name) = top_element(scd.value()) {
                out.push((s, name));
            }
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// The property shapes of a node shape: its own, those of the shapes it takes in with
    /// `sh:node` (a base type, a group), and those in its `sh:and`, `sh:or` and `sh:xone`
    /// branches.
    fn properties(&self, shape: &Term, seen: &mut Vec<Term>, out: &mut Vec<Term>) {
        if seen.contains(shape) || seen.len() > 10_000 {
            return;
        }
        seen.push(shape.clone());
        for p in self.objects(shape, &format!("{SH}property")) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
        let mut inner = self.objects(shape, &format!("{SH}node"));
        for k in ["and", "or", "xone"] {
            for head in self.objects(shape, &format!("{SH}{k}")) {
                inner.extend(self.list(&head));
            }
        }
        for n in inner {
            self.properties(&n, seen, out);
        }
    }

    /// The classes an entity of a node shape gets: the class the property shape linking to it
    /// names, and the node shape's target class.
    fn classes(&self, link: Option<&Term>, shape: &Term) -> Vec<String> {
        let mut out = Vec::new();
        let named = link.and_then(|l| self.object(l, &format!("{SH}class")));
        for c in named
            .into_iter()
            .chain(self.object(shape, &format!("{SH}targetClass")))
        {
            if let Term::NamedNode(n) = c
                && !out.iter().any(|o| o == n.as_str())
            {
                out.push(n.as_str().to_owned());
            }
        }
        out
    }

    /// For a property shape whose values are SKOS concepts, the concepts of its scheme by
    /// their notation. xsd2rdf names the scheme in the shape's SPARQL constraint.
    fn concepts(&self, shape: &Term) -> Option<BTreeMap<String, String>> {
        if !self
            .object(shape, &format!("{SH}class"))
            .is_some_and(|c| is(&c, &format!("{SKOS}Concept")))
        {
            return None;
        }
        let mut out = BTreeMap::new();
        for c in self.objects(shape, &format!("{SH}sparql")) {
            let Some(select) = self.text(&c, &format!("{SH}select")) else {
                continue;
            };
            let Some(scheme) = select
                .split("skos:inScheme <")
                .nth(1)
                .and_then(|r| r.split('>').next())
            else {
                continue;
            };
            let scheme: Term = NamedNodeRef::new_unchecked(scheme).into_owned().into();
            for t in self.g.triples_for_object(&scheme) {
                if t.predicate.as_str() != format!("{SKOS}inScheme") {
                    continue;
                }
                let concept: Term = t.subject.into_owned().into();
                if let (Some(n), Term::NamedNode(iri)) =
                    (self.text(&concept, &format!("{SKOS}notation")), &concept)
                {
                    out.insert(n, iri.as_str().to_owned());
                }
            }
        }
        Some(out)
    }
}

fn is(t: &Term, iri: &str) -> bool {
    matches!(t, Term::NamedNode(n) if n.as_str() == iri)
}

/// The XML name and namespace of a top-level element's designator, `None` for anything
/// deeper or not an element.
fn top_element(scd: &str) -> Option<(String, Option<String>)> {
    let (bindings, path) = scd.split_once("xscd(/")?;
    let step = path.strip_suffix(')')?.strip_prefix("schemaElement::")?;
    if step.contains('/') {
        return None;
    }
    let Some((prefix, local)) = step.split_once(':') else {
        return Some((step.to_owned(), None));
    };
    let ns = bindings
        .split("xmlns(")
        .filter_map(|b| b.strip_suffix(')')?.split_once('='))
        .find(|(p, _)| *p == prefix)
        .map(|(_, ns)| ns.to_owned());
    Some((local.to_owned(), ns))
}

/// The IRI Facade-X gives an element's type or an attribute's key.
fn xml_iri(local: &str, ns: Option<&str>) -> String {
    match ns {
        None | Some("") => format!("{XYZ}{local}"),
        Some(ns) if ns.ends_with('/') || ns.ends_with('#') => format!("{ns}{local}"),
        Some(ns) => format!("{ns}#{local}"),
    }
}

fn local_name(iri: &str) -> &str {
    iri.rsplit(['/', '#']).next().unwrap_or(iri)
}

/// Where an element is: the pattern from the root to it and its container's variable.
struct Position {
    pattern: String,
    var: String,
    depth: usize,
}

struct Branch {
    pattern: String,
    template: String,
}

struct Query {
    ns: String,
    branches: Vec<Branch>,
}

impl Query {
    fn fresh(&self, name: &str) -> String {
        format!("?b{}_{name}", self.branches.len())
    }

    /// The IRI of the entity a container stands for.
    fn entity(&self, var: &str, class: Option<&str>, into: &str) -> String {
        let class = class.map_or("entity", local_name);
        format!(
            "BIND(IRI(CONCAT({}, MD5(STRAFTER(STR({var}), STR(?n0))))) AS {into})",
            literal(&format!("{}{class}/", self.ns))
        )
    }

    fn walk(
        &mut self,
        m: &Model,
        shape: &Term,
        at: &Position,
        classes: &[String],
        path: &mut Vec<Term>,
    ) {
        let e = self.fresh("e");
        let me = self.entity(&at.var, classes.first().map(String::as_str), &e);
        if !classes.is_empty() {
            let types: Vec<_> = classes.iter().map(|c| format!("<{c}>")).collect();
            self.branches.push(Branch {
                pattern: format!("{}\n{me}", at.pattern),
                template: format!("{e} a {} .", types.join(" , ")),
            });
        }
        let mut props = Vec::new();
        m.properties(shape, &mut Vec::new(), &mut props);
        for p in props {
            let Some(Term::NamedNode(pred)) = m.object(&p, &format!("{SH}path")) else {
                continue;
            };
            let kind = m.text(&p, &format!("{XSD2RDF}xmlNodeKind"));
            let name = m.text(&p, &format!("{XSD2RDF}xmlName"));
            let ns = m.text(&p, &format!("{XSD2RDF}xmlNamespace"));
            let e = self.fresh("e");
            let me = self.entity(&at.var, classes.first().map(String::as_str), &e);
            let raw = self.fresh("raw");
            let child = self.fresh("c");
            let slot = self.fresh("s");
            let pick = match (kind.as_deref(), &name) {
                (Some("attribute"), Some(n)) => {
                    format!("{} <{}> {raw} .", at.var, xml_iri(n, ns.as_deref()))
                }
                (Some("element"), Some(n)) => {
                    let el = format!(
                        "{} {slot} {child} . {child} a <{}> .",
                        at.var,
                        xml_iri(n, ns.as_deref())
                    );
                    if let Some(inner) = m.object(&p, &format!("{SH}node")) {
                        if path.contains(&inner) || at.depth >= MAX_DEPTH {
                            continue;
                        }
                        let inner_classes = m.classes(Some(&p), &inner);
                        let ce = self.fresh("ce");
                        let link =
                            self.entity(&child, inner_classes.first().map(String::as_str), &ce);
                        self.branches.push(Branch {
                            pattern: format!("{}\n{me}\n{el}\n{link}", at.pattern),
                            template: format!("{e} {pred} {ce} ."),
                        });
                        let var = format!("?n{}", at.depth + 1);
                        let next = Position {
                            pattern: format!(
                                "{}\n{} ?s{} {var} . {var} a <{}> .",
                                at.pattern,
                                at.var,
                                at.depth + 1,
                                xml_iri(n, ns.as_deref())
                            ),
                            var,
                            depth: at.depth + 1,
                        };
                        path.push(inner.clone());
                        self.walk(m, &inner, &next, &inner_classes, path);
                        path.pop();
                        continue;
                    }
                    format!("{el} {child} <{RDF}_1> {raw} . FILTER(isLiteral({raw}))")
                }
                (Some("text"), _) => format!(
                    "{} {slot} {raw} . FILTER(isLiteral({raw}) && STRSTARTS(STR({slot}), \"{RDF}_\"))",
                    at.var
                ),
                _ => continue,
            };
            let v = self.fresh("v");
            let value = match m.concepts(&p) {
                Some(concepts) => {
                    let rows: String = concepts
                        .iter()
                        .map(|(n, c)| format!(" ({} <{c}>)", literal(n)))
                        .collect();
                    let key = self.fresh("k");
                    format!("BIND(STR({raw}) AS {key})\nVALUES ({key} {v}) {{{rows} }}")
                }
                None => match m.object(&p, &format!("{SH}datatype")) {
                    Some(Term::NamedNode(dt))
                        if dt.as_str() != "http://www.w3.org/2001/XMLSchema#string" =>
                    {
                        format!("BIND(STRDT(STR({raw}), {dt}) AS {v})")
                    }
                    _ => format!("BIND(STR({raw}) AS {v})"),
                },
            };
            self.branches.push(Branch {
                pattern: format!("{}\n{me}\n{pick}\n{value}", at.pattern),
                template: format!("{e} {pred} {v} ."),
            });
        }
    }

    fn text(&self) -> String {
        let mut q = String::from(
            "# Generated by `tripletrail schema lift` from the schema's shapes and provenance.\n\
             # Run it with `tripletrail lift THIS.rq -v location=FILE.xml`.\n\
             PREFIX fx: <http://sparql.xyz/facade-x/ns/>\nCONSTRUCT {\n",
        );
        for b in &self.branches {
            let _ = writeln!(q, "  {}", b.template);
        }
        q.push_str("}\nWHERE {\n  SERVICE <x-sparql-anything:> {\n    fx:properties fx:location ?_location ; fx:blank-nodes false .\n");
        for (i, b) in self.branches.iter().enumerate() {
            q.push_str(if i == 0 { "    {\n" } else { "    UNION {\n" });
            for line in b.pattern.lines() {
                let _ = writeln!(q, "      {line}");
            }
            q.push_str("    }\n");
        }
        q.push_str("  }\n}\n");
        q
    }
}

/// A SPARQL string literal.
fn literal(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_top_level_element_is_read_from_its_designator() {
        assert_eq!(
            top_element("st.xsd#xmlns(t=http://example.org/st)xscd(/schemaElement::t:stations)"),
            Some(("stations".into(), Some("http://example.org/st".into())))
        );
        assert_eq!(
            top_element("st.xsd#xscd(/schemaElement::stations)"),
            Some(("stations".into(), None))
        );
        assert_eq!(
            top_element(
                "st.xsd#xmlns(t=http://example.org/st)xscd(/type::t:T/model::sequence/schemaElement::t:a)"
            ),
            None
        );
        assert_eq!(
            xml_iri("a", Some("http://example.org/st")),
            "http://example.org/st#a"
        );
        assert_eq!(
            xml_iri("a", Some("http://example.org/st/")),
            "http://example.org/st/a"
        );
        assert_eq!(xml_iri("a", None), "http://sparql.xyz/facade-x/data/a");
    }
}
