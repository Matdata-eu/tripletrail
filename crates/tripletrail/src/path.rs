//! `SERVICE <http://w3id.org/tripletrail/path>`: unweighted path search over any predicates
//! (#234), returning the paths as rows.
//!
//! ```sparql
//! PREFIX path: <http://w3id.org/tripletrail/path#>
//! SERVICE <http://w3id.org/tripletrail/path> {
//!   [] path:source ex:a ; path:target ex:e ; path:predicate ex:next ;
//!      path:path ?p ; path:step ?i ; path:from ?from ; path:edge ?edge ; path:to ?to .
//! }
//! ```
//!
//! Parameters:
//!
//! - `path:source` and `path:target`: where paths start and end, IRIs or literals, each given
//!   once or more (or as a list). Without a target every node reached is an end; without a
//!   source the search runs backwards from the targets. One of them is needed.
//! - The edges: `path:predicate` (once or more, or a list), followed from subject to object;
//!   or a sub-select whose first two variables are an edge's ends and an optional third its
//!   label, as `pgr:` takes its edges; or neither, for every triple from an IRI to an IRI,
//!   read from the indexes as the search goes (#819), less those of the predicates of
//!   `path:exceptPredicate` (once or more, or a list). From one source to one target, the
//!   shortest path over every predicate is searched from both ends at once, so it costs the
//!   neighbourhoods of the two ends rather than the size of the data.
//! - `path:direction "out"` (the default), `"in"` (object to subject) or `"both"`.
//! - `path:graph <g>`: search the edges of one named graph instead of the default graph
//!   (all graphs when the engine reads the default graph as their union).
//! - `path:minLength` (default 0) and `path:maxLength` (default unlimited), in edges.
//! - `path:mode "shortest"` (the default): one shortest path from each start to each end it
//!   reaches, the first found breadth first. `"simple"`: every path that visits no node
//!   twice. `"all"`: every walk, nodes repeated, which needs `path:maxLength`.
//! - `path:maxPaths n`: at most `n` paths from each start to each end (`simple` and `all`).
//!
//! Outputs, each a variable: `path:path` (the path's number, from 1), `path:start`,
//! `path:end`, `path:length` (in edges), and per edge `path:step` (from 1), `path:from`,
//! `path:edge` and `path:to`, in the direction walked. With any of the per-edge outputs there
//! is a row for each edge of each path (a path of no edges gives one row, the per-edge
//! outputs unbound); without them a row for each path. Hop counts, as Virtuoso's
//! `t_step('step_no')`, are `path:end ?n ; path:length ?d` from a source in shortest mode.

use crate::window::{Value, collect, parameters, projected};
use oxigraph::model::vocab::xsd;
use oxigraph::model::{GraphName, GraphNameRef, Literal, NamedNode, NamedOrBlankNodeRef, Term};
use oxigraph::sparql::{
    CancellationToken, QueryEvaluationError, QueryResults, QuerySolution, QuerySolutionIter,
    ServiceHandler, SparqlEvaluator, Variable,
};
use oxiri::Iri;
use spargebra::algebra::GraphPattern;
use spargebra::term::TermPattern;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tripletrail_store::Dataset;

pub const PATH_SERVICE: &str = "http://w3id.org/tripletrail/path";
const PATH: &str = "http://w3id.org/tripletrail/path#";
/// The most edges, over all paths, one call may return before it is refused.
const MAX_EDGES: usize = 1_000_000;

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("path: {0}")]
    Request(String),
    #[error("path: reading the edges failed: {0}")]
    Input(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("path: the query was cancelled")]
    Cancelled,
}

fn bad(msg: impl Into<String>) -> PathError {
    PathError::Request(msg.into())
}

/// The handler of the path SERVICE.
#[derive(Clone)]
pub struct PathService {
    store: Dataset,
    evaluator: fn() -> SparqlEvaluator,
    union_default_graph: bool,
    cancellation: Option<CancellationToken>,
    /// The graphs the search reads, for a caller who reads only some graphs (#203): the
    /// calling query's dataset. `None` reads the store as configured.
    dataset: Option<oxigraph::sparql::QueryDataset>,
}

impl PathService {
    pub fn new(
        store: Dataset,
        evaluator: fn() -> SparqlEvaluator,
        union_default_graph: bool,
        cancellation: Option<CancellationToken>,
    ) -> Self {
        Self {
            store,
            evaluator,
            union_default_graph,
            cancellation,
            dataset: None,
        }
    }

    /// The same service, reading only the graphs of `dataset` (#203): its default graphs for
    /// the edges without `path:graph`, one of its named graphs with it, and `dataset` for
    /// the edge sub-select.
    #[must_use]
    pub fn with_dataset(self, dataset: oxigraph::sparql::QueryDataset) -> Self {
        Self {
            dataset: Some(dataset),
            ..self
        }
    }

    /// An evaluator for the edge sub-select, with the calling query's token.
    fn sub_select_evaluator(&self) -> SparqlEvaluator {
        match &self.cancellation {
            Some(token) => (self.evaluator)().with_cancellation_token(token.clone()),
            None => (self.evaluator)(),
        }
    }

    fn cancelled(&self) -> bool {
        self.cancellation.as_ref().is_some_and(|t| t.is_cancelled())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Shortest,
    Simple,
    All,
}

#[derive(Clone, Copy, PartialEq)]
enum Direction {
    Out,
    In,
    Both,
}

impl Direction {
    fn reversed(self) -> Self {
        match self {
            Self::Out => Self::In,
            Self::In => Self::Out,
            Self::Both => Self::Both,
        }
    }
}

/// An edge walked: from, label, to.
type Step = (Term, Term, Term);

/// A node's edges: (label, neighbour) pairs.
type Next = Arc<[(Term, Term)]>;

/// The paths found so far.
#[derive(Default)]
struct Found {
    paths: Vec<Vec<Step>>,
    /// The starts of paths of no edges.
    empty: Vec<Term>,
    /// The edges of all the paths.
    edges: usize,
}

impl Found {
    fn push(&mut self, path: Vec<Step>) -> Result<(), PathError> {
        self.edges += path.len();
        if self.edges > MAX_EDGES {
            return Err(bad(format!(
                "the paths have more than {MAX_EDGES} edges: \
                 set a path:target, path:maxLength or path:maxPaths"
            )));
        }
        self.paths.push(path);
        Ok(())
    }
}

/// The edges, read from the store as the search reaches nodes, or all from a sub-select.
struct Edges<'a> {
    service: &'a PathService,
    /// The predicates followed; none: every predicate but `except`, between IRIs.
    predicates: Vec<NamedNode>,
    except: HashSet<NamedNode>,
    graph: Option<GraphName>,
    direction: Direction,
    /// Each node's (label, neighbour) pairs, as read.
    next: HashMap<Term, Next>,
    /// Whether `next` holds every edge (from a sub-select).
    complete: bool,
}

impl Edges<'_> {
    fn of(&mut self, node: &Term) -> Result<Next, PathError> {
        if let Some(n) = self.next.get(node) {
            return Ok(Arc::clone(n));
        }
        if self.complete {
            return Ok(Arc::from([]));
        }
        // The graphs to read: under a caller's dataset, its default graphs (or the named
        // graph asked for, if it is one of its own); else as the store reads.
        let scoped: Option<Vec<GraphName>> =
            self.service.dataset.as_ref().map(|d| match &self.graph {
                Some(GraphName::NamedNode(g)) => d
                    .available_named_graphs()
                    .is_some_and(|named| named.iter().any(|n| n.as_ref() == g.as_ref().into()))
                    .then(|| GraphName::NamedNode(g.clone()))
                    .into_iter()
                    .collect(),
                Some(_) => Vec::new(),
                None => d
                    .default_graph_graphs()
                    .map(<[_]>::to_vec)
                    .unwrap_or_default(),
            });
        let graphs: Vec<Option<GraphNameRef<'_>>> = match &scoped {
            Some(graphs) => graphs.iter().map(|g| Some(g.as_ref())).collect(),
            None => vec![match self.graph.as_ref().map(GraphName::as_ref) {
                Some(g) => Some(g),
                None if self.service.union_default_graph => None,
                None => Some(GraphNameRef::DefaultGraph),
            }],
        };
        let mut out: Vec<(Term, Term)> = Vec::new();
        for graph in graphs {
            self.read(node, graph, &mut out)?;
        }
        // The same triple in several graphs (a union default graph) is one edge.
        let mut seen = HashSet::new();
        out.retain(|e| seen.insert(e.clone()));
        let out: Next = out.into();
        self.next.insert(node.clone(), Arc::clone(&out));
        Ok(out)
    }

    /// The edges of `node` in one graph (`None`: every graph).
    fn read(
        &self,
        node: &Term,
        graph: Option<GraphNameRef<'_>>,
        out: &mut Vec<(Term, Term)>,
    ) -> Result<(), PathError> {
        if self.predicates.is_empty() {
            self.any_predicate(node, graph, out)?;
        }
        for p in &self.predicates {
            if self.direction != Direction::In {
                let subject: Option<NamedOrBlankNodeRef<'_>> = match node {
                    Term::NamedNode(n) => Some(n.into()),
                    Term::BlankNode(b) => Some(b.into()),
                    _ => None,
                };
                if let Some(s) = subject {
                    for q in
                        self.service
                            .store
                            .quads_for_pattern(Some(s), Some(p.as_ref()), None, graph)
                    {
                        let q = q.map_err(|e| PathError::Input(Box::new(e)))?;
                        out.push((p.clone().into(), q.object));
                    }
                }
            }
            if self.direction != Direction::Out {
                for q in self.service.store.quads_for_pattern(
                    None,
                    Some(p.as_ref()),
                    Some(node.as_ref()),
                    graph,
                ) {
                    let q = q.map_err(|e| PathError::Input(Box::new(e)))?;
                    out.push((p.clone().into(), q.subject.into()));
                }
            }
        }
        Ok(())
    }
}

impl Edges<'_> {
    /// The links of `node` to IRIs over every predicate but the excepted ones.
    fn any_predicate(
        &self,
        node: &Term,
        graph: Option<GraphNameRef<'_>>,
        out: &mut Vec<(Term, Term)>,
    ) -> Result<(), PathError> {
        let Term::NamedNode(n) = node else {
            return Ok(());
        };
        let store = &self.service.store;
        if self.direction != Direction::In {
            for q in store.quads_for_pattern(Some(n.as_ref().into()), None, None, graph) {
                let q = q.map_err(|e| PathError::Input(Box::new(e)))?;
                if q.object.is_named_node() && !self.except.contains(&q.predicate) {
                    out.push((q.predicate.into(), q.object));
                }
            }
        }
        if self.direction != Direction::Out {
            for q in store.quads_for_pattern(None, None, Some(node.as_ref()), graph) {
                let q = q.map_err(|e| PathError::Input(Box::new(e)))?;
                if q.subject.is_named_node() && !self.except.contains(&q.predicate) {
                    out.push((q.predicate.into(), q.subject.into()));
                }
            }
        }
        Ok(())
    }
}

/// The search's settings.
struct Search {
    mode: Mode,
    min: usize,
    max: Option<usize>,
    max_paths: Option<usize>,
}

impl ServiceHandler for PathService {
    type Error = PathError;

    fn handle(
        &self,
        pattern: &GraphPattern,
        _base_iri: Option<&Iri<String>>,
    ) -> Result<QuerySolutionIter<'static>, PathError> {
        let mut triples = Vec::new();
        let mut subselects = Vec::new();
        collect(pattern, &mut triples, &mut subselects, "path").map_err(PathError::Request)?;
        let params = parameters(&triples, PATH, "path").map_err(PathError::Request)?;

        let mut sources = Vec::new();
        let mut targets = Vec::new();
        let mut predicates = Vec::new();
        let mut except = HashSet::new();
        let mut direction = Direction::Out;
        let mut graph = None;
        let mut search = Search {
            mode: Mode::Shortest,
            min: 0,
            max: None,
            max_paths: None,
        };
        let mut outputs: Vec<(Output, Variable)> = Vec::new();
        for (name, value) in &params {
            let items = match value {
                Value::One(t) => std::slice::from_ref(t),
                Value::List(items) => items.as_slice(),
            };
            let one = || -> Result<&TermPattern, PathError> {
                match items {
                    [t] => Ok(t),
                    _ => Err(bad(format!("path:{name} takes one value"))),
                }
            };
            let text = || -> Result<String, PathError> {
                match one()? {
                    TermPattern::Literal(l) => Ok(l.value().to_owned()),
                    _ => Err(bad(format!("path:{name} takes a literal"))),
                }
            };
            let count = || -> Result<usize, PathError> {
                text()?
                    .parse()
                    .map_err(|_| bad(format!("path:{name} takes a whole number")))
            };
            match name.as_str() {
                "source" | "target" => {
                    for t in items {
                        let term: Term = match t {
                            TermPattern::NamedNode(n) => n.clone().into(),
                            TermPattern::Literal(l) => l.clone().into(),
                            _ => {
                                return Err(bad(format!(
                                    "path:{name} takes IRIs or literals, not {t}"
                                )));
                            }
                        };
                        if name == "source" {
                            sources.push(term);
                        } else {
                            targets.push(term);
                        }
                    }
                }
                "predicate" => {
                    for t in items {
                        let TermPattern::NamedNode(n) = t else {
                            return Err(bad("path:predicate takes IRIs"));
                        };
                        predicates.push(n.clone());
                    }
                }
                "exceptPredicate" => {
                    for t in items {
                        let TermPattern::NamedNode(n) = t else {
                            return Err(bad("path:exceptPredicate takes IRIs"));
                        };
                        except.insert(n.clone());
                    }
                }
                "direction" => {
                    direction = match text()?.as_str() {
                        "out" => Direction::Out,
                        "in" => Direction::In,
                        "both" => Direction::Both,
                        other => {
                            return Err(bad(format!(
                                "path:direction is \"out\", \"in\" or \"both\", not \"{other}\""
                            )));
                        }
                    }
                }
                "graph" => {
                    let TermPattern::NamedNode(g) = one()? else {
                        return Err(bad("path:graph takes an IRI"));
                    };
                    graph = Some(GraphName::NamedNode(g.clone()));
                }
                "mode" => {
                    search.mode = match text()?.as_str() {
                        "shortest" => Mode::Shortest,
                        "simple" => Mode::Simple,
                        "all" => Mode::All,
                        other => {
                            return Err(bad(format!(
                                "path:mode is \"shortest\", \"simple\" or \"all\", not \"{other}\""
                            )));
                        }
                    }
                }
                "minLength" => search.min = count()?,
                "maxLength" => search.max = Some(count()?),
                "maxPaths" => search.max_paths = Some(count()?),
                _ => {
                    let output = match name.as_str() {
                        "path" => Output::Path,
                        "start" => Output::Start,
                        "end" => Output::End,
                        "length" => Output::Length,
                        "step" => Output::Step,
                        "from" => Output::From,
                        "edge" => Output::Edge,
                        "to" => Output::To,
                        _ => return Err(bad(format!("unknown parameter path:{name}"))),
                    };
                    let TermPattern::Variable(v) = one()? else {
                        return Err(bad(format!("path:{name} takes a variable")));
                    };
                    if outputs.iter().any(|(_, w)| w == v) {
                        return Err(bad(format!(
                            "?{} is bound twice: give each output its own variable",
                            v.as_str()
                        )));
                    }
                    outputs.push((output, v.clone()));
                }
            }
        }
        if sources.is_empty() && targets.is_empty() {
            return Err(bad("give a path:source or a path:target"));
        }
        if outputs.is_empty() {
            return Err(bad("give at least one output, such as path:end ?node"));
        }
        if search.mode == Mode::All && search.max.is_none() {
            return Err(bad(
                "path:mode \"all\" walks cycles for ever: give a path:maxLength",
            ));
        }
        if search.max.is_some_and(|max| max < search.min) {
            return Err(bad("path:maxLength is less than path:minLength"));
        }

        if !except.is_empty() && !predicates.is_empty() {
            return Err(bad(
                "path:exceptPredicate is for a search over every predicate: give it or path:predicate",
            ));
        }
        let mut edges = Edges {
            service: self,
            predicates,
            except,
            graph,
            direction,
            next: HashMap::new(),
            complete: false,
        };
        match subselects.as_slice() {
            [] => {}
            [_] if !edges.except.is_empty() => {
                return Err(bad(
                    "path:exceptPredicate is for a search over every predicate, not a sub-select",
                ));
            }
            [input] if edges.predicates.is_empty() => self.load(input, &mut edges)?,
            [_] => {
                return Err(bad(
                    "give path:predicate or a sub-select of edges, not both",
                ));
            }
            _ => return Err(bad("give at most one sub-select of edges")),
        }

        // Without sources, search from the targets the other way, then turn the paths round.
        let backwards = sources.is_empty();
        let (starts, ends) = if backwards {
            edges.direction = direction.reversed();
            if edges.complete {
                edges.next = reversed(&edges.next, direction);
            }
            (targets, Vec::new())
        } else {
            (sources, targets)
        };
        let ends: HashSet<Term> = ends.into_iter().collect();

        let mut found = Found::default();
        // One target, the shortest path over every predicate: from both ends.
        let both_ends = search.mode == Mode::Shortest
            && ends.len() == 1
            && edges.predicates.is_empty()
            && !edges.complete;
        let mut back = both_ends.then(|| Edges {
            service: self,
            predicates: Vec::new(),
            except: edges.except.clone(),
            graph: edges.graph.clone(),
            direction: edges.direction.reversed(),
            next: HashMap::new(),
            complete: false,
        });
        for start in &starts {
            let before = found.paths.len();
            match &mut back {
                Some(back) => {
                    let end = ends.iter().next().expect("one end");
                    self.meet(start, end, &search, &mut edges, back, &mut found)?;
                }
                None => self.search(start, &ends, &search, &mut edges, &mut found)?,
            }
            if backwards {
                for p in &mut found.paths[before..] {
                    p.reverse();
                    for (from, _, to) in p.iter_mut() {
                        std::mem::swap(from, to);
                    }
                }
            }
        }
        Ok(rows(found, &outputs))
    }
}

/// What an output variable binds.
#[derive(Clone, Copy, PartialEq)]
enum Output {
    Path,
    Start,
    End,
    Length,
    Step,
    From,
    Edge,
    To,
}

impl PathService {
    /// Reads every edge of a sub-select into `edges`.
    fn load(&self, input: &GraphPattern, edges: &mut Edges<'_>) -> Result<(), PathError> {
        let vars = projected(input);
        let (from, to, label) = match vars.as_slice() {
            [a, b] => (a.clone(), b.clone(), None),
            [a, b, c, ..] => (a.clone(), b.clone(), Some(c.clone())),
            _ => {
                return Err(bad(
                    "the sub-select gives an edge's two ends (and its label third)",
                ));
            }
        };
        let query = spargebra::Query::Select {
            dataset: None,
            pattern: input.clone(),
            base_iri: None,
        };
        let QueryResults::Solutions(solutions) = self
            .store
            .execute({
                let mut prepared = self.sub_select_evaluator().for_query(query);
                if let Some(d) = &self.dataset {
                    *prepared.dataset_mut() = d.clone();
                }
                prepared
            })
            .map_err(|e| PathError::Input(Box::new(e)))?
        else {
            return Err(bad("the sub-select must be a SELECT"));
        };
        let mut next: HashMap<Term, Vec<(Term, Term)>> = HashMap::new();
        let mut seen = HashSet::new();
        for s in solutions {
            let s = s.map_err(|e| PathError::Input(Box::new(e)))?;
            let (Some(a), Some(b)) = (s.get(&from), s.get(&to)) else {
                continue;
            };
            let l = label
                .as_ref()
                .and_then(|l| s.get(l))
                .cloned()
                .unwrap_or_else(|| Literal::from(false).into());
            if !seen.insert((a.clone(), l.clone(), b.clone())) {
                continue;
            }
            if edges.direction != Direction::In {
                next.entry(a.clone())
                    .or_default()
                    .push((l.clone(), b.clone()));
            }
            if edges.direction != Direction::Out {
                next.entry(b.clone()).or_default().push((l, a.clone()));
            }
        }
        edges.next = next.into_iter().map(|(k, v)| (k, v.into())).collect();
        edges.complete = true;
        Ok(())
    }

    /// Adds the paths from `start` to `found`.
    fn search(
        &self,
        start: &Term,
        ends: &HashSet<Term>,
        search: &Search,
        edges: &mut Edges<'_>,
        found: &mut Found,
    ) -> Result<(), PathError> {
        let wanted = |t: &Term| ends.is_empty() || ends.contains(t);
        if search.min == 0 && wanted(start) {
            found.empty.push(start.clone());
        }
        let max = search.max.unwrap_or(usize::MAX);
        let mut work = 0usize;
        let mut tick = || -> Result<(), PathError> {
            work += 1;
            if work.is_multiple_of(1024) && self.cancelled() {
                return Err(PathError::Cancelled);
            }
            Ok(())
        };
        match search.mode {
            Mode::Shortest => {
                // Breadth first; each node keeps the step that first reached it.
                let mut parent: HashMap<Term, Option<(Term, Term)>> = HashMap::new();
                parent.insert(start.clone(), None);
                let mut level = vec![start.clone()];
                let mut depth = 0;
                let mut reached = usize::from(!ends.is_empty() && ends.contains(start));
                let mut order: VecDeque<(Term, usize)> = VecDeque::new();
                while !level.is_empty() && depth < max {
                    if !ends.is_empty() && reached == ends.len() {
                        break;
                    }
                    depth += 1;
                    let mut next_level = Vec::new();
                    for node in &level {
                        for (label, to) in edges.of(node)?.iter() {
                            tick()?;
                            if parent.contains_key(to) {
                                continue;
                            }
                            parent.insert(to.clone(), Some((node.clone(), label.clone())));
                            if ends.contains(to) {
                                reached += 1;
                            }
                            order.push_back((to.clone(), depth));
                            next_level.push(to.clone());
                        }
                    }
                    level = next_level;
                }
                for (end, depth) in order {
                    if depth < search.min || !wanted(&end) {
                        continue;
                    }
                    let mut p = Vec::with_capacity(depth);
                    let mut at = end;
                    while let Some(Some((from, label))) = parent.get(&at) {
                        p.push((from.clone(), label.clone(), at.clone()));
                        at = from.clone();
                    }
                    p.reverse();
                    found.push(p)?;
                }
            }
            Mode::Simple | Mode::All => {
                // Depth first, with the neighbours of each node on the path and the next to try.
                let simple = search.mode == Mode::Simple;
                let mut per_end: HashMap<Term, usize> = HashMap::new();
                if search.min == 0 && wanted(start) {
                    per_end.insert(start.clone(), 1);
                }
                let mut path: Vec<Step> = Vec::new();
                let mut on_path: HashSet<Term> = HashSet::from([start.clone()]);
                let mut stack: Vec<(Next, usize)> = vec![(edges.of(start)?, 0)];
                while let Some((next, i)) = stack.last_mut() {
                    let Some((label, to)) = next.get(*i).cloned() else {
                        stack.pop();
                        if let Some((_, _, last)) = path.pop()
                            && simple
                        {
                            on_path.remove(&last);
                        }
                        continue;
                    };
                    *i += 1;
                    tick()?;
                    if simple && on_path.contains(&to) {
                        continue;
                    }
                    let from = path.last().map_or_else(|| start.clone(), |s| s.2.clone());
                    path.push((from, label, to.clone()));
                    if simple {
                        on_path.insert(to.clone());
                    }
                    if path.len() >= search.min && wanted(&to) {
                        let n = per_end.entry(to.clone()).or_default();
                        if search.max_paths.is_none_or(|m| *n < m) {
                            *n += 1;
                            found.push(path.clone())?;
                        }
                    }
                    let further = if path.len() < max {
                        edges.of(&to)?
                    } else {
                        Arc::from([])
                    };
                    stack.push((further, 0));
                }
            }
        }
        Ok(())
    }
}

impl PathService {
    /// Adds a shortest path from `start` to `end` to `found`, searched breadth first from both
    /// ends, a whole level of the smaller frontier at a time: `forward` reads the edges in the
    /// direction walked, `back` the other way.
    fn meet(
        &self,
        start: &Term,
        end: &Term,
        search: &Search,
        forward: &mut Edges<'_>,
        back: &mut Edges<'_>,
        found: &mut Found,
    ) -> Result<(), PathError> {
        if start == end {
            if search.min == 0 {
                found.empty.push(start.clone());
            }
            return Ok(());
        }
        let max = search.max.unwrap_or(usize::MAX);
        // Each side's nodes, with the step that reached each (from the start: the node before it
        // and the label; from the end: the node after it) and how many edges away it is.
        let mut ahead = Side::new(start);
        let mut behind = Side::new(end);
        let meeting = loop {
            if ahead.level.is_empty()
                || behind.level.is_empty()
                || ahead.depth + behind.depth >= max
            {
                return Ok(());
            }
            let met = if ahead.level.len() <= behind.level.len() {
                ahead.grow(forward, &behind, self)?
            } else {
                behind.grow(back, &ahead, self)?
            };
            if let Some(node) = met {
                break node;
            }
        };
        // From the start to the meeting node, then on to the end.
        let mut path = Vec::new();
        let mut at = meeting.clone();
        while let Some(Some((before, label))) = ahead.reached.get(&at).map(|r| &r.0) {
            path.push((before.clone(), label.clone(), at.clone()));
            at = before.clone();
        }
        path.reverse();
        let mut at = meeting;
        while let Some(Some((after, label))) = behind.reached.get(&at).map(|r| &r.0) {
            path.push((at.clone(), label.clone(), after.clone()));
            at = after.clone();
        }
        if path.len() >= search.min && path.len() <= max {
            found.push(path)?;
        }
        Ok(())
    }
}

/// One side of a search from both ends.
struct Side {
    /// Each node reached: the step that reached it (the node it came from and the label) and
    /// its distance from this side's end.
    reached: HashMap<Term, (Option<(Term, Term)>, usize)>,
    /// The nodes reached last.
    level: Vec<Term>,
    depth: usize,
}

impl Side {
    fn new(node: &Term) -> Self {
        Self {
            reached: HashMap::from([(node.clone(), (None, 0))]),
            level: vec![node.clone()],
            depth: 0,
        }
    }

    /// Reaches the next level. Gives the node of it, also reached by `other`, nearest to
    /// `other`'s end, if there is one: the paths through it are the shortest.
    fn grow(
        &mut self,
        edges: &mut Edges<'_>,
        other: &Side,
        service: &PathService,
    ) -> Result<Option<Term>, PathError> {
        self.depth += 1;
        let mut next = Vec::new();
        let mut met: Option<(usize, Term)> = None;
        let mut work = 0usize;
        for node in std::mem::take(&mut self.level) {
            for (label, to) in edges.of(&node)?.iter() {
                work += 1;
                if work.is_multiple_of(1024) && service.cancelled() {
                    return Err(PathError::Cancelled);
                }
                if self.reached.contains_key(to) {
                    continue;
                }
                self.reached.insert(
                    to.clone(),
                    (Some((node.clone(), label.clone())), self.depth),
                );
                next.push(to.clone());
                if let Some((_, rest)) = other.reached.get(to)
                    && met.as_ref().is_none_or(|(r, _)| rest < r)
                {
                    met = Some((*rest, to.clone()));
                }
            }
        }
        self.level = next;
        Ok(met.map(|(_, n)| n))
    }
}

/// The edges of `next` turned round, for a backwards search over a sub-select's edges.
fn reversed(next: &HashMap<Term, Next>, direction: Direction) -> HashMap<Term, Next> {
    if direction == Direction::Both {
        return next.clone();
    }
    let mut out: HashMap<Term, Vec<(Term, Term)>> = HashMap::new();
    for (from, list) in next {
        for (label, to) in list.iter() {
            out.entry(to.clone())
                .or_default()
                .push((label.clone(), from.clone()));
        }
    }
    out.into_iter().map(|(k, v)| (k, v.into())).collect()
}

/// The result rows: per edge when an output is per edge, else per path.
fn rows(found: Found, outputs: &[(Output, Variable)]) -> QuerySolutionIter<'static> {
    let per_edge = outputs
        .iter()
        .any(|(o, _)| matches!(o, Output::Step | Output::From | Output::Edge | Output::To));
    let variables: Arc<[Variable]> = outputs.iter().map(|(_, v)| v.clone()).collect();
    let int = |n: usize| -> Term { Literal::new_typed_literal(n.to_string(), xsd::INTEGER).into() };
    let mut solutions = Vec::new();
    let mut row =
        |id: usize, start: &Term, end: &Term, length: usize, step: Option<(usize, &Step)>| {
            let values: Vec<Option<Term>> = outputs
                .iter()
                .map(|(o, _)| match o {
                    Output::Path => Some(int(id)),
                    Output::Start => Some(start.clone()),
                    Output::End => Some(end.clone()),
                    Output::Length => Some(int(length)),
                    Output::Step => step.map(|(i, _)| int(i + 1)),
                    Output::From => step.map(|(_, s)| s.0.clone()),
                    Output::Edge => step.map(|(_, s)| s.1.clone()),
                    Output::To => step.map(|(_, s)| s.2.clone()),
                })
                .collect();
            solutions.push(QuerySolution::from((Arc::clone(&variables), values)));
        };
    let mut id = 0;
    for node in &found.empty {
        id += 1;
        row(id, node, node, 0, None);
    }
    for p in &found.paths {
        id += 1;
        let (start, end) = (&p[0].0, &p[p.len() - 1].2);
        if per_edge {
            for (i, s) in p.iter().enumerate() {
                row(id, start, end, p.len(), Some((i, s)));
            }
        } else {
            row(id, start, end, p.len(), None);
        }
    }
    QuerySolutionIter::new(
        variables,
        solutions.into_iter().map(Ok::<_, QueryEvaluationError>),
    )
}
