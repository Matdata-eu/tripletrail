//! Validation results and the W3C validation report.

use crate::path::Path;
use crate::vocab::{rdf, sh};
use oxigraph::model::{BlankNode, Graph, Literal, NamedNode, Term, Triple};
use std::borrow::Cow;

/// One constraint failure: a `sh:ValidationResult` of the report.
#[derive(Debug, Clone)]
pub struct ValidationResult<'a> {
    /// `sh:focusNode`.
    pub focus_node: Term,
    /// `sh:resultPath`: the path of the property shape the constraint belongs to, or (for
    /// `sh:closed`) the property that is not allowed.
    pub result_path: Option<Cow<'a, Path>>,
    /// `sh:value`: the value node that fails, for constraints checked value by value.
    pub value: Option<Term>,
    /// `sh:sourceShape`: the shape the failing constraint belongs to.
    pub source_shape: &'a Term,
    /// `sh:sourceConstraintComponent`.
    pub source_constraint_component: Term,
    /// `sh:sourceConstraint`: the `sh:sparql` node of a SPARQL-based constraint.
    pub source_constraint: Option<Term>,
    /// `sh:resultSeverity`: the source shape's `sh:severity`, `sh:Violation` by default.
    pub severity: &'a Term,
    /// `sh:resultMessage`s: the source shape's `sh:message`s, or else those of the
    /// SPARQL-based constraint.
    pub messages: &'a [Term],
    /// `sh:detail`s: for a failure of a shape reached through `sh:node`, `sh:and`, `sh:or`,
    /// `sh:xone` or `sh:qualifiedValueShape`, the results of the value node against that shape
    /// (see [`crate::Validator::report`]).
    pub details: Vec<ValidationResult<'a>>,
}

impl ValidationResult<'_> {
    /// Writes the result as triples, with `node` as its subject.
    pub fn write(&self, node: &BlankNode, sink: &mut dyn FnMut(Triple)) {
        let mut t = |p: oxigraph::model::NamedNodeRef<'_>, o: Term| {
            sink(Triple::new(node.clone(), p, o));
        };
        t(rdf::TYPE, sh::VALIDATION_RESULT.into());
        t(sh::FOCUS_NODE, self.focus_node.clone());
        if let Some(path) = &self.result_path {
            let mut g = Graph::new();
            let p = path.write(&mut g);
            t(sh::RESULT_PATH, p);
            for triple in g.iter() {
                sink(triple.into_owned());
            }
        }
        let mut t = |p: oxigraph::model::NamedNodeRef<'_>, o: Term| {
            sink(Triple::new(node.clone(), p, o));
        };
        if let Some(v) = &self.value {
            t(sh::VALUE, v.clone());
        }
        t(sh::SOURCE_SHAPE, self.source_shape.clone());
        t(
            sh::SOURCE_CONSTRAINT_COMPONENT,
            self.source_constraint_component.clone(),
        );
        if let Some(c) = &self.source_constraint {
            t(sh::SOURCE_CONSTRAINT, c.clone());
        }
        t(sh::RESULT_SEVERITY, self.severity.clone());
        for m in self.messages {
            t(sh::RESULT_MESSAGE, m.clone());
        }
        for d in &self.details {
            let detail = BlankNode::default();
            sink(Triple::new(node.clone(), sh::DETAIL, detail.clone()));
            d.write(&detail, sink);
        }
    }
}

/// Writes a W3C validation report as triples, one result at a time (the report is never held
/// in memory): [`ReportWriter::result`] for every result, then [`ReportWriter::finish`] for the
/// `sh:ValidationReport` node and `sh:conforms`.
/// How many nodes a shape was checked against (see [`crate::Validator::shape_counts`]).
#[derive(Debug, Clone)]
pub struct ShapeCount {
    pub shape: Term,
    /// Whether it is a property shape (with `sh:path`).
    pub property: bool,
    pub focus_nodes: usize,
    pub value_nodes: usize,
}

pub struct ReportWriter {
    report: BlankNode,
    conforms: bool,
}

impl Default for ReportWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl ReportWriter {
    pub fn new() -> Self {
        Self {
            report: BlankNode::default(),
            conforms: true,
        }
    }

    pub fn result(&mut self, r: &ValidationResult<'_>, sink: &mut dyn FnMut(Triple)) {
        self.conforms = false;
        let node = BlankNode::default();
        sink(Triple::new(self.report.clone(), sh::RESULT, node.clone()));
        r.write(&node, sink);
    }

    /// Writes the counts of [`crate::Validator::shape_counts`] on the report:
    /// `ttsh:shapeStatistics` to one IRI node per shape (not a blank node, for pipelines that
    /// skolemise), with `ttsh:shape`, `ttsh:focusNodes` and `ttsh:valueNodes`.
    pub fn statistics(&mut self, counts: &[ShapeCount], sink: &mut dyn FnMut(Triple)) {
        use crate::vocab::ttsh;
        let run = BlankNode::default();
        for (i, c) in counts.iter().enumerate() {
            let node = oxigraph::model::NamedNode::new_unchecked(format!(
                "urn:tripletrail:shape-statistics:{}:{i}",
                run.as_str()
            ));
            sink(Triple::new(
                self.report.clone(),
                ttsh::SHAPE_STATISTICS,
                node.clone(),
            ));
            sink(Triple::new(node.clone(), ttsh::SHAPE, c.shape.clone()));
            sink(Triple::new(
                node.clone(),
                ttsh::FOCUS_NODES,
                Literal::from(c.focus_nodes as i64),
            ));
            sink(Triple::new(
                node,
                ttsh::VALUE_NODES,
                Literal::from(c.value_nodes as i64),
            ));
        }
    }

    /// Writes the report node; returns whether the data conforms (no results at all,
    /// whatever their severity, as SHACL defines it).
    pub fn finish(self, sink: &mut dyn FnMut(Triple)) -> bool {
        sink(Triple::new(
            self.report.clone(),
            rdf::TYPE,
            sh::VALIDATION_REPORT,
        ));
        sink(Triple::new(
            self.report,
            sh::CONFORMS,
            Literal::from(self.conforms),
        ));
        self.conforms
    }
}

/// The constraint component of a SHACL Core constraint, by its local name.
pub(crate) fn core_component(local: &str) -> Term {
    NamedNode::new_unchecked(format!("{}{local}ConstraintComponent", sh::NS)).into()
}
