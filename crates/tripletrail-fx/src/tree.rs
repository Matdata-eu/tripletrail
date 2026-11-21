//! Building the Facade-X graph of a source: containers (blank nodes, or IRIs under the root
//! when `blank-nodes=false`), slots `rdf:_n`, keys in the data namespace, and literal values.

use crate::options::Needs;
use crate::{FX, RDF};
use oxigraph::model::vocab::{rdf, xsd};
use oxigraph::model::{
    BlankNode, GraphName, Literal, NamedNode, NamedNodeRef, NamedOrBlankNode, Quad, Term,
};

/// Options common to every format.
#[derive(Debug, Clone)]
pub(crate) struct Common {
    /// The namespace of keys (`namespace`, default `http://sparql.xyz/facade-x/data/`).
    pub namespace: String,
    /// `blank-nodes` (default true).
    pub blank_nodes: bool,
    /// The root IRI: the `root` option, else the location, else one derived from the content.
    pub root: String,
    /// `null-string`: values equal to it give no triple.
    pub null_string: Option<String>,
    /// `trim-strings`.
    pub trim: bool,
    /// The location's IRI, when the source has one (the document IRI of `html.metadata`).
    pub location: Option<String>,
}

/// Collects the triples of one source as quads: in the default graph, and in the named
/// graph of the root when the pattern uses `GRAPH`.
pub(crate) struct Sink<'a> {
    pub common: &'a Common,
    needs: &'a Needs,
    graph: NamedNode,
    pub quads: Vec<Quad>,
}

impl<'a> Sink<'a> {
    pub(crate) fn new(common: &'a Common, needs: &'a Needs) -> Self {
        Self {
            common,
            needs,
            graph: NamedNode::new_unchecked(&common.root),
            quads: Vec::new(),
        }
    }

    /// A container at `path` under the root (`""` for the root itself). Paths are only used
    /// when containers are IRIs; `path` is not called otherwise.
    pub(crate) fn container(&self, path: impl FnOnce() -> String) -> NamedOrBlankNode {
        if self.common.blank_nodes {
            BlankNode::default().into()
        } else {
            let path = path();
            if path.is_empty() {
                NamedNode::new_unchecked(&self.common.root).into()
            } else {
                NamedNode::new_unchecked(format!("{}/{}", self.common.root, path)).into()
            }
        }
    }

    /// The root container, typed `fx:Root`.
    pub(crate) fn root(&mut self) -> NamedOrBlankNode {
        let root = self.container(String::new);
        self.emit_iri(
            &root,
            rdf::TYPE,
            Term::NamedNode(NamedNode::new_unchecked(format!("{FX}Root"))),
        );
        root
    }

    pub(crate) fn key_iri(&self, key: &str) -> String {
        let mut iri = self.common.namespace.clone();
        escape_iri(key, &mut iri);
        iri
    }

    /// `subject <namespace + key> object`.
    pub(crate) fn key(&mut self, subject: &NamedOrBlankNode, key: &str, object: Term) {
        let p = self.key_iri(key);
        if self.needs.keep(&p) {
            self.push(subject, NamedNode::new_unchecked(p), object);
        }
    }

    /// `subject rdf:_n object`.
    pub(crate) fn slot(&mut self, subject: &NamedOrBlankNode, n: usize, object: Term) {
        let p = format!("{RDF}_{n}");
        if self.needs.keep(&p) {
            self.push(subject, NamedNode::new_unchecked(p), object);
        }
    }

    /// The predicate `iri` when the pattern can match it, to emit with [`Sink::emit`].
    pub(crate) fn predicate(&self, iri: String) -> Option<NamedNode> {
        self.needs.keep(&iri).then(|| NamedNode::new_unchecked(iri))
    }

    /// `subject predicate object`, for a predicate from [`Sink::predicate`].
    pub(crate) fn emit(&mut self, subject: &NamedOrBlankNode, predicate: &NamedNode, object: Term) {
        self.push(subject, predicate.clone(), object);
    }

    fn emit_iri(&mut self, subject: &NamedOrBlankNode, p: NamedNodeRef<'_>, object: Term) {
        if self.needs.keep(p.as_str()) {
            self.push(subject, p.into_owned(), object);
        }
    }

    fn push(&mut self, subject: &NamedOrBlankNode, predicate: NamedNode, object: Term) {
        if self.needs.named_graph {
            self.quads.push(Quad::new(
                subject.clone(),
                predicate.clone(),
                object.clone(),
                GraphName::NamedNode(self.graph.clone()),
            ));
        }
        self.quads.push(Quad::new(
            subject.clone(),
            predicate,
            object,
            GraphName::DefaultGraph,
        ));
    }

    /// `subject rdf:type <class>`.
    pub(crate) fn class(&mut self, subject: &NamedOrBlankNode, class: String) {
        if self.needs.keep(rdf::TYPE.as_str()) {
            self.push(
                subject,
                rdf::TYPE.into_owned(),
                NamedNode::new_unchecked(class).into(),
            );
        }
    }

    /// A string tagged `lang`, or none for the null string. Trims first with `trim-strings`.
    pub(crate) fn lang_string(&self, value: &str, lang: &str) -> Option<Term> {
        let value = if self.common.trim {
            value.trim()
        } else {
            value
        };
        if self.common.null_string.as_deref() == Some(value) {
            return None;
        }
        Some(
            Literal::new_language_tagged_literal(value, lang)
                .unwrap_or_else(|_| Literal::new_simple_literal(value))
                .into(),
        )
    }

    /// A string value, or none for the null string. Trims first with `trim-strings`.
    pub(crate) fn string(&self, value: &str) -> Option<Term> {
        self.typed(value, xsd::STRING)
    }

    /// A value of type `datatype`, or none for the null string.
    pub(crate) fn typed(&self, value: &str, datatype: NamedNodeRef<'_>) -> Option<Term> {
        let value = if self.common.trim {
            value.trim()
        } else {
            value
        };
        if self.common.null_string.as_deref() == Some(value) {
            return None;
        }
        Some(Literal::new_typed_literal(value, datatype).into())
    }
}

/// Appends `s` to `out`, percent-encoding the characters an IRI may not contain (space,
/// `<>"{}|\^` and the backtick, and control characters). Other characters are kept, so
/// valid IRI text is unchanged.
pub(crate) fn escape_iri(s: &str, out: &mut String) {
    for c in s.chars() {
        if c <= ' '
            || matches!(
                c,
                '<' | '>' | '"' | '{' | '}' | '|' | '\\' | '^' | '`' | '\u{7f}'
            )
        {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        } else {
            out.push(c);
        }
    }
}

/// `f` as an `xsd:double` lexical form: as Java writes it ([`java_double`]), but `INF`,
/// `-INF` and `NaN` for the values Java writes `Infinity`, `-Infinity` and `NaN`.
pub(crate) fn xsd_double(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "INF" } else { "-INF" }.into()
    } else {
        java_double(f)
    }
}

/// `f` as Java's `Double.toString` writes it: the shortest digits that read back as `f`,
/// in plain notation with at least one fraction digit from 0.001 up to 10^7
/// (`1.0`, `0.25`), else as `d.dddE<exp>` (`1.0E7`, `1.5E-5`).
pub(crate) fn java_double(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let a = f.abs();
    if (1e-3..1e7).contains(&a) {
        let s = format!("{f}");
        return if s.contains('.') { s } else { s + ".0" };
    }
    let s = format!("{f:e}");
    let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
    if mantissa.contains('.') {
        format!("{mantissa}E{exp}")
    } else {
        format!("{mantissa}.0E{exp}")
    }
}

#[cfg(test)]
mod tests {
    use crate::tree::java_double;

    #[test]
    fn doubles_as_java_writes_them() {
        assert_eq!(java_double(1.0), "1.0");
        assert_eq!(java_double(-2.5), "-2.5");
        assert_eq!(java_double(0.001), "0.001");
        assert_eq!(java_double(0.0001), "1.0E-4");
        assert_eq!(java_double(1234567.0), "1234567.0");
        assert_eq!(java_double(1e7), "1.0E7");
        assert_eq!(java_double(1.5e10), "1.5E10");
        assert_eq!(java_double(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(java_double(0.0), "0.0");
        assert_eq!(super::xsd_double(f64::INFINITY), "INF");
        assert_eq!(super::xsd_double(f64::NEG_INFINITY), "-INF");
        assert_eq!(super::xsd_double(f64::NAN), "NaN");
        assert_eq!(super::xsd_double(1.0), "1.0");
    }
}
