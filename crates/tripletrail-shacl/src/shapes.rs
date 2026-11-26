//! Shapes read from a shapes graph into a compact form: paths parsed, lists read, patterns
//! compiled and shapes referring to each other by index.

use crate::ShaclError;
use crate::graph::{DataGraph, subject};
use crate::path::{Path, list};
use crate::rules::{NodeExpr, Rule, RuleKind};
use crate::sparql::{self, ShapeQuery, var};
use crate::value::number;
use crate::vocab::{rdf, rdfs, sh};
use oxigraph::model::{NamedNode, NamedNodeRef, Term};
use regex::Regex;
use rustc_hash::{FxHashMap, FxHashSet};
use spargebra::Query;
use spargebra::algebra::GraphPattern;
use std::collections::BTreeSet;
use std::rc::Rc;

/// A shape's index in its [`Shapes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShapeId(pub(crate) u32);

/// A node or property shape.
#[derive(Debug)]
pub struct Shape {
    /// The shape's node in the shapes graph.
    pub node: Term,
    pub(crate) path: Option<Path>,
    pub(crate) targets: Vec<Target>,
    pub(crate) components: Vec<Component>,
    pub(crate) deactivated: bool,
    /// `sh:severity` (`sh:Violation` when absent).
    pub(crate) severity: Term,
    /// `sh:message`s, used for every result of the shape.
    pub(crate) messages: Vec<Term>,
    /// The shape's IRI, or for a blank node shape a stable IRI named for it (see
    /// [`Shape::iri`]).
    pub(crate) iri: NamedNode,
}

impl Shape {
    /// The path of a property shape.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_ref()
    }

    /// Whether the shape has targets.
    pub fn has_targets(&self) -> bool {
        !self.targets.is_empty()
    }

    /// The shape's IRI. A blank node shape (a property shape written inline, say) has a new
    /// label in every read of the shapes graph, so it gets an IRI that is the same in every
    /// run while the shape says the same thing: its nearest ancestor with an IRI (the node
    /// shape it belongs to), then `#` (`-` when that IRI has a `#`) and 16 hexadecimal digits
    /// of a 64-bit FNV-1a hash of the shape's triples written out, nested blank nodes and
    /// lists in full and sorted. With no ancestor with an IRI it is
    /// `urn:x-tripletrail-shacl:shape:` and the digits. Two blank node shapes of one ancestor
    /// that say exactly the same thing share it.
    pub fn iri(&self) -> &NamedNode {
        &self.iri
    }
}

#[derive(Debug)]
pub(crate) enum Target {
    Node(Term),
    Class(Term),
    SubjectsOf(NamedNode),
    ObjectsOf(NamedNode),
    /// A SPARQL-based target (an index in [`Shapes::target_queries`]): the `?this` of its
    /// solutions.
    Sparql(u32),
}

/// A SPARQL-based constraint, or a constraint of a SPARQL-based constraint component.
#[derive(Debug)]
pub(crate) struct SparqlCheck {
    /// The shape it belongs to (whose path gives the value nodes of an ASK validator).
    pub shape: ShapeId,
    /// A SELECT query (solutions are violations); for an ASK validator, the ASK pattern as
    /// `SELECT DISTINCT $this $value` (the value nodes that conform).
    pub query: ShapeQuery,
    /// An ASK validator, checked per value node with `$value` bound.
    pub ask: bool,
    /// `$value` is bound to the focus node (SELECT validators of node shapes).
    pub bind_value: bool,
    /// `$this` is projected, so one query can check many focus nodes.
    pub batch: bool,
    /// The constraint component it checks: `sh:SPARQLConstraintComponent` or the SPARQL-based
    /// constraint component's node.
    pub source: Term,
    /// The `sh:message`s of the SPARQL-based constraint or validator.
    pub messages: Vec<Term>,
    /// The `sh:sparql` node of a SPARQL-based constraint (`sh:sourceConstraint`).
    pub constraint: Option<Term>,
}

pub(crate) const KIND_IRI: u8 = 1;
pub(crate) const KIND_BLANK: u8 = 2;
pub(crate) const KIND_LITERAL: u8 = 4;

#[derive(Debug)]
pub(crate) enum Component {
    MinCount(usize),
    MaxCount(usize),
    HasValue(Term),
    In(FxHashSet<Term>),
    NodeKind(u8),
    Datatype(NamedNode),
    MinExclusive(Term),
    MinInclusive(Term),
    MaxExclusive(Term),
    MaxInclusive(Term),
    MinLength(usize),
    MaxLength(usize),
    Pattern(Regex),
    LanguageIn(Vec<String>),
    UniqueLang,
    Class(Term),
    Equals(NamedNode),
    Disjoint(NamedNode),
    LessThan(NamedNode),
    LessThanOrEquals(NamedNode),
    Closed {
        allowed: FxHashSet<NamedNode>,
    },
    Not(ShapeId),
    And(Vec<ShapeId>),
    Or(Vec<ShapeId>),
    Xone(Vec<ShapeId>),
    Node(ShapeId),
    Property(ShapeId),
    Qualified {
        shape: ShapeId,
        min: Option<usize>,
        max: Option<usize>,
        disjoint: bool,
        /// The qualified value shapes of the sibling property shapes (when `disjoint`).
        siblings: Vec<ShapeId>,
    },
    /// A SHACL-SPARQL constraint (an index in [`Shapes::checks`]).
    Sparql(u32),
}

impl Component {
    /// Rough cost, to check cheap components first.
    fn cost(&self) -> u8 {
        match self {
            Self::MinCount(_) | Self::MaxCount(_) | Self::HasValue(_) | Self::In(_) => 0,
            Self::NodeKind(_)
            | Self::Datatype(_)
            | Self::MinExclusive(_)
            | Self::MinInclusive(_)
            | Self::MaxExclusive(_)
            | Self::MaxInclusive(_)
            | Self::MinLength(_)
            | Self::MaxLength(_)
            | Self::LanguageIn(_)
            | Self::UniqueLang => 1,
            Self::Pattern(_) => 2,
            Self::Class(_)
            | Self::Equals(_)
            | Self::Disjoint(_)
            | Self::LessThan(_)
            | Self::LessThanOrEquals(_)
            | Self::Closed { .. } => 3,
            Self::Sparql(_) => 5,
            _ => 4,
        }
    }
}

/// Compiled shapes.
#[derive(Debug, Default)]
pub struct Shapes {
    shapes: Vec<Shape>,
    by_node: FxHashMap<Term, ShapeId>,
    targeted: Vec<ShapeId>,
    pub(crate) checks: Vec<SparqlCheck>,
    pub(crate) target_queries: Vec<ShapeQuery>,
    pub(crate) rules: Vec<Rule>,
}

impl Shapes {
    /// Compiles the shapes of a shapes graph: those typed `sh:NodeShape` or `sh:PropertyShape`,
    /// those with targets, and the shapes they refer to.
    pub fn compile(g: &dyn DataGraph) -> Result<Self, ShaclError> {
        Self::compile_with(g, &[])
    }

    /// As [`Shapes::compile`], also compiling the shapes at `roots` (which must have at least one
    /// triple in the shapes graph).
    pub fn compile_with(g: &dyn DataGraph, roots: &[Term]) -> Result<Self, ShaclError> {
        let mut c = Compiler {
            g,
            shapes: Self::default(),
            components: Rc::new(Vec::new()),
        };
        c.components = Rc::new(c.components()?);
        let mut candidates = Vec::new();
        let mut pairs = Vec::new();
        g.with_predicate(rdf::TYPE, &mut pairs);
        for (s, o) in pairs.drain(..) {
            if o.as_ref() == sh::NODE_SHAPE.into() || o.as_ref() == sh::PROPERTY_SHAPE.into() {
                candidates.push(s);
            }
        }
        for p in [
            sh::TARGET_NODE,
            sh::TARGET_CLASS,
            sh::TARGET_SUBJECTS_OF,
            sh::TARGET_OBJECTS_OF,
            sh::TARGET,
        ] {
            g.with_predicate(p, &mut pairs);
            candidates.extend(pairs.drain(..).map(|(s, _)| s));
        }
        for r in roots {
            let mut out = Vec::new();
            g.outgoing(r, &mut out);
            if out.is_empty() {
                return Err(ShaclError::Shape(format!(
                    "no shape {r} in the shapes graph"
                )));
            }
            candidates.push(r.clone());
        }
        for node in candidates {
            c.shape(&node, 0)?;
        }
        c.finish();
        c.name_blank_shapes();
        Ok(c.shapes)
    }

    /// The shape at `node`, if it was compiled.
    pub fn shape(&self, node: &Term) -> Option<ShapeId> {
        self.by_node.get(node).copied()
    }

    pub fn get(&self, id: ShapeId) -> &Shape {
        &self.shapes[id.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.shapes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }

    /// The shapes with targets.
    pub fn targeted(&self) -> &[ShapeId] {
        &self.targeted
    }

    /// Whether some shape has SHACL-AF rules (`sh:rule`).
    pub fn has_rules(&self) -> bool {
        !self.rules.is_empty()
    }

    /// Whether validation runs SPARQL queries (SPARQL-based constraints, constraint components
    /// or targets).
    pub fn uses_sparql(&self) -> bool {
        !self.checks.is_empty() || !self.target_queries.is_empty()
    }

    /// The predicates validation reads in the data graph, or `None` when it may read any
    /// (a closed shape, SPARQL queries, or rules).
    pub fn reads(&self) -> Option<BTreeSet<String>> {
        if self.uses_sparql() || self.has_rules() {
            return None;
        }
        let mut out = BTreeSet::new();
        let mut classes = false;
        for s in &self.shapes {
            if let Some(p) = &s.path {
                p.predicates(&mut out);
            }
            for t in &s.targets {
                match t {
                    Target::Node(_) | Target::Sparql(_) => {}
                    Target::Class(_) => classes = true,
                    Target::SubjectsOf(p) | Target::ObjectsOf(p) => {
                        out.insert(p.as_str().to_owned());
                    }
                }
            }
            for c in &s.components {
                match c {
                    Component::Class(_) => classes = true,
                    Component::Equals(p)
                    | Component::Disjoint(p)
                    | Component::LessThan(p)
                    | Component::LessThanOrEquals(p) => {
                        out.insert(p.as_str().to_owned());
                    }
                    Component::Closed { .. } => return None,
                    _ => {}
                }
            }
        }
        if classes {
            out.insert(rdf::TYPE.as_str().to_owned());
            out.insert(rdfs::SUB_CLASS_OF.as_str().to_owned());
        }
        Some(out)
    }
}

struct Compiler<'a> {
    g: &'a dyn DataGraph,
    shapes: Shapes,
    /// The SPARQL-based constraint components of the shapes graph.
    components: Rc<Vec<ComponentDef>>,
}

/// A SPARQL-based constraint component (`sh:ConstraintComponent`).
struct ComponentDef {
    node: Term,
    params: Vec<Param>,
    validator: Option<ValidatorDef>,
    node_validator: Option<ValidatorDef>,
    property_validator: Option<ValidatorDef>,
}

/// A parameter of a constraint component or target type.
#[derive(Clone)]
struct Param {
    path: NamedNode,
    /// The variable it binds: the local name of its path.
    name: String,
    optional: bool,
}

/// An ASK or SELECT validator.
struct ValidatorDef {
    node: Term,
    text: String,
    ask: bool,
    prefixes: Vec<(String, String)>,
}

/// The local name of an IRI (after its last `#`, `/` or `:`).
fn local_name(n: &NamedNode) -> &str {
    let s = n.as_str();
    s.rfind(['#', '/', ':']).map_or(s, |i| &s[i + 1..])
}

/// Every combination of one value per parameter (absent optional parameters unbound).
fn combinations(values: &[(Param, Vec<Term>)]) -> Vec<Vec<(String, Term)>> {
    let mut out: Vec<Vec<(String, Term)>> = vec![Vec::new()];
    for (p, vs) in values {
        if vs.is_empty() {
            continue;
        }
        let mut next = Vec::new();
        for combo in &out {
            for v in vs {
                let mut c = combo.clone();
                c.push((p.name.clone(), v.clone()));
                next.push(c);
            }
        }
        out = next;
        if out.len() > 64 {
            out.truncate(64);
        }
    }
    out
}

fn bad(msg: String) -> ShaclError {
    ShaclError::Shape(msg)
}

impl Compiler<'_> {
    fn objects(&self, s: &Term, p: NamedNodeRef<'_>) -> Vec<Term> {
        let mut out = Vec::new();
        self.g.objects(s, p, &mut out);
        out
    }

    fn integer(&self, node: &Term, p: NamedNodeRef<'_>) -> Result<Option<usize>, ShaclError> {
        match self.objects(node, p).first() {
            None => Ok(None),
            Some(Term::Literal(l)) => l
                .value()
                .trim()
                .parse()
                .map(Some)
                .map_err(|_| bad(format!("{p} of {node} must be a non-negative integer"))),
            Some(other) => Err(bad(format!("{p} of {node} is {other}, not an integer"))),
        }
    }

    fn flag(&self, node: &Term, p: NamedNodeRef<'_>) -> bool {
        self.objects(node, p)
            .first()
            // Parameters compare as RDF terms: only `true` (not `"1"^^xsd:boolean`) is true.
            .is_some_and(|t| matches!(t, Term::Literal(l) if l.value() == "true"))
    }

    fn iri(&self, node: &Term, p: NamedNodeRef<'_>, t: Term) -> Result<NamedNode, ShaclError> {
        match t {
            Term::NamedNode(n) => Ok(n),
            other => Err(bad(format!("{p} of {node} is {other}, not an IRI"))),
        }
    }

    fn shapes_of(&mut self, list_head: &Term, depth: usize) -> Result<Vec<ShapeId>, ShaclError> {
        list(self.g, list_head)?
            .iter()
            .map(|m| self.shape(m, depth + 1))
            .collect()
    }

    fn shape(&mut self, node: &Term, depth: usize) -> Result<ShapeId, ShaclError> {
        if let Some(&id) = self.shapes.by_node.get(node) {
            return Ok(id);
        }
        if subject(node).is_none() {
            return Err(bad(format!("{node} cannot be a shape")));
        }
        if depth > 256 {
            return Err(bad(format!("shapes nested too deep at {node}")));
        }
        let id = ShapeId(self.shapes.shapes.len() as u32);
        self.shapes.by_node.insert(node.clone(), id);
        self.shapes.shapes.push(Shape {
            node: node.clone(),
            path: None,
            targets: Vec::new(),
            components: Vec::new(),
            deactivated: false,
            severity: sh::VIOLATION.into(),
            messages: Vec::new(),
            iri: match node {
                Term::NamedNode(n) => n.clone(),
                // Named in `Compiler::name_blank_shapes`.
                _ => NamedNode::new_unchecked("urn:x-tripletrail-shacl:shape:"),
            },
        });

        let path = match self.objects(node, sh::PATH).first() {
            Some(p) => Some(Path::parse(self.g, p)?),
            None => None,
        };
        let deactivated = self.flag(node, sh::DEACTIVATED);

        let mut targets = Vec::new();
        for t in self.objects(node, sh::TARGET_NODE) {
            targets.push(Target::Node(t));
        }
        for t in self.objects(node, sh::TARGET_CLASS) {
            targets.push(Target::Class(t));
        }
        for t in self.objects(node, sh::TARGET_SUBJECTS_OF) {
            targets.push(Target::SubjectsOf(self.iri(
                node,
                sh::TARGET_SUBJECTS_OF,
                t,
            )?));
        }
        for t in self.objects(node, sh::TARGET_OBJECTS_OF) {
            targets.push(Target::ObjectsOf(self.iri(
                node,
                sh::TARGET_OBJECTS_OF,
                t,
            )?));
        }
        for t in self.objects(node, sh::TARGET) {
            let q = self.target_query(&t)?;
            self.shapes.target_queries.push(q);
            targets.push(Target::Sparql(
                (self.shapes.target_queries.len() - 1) as u32,
            ));
        }
        let types = self.objects(node, rdf::TYPE);
        let is_class = types.iter().any(|t| {
            t.as_ref() == rdfs::CLASS.into()
                || matches!(t, Term::NamedNode(n) if n.as_str() == "http://www.w3.org/2002/07/owl#Class")
        });
        let is_shape = types.iter().any(|t| {
            t.as_ref() == sh::NODE_SHAPE.into() || t.as_ref() == sh::PROPERTY_SHAPE.into()
        });
        if is_class && is_shape {
            targets.push(Target::Class(node.clone()));
        }

        let mut cs = Vec::new();
        if let Some(n) = self.integer(node, sh::MIN_COUNT)? {
            cs.push(Component::MinCount(n));
        }
        if let Some(n) = self.integer(node, sh::MAX_COUNT)? {
            cs.push(Component::MaxCount(n));
        }
        for v in self.objects(node, sh::HAS_VALUE) {
            cs.push(Component::HasValue(v));
        }
        for l in self.objects(node, sh::IN) {
            cs.push(Component::In(list(self.g, &l)?.into_iter().collect()));
        }
        for k in self.objects(node, sh::NODE_KIND) {
            let kind = match &k {
                Term::NamedNode(n) => match n.as_str().strip_prefix(sh::NS) {
                    Some("IRI") => KIND_IRI,
                    Some("BlankNode") => KIND_BLANK,
                    Some("Literal") => KIND_LITERAL,
                    Some("BlankNodeOrIRI") => KIND_BLANK | KIND_IRI,
                    Some("BlankNodeOrLiteral") => KIND_BLANK | KIND_LITERAL,
                    Some("IRIOrLiteral") => KIND_IRI | KIND_LITERAL,
                    _ => 0,
                },
                _ => 0,
            };
            if kind == 0 {
                return Err(bad(format!("{node}: unknown sh:nodeKind {k}")));
            }
            cs.push(Component::NodeKind(kind));
        }
        for d in self.objects(node, sh::DATATYPE) {
            cs.push(Component::Datatype(self.iri(node, sh::DATATYPE, d)?));
        }
        for v in self.objects(node, sh::MIN_EXCLUSIVE) {
            cs.push(Component::MinExclusive(v));
        }
        for v in self.objects(node, sh::MIN_INCLUSIVE) {
            cs.push(Component::MinInclusive(v));
        }
        for v in self.objects(node, sh::MAX_EXCLUSIVE) {
            cs.push(Component::MaxExclusive(v));
        }
        for v in self.objects(node, sh::MAX_INCLUSIVE) {
            cs.push(Component::MaxInclusive(v));
        }
        if let Some(n) = self.integer(node, sh::MIN_LENGTH)? {
            cs.push(Component::MinLength(n));
        }
        if let Some(n) = self.integer(node, sh::MAX_LENGTH)? {
            cs.push(Component::MaxLength(n));
        }
        let flags = self
            .objects(node, sh::FLAGS)
            .first()
            .and_then(|f| match f {
                Term::Literal(l) => Some(l.value().to_owned()),
                _ => None,
            })
            .unwrap_or_default();
        for p in self.objects(node, sh::PATTERN) {
            let Term::Literal(p) = p else {
                return Err(bad(format!("{node}: sh:pattern must be a string")));
            };
            cs.push(Component::Pattern(regex(p.value(), &flags)?));
        }
        for l in self.objects(node, sh::LANGUAGE_IN) {
            let ranges = list(self.g, &l)?
                .into_iter()
                .map(|t| match t {
                    Term::Literal(l) => Ok(l.value().to_owned()),
                    other => Err(bad(format!("{node}: sh:languageIn holds {other}"))),
                })
                .collect::<Result<_, _>>()?;
            cs.push(Component::LanguageIn(ranges));
        }
        if self.flag(node, sh::UNIQUE_LANG) {
            cs.push(Component::UniqueLang);
        }
        for c in self.objects(node, sh::CLASS) {
            cs.push(Component::Class(c));
        }
        for (p, make) in [
            (sh::EQUALS, Component::Equals as fn(NamedNode) -> Component),
            (sh::DISJOINT, Component::Disjoint),
            (sh::LESS_THAN, Component::LessThan),
            (sh::LESS_THAN_OR_EQUALS, Component::LessThanOrEquals),
        ] {
            for o in self.objects(node, p) {
                cs.push(make(self.iri(node, p, o)?));
            }
        }
        if self.flag(node, sh::CLOSED) {
            let mut allowed = FxHashSet::default();
            for l in self.objects(node, sh::IGNORED_PROPERTIES) {
                for t in list(self.g, &l)? {
                    allowed.insert(self.iri(node, sh::IGNORED_PROPERTIES, t)?);
                }
            }
            cs.push(Component::Closed { allowed });
        }
        for s in self.objects(node, sh::NOT) {
            cs.push(Component::Not(self.shape(&s, depth + 1)?));
        }
        for l in self.objects(node, sh::AND) {
            cs.push(Component::And(self.shapes_of(&l, depth)?));
        }
        for l in self.objects(node, sh::OR) {
            cs.push(Component::Or(self.shapes_of(&l, depth)?));
        }
        for l in self.objects(node, sh::XONE) {
            cs.push(Component::Xone(self.shapes_of(&l, depth)?));
        }
        for s in self.objects(node, sh::NODE) {
            cs.push(Component::Node(self.shape(&s, depth + 1)?));
        }
        for s in self.objects(node, sh::PROPERTY) {
            let p = self.shape(&s, depth + 1)?;
            cs.push(Component::Property(p));
        }
        if let Some(s) = self.objects(node, sh::QUALIFIED_VALUE_SHAPE).first() {
            let shape = self.shape(s, depth + 1)?;
            cs.push(Component::Qualified {
                shape,
                min: self.integer(node, sh::QUALIFIED_MIN_COUNT)?,
                max: self.integer(node, sh::QUALIFIED_MAX_COUNT)?,
                disjoint: self.flag(node, sh::QUALIFIED_VALUE_SHAPES_DISJOINT),
                siblings: Vec::new(),
            });
        }
        for c in self.objects(node, sh::SPARQL) {
            if self.flag(&c, sh::DEACTIVATED) {
                continue;
            }
            let Some(text) = self.text(&c, sh::SELECT) else {
                return Err(bad(format!("{node}: sh:sparql {c} has no sh:select")));
            };
            let prefixes = sparql::prefixes(self.g, &c)?;
            let query = sparql::parse(&text, &prefixes, path.as_ref(), &c)?;
            sparql::check_prebinding(&query, &["this", "currentShape", "shapesGraph"], &c)?;
            let check = self.select_check(
                id,
                query,
                node,
                Vec::new(),
                false,
                &c,
                sh::SPARQL_CONSTRAINT_COMPONENT.into(),
            )?;
            cs.push(check);
        }
        for def in Rc::clone(&self.components).iter() {
            cs.extend(self.component_checks(id, node, path.as_ref(), def)?);
        }
        let mut rules = Vec::new();
        if !deactivated {
            for r in self.objects(node, sh::RULE) {
                if let Some(rule) = self.rule(id, node, &r, depth)? {
                    rules.push(rule);
                }
            }
        }
        self.shapes.rules.extend(rules);
        cs.sort_by_key(Component::cost);

        let severity = self.objects(node, sh::SEVERITY).into_iter().next();
        let messages = self.objects(node, sh::MESSAGE);
        let s = &mut self.shapes.shapes[id.0 as usize];
        s.path = path;
        s.deactivated = deactivated;
        s.components = cs;
        if let Some(severity) = severity {
            s.severity = severity;
        }
        s.messages = messages;
        if !targets.is_empty() {
            self.shapes.targeted.push(id);
        }
        s.targets = targets;
        Ok(id)
    }

    /// The string value of `node p`, if any.
    fn text(&self, node: &Term, p: NamedNodeRef<'_>) -> Option<String> {
        match self.objects(node, p).into_iter().next()? {
            Term::Literal(l) => Some(l.value().to_owned()),
            _ => None,
        }
    }

    /// The parameters (`sh:parameter`) of a constraint component or target type.
    fn params(&self, node: &Term) -> Result<Vec<Param>, ShaclError> {
        let mut out = Vec::new();
        for p in self.objects(node, sh::PARAMETER) {
            let Some(Term::NamedNode(path)) = self.objects(&p, sh::PATH).into_iter().next() else {
                return Err(bad(format!(
                    "{node}: parameter {p} needs an IRI as sh:path"
                )));
            };
            out.push(Param {
                name: local_name(&path).to_owned(),
                optional: self.flag(&p, sh::OPTIONAL),
                path,
            });
        }
        Ok(out)
    }

    /// The SPARQL-based constraint components of the shapes graph.
    fn components(&self) -> Result<Vec<ComponentDef>, ShaclError> {
        let mut out = Vec::new();
        // SHACL instances of sh:ConstraintComponent: typed with it or a subclass of it.
        let mut classes: Vec<Term> = vec![sh::CONSTRAINT_COMPONENT.into_owned().into()];
        let mut i = 0;
        while i < classes.len() && classes.len() < 256 {
            let mut sub = Vec::new();
            self.g.subjects(rdfs::SUB_CLASS_OF, &classes[i], &mut sub);
            for c in sub {
                if !classes.contains(&c) {
                    classes.push(c);
                }
            }
            i += 1;
        }
        let mut nodes = Vec::new();
        for class in &classes {
            self.g.subjects(rdf::TYPE, class, &mut nodes);
        }
        crate::path::dedup(&mut nodes);
        for node in nodes {
            let validator = |p| -> Result<Option<ValidatorDef>, ShaclError> {
                let Some(v) = self.objects(&node, p).into_iter().next() else {
                    return Ok(None);
                };
                let (text, ask) = match (self.text(&v, sh::ASK), self.text(&v, sh::SELECT)) {
                    (Some(t), _) => (t, true),
                    (None, Some(t)) => (t, false),
                    (None, None) => {
                        return Err(bad(format!(
                            "{node}: validator {v} has no sh:ask or sh:select"
                        )));
                    }
                };
                Ok(Some(ValidatorDef {
                    prefixes: sparql::prefixes(self.g, &v)?,
                    node: v,
                    text,
                    ask,
                }))
            };
            out.push(ComponentDef {
                params: self.params(&node)?,
                validator: validator(sh::VALIDATOR)?,
                node_validator: validator(sh::NODE_VALIDATOR)?,
                property_validator: validator(sh::PROPERTY_VALIDATOR)?,
                node,
            });
        }
        Ok(out)
    }

    /// A SELECT check (a SPARQL-based constraint or a SELECT validator) of shape `id`.
    #[allow(clippy::too_many_arguments)]
    fn select_check(
        &mut self,
        id: ShapeId,
        mut query: Query,
        shape: &Term,
        mut constants: Vec<(spargebra::term::Variable, Term)>,
        bind_value: bool,
        from: &Term,
        source: Term,
    ) -> Result<Component, ShaclError> {
        let messages = self.objects(from, sh::MESSAGE);
        let Query::Select { pattern, .. } = &mut query else {
            return Err(bad(format!("{from}: expected a SELECT query")));
        };
        let batch = sparql::project(pattern, &var("this"));
        constants.push((var("currentShape"), shape.clone()));
        let constraint = (source == sh::SPARQL_CONSTRAINT_COMPONENT.into()).then(|| from.clone());
        self.shapes.checks.push(SparqlCheck {
            shape: id,
            query: ShapeQuery { query, constants },
            ask: false,
            bind_value,
            batch,
            source,
            messages,
            constraint,
        });
        Ok(Component::Sparql((self.shapes.checks.len() - 1) as u32))
    }

    /// The checks a SPARQL-based constraint component adds to shape `id`: one per
    /// combination of its parameter values, if the shape has every mandatory parameter.
    fn component_checks(
        &mut self,
        id: ShapeId,
        shape: &Term,
        path: Option<&Path>,
        def: &ComponentDef,
    ) -> Result<Vec<Component>, ShaclError> {
        if def.params.is_empty() {
            return Ok(Vec::new());
        }
        let mut values = Vec::new();
        for p in &def.params {
            let vs = self.objects(shape, p.path.as_ref());
            if vs.is_empty() && !p.optional {
                return Ok(Vec::new());
            }
            values.push((p.clone(), vs));
        }
        let validator = match path {
            None => def.node_validator.as_ref().or(def.validator.as_ref()),
            Some(_) => def.property_validator.as_ref().or(def.validator.as_ref()),
        };
        // Without a validator for this kind of shape, the component is ignored.
        let Some(v) = validator else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for combo in combinations(&values) {
            let constants: Vec<_> = combo.into_iter().map(|(n, t)| (var(&n), t)).collect();
            let query = sparql::parse(&v.text, &v.prefixes, path, &v.node)
                .map_err(|e| bad(format!("{}: {e}", def.node)))?;
            let mut prebound = vec!["this", "value", "currentShape", "shapesGraph"];
            prebound.extend(def.params.iter().map(|p| p.name.as_str()));
            sparql::check_prebinding(&query, &prebound, &v.node)?;
            if !v.ask {
                out.push(self.select_check(
                    id,
                    query,
                    shape,
                    constants,
                    path.is_none(),
                    &v.node,
                    def.node.clone(),
                )?);
                continue;
            }
            let Query::Ask {
                pattern, base_iri, ..
            } = query
            else {
                return Err(bad(format!("{}: sh:ask needs an ASK query", v.node)));
            };
            let mut constants = constants;
            constants.push((var("currentShape"), shape.clone()));
            // The parser projects the query's variables (`SELECT *`): projected differently.
            let pattern = match pattern {
                GraphPattern::Project { inner, .. } => *inner,
                other => other,
            };
            let query = Query::Select {
                dataset: None,
                pattern: GraphPattern::Distinct {
                    inner: Box::new(GraphPattern::Project {
                        inner: Box::new(pattern),
                        variables: vec![var("this"), var("value")],
                    }),
                },
                base_iri,
            };
            self.shapes.checks.push(SparqlCheck {
                shape: id,
                query: ShapeQuery { query, constants },
                ask: true,
                bind_value: false,
                batch: true,
                source: def.node.clone(),
                messages: self.objects(&v.node, sh::MESSAGE),
                constraint: None,
            });
            out.push(Component::Sparql((self.shapes.checks.len() - 1) as u32));
        }
        Ok(out)
    }

    /// A SPARQL-based target (`sh:select`), or an instance of a SPARQL-based target type.
    fn target_query(&self, t: &Term) -> Result<ShapeQuery, ShaclError> {
        let select = |q: &Term, constants| -> Result<ShapeQuery, ShaclError> {
            let text = self.text(q, sh::SELECT).expect("checked");
            let prefixes = sparql::prefixes(self.g, q)?;
            let query = sparql::parse(&text, &prefixes, None, q)?;
            if !matches!(query, Query::Select { .. }) {
                return Err(bad(format!("{q}: a target needs a SELECT query")));
            }
            Ok(ShapeQuery { query, constants })
        };
        if self.text(t, sh::SELECT).is_some() {
            return select(t, Vec::new());
        }
        for ty in self.objects(t, rdf::TYPE) {
            if self.text(&ty, sh::SELECT).is_none() {
                continue;
            }
            let mut constants = Vec::new();
            for p in self.params(&ty)? {
                match self.objects(t, p.path.as_ref()).into_iter().next() {
                    Some(v) => constants.push((var(&p.name), v)),
                    None if p.optional => {}
                    None => {
                        return Err(bad(format!(
                            "target {t} of type {ty} has no value for {}",
                            p.path
                        )));
                    }
                }
            }
            return select(&ty, constants);
        }
        Err(bad(format!(
            "sh:target {t}: neither sh:select nor a SPARQL-based target type"
        )))
    }

    /// A rule (`sh:rule`) of shape `id`, or `None` when it is deactivated.
    fn rule(
        &mut self,
        id: ShapeId,
        shape: &Term,
        r: &Term,
        depth: usize,
    ) -> Result<Option<Rule>, ShaclError> {
        if self.flag(r, sh::DEACTIVATED) {
            return Ok(None);
        }
        let order = match self.objects(r, sh::ORDER).first() {
            None => 0.0,
            Some(o) => {
                number(o).ok_or_else(|| bad(format!("{r}: sh:order {o} is not a number")))?
            }
        };
        let mut conditions = Vec::new();
        for c in self.objects(r, sh::CONDITION) {
            conditions.push(self.shape(&c, depth + 1)?);
        }
        let kind = if let Some(text) = self.text(r, sh::CONSTRUCT) {
            let prefixes = sparql::prefixes(self.g, r)?;
            let query = sparql::parse(&text, &prefixes, None, r)?;
            if !matches!(query, Query::Construct { .. }) {
                return Err(bad(format!("{r}: sh:construct needs a CONSTRUCT query")));
            }
            RuleKind::Sparql(ShapeQuery {
                query,
                constants: vec![(var("currentShape"), shape.clone())],
            })
        } else {
            let part = |c: &mut Self, p| -> Result<NodeExpr, ShaclError> {
                let Some(e) = c.objects(r, p).into_iter().next() else {
                    return Err(bad(format!("{r}: a triple rule needs {p}")));
                };
                c.expr(&e, depth + 1)
            };
            if self.objects(r, sh::SUBJECT).is_empty() {
                return Err(bad(format!(
                    "{r}: a rule needs sh:construct (sh:SPARQLRule) or sh:subject, sh:predicate and sh:object (sh:TripleRule)"
                )));
            }
            RuleKind::Triple {
                subject: part(self, sh::SUBJECT)?,
                predicate: part(self, sh::PREDICATE)?,
                object: part(self, sh::OBJECT)?,
            }
        };
        Ok(Some(Rule {
            shape: id,
            node: r.clone(),
            order,
            conditions,
            kind,
        }))
    }

    /// A node expression (SHACL-AF): `sh:this`, a constant, a path (`sh:path`, with
    /// `sh:nodes`), a filter shape (`sh:filterShape`, with `sh:nodes`), `sh:intersection` or
    /// `sh:union`.
    fn expr(&mut self, e: &Term, depth: usize) -> Result<NodeExpr, ShaclError> {
        if depth > 64 {
            return Err(bad(format!("node expression {e} is nested too deep")));
        }
        if e.as_ref() == sh::THIS.into() {
            return Ok(NodeExpr::This);
        }
        if !matches!(e, Term::BlankNode(_)) {
            return Ok(NodeExpr::Constant(e.clone()));
        }
        let nodes = |c: &mut Self| -> Result<Box<NodeExpr>, ShaclError> {
            Ok(Box::new(match c.objects(e, sh::NODES).into_iter().next() {
                Some(n) => c.expr(&n, depth + 1)?,
                None => NodeExpr::This,
            }))
        };
        if let Some(p) = self.objects(e, sh::PATH).into_iter().next() {
            let path = Path::parse(self.g, &p)?;
            return Ok(NodeExpr::Path {
                path,
                nodes: nodes(self)?,
            });
        }
        if let Some(s) = self.objects(e, sh::FILTER_SHAPE).into_iter().next() {
            let shape = self.shape(&s, depth + 1)?;
            return Ok(NodeExpr::Filter {
                shape,
                nodes: nodes(self)?,
            });
        }
        for (p, union) in [(sh::INTERSECTION, false), (sh::UNION, true)] {
            if let Some(l) = self.objects(e, p).into_iter().next() {
                let members = list(self.g, &l)?
                    .iter()
                    .map(|m| self.expr(m, depth + 1))
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(if union {
                    NodeExpr::Union(members)
                } else {
                    NodeExpr::Intersection(members)
                });
            }
        }
        Err(bad(format!("{e} is not a supported node expression")))
    }

    /// Fills in what needs every shape compiled: the properties a closed shape allows, and the
    /// siblings of disjoint qualified value shapes.
    fn finish(&mut self) {
        let shapes = &self.shapes.shapes;
        let mut allowed_of = Vec::new();
        let mut siblings_of: FxHashMap<ShapeId, Vec<ShapeId>> = FxHashMap::default();
        for (i, s) in shapes.iter().enumerate() {
            let properties: Vec<ShapeId> = s
                .components
                .iter()
                .filter_map(|c| match c {
                    Component::Property(p) => Some(*p),
                    _ => None,
                })
                .collect();
            if s.components
                .iter()
                .any(|c| matches!(c, Component::Closed { .. }))
            {
                let allowed: Vec<NamedNode> = properties
                    .iter()
                    .filter_map(|p| shapes[p.0 as usize].path.as_ref()?.as_predicate().cloned())
                    .collect();
                allowed_of.push((i, allowed));
            }
            let qualified = |p: &ShapeId| {
                shapes[p.0 as usize]
                    .components
                    .iter()
                    .find_map(|c| match c {
                        Component::Qualified { shape, .. } => Some(*shape),
                        _ => None,
                    })
            };
            for p in &properties {
                let siblings = properties
                    .iter()
                    .filter(|q| *q != p)
                    .filter_map(qualified)
                    .collect::<Vec<_>>();
                siblings_of.entry(*p).or_default().extend(siblings);
            }
        }
        for (i, allowed) in allowed_of {
            for c in &mut self.shapes.shapes[i].components {
                if let Component::Closed { allowed: a } = c {
                    a.extend(allowed.iter().cloned());
                }
            }
        }
        for (p, siblings) in siblings_of {
            for c in &mut self.shapes.shapes[p.0 as usize].components {
                if let Component::Qualified {
                    disjoint: true,
                    siblings: s,
                    ..
                } = c
                {
                    s.extend(siblings.iter().copied());
                    s.sort();
                    s.dedup();
                }
            }
        }
    }
}

impl Compiler<'_> {
    /// Gives each blank node shape its stable IRI (see [`Shape::iri`]).
    fn name_blank_shapes(&mut self) {
        let g = self.g;
        for s in &mut self.shapes.shapes {
            if !matches!(s.node, Term::BlankNode(_)) {
                continue;
            }
            let mut text = String::new();
            written(g, &s.node, &mut Vec::new(), &mut text);
            let hash = fnv1a(text.as_bytes());
            s.iri = NamedNode::new_unchecked(match named_ancestor(g, &s.node) {
                Some(a) if a.as_str().contains('#') => format!("{}-{hash:016x}", a.as_str()),
                Some(a) => format!("{}#{hash:016x}", a.as_str()),
                None => format!("urn:x-tripletrail-shacl:shape:{hash:016x}"),
            });
        }
    }
}

/// 64-bit FNV-1a: fixed by its definition, so the same in every build and run.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Appends `t` written out: an IRI or literal in N-Triples, a blank node as `[ p o ; … ]` with
/// its triples written out the same way and sorted (`[ … ]` for one it is already inside).
fn written(g: &dyn DataGraph, t: &Term, inside: &mut Vec<Term>, out: &mut String) {
    use std::fmt::Write;
    if !matches!(t, Term::BlankNode(_)) {
        let _ = write!(out, "{t}");
        return;
    }
    if inside.contains(t) {
        out.push_str("[ … ]");
        return;
    }
    inside.push(t.clone());
    let mut pairs = Vec::new();
    g.outgoing(t, &mut pairs);
    let mut parts: Vec<String> = pairs
        .iter()
        .map(|(p, o)| {
            let mut part = format!("{p} ");
            written(g, o, inside, &mut part);
            part
        })
        .collect();
    inside.pop();
    parts.sort_unstable();
    out.push_str("[ ");
    out.push_str(&parts.join(" ; "));
    out.push_str(" ]");
}

/// The IRI nearest to blank node `t` among the subjects that refer to it, through blank
/// nodes (the list nodes of `sh:or`, an inline node shape), breadth first; at one distance,
/// the least IRI.
fn named_ancestor(g: &dyn DataGraph, t: &Term) -> Option<NamedNode> {
    let mut seen: FxHashSet<Term> = FxHashSet::default();
    let mut level = vec![t.clone()];
    seen.insert(t.clone());
    let mut triples = Vec::new();
    while !level.is_empty() {
        let mut next = Vec::new();
        let mut named: Option<NamedNode> = None;
        for node in &level {
            triples.clear();
            g.triples(None, None, Some(node), &mut triples);
            for tr in &triples {
                let s = tr.0.clone();
                match &s {
                    Term::NamedNode(n) => {
                        if named.as_ref().is_none_or(|m| n.as_str() < m.as_str()) {
                            named = Some(n.clone());
                        }
                    }
                    _ => {
                        if seen.insert(s.clone()) {
                            next.push(s);
                        }
                    }
                }
            }
        }
        if named.is_some() {
            return named;
        }
        level = next;
    }
    None
}

/// An XPath regular expression with `sh:flags`, as a Rust one.
fn regex(pattern: &str, flags: &str) -> Result<Regex, ShaclError> {
    let mut prefix = String::new();
    let mut quote = false;
    for f in flags.chars() {
        match f {
            'i' | 's' | 'm' | 'x' => {
                if prefix.is_empty() {
                    prefix.push_str("(?");
                }
                prefix.push(f);
            }
            'q' => quote = true,
            _ => return Err(bad(format!("unsupported sh:flags {flags:?}"))),
        }
    }
    if !prefix.is_empty() {
        prefix.push(')');
    }
    let body = if quote {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    Regex::new(&format!("{prefix}{body}")).map_err(|e| bad(format!("sh:pattern {pattern:?}: {e}")))
}
