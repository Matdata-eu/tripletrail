//! Checking focus nodes against compiled shapes.

use crate::ShaclError;
use crate::graph::DataGraph;
use crate::path::{Path, dedup};
use crate::report::{ShapeCount, ValidationResult, core_component, fill_messages, template_names};
use crate::shapes::{
    Component, KIND_BLANK, KIND_IRI, KIND_LITERAL, Shape, ShapeId, Shapes, Target,
};
use crate::sparql::{Bindings, Evaluator, QueryData, Runner, plain_evaluator, var};
use crate::value::{compare, has_datatype, lang_matches, string_of};
use crate::vocab::{rdf, rdfs};
use oxigraph::model::{NamedNode, Term};
use rustc_hash::{FxHashMap, FxHashSet};
use std::borrow::{Borrow, Cow};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::rc::Rc;

/// How deep shapes may refer to shapes (through `sh:node`, `sh:property`, ...) for one focus
/// node; deeper (recursive shapes) does not conform.
const MAX_DEPTH: usize = 64;

/// About how many results [`Validator::report`] gives as `sh:detail` of one focus node's
/// results against one targeted shape, at every depth together: once this many are found, no
/// more nested shapes are explained, though the results already being written are kept (a
/// wide `sh:or` over deep shapes would otherwise grow the report exponentially).
pub const MAX_DETAILS: usize = 1000;

/// Focus nodes (or focus and value node pairs) bound per SHACL-SPARQL query at most.
pub(crate) const SPARQL_BATCH: usize = 4096;

/// Checks focus nodes against [`Shapes`], reading a data graph. Keeps caches (subclass
/// lookups, SPARQL results and targets), so make one per thread and reuse it for many nodes.
///
/// SHACL-SPARQL constraints run one query per focus node when checked one by one; call
/// [`Validator::prefetch`] (or [`Validator::prefetch_for`]) with a batch of focus nodes first
/// to check them all with one query per constraint. A query that fails makes its focus nodes
/// fail; [`Validator::take_error`] tells why.
/// The results of failing focus nodes of SPARQL checks: `(sh:value, sh:resultPath)`.
/// Per SPARQL check and focus node, its results: `sh:value`, `sh:resultPath`, and the
/// bindings its messages are filled in from.
type SparqlResults = RefCell<FxHashMap<(u32, Term), Vec<SparqlRow>>>;
type SparqlRow = (Option<Term>, Option<Path>, Vec<(String, Term)>);

pub struct Validator<'a> {
    shapes: &'a Shapes,
    data: &'a dyn DataGraph,
    evaluator: &'a dyn Evaluator,
    shapes_graph: Option<(&'a NamedNode, &'a dyn DataGraph)>,
    /// With rules: the data graph is the evaluator's data graph and these inferred triples
    /// (see [`Evaluator::focus_nodes_native`]).
    inferred: Option<&'a [oxigraph::model::Triple]>,
    /// `(class, superclass)` -> whether the first is a subclass of the second.
    subclass: RefCell<FxHashMap<(Term, Term), bool>>,
    /// `(SPARQL check, focus node)` -> whether the focus node passes it.
    sparql: RefCell<FxHashMap<(u32, Term), bool>>,
    /// `(SPARQL check, focus node)` -> the results of a focus node that fails it
    /// (`sh:value`, `sh:resultPath`), kept from the query that checked it.
    sparql_results: SparqlResults,
    /// The focus nodes of SPARQL-based targets.
    targets: RefCell<FxHashMap<u32, Rc<FxHashSet<Term>>>>,
    error: RefCell<Option<ShaclError>>,
}

impl<'a> Validator<'a> {
    pub fn new(shapes: &'a Shapes, data: &'a dyn DataGraph) -> Self {
        static PLAIN: fn() -> oxigraph::sparql::SparqlEvaluator = plain_evaluator;
        Self {
            shapes,
            data,
            evaluator: &PLAIN,
            shapes_graph: None,
            inferred: None,
            subclass: RefCell::default(),
            sparql: RefCell::default(),
            sparql_results: RefCell::default(),
            targets: RefCell::default(),
            error: RefCell::default(),
        }
    }

    /// Runs SHACL-SPARQL queries with evaluators made by `evaluator` (with custom functions).
    pub fn with_evaluator(mut self, evaluator: &'a dyn Evaluator) -> Self {
        self.evaluator = evaluator;
        self
    }

    /// For rules: the data graph is the evaluator's data graph and these inferred triples, in
    /// the order they came, so SPARQL targets may run on the evaluator's own store.
    pub(crate) fn with_inferred(mut self, inferred: &'a [oxigraph::model::Triple]) -> Self {
        self.inferred = Some(inferred);
        self
    }

    /// The shapes graph and its IRI, which `$shapesGraph` names in SHACL-SPARQL queries.
    pub fn with_shapes_graph(mut self, name: &'a NamedNode, graph: &'a dyn DataGraph) -> Self {
        self.shapes_graph = Some((name, graph));
        self
    }

    /// The first error of a SHACL-SPARQL query since the last call, if any.
    pub fn take_error(&self) -> Option<ShaclError> {
        self.error.borrow_mut().take()
    }

    fn fail(&self, e: ShaclError) {
        let mut error = self.error.borrow_mut();
        if error.is_none() {
            *error = Some(e);
        }
    }

    fn runner(&self) -> Runner<'_> {
        Runner {
            data: QueryData {
                data: self.data,
                shapes: self.shapes_graph,
                delta: None,
            },
            evaluator: self.evaluator,
        }
    }

    /// Whether `focus` conforms to `shape`, whatever the shape's targets.
    pub fn conforms_to(&self, focus: &Term, shape: ShapeId) -> bool {
        self.check(focus, shape, 0)
    }

    /// Whether `focus` is a target of `shape`.
    pub fn is_target(&self, focus: &Term, shape: ShapeId) -> bool {
        self.shapes.get(shape).targets.iter().any(|t| match t {
            Target::Node(n) => n == focus,
            Target::Class(c) => self.instance_of(focus, c),
            Target::SubjectsOf(p) => self.data.has_object(focus, p.as_ref()),
            Target::ObjectsOf(p) => self.data.has_subject(p.as_ref(), focus),
            Target::Sparql(i) => self.sparql_targets(*i).contains(focus),
        })
    }

    /// The focus nodes of a SPARQL-based target (cached).
    fn sparql_targets(&self, i: u32) -> Rc<FxHashSet<Term>> {
        if let Some(set) = self.targets.borrow().get(&i) {
            return Rc::clone(set);
        }
        let q = &self.shapes.target_queries[i as usize];
        let native = self
            .inferred
            .and_then(|log| self.runner().focus_nodes_native(q, log));
        let found = native.unwrap_or_else(|| {
            self.runner()
                .select(q, Bindings::default())
                .map(|solutions| {
                    solutions
                        .iter()
                        .filter_map(|s| s.get("this").cloned())
                        .collect()
                })
        });
        let set: FxHashSet<Term> = match found {
            Ok(foci) => foci.into_iter().collect(),
            Err(e) => {
                self.fail(e);
                FxHashSet::default()
            }
        };
        let set = Rc::new(set);
        self.targets.borrow_mut().insert(i, Rc::clone(&set));
        set
    }

    /// Runs the SHACL-SPARQL constraints of every shape targeting some of `foci` on those
    /// focus nodes, with one query per constraint (per few thousand nodes), and keeps the
    /// results for [`Validator::conforms`]. Without SHACL-SPARQL it does nothing.
    pub fn prefetch<T: Borrow<Term>>(&self, foci: &[T]) {
        if self.shapes.checks.is_empty() {
            return;
        }
        for &s in self.shapes.targeted() {
            let targets: Vec<&Term> = foci
                .iter()
                .map(Borrow::borrow)
                .filter(|f| self.is_target(f, s))
                .collect();
            self.prefetch_for(s, &targets);
        }
    }

    /// Like [`Validator::prefetch`], for checking `foci` against `shape` (whatever its
    /// targets): runs the SHACL-SPARQL constraints of the shape, and of the shapes checked on
    /// the same focus nodes (its property shapes, `sh:node`, `sh:and`, ...).
    pub fn prefetch_for<T: Borrow<Term>>(&self, shape: ShapeId, foci: &[T]) {
        if self.shapes.checks.is_empty() || foci.is_empty() {
            return;
        }
        let mut shapes = vec![shape];
        let mut i = 0;
        while i < shapes.len() {
            let s = self.shapes.get(shapes[i]);
            i += 1;
            let node_shape = s.path.is_none();
            for c in &s.components {
                let next: &[ShapeId] = match c {
                    Component::Property(p) if node_shape => std::slice::from_ref(p),
                    Component::Node(p) | Component::Not(p) if node_shape => std::slice::from_ref(p),
                    Component::And(l) | Component::Or(l) | Component::Xone(l) if node_shape => l,
                    Component::Sparql(k) => {
                        let todo: Vec<&Term> = {
                            let done = self.sparql.borrow();
                            let mut todo: Vec<&Term> = foci
                                .iter()
                                .map(Borrow::borrow)
                                .filter(|f| !done.contains_key(&(*k, (*f).clone())))
                                .collect();
                            todo.dedup();
                            todo
                        };
                        self.run_check(*k, &todo);
                        &[]
                    }
                    _ => &[],
                };
                for n in next {
                    if !shapes.contains(n) {
                        shapes.push(*n);
                    }
                }
            }
        }
    }

    /// The variables the messages a result of `check` reports name (`{$x}`, `{?x}`): the
    /// shape's own `sh:message`, else the check's.
    fn wanted(&self, check: &crate::shapes::SparqlCheck) -> Vec<String> {
        let shape = self.shapes.get(check.shape);
        template_names(if shape.messages.is_empty() {
            &check.messages
        } else {
            &shape.messages
        })
    }

    /// Runs SPARQL check `k` on `foci` and keeps whether each passes.
    fn run_check(&self, k: u32, foci: &[&Term]) {
        if foci.is_empty() {
            return;
        }
        let check = &self.shapes.checks[k as usize];
        let wanted = self.wanted(check);
        let runner = self.runner();
        let mut results: Vec<(Term, bool)> = Vec::with_capacity(foci.len());
        {
            // Checked again (a focus node in two prefetches): its results start over.
            let mut rows = self.sparql_results.borrow_mut();
            for f in foci {
                rows.remove(&(k, (*f).clone()));
            }
        }
        if check.ask {
            // One row per focus and value node; the solutions are the pairs that conform.
            let path = self.shapes.get(check.shape).path.as_ref();
            let mut pairs: Vec<(Term, Term)> = Vec::new();
            for f in foci {
                match path {
                    Some(p) => pairs.extend(
                        p.values(self.data, f)
                            .into_iter()
                            .map(|v| ((*f).clone(), v)),
                    ),
                    None => pairs.push(((*f).clone(), (*f).clone())),
                }
            }
            let mut passing: FxHashSet<(Term, Term)> = FxHashSet::default();
            let mut failed: FxHashSet<Term> = FxHashSet::default();
            for chunk in pairs.chunks(SPARQL_BATCH) {
                let mut b = Bindings::new(vec![var("this"), var("value")]);
                b.rows = chunk
                    .iter()
                    .map(|(f, v)| vec![Some(f.clone()), Some(v.clone())])
                    .collect();
                match runner.select(&check.query, b) {
                    Ok(solutions) => {
                        passing.extend(solutions.iter().filter_map(|s| {
                            Some((s.get("this")?.clone(), s.get("value")?.clone()))
                        }))
                    }
                    Err(e) => {
                        failed.extend(chunk.iter().map(|(f, _)| f.clone()));
                        self.fail(e);
                    }
                }
            }
            let mut ok: FxHashMap<&Term, bool> = foci.iter().map(|f| (*f, true)).collect();
            let mut rows = self.sparql_results.borrow_mut();
            for (f, v) in &pairs {
                if failed.contains(f) || !passing.contains(&(f.clone(), v.clone())) {
                    ok.insert(f, false);
                    let bindings =
                        pick(&wanted, constants(check).chain([("this", f), ("value", v)]));
                    rows.entry((k, f.clone()))
                        .or_default()
                        .push((Some(v.clone()), None, bindings));
                }
            }
            drop(rows);
            results.extend(ok.into_iter().map(|(f, ok)| (f.clone(), ok)));
        } else {
            let mut variables = vec![var("this")];
            if check.bind_value {
                variables.push(var("value"));
            }
            let row = |f: &Term| {
                let mut r = vec![Some(f.clone())];
                if check.bind_value {
                    r.push(Some(f.clone()));
                }
                r
            };
            let size = if check.batch { SPARQL_BATCH } else { 1 };
            for chunk in foci.chunks(size) {
                let mut b = Bindings::new(variables.clone());
                b.rows = chunk.iter().map(|f| row(f)).collect();
                match runner.select(&check.query, b) {
                    Ok(solutions) => {
                        // Each solution is a result of the focus node it binds `$this` to
                        // (the only one, unbatched), kept for the report.
                        let shape = self.shapes.get(check.shape);
                        let mut violating: FxHashSet<Term> = FxHashSet::default();
                        let mut rows = self.sparql_results.borrow_mut();
                        for s in &solutions {
                            let focus = match s.get("this") {
                                Some(t) if check.batch => t,
                                _ => chunk[0],
                            };
                            let value = s
                                .get("value")
                                .cloned()
                                .or_else(|| shape.path.is_none().then(|| focus.clone()));
                            let path = match s.get("path") {
                                Some(Term::NamedNode(p)) => Some(Path::Predicate(p.clone())),
                                _ => None,
                            };
                            violating.insert(focus.clone());
                            let bindings = pick(
                                &wanted,
                                constants(check).chain(s.iter().map(|(v, t)| (v.as_str(), t))),
                            );
                            rows.entry((k, focus.clone()))
                                .or_default()
                                .push((value, path, bindings));
                        }
                        results.extend(
                            chunk
                                .iter()
                                .map(|f| ((*f).clone(), !violating.contains(*f))),
                        );
                    }
                    Err(e) => {
                        results.extend(chunk.iter().map(|f| ((*f).clone(), false)));
                        self.fail(e);
                    }
                }
            }
        }
        let mut cache = self.sparql.borrow_mut();
        for (f, ok) in results {
            cache.insert((k, f), ok);
        }
    }

    /// The results of SPARQL check `k` for `focus`, which fails it: `(sh:value, sh:resultPath)`
    /// per solution of a SELECT check (`?value`, or the focus node of a node shape; `?path`
    /// if it is an IRI, or else the shape's path), per failing value node of an ASK validator.
    /// They come from the query that checked the focus node; a focus node that failed
    /// without any (a query error) gets one result.
    fn sparql_results(&self, k: u32, focus: &Term) -> Vec<SparqlRow> {
        if !self.sparql.borrow().contains_key(&(k, focus.clone())) {
            self.run_check(k, &[focus]);
        }
        match self.sparql_results.borrow().get(&(k, focus.clone())) {
            Some(rows) if !rows.is_empty() => rows.clone(),
            _ => {
                let check = &self.shapes.checks[k as usize];
                let shape = self.shapes.get(check.shape);
                let bindings = pick(
                    &self.wanted(check),
                    constants(check).chain([("this", focus)]),
                );
                vec![(shape.path.is_none().then(|| focus.clone()), None, bindings)]
            }
        }
    }

    /// Whether `focus` passes SPARQL check `k`.
    fn sparql_check(&self, k: u32, focus: &Term) -> bool {
        let key = (k, focus.clone());
        if let Some(&ok) = self.sparql.borrow().get(&key) {
            return ok;
        }
        self.run_check(k, &[focus]);
        self.sparql.borrow().get(&key).copied().unwrap_or(false)
    }

    /// Whether `focus` conforms to every shape that targets it.
    pub fn conforms(&self, focus: &Term) -> bool {
        self.shapes
            .targeted()
            .iter()
            .all(|&s| !self.is_target(focus, s) || self.check(focus, s, 0))
    }

    /// The focus nodes of a shape's targets in the data graph.
    pub fn focus_nodes(&self, shape: ShapeId) -> Vec<Term> {
        let mut out = Vec::new();
        for t in &self.shapes.get(shape).targets {
            match t {
                Target::Node(n) => out.push(n.clone()),
                Target::Class(c) => {
                    for class in self.subclasses(c) {
                        self.data.subjects(rdf::TYPE, &class, &mut out);
                    }
                }
                Target::SubjectsOf(p) => {
                    let mut pairs = Vec::new();
                    self.data.with_predicate(p.as_ref(), &mut pairs);
                    out.extend(pairs.into_iter().map(|(s, _)| s));
                }
                Target::ObjectsOf(p) => {
                    let mut pairs = Vec::new();
                    self.data.with_predicate(p.as_ref(), &mut pairs);
                    out.extend(pairs.into_iter().map(|(_, o)| o));
                }
                Target::Sparql(i) => out.extend(self.sparql_targets(*i).iter().cloned()),
            }
        }
        dedup(&mut out);
        out
    }

    /// Validates the whole data graph: every focus node of every targeted shape. Returns the
    /// `(focus node, shape)` pairs that do not conform; empty when the graph conforms.
    pub fn validate(&self) -> Vec<(Term, ShapeId)> {
        let mut out = Vec::new();
        for &s in self.shapes.targeted() {
            let foci = self.focus_nodes(s);
            self.prefetch_for(s, &foci);
            for focus in foci {
                if !self.check(&focus, s, 0) {
                    out.push((focus, s));
                }
            }
        }
        out
    }

    /// Validates the whole data graph as [`Validator::validate`] does, and gives why each
    /// focus node fails: one [`ValidationResult`] per failing constraint (per failing value
    /// node, for constraints on each value), as they are found. Results of property shapes
    /// name the property shape as `sh:sourceShape`; failures of shapes reached through
    /// `sh:node`, `sh:not`, `sh:and`, `sh:or`, `sh:xone` or `sh:qualifiedValueShape` are one
    /// result of that constraint, whose [`ValidationResult::details`] (`sh:detail`) are the
    /// results of the value node against the nested shapes it fails: the shape of `sh:node`,
    /// the members of `sh:and`, `sh:or` and `sh:xone`, and for `sh:qualifiedMinCount` the
    /// qualified shape (`sh:not` has none, its shape conforms). At most [`MAX_DETAILS`] per
    /// focus node and targeted shape.
    pub fn report(&self, emit: &mut dyn FnMut(ValidationResult<'a>)) {
        for &s in self.shapes.targeted() {
            let foci = self.focus_nodes(s);
            self.prefetch_for(s, &foci);
            for focus in foci {
                if !self.check(&focus, s, 0) {
                    self.explain(&focus, s, 0, &Cell::new(MAX_DETAILS), emit);
                }
            }
        }
    }

    /// Why `focus` fails `shape`, as [`Validator::report`] gives it, whatever the shape's
    /// targets; nothing when it conforms.
    pub fn report_focus(
        &self,
        focus: &Term,
        shape: ShapeId,
        emit: &mut dyn FnMut(ValidationResult<'a>),
    ) {
        if !self.check(focus, shape, 0) {
            self.explain(focus, shape, 0, &Cell::new(MAX_DETAILS), emit);
        }
    }

    /// How many nodes each shape was checked against in [`Validator::report`], for every shape
    /// reached (including those that conform) and every targeted shape (with 0 focus nodes
    /// if its targets are empty), in the order of the shapes graph: a targeted
    /// shape is checked against its focus nodes; a shape reached through `sh:property`,
    /// `sh:node`, `sh:not`, `sh:and`, `sh:or`, `sh:xone` or `sh:qualifiedValueShape` against
    /// the value nodes of its parent, once per parent focus node (so a node reached from two
    /// parents counts twice). The value nodes of a property shape are those of its path; a
    /// node shape's value node is its focus node.
    pub fn shape_counts(&self) -> Vec<ShapeCount> {
        let mut counts = vec![(0usize, 0usize); self.shapes.len()];
        for &s in self.shapes.targeted() {
            for focus in self.focus_nodes(s) {
                self.count(&focus, s, 0, &mut counts);
            }
        }
        let shapes = self.shapes;
        let targeted: FxHashSet<u32> = shapes.targeted().iter().map(|s| s.0).collect();
        counts
            .into_iter()
            .enumerate()
            .filter(|(i, (f, _))| *f > 0 || targeted.contains(&(*i as u32)))
            .map(|(i, (focus_nodes, value_nodes))| {
                let s = shapes.get(ShapeId(i as u32));
                ShapeCount {
                    shape: s.node.clone(),
                    property: s.path.is_some(),
                    focus_nodes,
                    value_nodes,
                }
            })
            .collect()
    }

    fn count(&self, focus: &Term, shape: ShapeId, depth: usize, counts: &mut [(usize, usize)]) {
        if depth > MAX_DEPTH {
            return;
        }
        let s = self.shapes.get(shape);
        if s.deactivated {
            return;
        }
        let values = match &s.path {
            Some(p) => p.values(self.data, focus),
            None => vec![focus.clone()],
        };
        let c = &mut counts[shape.0 as usize];
        c.0 += 1;
        c.1 += values.len();
        for c in &s.components {
            let nested: &[ShapeId] = match c {
                Component::Property(p) | Component::Node(p) | Component::Not(p) => {
                    std::slice::from_ref(p)
                }
                Component::Qualified { shape, .. } => std::slice::from_ref(shape),
                Component::And(list) | Component::Or(list) | Component::Xone(list) => list,
                _ => continue,
            };
            for &n in nested {
                for v in &values {
                    self.count(v, n, depth + 1, counts);
                }
            }
        }
    }

    /// The results of `focus` against `shape`, which it fails, `depth` shapes down; their
    /// details take from `budget`.
    fn explain(
        &self,
        focus: &Term,
        shape: ShapeId,
        depth: usize,
        budget: &Cell<usize>,
        emit: &mut dyn FnMut(ValidationResult<'a>),
    ) {
        let shapes: &'a Shapes = self.shapes;
        let s: &'a Shape = shapes.get(shape);
        if s.deactivated || depth > MAX_DEPTH {
            return;
        }
        let values = match &s.path {
            Some(p) => p.values(self.data, focus),
            None => vec![focus.clone()],
        };
        let result =
            |value: Option<Term>, component: Term, messages: &'a [Term]| ValidationResult {
                focus_node: focus.clone(),
                result_path: s.path.as_ref().map(Cow::Borrowed),
                value,
                source_shape: &s.node,
                source_constraint_component: component,
                source_constraint: None,
                severity: &s.severity,
                messages: Cow::Borrowed(if s.messages.is_empty() {
                    messages
                } else {
                    &s.messages
                }),
                details: Vec::new(),
            };
        for c in &s.components {
            if let Component::Property(p) = c {
                for v in &values {
                    if !self.check(v, *p, depth + 1) {
                        self.explain(v, *p, depth + 1, budget, emit);
                    }
                }
                continue;
            }
            if self.component(c, focus, &values, depth) {
                continue;
            }
            // Components whose results name other value nodes than "each value that fails".
            match c {
                Component::HasValue(_) => {
                    emit(result(None, core_component("HasValue"), &[]));
                    continue;
                }
                Component::UniqueLang => {
                    // One result per language tag used more than once.
                    let mut seen: FxHashMap<String, usize> = FxHashMap::default();
                    for v in &values {
                        if let Term::Literal(l) = v
                            && let Some(tag) = l.language()
                        {
                            *seen.entry(tag.to_ascii_lowercase()).or_default() += 1;
                        }
                    }
                    for _ in seen.values().filter(|&&n| n > 1) {
                        emit(result(None, core_component("UniqueLang"), &[]));
                    }
                    continue;
                }
                Component::Equals(p) => {
                    // Each value node that is not a value of `p`, and each value of `p`
                    // that is not a value node.
                    let others = self.objects(focus, p);
                    let component = core_component("Equals");
                    for v in values.iter().filter(|v| !others.contains(v)) {
                        emit(result(Some(v.clone()), component.clone(), &[]));
                    }
                    for o in others.iter().filter(|o| !values.contains(o)) {
                        emit(result(Some(o.clone()), component.clone(), &[]));
                    }
                    continue;
                }
                Component::LessThan(p) | Component::LessThanOrEquals(p) => {
                    // One result per value node and value of `p` that are not in order.
                    let strict = matches!(c, Component::LessThan(_));
                    let component = core_component(if strict {
                        "LessThan"
                    } else {
                        "LessThanOrEquals"
                    });
                    let others = self.objects(focus, p);
                    for v in &values {
                        for o in &others {
                            let ok = match compare(v, o) {
                                Some(Ordering::Less) => true,
                                Some(Ordering::Equal) => !strict,
                                _ => false,
                            };
                            if !ok {
                                emit(result(Some(v.clone()), component.clone(), &[]));
                            }
                        }
                    }
                    continue;
                }
                Component::Closed { allowed } => {
                    // One result per statement of a property that is not allowed, with the
                    // property as path and its object as value.
                    let component = core_component("Closed");
                    for v in &values {
                        let mut out = Vec::new();
                        self.data.outgoing(v, &mut out);
                        for (p, o) in out.into_iter().filter(|(p, _)| !allowed.contains(p)) {
                            emit(ValidationResult {
                                focus_node: focus.clone(),
                                result_path: Some(Cow::Owned(Path::Predicate(p))),
                                value: Some(o),
                                source_shape: &s.node,
                                source_constraint_component: component.clone(),
                                source_constraint: None,
                                severity: &s.severity,
                                messages: Cow::Borrowed(&s.messages),
                                details: Vec::new(),
                            });
                        }
                    }
                    continue;
                }
                _ => {}
            }
            let (local, per_value) = match c {
                Component::MinCount(_) => ("MinCount", false),
                Component::MaxCount(_) => ("MaxCount", false),
                Component::HasValue(_) => ("HasValue", false),
                Component::UniqueLang => ("UniqueLang", false),
                Component::Equals(_) => ("Equals", false),
                Component::Qualified {
                    shape,
                    min,
                    siblings,
                    ..
                } => {
                    let count = self.qualified_count(*shape, siblings, &values, depth);
                    if min.is_some_and(|m| count < m) {
                        // Too few conform: why the others do not.
                        let value = s.path.is_none().then(|| focus.clone());
                        let mut r = result(value, core_component("QualifiedMinCount"), &[]);
                        for v in &values {
                            if budget.get() > 0 && !self.check(v, *shape, depth + 1) {
                                self.details(v, *shape, depth + 1, budget, &mut r.details);
                            }
                        }
                        emit(r);
                        continue;
                    } else {
                        ("QualifiedMaxCount", false)
                    }
                }
                Component::In(_) => ("In", true),
                Component::NodeKind(_) => ("NodeKind", true),
                Component::Datatype(_) => ("Datatype", true),
                Component::MinExclusive(_) => ("MinExclusive", true),
                Component::MinInclusive(_) => ("MinInclusive", true),
                Component::MaxExclusive(_) => ("MaxExclusive", true),
                Component::MaxInclusive(_) => ("MaxInclusive", true),
                Component::MinLength(_) => ("MinLength", true),
                Component::MaxLength(_) => ("MaxLength", true),
                Component::Pattern(_) => ("Pattern", true),
                Component::LanguageIn(_) => ("LanguageIn", true),
                Component::Class(_) => ("Class", true),
                Component::Disjoint(_) => ("Disjoint", true),
                Component::LessThan(_) => ("LessThan", true),
                Component::LessThanOrEquals(_) => ("LessThanOrEquals", true),
                Component::Closed { .. } => ("Closed", true),
                Component::Not(_) => ("Not", true),
                Component::And(_) => ("And", true),
                Component::Or(_) => ("Or", true),
                Component::Xone(_) => ("Xone", true),
                Component::Node(_) => ("Node", true),
                Component::Property(_) => unreachable!("handled above"),
                Component::Sparql(k) => {
                    let check = &shapes.checks[*k as usize];
                    for (value, path, bindings) in self.sparql_results(*k, focus) {
                        let mut r = result(value, check.source.clone(), &check.messages);
                        if let Cow::Borrowed(messages) = r.messages {
                            r.messages = fill_messages(messages, &bindings);
                        }
                        if let Some(path) = path {
                            r.result_path = Some(Cow::Owned(path));
                        }
                        r.source_constraint = check.constraint.clone();
                        emit(r);
                    }
                    continue;
                }
            };
            let component = core_component(local);
            if per_value {
                for v in &values {
                    if !self.component(c, focus, std::slice::from_ref(v), depth) {
                        let mut r = result(Some(v.clone()), component.clone(), &[]);
                        let nested: &[ShapeId] = match c {
                            Component::Node(n) => std::slice::from_ref(n),
                            Component::And(l) | Component::Or(l) | Component::Xone(l) => l,
                            _ => &[],
                        };
                        for &n in nested {
                            if budget.get() > 0 && !self.check(v, n, depth + 1) {
                                self.details(v, n, depth + 1, budget, &mut r.details);
                            }
                        }
                        emit(r);
                    }
                }
            } else {
                let value = s.path.is_none().then(|| focus.clone());
                emit(result(value, component, &[]));
            }
        }
    }

    /// Adds to `out` the results of `value` against `shape`, which it fails, as long as
    /// `budget` lasts.
    fn details(
        &self,
        value: &Term,
        shape: ShapeId,
        depth: usize,
        budget: &Cell<usize>,
        out: &mut Vec<ValidationResult<'a>>,
    ) {
        if budget.get() == 0 {
            return;
        }
        // A result is kept even when its own details used up the budget, so that every level
        // says what failed; only details not yet looked for are left out.
        self.explain(value, shape, depth, budget, &mut |r| {
            budget.set(budget.get().saturating_sub(1));
            out.push(r);
        });
    }

    /// `c` and its subclasses, transitively.
    fn subclasses(&self, c: &Term) -> Vec<Term> {
        let mut out = vec![c.clone()];
        let mut seen: FxHashSet<Term> = out.iter().cloned().collect();
        let mut i = 0;
        let mut next = Vec::new();
        while i < out.len() {
            next.clear();
            self.data.subjects(rdfs::SUB_CLASS_OF, &out[i], &mut next);
            for n in next.drain(..) {
                if seen.insert(n.clone()) {
                    out.push(n);
                }
            }
            i += 1;
        }
        out
    }

    /// Whether `node` is a SHACL instance of `class` (`rdf:type/rdfs:subClassOf*`).
    fn instance_of(&self, node: &Term, class: &Term) -> bool {
        if matches!(node, Term::Literal(_)) {
            return false;
        }
        let mut types = Vec::new();
        self.data.objects(node, rdf::TYPE, &mut types);
        types
            .iter()
            .any(|t| t == class || self.is_subclass(t, class))
    }

    fn is_subclass(&self, t: &Term, class: &Term) -> bool {
        let key = (t.clone(), class.clone());
        if let Some(&known) = self.subclass.borrow().get(&key) {
            return known;
        }
        let mut seen: FxHashSet<Term> = FxHashSet::default();
        let mut frontier = vec![t.clone()];
        let mut found = false;
        let mut next = Vec::new();
        'search: while !frontier.is_empty() {
            next.clear();
            for n in &frontier {
                self.data.objects(n, rdfs::SUB_CLASS_OF, &mut next);
            }
            frontier.clear();
            for n in next.drain(..) {
                if &n == class {
                    found = true;
                    break 'search;
                }
                if seen.insert(n.clone()) {
                    frontier.push(n);
                }
            }
        }
        self.subclass.borrow_mut().insert(key, found);
        found
    }

    fn check(&self, focus: &Term, shape: ShapeId, depth: usize) -> bool {
        if depth > MAX_DEPTH {
            return false;
        }
        let s = self.shapes.get(shape);
        if s.deactivated {
            return true;
        }
        let values = match &s.path {
            Some(p) => p.values(self.data, focus),
            None => vec![focus.clone()],
        };
        s.components
            .iter()
            .all(|c| self.component(c, focus, &values, depth))
    }

    /// The value nodes that conform to a qualified value shape and to none of its siblings.
    fn qualified_count(
        &self,
        shape: ShapeId,
        siblings: &[ShapeId],
        values: &[Term],
        depth: usize,
    ) -> usize {
        let conforms = |v: &Term, s: ShapeId| self.check(v, s, depth + 1);
        values
            .iter()
            .filter(|v| conforms(v, shape) && !siblings.iter().any(|s| conforms(v, *s)))
            .count()
    }

    fn objects(&self, focus: &Term, p: &oxigraph::model::NamedNode) -> Vec<Term> {
        let mut out = Vec::new();
        self.data.objects(focus, p.as_ref(), &mut out);
        dedup(&mut out);
        out
    }

    fn component(&self, c: &Component, focus: &Term, values: &[Term], depth: usize) -> bool {
        let range = |bound: &Term, ok: fn(Ordering) -> bool| {
            values.iter().all(|v| compare(v, bound).is_some_and(ok))
        };
        let conforms = |v: &Term, s: ShapeId| self.check(v, s, depth + 1);
        match c {
            Component::MinCount(n) => values.len() >= *n,
            Component::MaxCount(n) => values.len() <= *n,
            Component::HasValue(t) => values.contains(t),
            Component::In(set) => values.iter().all(|v| set.contains(v)),
            Component::NodeKind(mask) => values.iter().all(|v| {
                let kind = match v {
                    Term::NamedNode(_) => KIND_IRI,
                    Term::BlankNode(_) => KIND_BLANK,
                    Term::Literal(_) => KIND_LITERAL,
                    #[allow(unreachable_patterns)]
                    _ => 0,
                };
                kind & mask != 0
            }),
            Component::Datatype(dt) => values
                .iter()
                .all(|v| matches!(v, Term::Literal(l) if has_datatype(l, dt.as_str()))),
            Component::MinExclusive(b) => range(b, |o| o == Ordering::Greater),
            Component::MinInclusive(b) => range(b, |o| o != Ordering::Less),
            Component::MaxExclusive(b) => range(b, |o| o == Ordering::Less),
            Component::MaxInclusive(b) => range(b, |o| o != Ordering::Greater),
            Component::MinLength(n) => values
                .iter()
                .all(|v| string_of(v).is_some_and(|s| s.chars().count() >= *n)),
            Component::MaxLength(n) => values
                .iter()
                .all(|v| string_of(v).is_some_and(|s| s.chars().count() <= *n)),
            Component::Pattern(re) => values
                .iter()
                .all(|v| string_of(v).is_some_and(|s| re.is_match(s))),
            Component::LanguageIn(ranges) => values.iter().all(|v| match v {
                Term::Literal(l) => l
                    .language()
                    .is_some_and(|tag| ranges.iter().any(|r| lang_matches(tag, r))),
                _ => false,
            }),
            Component::UniqueLang => {
                let mut seen = FxHashSet::default();
                values.iter().all(|v| match v {
                    Term::Literal(l) => l
                        .language()
                        .is_none_or(|tag| seen.insert(tag.to_ascii_lowercase())),
                    _ => true,
                })
            }
            Component::Class(class) => values.iter().all(|v| self.instance_of(v, class)),
            Component::Equals(p) => {
                let others = self.objects(focus, p);
                others.len() == values.len() && values.iter().all(|v| others.contains(v))
            }
            Component::Disjoint(p) => {
                let others = self.objects(focus, p);
                values.iter().all(|v| !others.contains(v))
            }
            Component::LessThan(p) | Component::LessThanOrEquals(p) => {
                let strict = matches!(c, Component::LessThan(_));
                let others = self.objects(focus, p);
                values.iter().all(|v| {
                    others.iter().all(|o| match compare(v, o) {
                        Some(Ordering::Less) => true,
                        Some(Ordering::Equal) => !strict,
                        _ => false,
                    })
                })
            }
            Component::Closed { allowed } => values.iter().all(|v| {
                let mut out = Vec::new();
                self.data.outgoing(v, &mut out);
                out.iter().all(|(p, _)| allowed.contains(p))
            }),
            Component::Not(s) => values.iter().all(|v| !conforms(v, *s)),
            Component::And(list) => values.iter().all(|v| list.iter().all(|s| conforms(v, *s))),
            Component::Or(list) => values.iter().all(|v| list.iter().any(|s| conforms(v, *s))),
            Component::Xone(list) => values
                .iter()
                .all(|v| list.iter().filter(|s| conforms(v, **s)).count() == 1),
            Component::Node(s) | Component::Property(s) => values.iter().all(|v| conforms(v, *s)),
            Component::Qualified {
                shape,
                min,
                max,
                siblings,
                ..
            } => {
                let count = self.qualified_count(*shape, siblings, values, depth);
                min.is_none_or(|m| count >= m) && max.is_none_or(|m| count <= m)
            }
            Component::Sparql(k) => self.sparql_check(*k, focus),
        }
    }
}

/// The constants a SPARQL check's query is run with (a component's parameters and
/// `$currentShape`), which messages may name too.
fn constants(check: &crate::shapes::SparqlCheck) -> impl Iterator<Item = (&str, &Term)> {
    check.query.constants.iter().map(|(v, t)| (v.as_str(), t))
}

/// The bindings of a result that its messages name: none when no message has a template,
/// so a check with plain messages keeps nothing per failing row.
fn pick<'t>(
    wanted: &[String],
    bindings: impl Iterator<Item = (&'t str, &'t Term)>,
) -> Vec<(String, Term)> {
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut kept: Vec<(String, Term)> = Vec::new();
    for (name, term) in bindings {
        if wanted.iter().any(|w| w == name) && !kept.iter().any(|(n, _)| n == name) {
            kept.push((name.to_owned(), term.clone()));
        }
    }
    kept
}
