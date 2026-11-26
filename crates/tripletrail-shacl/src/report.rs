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
    /// The source shape's IRI, the same in every run even for a blank node shape (see
    /// [`crate::Shape::iri`]). Not part of the W3C report.
    pub source_shape_iri: &'a NamedNode,
    /// `sh:sourceConstraintComponent`.
    pub source_constraint_component: Term,
    /// `sh:sourceConstraint`: the `sh:sparql` node of a SPARQL-based constraint.
    pub source_constraint: Option<Term>,
    /// `sh:resultSeverity`: the source shape's `sh:severity`, `sh:Violation` by default.
    pub severity: &'a Term,
    /// `sh:resultMessage`s: the source shape's `sh:message`s, or else those of the
    /// SPARQL-based constraint, with the `{$var}` and `{?var}` of a SPARQL-based result
    /// filled in from its solution (SHACL 5.3.2).
    pub messages: Cow<'a, [Term]>,
    /// `sh:detail`s: for a failure of a shape reached through `sh:node`, `sh:and`, `sh:or`,
    /// `sh:xone` or `sh:qualifiedValueShape`, the results of the value node against that shape
    /// (see [`crate::Validator::report`]).
    pub details: Vec<ValidationResult<'a>>,
}

/// The most results whose [finding keys](ValidationResult::key) a comparison holds: each takes
/// about 40 bytes in a hash map (the 16-byte key, a count and a shape index, and the map's own
/// overhead), so 4 million take about 160 MB. Past it, a comparison gives counts by shape only.
pub const FINDING_KEYS_HELD: usize = 4_000_000;

/// 128-bit FNV-1a, fed as text: fixed by its definition, so a key is the same in every build
/// and run, and can be stored to compare with a later run.
struct Fnv128(u128);

impl Fnv128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

    fn bytes(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u128::from(*b);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    /// One field: its text, then a byte that no N-Triples term or SPARQL path holds, so that
    /// fields cannot run into each other.
    fn field(&mut self, f: impl std::fmt::Display) {
        use std::fmt::Write;
        let _ = write!(self, "{f}");
        self.bytes(&[0xff]);
    }
}

impl std::fmt::Write for Fnv128 {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.bytes(s.as_bytes());
        Ok(())
    }
}

impl ValidationResult<'_> {
    /// The finding key: one result told apart from every other by its focus node, path, value,
    /// source shape (by its [stable IRI](crate::Shape::iri)), constraint component and
    /// severity, as a 128-bit FNV-1a hash of them written out (terms in N-Triples, the path in
    /// SPARQL syntax). The same result in two runs, or with two sets of shapes in which its
    /// shape says the same thing, has the same key; messages and `sh:detail`s do not count.
    /// Proposal validation and quality runs compare results by it; see
    /// [`FINDING_KEYS_HELD`] for what holding them costs.
    pub fn key(&self) -> u128 {
        let mut h = Fnv128(Fnv128::OFFSET);
        h.field(&self.focus_node);
        match &self.result_path {
            Some(p) => h.field(p.to_sparql()),
            None => h.field(""),
        }
        match &self.value {
            Some(v) => h.field(v),
            None => h.field(""),
        }
        h.field(self.source_shape_iri);
        h.field(&self.source_constraint_component);
        h.field(self.severity);
        h.0
    }
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
        for m in self.messages.iter() {
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
    /// The shape's IRI, the same in every run even for a blank node shape (see
    /// [`crate::Shape::iri`]).
    pub iri: NamedNode,
    /// Whether it is a property shape (with `sh:path`).
    pub property: bool,
    /// Checks against a focus node: a node reached from two parents counts twice.
    pub focus_nodes: usize,
    pub value_nodes: usize,
    /// Distinct focus nodes it was checked against: each node once, however it was reached
    /// and however many targets selected it.
    pub distinct_focus_nodes: usize,
    /// Distinct focus nodes of the results whose `sh:sourceShape` it is, so that
    /// `failing_focus_nodes / distinct_focus_nodes` is the shape's failure rate. Filled in by
    /// [`FailingFocusNodes::fill`]; 0 until then.
    pub failing_focus_nodes: usize,
}

/// A 64-bit hash of a term, the same in every run: sets of nodes keep hashes, not terms, so
/// that counting the focus nodes of a large graph costs 8 bytes a node (two different
/// nodes count as one with a probability of about n²/2⁶⁵, under one in a million below
/// six million nodes).
pub(crate) fn term_hash(t: &Term) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    t.hash(&mut h);
    h.finish()
}

/// The distinct focus nodes of the results of each source shape, gathered while the results
/// are given (see [`ShapeCount::failing_focus_nodes`]).
#[derive(Default)]
pub struct FailingFocusNodes(rustc_hash::FxHashSet<(u64, u64)>);

impl FailingFocusNodes {
    /// Counts the focus node of `r` for its source shape, and those of its `sh:detail`s (a
    /// value node that fails a shape reached through `sh:node`, ...) for theirs.
    pub fn add(&mut self, r: &ValidationResult<'_>) {
        self.0
            .insert((term_hash(r.source_shape), term_hash(&r.focus_node)));
        for d in &r.details {
            self.add(d);
        }
    }

    /// Sets [`ShapeCount::failing_focus_nodes`] of each count.
    pub fn fill(&self, counts: &mut [ShapeCount]) {
        let mut per_shape: rustc_hash::FxHashMap<u64, usize> = Default::default();
        for (shape, _) in &self.0 {
            *per_shape.entry(*shape).or_default() += 1;
        }
        for c in counts {
            c.failing_focus_nodes = per_shape.get(&term_hash(&c.shape)).copied().unwrap_or(0);
        }
    }
}

/// Sample results a [`Cell`] keeps.
pub const CELL_SAMPLES: usize = 3;

/// The results of one rule (source shape) with one severity and one constraint component: a
/// cell of the run's index, gathered by [`Cells`].
#[derive(Debug, Clone)]
pub struct Cell {
    /// The source shape's stable IRI (see [`crate::Shape::iri`]).
    pub shape_iri: NamedNode,
    pub severity: Term,
    pub component: Term,
    /// Results in the cell.
    pub results: u64,
    /// Distinct focus nodes of those results (by a 64-bit hash, as
    /// [`ShapeCount::failing_focus_nodes`]).
    pub focus_nodes: usize,
    /// The first [`CELL_SAMPLES`] results, in the order they were found.
    pub samples: Vec<Sample>,
}

/// One sample result of a [`Cell`].
#[derive(Debug, Clone)]
pub struct Sample {
    pub focus_node: Term,
    pub value: Option<Term>,
    /// Its [finding key](ValidationResult::key).
    pub key: u128,
    /// Its node in the report, when [`ReportWriter`] gathered the cells.
    pub node: Option<BlankNode>,
}

/// A validation run in figures, made while the results are found (see [`Cells`]).
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// Results (those in `sh:detail`s not counted).
    pub results: u64,
    /// Distinct focus nodes with a result.
    pub failing_focus_nodes: usize,
    /// One per rule, severity and constraint component with results, the most results first.
    pub cells: Vec<Cell>,
}

impl Summary {
    /// Results by severity, the most first.
    pub fn by_severity(&self) -> Vec<(Term, u64)> {
        let mut out: Vec<(Term, u64)> = Vec::new();
        for c in &self.cells {
            match out.iter_mut().find(|(s, _)| *s == c.severity) {
                Some((_, n)) => *n += c.results,
                None => out.push((c.severity.clone(), c.results)),
            }
        }
        out.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.to_string().cmp(&b.0.to_string()))
        });
        out
    }
}

/// The cell index of a run (rule × severity × constraint component), gathered result by result
/// as they are given, so the summary is ready when validation ends: each result costs a hash
/// of its cell and of its focus node, and each distinct focus node 8 bytes per cell it is in
/// and 8 for the run. Results in `sh:detail`s explain their parent and are not counted.
#[derive(Default)]
pub struct Cells {
    by_hash: rustc_hash::FxHashMap<u64, Vec<u32>>,
    cells: Vec<(Cell, rustc_hash::FxHashSet<u64>)>,
    focus_nodes: rustc_hash::FxHashSet<u64>,
    results: u64,
}

impl Cells {
    pub fn add(&mut self, r: &ValidationResult<'_>) {
        self.add_at(r, None);
    }

    fn add_at(&mut self, r: &ValidationResult<'_>, node: Option<&BlankNode>) {
        use std::hash::{Hash, Hasher};
        self.results += 1;
        let focus = term_hash(&r.focus_node);
        self.focus_nodes.insert(focus);
        let mut h = rustc_hash::FxHasher::default();
        r.source_shape_iri.as_str().hash(&mut h);
        r.severity.hash(&mut h);
        r.source_constraint_component.hash(&mut h);
        let bucket = self.by_hash.entry(h.finish()).or_default();
        let found = bucket.iter().copied().find(|&i| {
            let c = &self.cells[i as usize].0;
            c.shape_iri == *r.source_shape_iri
                && c.severity == *r.severity
                && c.component == r.source_constraint_component
        });
        let i = match found {
            Some(i) => i as usize,
            None => {
                bucket.push(self.cells.len() as u32);
                self.cells.push((
                    Cell {
                        shape_iri: r.source_shape_iri.clone(),
                        severity: r.severity.clone(),
                        component: r.source_constraint_component.clone(),
                        results: 0,
                        focus_nodes: 0,
                        samples: Vec::new(),
                    },
                    Default::default(),
                ));
                self.cells.len() - 1
            }
        };
        let (cell, seen) = &mut self.cells[i];
        cell.results += 1;
        seen.insert(focus);
        if cell.samples.len() < CELL_SAMPLES {
            cell.samples.push(Sample {
                focus_node: r.focus_node.clone(),
                value: r.value.clone(),
                key: r.key(),
                node: node.cloned(),
            });
        }
    }

    pub fn finish(self) -> Summary {
        let mut cells: Vec<Cell> = self
            .cells
            .into_iter()
            .map(|(mut c, seen)| {
                c.focus_nodes = seen.len();
                c
            })
            .collect();
        cells.sort_by(|a, b| {
            b.results
                .cmp(&a.results)
                .then_with(|| a.shape_iri.as_str().cmp(b.shape_iri.as_str()))
                .then_with(|| a.severity.to_string().cmp(&b.severity.to_string()))
                .then_with(|| a.component.to_string().cmp(&b.component.to_string()))
        });
        Summary {
            results: self.results,
            failing_focus_nodes: self.focus_nodes.len(),
            cells,
        }
    }
}

pub struct ReportWriter {
    report: BlankNode,
    conforms: bool,
    cells: Option<Cells>,
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
            cells: None,
        }
    }

    /// Also gathers the run's [`Cells`] as results are written, for [`ReportWriter::statistics`]
    /// to write the summary and the cell index.
    pub fn with_cells(mut self) -> Self {
        self.cells = Some(Cells::default());
        self
    }

    pub fn result(&mut self, r: &ValidationResult<'_>, sink: &mut dyn FnMut(Triple)) {
        self.conforms = false;
        let node = BlankNode::default();
        sink(Triple::new(self.report.clone(), sh::RESULT, node.clone()));
        r.write(&node, sink);
        if let Some(c) = &mut self.cells {
            c.add_at(r, Some(&node));
        }
    }

    /// With [`ReportWriter::with_cells`], the summary of the results written so far (once:
    /// `None` after).
    pub fn take_summary(&mut self) -> Option<Summary> {
        self.cells.take().map(Cells::finish)
    }

    /// Writes the run's summary on the report (`ttsh:results`, `ttsh:failingFocusNodes`) and
    /// its cell index: `ttsh:cell` to one IRI node per cell, with `ttsh:shapeIri`,
    /// `ttsh:severity`, `ttsh:component`, `ttsh:results`, `ttsh:failingFocusNodes` and
    /// `ttsh:sample` to up to [`CELL_SAMPLES`] of its results.
    pub fn summary(&mut self, summary: &Summary, sink: &mut dyn FnMut(Triple)) {
        use crate::vocab::ttsh;
        let count = |n: u64| Literal::from(i64::try_from(n).unwrap_or(i64::MAX));
        sink(Triple::new(
            self.report.clone(),
            ttsh::RESULTS,
            count(summary.results),
        ));
        sink(Triple::new(
            self.report.clone(),
            ttsh::FAILING_FOCUS_NODES,
            count(summary.failing_focus_nodes as u64),
        ));
        let run = BlankNode::default();
        for (i, c) in summary.cells.iter().enumerate() {
            let node =
                NamedNode::new_unchecked(format!("urn:tripletrail:cell:{}:{i}", run.as_str()));
            sink(Triple::new(self.report.clone(), ttsh::CELL, node.clone()));
            sink(Triple::new(
                node.clone(),
                ttsh::SHAPE_IRI,
                c.shape_iri.clone(),
            ));
            sink(Triple::new(
                node.clone(),
                ttsh::SEVERITY,
                c.severity.clone(),
            ));
            sink(Triple::new(
                node.clone(),
                ttsh::COMPONENT,
                c.component.clone(),
            ));
            sink(Triple::new(node.clone(), ttsh::RESULTS, count(c.results)));
            sink(Triple::new(
                node.clone(),
                ttsh::FAILING_FOCUS_NODES,
                count(c.focus_nodes as u64),
            ));
            for s in c.samples.iter().filter_map(|s| s.node.as_ref()) {
                sink(Triple::new(node.clone(), ttsh::SAMPLE, s.clone()));
            }
        }
    }

    /// Writes the counts of [`crate::Validator::shape_counts`] on the report:
    /// `ttsh:shapeStatistics` to one IRI node per shape (not a blank node, for pipelines that
    /// skolemise), with `ttsh:shape`, `ttsh:shapeIri`, `ttsh:focusNodes`, `ttsh:valueNodes`,
    /// `ttsh:distinctFocusNodes` and `ttsh:failingFocusNodes`.
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
            sink(Triple::new(node.clone(), ttsh::SHAPE_IRI, c.iri.clone()));
            sink(Triple::new(
                node.clone(),
                ttsh::FOCUS_NODES,
                Literal::from(c.focus_nodes as i64),
            ));
            sink(Triple::new(
                node.clone(),
                ttsh::VALUE_NODES,
                Literal::from(c.value_nodes as i64),
            ));
            sink(Triple::new(
                node.clone(),
                ttsh::DISTINCT_FOCUS_NODES,
                Literal::from(c.distinct_focus_nodes as i64),
            ));
            sink(Triple::new(
                node,
                ttsh::FAILING_FOCUS_NODES,
                Literal::from(c.failing_focus_nodes as i64),
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

/// `messages` with each `{$name}` and `{?name}` filled in from `bindings` (SHACL 5.3.2,
/// 6.4): an IRI as the IRI, a literal as its lexical form, a blank node as `_:label`. A
/// variable without a binding is left as written. Messages without a template are borrowed.
pub(crate) fn fill_messages<'a>(
    messages: &'a [Term],
    bindings: &[(String, Term)],
) -> Cow<'a, [Term]> {
    let templated = |m: &Term| matches!(m, Term::Literal(l) if l.value().contains('{'));
    if bindings.is_empty() || !messages.iter().any(templated) {
        return Cow::Borrowed(messages);
    }
    Cow::Owned(
        messages
            .iter()
            .map(|m| match m {
                Term::Literal(l) if templated(m) => {
                    let text = fill(l.value(), bindings);
                    match l.language() {
                        Some(lang) => Literal::new_language_tagged_literal_unchecked(text, lang),
                        None => Literal::new_typed_literal(text, l.datatype()),
                    }
                    .into()
                }
                other => other.clone(),
            })
            .collect(),
    )
}

/// The names of the variables `messages` name as `{$x}` or `{?x}`, each once.
pub(crate) fn template_names(messages: &[Term]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for m in messages {
        let Term::Literal(l) = m else { continue };
        let mut rest = l.value();
        while let Some(at) = rest.find('{') {
            let after = &rest[at + 1..];
            rest = after;
            let Some(tail) = after.strip_prefix('$').or_else(|| after.strip_prefix('?')) else {
                continue;
            };
            if let Some(end) = tail.find('}') {
                let name = &tail[..end];
                if !names.iter().any(|n| n == name) {
                    names.push(name.to_owned());
                }
            }
        }
    }
    names
}

fn fill(template: &str, bindings: &[(String, Term)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(at) = rest.find('{') {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        let filled = after
            .strip_prefix("{$")
            .or_else(|| after.strip_prefix("{?"))
            .and_then(|tail| {
                let end = tail.find('}')?;
                let name = &tail[..end];
                let (_, term) = bindings.iter().find(|(n, _)| n == name)?;
                Some((
                    match term {
                        Term::NamedNode(n) => n.as_str().to_owned(),
                        Term::Literal(l) => l.value().to_owned(),
                        other => other.to_string(),
                    },
                    end + 3,
                ))
            });
        match filled {
            Some((text, len)) => {
                out.push_str(&text);
                rest = &after[len..];
            }
            None => {
                out.push('{');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}
