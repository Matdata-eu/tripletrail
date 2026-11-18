//! SHACL Advanced Features rules (`sh:rule`): triple rules with node expressions and SPARQL
//! (CONSTRUCT) rules, run on the focus nodes of their shapes until they infer nothing new.

use crate::Inferred;
use crate::ShaclError;
use crate::graph::{DataGraph, Union};
use crate::path::{Path, dedup};
use crate::shapes::{Component, ShapeId, Shapes, Target};
use crate::sparql::{Bindings, Evaluator, QueryData, Runner, ShapeQuery, plain_evaluator, var};
use crate::validate::{SPARQL_BATCH, Validator};
use crate::vocab::{rdf, rdfs};
use oxigraph::model::{NamedNode, NamedOrBlankNode, Term, Triple};
use oxigraph::sparql::QuerySolution;
use rustc_hash::FxHashSet;
use spargebra::Query;
use spargebra::algebra::{
    AggregateExpression, Expression, GraphPattern, OrderExpression, PropertyPathExpression,
};
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Passes over the rules at most: a rule that keeps making new blank nodes never reaches a
/// fixpoint.
const MAX_PASSES: usize = 64;

/// A node expression (SHACL-AF 7).
#[derive(Debug)]
pub(crate) enum NodeExpr {
    /// `sh:this`: the focus node.
    This,
    /// An IRI or literal.
    Constant(Term),
    /// The values of a path from the nodes of `nodes`.
    Path {
        path: Path,
        nodes: Box<NodeExpr>,
    },
    /// The nodes of `nodes` that conform to `shape`.
    Filter {
        shape: ShapeId,
        nodes: Box<NodeExpr>,
    },
    Intersection(Vec<NodeExpr>),
    Union(Vec<NodeExpr>),
}

#[derive(Debug)]
pub(crate) enum RuleKind {
    /// `sh:TripleRule`: `subject predicate object` for every combination of their nodes.
    Triple {
        subject: NodeExpr,
        predicate: NodeExpr,
        object: NodeExpr,
    },
    /// `sh:SPARQLRule`: a CONSTRUCT query with `$this` pre-bound.
    Sparql(ShapeQuery),
}

#[derive(Debug)]
pub(crate) struct Rule {
    pub shape: ShapeId,
    pub node: Term,
    pub order: f64,
    /// Shapes the focus nodes must conform to.
    pub conditions: Vec<ShapeId>,
    pub kind: RuleKind,
}

/// Runs the rules of [`Shapes`] on a data graph, each on the focus nodes of its shape that
/// conform to its `sh:condition` shapes, seeing the triples earlier rules inferred. The rules
/// are stratified: a rule runs after the rules whose triples it may read (by the predicates
/// they write and it reads), otherwise in `sh:order` (then in the order they were read). A
/// rule that no rule of its own group feeds runs once; a group of rules that feed each other
/// runs in passes until one infers nothing new (at most 64 passes by default), each pass
/// running only the rules whose inputs changed. Nothing is written to the data graph: [`Inference::run`] returns
/// the inferred triples (those not already in the data graph, unless
/// [`Inference::with_existing`] asks for every constructed triple).
pub struct Inference<'a> {
    shapes: &'a Shapes,
    data: &'a dyn DataGraph,
    evaluator: &'a dyn Evaluator,
    shapes_graph: Option<(&'a NamedNode, &'a dyn DataGraph)>,
    max_passes: usize,
    existing: bool,
    semi_naive: bool,
    observer: Option<&'a dyn Fn(&RuleRun<'_>)>,
    provenance: bool,
    sequential: bool,
    plan: Option<&'a PlanSink>,
}

/// Where an inferred triple came from (see [`Inference::with_provenance`]): the first rule
/// run that constructed it.
#[derive(Debug, Clone)]
pub struct Origin {
    /// The rule node (`sh:rule` object).
    pub rule: Term,
    /// The shape the rule belongs to.
    pub shape: Term,
    /// The rule's group of rules and the pass, from 1 (as in [`RuleRun`]).
    pub group: usize,
    pub pass: usize,
    /// The focus node the rule ran on.
    pub focus: Option<Term>,
    /// For a `sh:SPARQLRule`, the solution of its pattern that made the triple.
    pub solution: Option<Arc<QuerySolution>>,
}

/// The focus node and solution of each constructed triple, with provenance.
type Trace = (Option<Term>, Option<Arc<QuerySolution>>);

/// Where [`Inference::with_plan`] puts the plan of a run.
pub type PlanSink = std::sync::Mutex<Option<Plan>>;

/// How a run of the rules is planned (its strata): the groups of rules in the order they
/// run, and the dependencies that put them there.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// The rules, in `sh:order`; groups and edges refer to them by index.
    pub rules: Vec<PlanRule>,
    /// The groups, in the order they run (the group numbers of [`RuleRun::group`], from 1).
    pub groups: Vec<PlanGroup>,
    /// Rule `to` depends on rule `from`: `from` writes what `to` reads, both known. A rule
    /// whose reads (writes) are unknown has none: its `reads` (`writes`) is `None`, and it
    /// may read (write) what any rule writes (reads). At most [`PLAN_EDGES`].
    pub edges: Vec<PlanEdge>,
    /// Whether edges were left out past [`PLAN_EDGES`].
    pub truncated: bool,
}

/// The most edges a [`Plan`] lists: rules that share keys (many writing `rdf:type` of one
/// class, say) make edges as the square of their number.
pub const PLAN_EDGES: usize = 10_000;

#[derive(Debug, Clone)]
pub struct PlanRule {
    pub rule: Term,
    pub shape: Term,
    pub order: f64,
    /// What the rule may read (predicate IRIs, `rdf:type <class>` for the instances of a
    /// class), or `None` for anything.
    pub reads: Option<Vec<String>>,
    /// What it may write, likewise.
    pub writes: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct PlanGroup {
    /// Indexes into [`Plan::rules`].
    pub rules: Vec<usize>,
    /// The group runs in passes to a fixpoint: one of its rules may get more results from
    /// what a rule of the group infers. Otherwise each rule runs once.
    pub recursive: bool,
}

#[derive(Debug, Clone)]
pub struct PlanEdge {
    pub from: usize,
    pub to: usize,
    /// What `from` writes and `to` reads.
    pub through: Vec<String>,
    /// More triples from `from` may give `to` more results; `false` when `to` reads them only
    /// under a negation (`FILTER NOT EXISTS`, `MINUS`), where they can only take results
    /// away: such an edge orders the groups but does not make a group recursive.
    pub positive: bool,
}

/// A read or write key as text: a predicate IRI, or `rdf:type <class>`.
fn key_label(key: &str) -> String {
    match key.strip_prefix(TYPE_KEY) {
        Some(class) => format!("{} {class}", rdf::TYPE.as_str()),
        None => key.to_owned(),
    }
}

/// The [`Plan`] of `rules` (in run order) run as `plan` groups.
fn explain(
    shapes: &Shapes,
    rules: &[&Rule],
    plan: &[(Vec<usize>, bool)],
    all: &[Option<BTreeSet<String>>],
    positive: &[Option<BTreeSet<String>>],
    writes: &[Option<BTreeSet<String>>],
) -> Plan {
    let labels = |keys: &Option<BTreeSet<String>>| {
        keys.as_ref()
            .map(|k| k.iter().map(|k| key_label(k)).collect())
    };
    let mut edges = Vec::new();
    let mut truncated = false;
    'all: for to in 0..rules.len() {
        let Some(read) = &all[to] else { continue };
        for (from, written) in writes.iter().enumerate().take(rules.len()) {
            let Some(w) = written else { continue };
            let through: Vec<String> = w
                .iter()
                .filter(|k| meets(&BTreeSet::from([(*k).clone()]), read))
                .map(|k| key_label(k))
                .collect();
            if through.is_empty() {
                continue;
            }
            if edges.len() == PLAN_EDGES {
                truncated = true;
                break 'all;
            }
            let positive = positive[to].as_ref().is_none_or(|r| meets(w, r));
            edges.push(PlanEdge {
                from,
                to,
                through,
                positive,
            });
        }
    }
    Plan {
        rules: rules
            .iter()
            .enumerate()
            .map(|(i, r)| PlanRule {
                rule: r.node.clone(),
                shape: shapes.get(r.shape).node.clone(),
                order: r.order,
                reads: labels(&all[i]),
                writes: labels(&writes[i]),
            })
            .collect(),
        groups: plan
            .iter()
            .map(|(rules, recursive)| PlanGroup {
                rules: rules.clone(),
                recursive: *recursive,
            })
            .collect(),
        edges,
        truncated,
    }
}

/// What one rule did in one pass, given to the observer of [`Inference::with_observer`].
#[derive(Debug)]
pub struct RuleRun<'r> {
    /// The rule's group of rules (see [`Inference`]), from 1, in the order the groups run.
    pub group: usize,
    /// Whether the group runs to a fixpoint (its rules feed each other); otherwise each of its
    /// rules runs once.
    pub recursive: bool,
    /// The pass of the rule's group of rules, from 1.
    pub pass: usize,
    /// The rule node (`sh:rule` object).
    pub rule: &'r Term,
    /// The shape the rule belongs to (a rule shared by several shapes runs once per shape).
    pub shape: &'r Term,
    /// False when the rule starts (the counts are then 0), true when it is done.
    pub done: bool,
    /// The focus nodes it ran on (after its `sh:condition` shapes).
    pub focus_nodes: usize,
    /// The triples it constructed.
    pub constructed: usize,
    /// Those not in the data graph nor inferred before.
    pub new: usize,
    pub elapsed: std::time::Duration,
}

impl<'a> Inference<'a> {
    pub fn new(shapes: &'a Shapes, data: &'a dyn DataGraph) -> Self {
        static PLAIN: fn() -> oxigraph::sparql::SparqlEvaluator = plain_evaluator;
        Self {
            shapes,
            data,
            evaluator: &PLAIN,
            shapes_graph: None,
            max_passes: MAX_PASSES,
            existing: false,
            semi_naive: true,
            observer: None,
            provenance: false,
            sequential: false,
            plan: None,
        }
    }

    /// Runs each rule once, in `sh:order` (then by the rule node), each seeing what the rules
    /// before it inferred: one pass, no stratification and no fixpoint, as a list of SPARQL
    /// `INSERT`s run in that order would. Off by default.
    pub fn with_sequential(mut self, sequential: bool) -> Self {
        self.sequential = sequential;
        self
    }

    /// Puts the [`Plan`] of the run in `sink` when the rules are about to run: which rules
    /// form each group, in what order the groups run, and why.
    pub fn with_plan(self, sink: &'a PlanSink) -> Self {
        self.with_plan_from(Some(sink))
    }

    /// [`Inference::with_plan`] when `sink` is given.
    pub fn with_plan_from(mut self, sink: Option<&'a PlanSink>) -> Self {
        self.plan = sink;
        self
    }

    /// Records where each inferred triple came from, for [`Inference::run_traced`]. SPARQL
    /// rules then run as SELECT queries whose solutions fill in their templates, and every
    /// pass runs them on all the data (not semi-naive): the same triples, more slowly.
    pub fn with_provenance(mut self, provenance: bool) -> Self {
        self.provenance = provenance;
        self
    }

    /// Stops after this many passes over the rules (64 by default) when they keep inferring
    /// new triples.
    pub fn with_max_passes(mut self, passes: usize) -> Self {
        self.max_passes = passes.max(1);
        self
    }

    /// Also returns the triples the rules construct that are already in the data graph (as
    /// TopBraid's `shaclinfer` does), for exports built from the rules' output. They do not
    /// count as new: the passes and [`RuleRun::new`] are the same either way.
    pub fn with_existing(mut self, existing: bool) -> Self {
        self.existing = existing;
        self
    }

    /// Runs SPARQL with evaluators made by `evaluator` (with custom functions).
    pub fn with_evaluator(mut self, evaluator: &'a dyn Evaluator) -> Self {
        self.evaluator = evaluator;
        self
    }

    /// The shapes graph and its IRI, which `$shapesGraph` names in queries.
    pub fn with_shapes_graph(mut self, name: &'a NamedNode, graph: &'a dyn DataGraph) -> Self {
        self.shapes_graph = Some((name, graph));
        self
    }

    /// Whether later passes of a recursive group run SPARQL rules only on what the triples
    /// inferred since their last run can add (semi-naive, the default), or on all the data
    /// again. Both infer the same triples.
    pub fn with_semi_naive(mut self, semi_naive: bool) -> Self {
        self.semi_naive = semi_naive;
        self
    }

    /// Calls `observer` when each rule of each pass starts and when it is done, with what it
    /// did (to report progress or find slow rules).
    pub fn with_observer(mut self, observer: &'a dyn Fn(&RuleRun<'_>)) -> Self {
        self.observer = Some(observer);
        self
    }

    /// The triples the rules infer, without those already in the data graph (unless
    /// [`Inference::with_existing`]).
    pub fn run(&self) -> Result<Inferred, ShaclError> {
        Ok(self.run_traced()?.0)
    }

    /// As [`Inference::run`], with the [`Origin`] of each inferred triple (in the same order)
    /// when [`Inference::with_provenance`] asks for them; else an empty list.
    pub fn run_traced(&self) -> Result<(Inferred, Vec<Origin>), ShaclError> {
        let mut origins = Vec::new();
        let mut existing_origins = Vec::new();
        let mut rules: Vec<&Rule> = self.shapes.rules.iter().collect();
        if self.sequential {
            rules.sort_by(|a, b| {
                a.order
                    .total_cmp(&b.order)
                    .then_with(|| a.node.to_string().cmp(&b.node.to_string()))
            });
        } else {
            rules.sort_by(|a, b| a.order.total_cmp(&b.order));
        }
        let mut inferred = Inferred::new();
        // Constructed triples already in the data graph, with `existing`.
        let mut existing = Inferred::new();
        // What each rule reads, and how many triples had been inferred when it last ran;
        // `latest` is, per predicate, the count when one with it was last inferred. After the
        // first pass, a rule runs again only if a triple inferred since it last ran has a
        // predicate it reads.
        let read: Vec<Option<RuleReads>> =
            rules.iter().map(|r| rule_reads(self.shapes, r)).collect();
        let all: Vec<Option<BTreeSet<String>>> = read
            .iter()
            .map(|r| r.as_ref().map(|r| r.all.clone()))
            .collect();
        // More triples of the predicates a rule reads only under a negation never give it
        // new results: they do not make it run again.
        let reads: Vec<Option<BTreeSet<String>>> =
            read.into_iter().map(|r| r.map(|r| r.positive)).collect();
        let mut writes: Vec<Option<BTreeSet<String>>> =
            rules.iter().map(|r| rule_writes(r)).collect();
        // Types are matched per class, with the subclasses of the data graph: a rule writing
        // `?x a ex:D` feeds the rules reading instances of `ex:D` and of its superclasses. When
        // a rule may write subclasses, any type may feed any class.
        let classes = Classes::new(
            self.data,
            writes.iter().all(|w| {
                w.as_ref()
                    .is_some_and(|w| !w.contains(rdfs::SUB_CLASS_OF.as_str()))
            }),
        );
        let (all, reads) = (classes.reads(all), classes.reads(reads));
        // What each rule's focus nodes depend on: those found for one rule serve the next
        // rule on the same shape and conditions, and the next pass, while no triple inferred
        // since has one of these keys.
        let focus_reads =
            classes.reads(rules.iter().map(|r| focus_reads(self.shapes, r)).collect());
        let found: Found = Default::default();
        for w in writes.iter_mut().flatten() {
            *w = classes.writes(w);
        }
        let mut seen = vec![0usize; rules.len()];
        // `inferred` keeps the triples in the order they came, so that `[seen[i]..]` of them are
        // those inferred since rule `i` last ran (its delta); and the focus nodes it last ran on.
        let mut last_foci: Vec<Option<FxHashSet<Term>>> = vec![None; rules.len()];
        let mut latest: rustc_hash::FxHashMap<String, usize> = Default::default();
        let mut keys = Keys::default();
        // Stratified: the groups of rules that depend on each other (through what they write
        // and read) run in dependency order, each to its own fixpoint; a rule that depends on
        // no rule of its group runs once.
        let plan = if self.sequential {
            (0..rules.len()).map(|i| (vec![i], false)).collect()
        } else {
            strata(&all, &reads, &writes)
        };
        if let Some(sink) = self.plan {
            *sink.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(explain(self.shapes, &rules, &plan, &all, &reads, &writes));
        }
        let ctx = Ctx {
            shapes: self.shapes,
            data: self.data,
            evaluator: self.evaluator,
            shapes_graph: self.shapes_graph,
            provenance: self.provenance,
        };
        let found = std::sync::Mutex::new(found);
        let semi_naive = self.semi_naive;
        // A batch of rules runs on the threads one query may use (`--query-threads`).
        let threads = tripletrail_store::par::query_threads();
        // In sequence: one group, one pass, the rules in order.
        let plan = if self.sequential {
            vec![((0..rules.len()).collect(), false)]
        } else {
            plan
        };
        for (number, (group, recursive)) in plan.into_iter().enumerate() {
            for pass in 1..=self.max_passes {
                let mut added = false;
                // The next rules run together, on several cores, each on what was inferred
                // before them, and their triples are merged in rule order (#891): those that
                // read nothing the rules before them in the batch may write, so that each sees
                // what it would see run one at a time. After the first pass, a rule runs again
                // only if a triple inferred since it last ran has a predicate it reads.
                let mut next = 0;
                while next < group.len() {
                    let mut batch: Vec<usize> = Vec::new();
                    while next < group.len() {
                        let j = group[next];
                        let feeds = |a: usize| match (&writes[a], &all[j]) {
                            (None, _) | (_, None) => true,
                            (Some(w), Some(r)) => meets(w, r),
                        };
                        if batch.iter().any(|&a| feeds(a)) {
                            break;
                        }
                        next += 1;
                        let affected = pass == 1
                            || match &reads[j] {
                                None => inferred.len() > seen[j],
                                Some(r) => {
                                    latest.iter().any(|(k, &at)| at > seen[j] && r.contains(k))
                                }
                            };
                        if affected {
                            batch.push(j);
                            if threads == 1 {
                                break;
                            }
                        }
                    }
                    let now = inferred.len();
                    let mut tasks = Vec::with_capacity(batch.len());
                    for &i in &batch {
                        let rule = rules[i];
                        if let Some(observer) = self.observer {
                            observer(&RuleRun {
                                group: if self.sequential { i + 1 } else { number + 1 },
                                recursive,
                                pass,
                                rule: &rule.node,
                                shape: &self.shapes.get(rule.shape).node,
                                done: false,
                                focus_nodes: 0,
                                constructed: 0,
                                new: 0,
                                elapsed: std::time::Duration::ZERO,
                            });
                        }
                        // After the rule's first run: only what the delta can add (#110).
                        let since = std::mem::replace(&mut seen[i], now);
                        let before = last_foci[i].take().filter(|_| recursive);
                        tasks.push((i, since, before));
                    }
                    let results = par_map(threads, tasks, |(i, since, before)| {
                        let reuse = Reuse {
                            found: &found,
                            reads: focus_reads[i].as_ref(),
                            latest: &latest,
                            now,
                        };
                        ctx.run_rule(rules[i], &inferred, since, before, semi_naive, reuse)
                    });
                    for (&i, result) in batch.iter().zip(results) {
                        let rule = rules[i];
                        let shape = &self.shapes.get(rule.shape).node;
                        let (focus_nodes, constructed, traces, ran_on, elapsed) = result?;
                        last_foci[i] = ran_on;
                        let count = constructed.len();
                        let mut new = 0;
                        let mut traces = traces.into_iter();
                        for t in constructed {
                            let trace = traces.next();
                            let origin = |trace: Option<Trace>| {
                                let (focus, solution) = trace.unwrap_or_default();
                                Origin {
                                    rule: rule.node.clone(),
                                    shape: shape.clone(),
                                    group: if self.sequential { i + 1 } else { number + 1 },
                                    pass,
                                    focus,
                                    solution,
                                }
                            };
                            let s: Term = t.subject.clone().into();
                            if self.data.contains(&s, t.predicate.as_ref(), &t.object) {
                                if self.existing && existing.insert(&t) && self.provenance {
                                    existing_origins.push(origin(trace));
                                }
                            } else if inferred.insert(&t) {
                                if self.provenance {
                                    origins.push(origin(trace));
                                }
                                new += 1;
                                let n = inferred.len();
                                for key in keys.of(&classes, &t) {
                                    match latest.get_mut(key) {
                                        Some(at) => *at = n,
                                        None => {
                                            latest.insert(key.clone(), n);
                                        }
                                    }
                                }
                            }
                        }
                        added |= new > 0;
                        if let Some(observer) = self.observer {
                            observer(&RuleRun {
                                group: if self.sequential { i + 1 } else { number + 1 },
                                recursive,
                                pass,
                                rule: &rule.node,
                                shape,
                                done: true,
                                focus_nodes,
                                constructed: count,
                                new,
                                elapsed,
                            });
                        }
                    }
                }
                if !added || !recursive {
                    break;
                }
            }
        }
        inferred.extend(existing.iter().cloned());
        origins.extend(existing_origins);
        Ok((inferred, origins))
    }
}

/// What evaluating a rule reads, shared by the threads that run the rules of a pass (#891).
struct Ctx<'a> {
    shapes: &'a Shapes,
    data: &'a dyn DataGraph,
    evaluator: &'a dyn Evaluator,
    shapes_graph: Option<(&'a NamedNode, &'a dyn DataGraph)>,
    provenance: bool,
}

/// What one rule run gave: its focus nodes, the triples it constructed (with their traces),
/// the focus nodes to remember for its next run, and how long it took.
type RuleResult = Result<
    (
        usize,
        Vec<Triple>,
        Vec<Trace>,
        Option<FxHashSet<Term>>,
        std::time::Duration,
    ),
    ShaclError,
>;

impl Ctx<'_> {
    /// Runs rule `rule` on the data and `inferred`: after its first run (`before`, the focus
    /// nodes it ran on), in a recursive group and semi-naive, only what `delta` can add.
    #[allow(clippy::too_many_arguments)]
    fn run_rule(
        &self,
        rule: &Rule,
        inferred: &Inferred,
        since: usize,
        before: Option<FxHashSet<Term>>,
        semi_naive: bool,
        reuse: Reuse<'_>,
    ) -> RuleResult {
        let started = std::time::Instant::now();
        let log = inferred.as_slice();
        let delta = &log[since..];
        let mut ran_on = None;
        let (focus_nodes, constructed, traces) = match before {
            Some(before) if semi_naive => {
                self.apply_delta(rule, inferred, log, delta, &before, &mut ran_on, reuse)?
            }
            _ => self.apply(rule, inferred, log, Some(&mut ran_on), Some(reuse))?,
        };
        Ok((focus_nodes, constructed, traces, ran_on, started.elapsed()))
    }

    /// The focus nodes of a rule on the data and `inferred`, after its conditions.
    /// With `reuse`, those found before for the same shape and conditions when no triple
    /// inferred since can change them.
    fn foci(
        &self,
        v: &Validator<'_>,
        rule: &Rule,
        reuse: Option<Reuse<'_>>,
    ) -> Result<Vec<Term>, ShaclError> {
        let Some(Reuse {
            found,
            reads: Some(reads),
            latest,
            now,
        }) = reuse
        else {
            return self.find_foci(v, rule);
        };
        let key = (rule.shape, rule.conditions.clone());
        if let Some((at, foci)) = found.lock().unwrap_or_else(|e| e.into_inner()).get(&key)
            && !latest.iter().any(|(k, &t)| t > *at && reads.contains(k))
        {
            return Ok(foci.clone());
        }
        let foci = self.find_foci(v, rule)?;
        found
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (now, foci.clone()));
        Ok(foci)
    }

    fn find_foci(&self, v: &Validator<'_>, rule: &Rule) -> Result<Vec<Term>, ShaclError> {
        let mut foci = v.focus_nodes(rule.shape);
        for &c in &rule.conditions {
            v.prefetch_for(c, &foci);
            foci.retain(|f| v.conforms_to(f, c));
        }
        if let Some(e) = v.take_error() {
            return Err(e);
        }
        Ok(foci)
    }

    fn validator<'v>(&'v self, union: &'v Union<'v>, log: &'v [Triple]) -> Validator<'v> {
        let mut v = Validator::new(self.shapes, union)
            .with_evaluator(self.evaluator)
            .with_inferred(log);
        if let Some((name, g)) = self.shapes_graph {
            v = v.with_shapes_graph(name, g);
        }
        v
    }

    /// A SPARQL rule after its first run: the full query on focus nodes it hasn't run on yet,
    /// the variants of [`crate::delta::variants`] on the others. The full query on all of
    /// them when the query has no variants (`ran_on` gets the focus nodes either way).
    #[allow(clippy::too_many_arguments)]
    fn apply_delta(
        &self,
        rule: &Rule,
        inferred: &Inferred,
        log: &[Triple],
        delta: &[Triple],
        before: &FxHashSet<Term>,
        ran_on: &mut Option<FxHashSet<Term>>,
        reuse: Reuse<'_>,
    ) -> Result<(usize, Vec<Triple>, Vec<Trace>), ShaclError> {
        let RuleKind::Sparql(q) = &rule.kind else {
            return self.apply(rule, inferred, log, Some(ran_on), Some(reuse));
        };
        let Query::Construct {
            template,
            pattern,
            base_iri,
            ..
        } = &q.query
        else {
            return self.apply(rule, inferred, log, Some(ran_on), Some(reuse));
        };
        let index = crate::delta::Delta::new(delta);
        let Some(variants) = crate::delta::variants(pattern, &index) else {
            return self.apply(rule, inferred, log, Some(ran_on), Some(reuse));
        };
        let union = Union(vec![self.data, inferred]);
        let v = self.validator(&union, log);
        let foci = self.foci(&v, rule, Some(reuse))?;
        let (old, new): (Vec<Term>, Vec<Term>) =
            foci.iter().cloned().partition(|f| before.contains(f));
        let (mut out, mut traces) = self.construct(rule, q, &union, None, log, &new)?;
        // The variants run on the focus nodes the rule ran on before.
        let variants = if old.is_empty() { Vec::new() } else { variants };
        for pattern in variants {
            let variant = ShapeQuery {
                query: Query::Construct {
                    template: template.clone(),
                    dataset: None,
                    pattern,
                    base_iri: base_iri.clone(),
                },
                constants: q.constants.clone(),
            };
            let (triples, traced) = self.construct(
                rule,
                &variant,
                &union,
                index.reads_graph().then_some(&index as &dyn DataGraph),
                log,
                &old,
            )?;
            out.extend(triples);
            traces.extend(traced);
        }
        *ran_on = Some(foci.iter().cloned().collect());
        Ok((foci.len(), out, traces))
    }

    /// The triples a SPARQL rule's query makes for these focus nodes. `delta`: the graph a
    /// semi-naive variant reads as [`crate::delta::DELTA_GRAPH`]. Without it, the query runs
    /// on the evaluator's own store when it has one (with `log`, the inferred triples, added),
    /// else on `union`.
    fn construct(
        &self,
        rule: &Rule,
        q: &ShapeQuery,
        union: &Union<'_>,
        delta: Option<&dyn DataGraph>,
        log: &[Triple],
        foci: &[Term],
    ) -> Result<(Vec<Triple>, Vec<Trace>), ShaclError> {
        let runner = Runner {
            data: QueryData {
                data: union,
                shapes: self.shapes_graph,
                delta,
            },
            evaluator: self.evaluator,
        };
        let mut out = Vec::new();
        let mut traces = Vec::new();
        // A query whose triples do not depend on the focus node (`$this` only in a sub-select
        // that does not return it) makes the same triples for each one: it runs once, on one
        // of them, as an INSERT with `FILTER EXISTS { focus }` would (#520). Each focus node
        // keeps its own trace with provenance.
        let one;
        let foci = if !self.provenance && foci.len() > 1 && !q.depends_on_focus() {
            one = [foci
                .iter()
                .find(|f| !matches!(f, Term::BlankNode(_)))
                .unwrap_or(&foci[0])
                .clone()];
            &one[..]
        } else {
            foci
        };
        // A query cannot name a blank node: those focus nodes always take the evaluator.
        let (blank, named): (Vec<&Term>, Vec<&Term>) =
            foci.iter().partition(|f| matches!(f, Term::BlankNode(_)));
        for (chunk, native) in named
            .chunks(SPARQL_BATCH)
            .map(|c| (c, delta.is_none()))
            .chain(blank.chunks(SPARQL_BATCH).map(|c| (c, false)))
        {
            let mut b = Bindings::new(vec![var("this")]);
            b.rows = chunk.iter().map(|f| vec![Some((*f).clone())]).collect();
            if self.provenance {
                // The same query, run as a SELECT to keep each triple's solution (#455).
                let traced = runner
                    .construct_traced(q, b, native.then_some(log))
                    .map_err(|e| ShaclError::Query(format!("rule {}: {e}", rule.node)))?;
                for (t, solution) in traced {
                    traces.push((solution.get("this").cloned(), Some(solution)));
                    out.push(t);
                }
                continue;
            }
            let triples = match native.then(|| runner.construct_native(q, b.clone(), log)) {
                Some(Some(triples)) => triples,
                _ => runner.construct(q, b),
            };
            out.extend(triples.map_err(|e| ShaclError::Query(format!("rule {}: {e}", rule.node)))?);
        }
        Ok((out, traces))
    }

    /// What one rule infers from the data and `inferred`, with its number of focus nodes.
    /// `ran_on` gets the focus nodes of a SPARQL rule, for [`Inference::apply_delta`] later.
    fn apply(
        &self,
        rule: &Rule,
        inferred: &Inferred,
        log: &[Triple],
        ran_on: Option<&mut Option<FxHashSet<Term>>>,
        reuse: Option<Reuse<'_>>,
    ) -> Result<(usize, Vec<Triple>, Vec<Trace>), ShaclError> {
        let union = Union(vec![self.data, inferred]);
        let v = self.validator(&union, log);
        let foci = self.foci(&v, rule, reuse)?;
        let mut out = Vec::new();
        let mut traces = Vec::new();
        match &rule.kind {
            RuleKind::Triple {
                subject,
                predicate,
                object,
            } => {
                for f in &foci {
                    let subjects = self.eval(&v, &union, subject, f);
                    let predicates = self.eval(&v, &union, predicate, f);
                    let objects = self.eval(&v, &union, object, f);
                    for s in &subjects {
                        let s = match s {
                            Term::NamedNode(n) => NamedOrBlankNode::from(n.clone()),
                            Term::BlankNode(b) => b.clone().into(),
                            _ => continue,
                        };
                        for p in &predicates {
                            let Term::NamedNode(p) = p else { continue };
                            for o in &objects {
                                out.push(Triple::new(s.clone(), p.clone(), o.clone()));
                                if self.provenance {
                                    traces.push((Some(f.clone()), None));
                                }
                            }
                        }
                    }
                }
                if let Some(e) = v.take_error() {
                    return Err(e);
                }
            }
            RuleKind::Sparql(q) => {
                (out, traces) = self.construct(rule, q, &union, None, log, &foci)?;
                if let Some(ran_on) = ran_on {
                    *ran_on = Some(foci.iter().cloned().collect());
                }
            }
        }
        Ok((foci.len(), out, traces))
    }

    /// The nodes of a node expression for focus node `focus`.
    fn eval(&self, v: &Validator<'_>, g: &dyn DataGraph, e: &NodeExpr, focus: &Term) -> Vec<Term> {
        let mut out = match e {
            NodeExpr::This => vec![focus.clone()],
            NodeExpr::Constant(t) => vec![t.clone()],
            NodeExpr::Path { path, nodes } => {
                let mut out = Vec::new();
                for n in self.eval(v, g, nodes, focus) {
                    out.extend(path.values(g, &n));
                }
                out
            }
            NodeExpr::Filter { shape, nodes } => self
                .eval(v, g, nodes, focus)
                .into_iter()
                .filter(|n| v.conforms_to(n, *shape))
                .collect(),
            NodeExpr::Union(list) => list
                .iter()
                .flat_map(|e| self.eval(v, g, e, focus))
                .collect(),
            NodeExpr::Intersection(list) => {
                let mut sets = list.iter().map(|e| self.eval(v, g, e, focus));
                let first = sets.next().unwrap_or_default();
                let rest: Vec<FxHashSet<Term>> = sets.map(|s| s.into_iter().collect()).collect();
                first
                    .into_iter()
                    .filter(|t| rest.iter().all(|s| s.contains(t)))
                    .collect()
            }
        };
        dedup(&mut out);
        out
    }
}

/// What a rule may read or write, as predicate IRIs, except that `rdf:type` with a known
/// class is the class's key ([`type_key`]); plain `rdf:type` stands for any class.
const TYPE_KEY: char = '\u{1}';

fn type_key(class: &str) -> String {
    format!("{TYPE_KEY}{class}")
}

fn is_type_key(key: &str) -> bool {
    key.starts_with(TYPE_KEY)
}

/// The key a triple of `predicate` with `object` reads or writes.
fn key_of(predicate: &str, object: Option<&str>) -> String {
    match object {
        Some(class) if predicate == rdf::TYPE.as_str() => type_key(class),
        _ => predicate.to_owned(),
    }
}

/// Whether a rule writing `writes` may change what a rule reading `reads` sees.
fn meets(writes: &BTreeSet<String>, reads: &BTreeSet<String>) -> bool {
    let any_type = rdf::TYPE.as_str();
    writes.iter().any(|w| {
        if w == any_type {
            reads.contains(any_type) || reads.iter().any(|r| is_type_key(r))
        } else if is_type_key(w) {
            reads.contains(w) || reads.contains(any_type)
        } else {
            reads.contains(w)
        }
    })
}

/// The variables a FILTER expression keeps to constant IRIs (`?v IN (ex:a, ex:b)`,
/// `?v = ex:a`, and conjunctions of them).
fn filter_iris(e: &Expression, out: &mut rustc_hash::FxHashMap<String, BTreeSet<String>>) {
    match e {
        Expression::And(a, b) => {
            filter_iris(a, out);
            filter_iris(b, out);
        }
        Expression::In(v, list) => {
            if let Expression::Variable(v) = &**v {
                let iris: Option<BTreeSet<String>> = list
                    .iter()
                    .map(|e| match e {
                        Expression::NamedNode(n) => Some(n.as_str().to_owned()),
                        _ => None,
                    })
                    .collect();
                if let Some(iris) = iris {
                    out.insert(v.as_str().to_owned(), iris);
                }
            }
        }
        Expression::Equal(a, b) | Expression::SameTerm(a, b) => {
            if let (Expression::Variable(v), Expression::NamedNode(n))
            | (Expression::NamedNode(n), Expression::Variable(v)) = (&**a, &**b)
            {
                out.insert(
                    v.as_str().to_owned(),
                    BTreeSet::from([n.as_str().to_owned()]),
                );
            }
        }
        _ => {}
    }
}

/// The IRIs an expression can evaluate to, when it can only be one of a few constant IRIs
/// (as in `BIND(IF(?c, ex:a, ex:b) AS ?p)`).
fn constant_iris(e: &Expression, out: &mut BTreeSet<String>) -> Option<()> {
    match e {
        Expression::NamedNode(n) => {
            out.insert(n.as_str().to_owned());
        }
        Expression::If(_, a, b) => {
            constant_iris(a, out)?;
            constant_iris(b, out)?;
        }
        Expression::Coalesce(list) => {
            for e in list {
                constant_iris(e, out)?;
            }
        }
        _ => return None,
    }
    Some(())
}

/// The constant IRIs each variable `BIND` gives in a pattern (`None` for a variable some
/// `BIND` may give anything).
fn bound_iris(p: &GraphPattern, out: &mut rustc_hash::FxHashMap<String, Option<BTreeSet<String>>>) {
    let mut visit = |p: &GraphPattern| bound_iris(p, out);
    match p {
        GraphPattern::Extend {
            inner,
            variable,
            expression,
        } => {
            let mut iris = BTreeSet::new();
            let found = constant_iris(expression, &mut iris).map(|()| iris);
            let entry = out
                .entry(variable.as_str().to_owned())
                .or_insert(Some(BTreeSet::new()));
            match (entry.as_mut(), found) {
                (Some(all), Some(iris)) => all.extend(iris),
                _ => *entry = None,
            }
            bound_iris(inner, out);
        }
        GraphPattern::Join { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right }
        | GraphPattern::LeftJoin { left, right, .. } => {
            visit(left);
            bound_iris(right, out);
        }
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Graph { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Group { inner, .. }
        | GraphPattern::Service { inner, .. } => visit(inner),
        _ => {}
    }
}

/// The subclass closure of the data graph, to match types per class.
struct Classes {
    /// `None`: types are not matched per class (rules may write subclasses).
    supers: Option<rustc_hash::FxHashMap<String, BTreeSet<String>>>,
}

impl Classes {
    fn new(data: &dyn DataGraph, per_class: bool) -> Self {
        if !per_class {
            return Self { supers: None };
        }
        let mut pairs = Vec::new();
        data.with_predicate(rdfs::SUB_CLASS_OF, &mut pairs);
        let mut direct: rustc_hash::FxHashMap<String, Vec<String>> = Default::default();
        for (sub, sup) in pairs {
            if let (Term::NamedNode(sub), Term::NamedNode(sup)) = (sub, sup) {
                direct
                    .entry(sub.into_string())
                    .or_default()
                    .push(sup.into_string());
            }
        }
        let mut supers: rustc_hash::FxHashMap<String, BTreeSet<String>> = Default::default();
        for class in direct.keys() {
            let mut all = BTreeSet::new();
            let mut todo = vec![class.clone()];
            while let Some(c) = todo.pop() {
                for s in direct.get(&c).into_iter().flatten() {
                    if all.insert(s.clone()) {
                        todo.push(s.clone());
                    }
                }
            }
            supers.insert(class.clone(), all);
        }
        Self {
            supers: Some(supers),
        }
    }

    /// A class key with the keys of its superclasses, or plain `rdf:type` when types are not
    /// matched per class.
    fn expand(&self, key: &str, out: &mut BTreeSet<String>) {
        if !is_type_key(key) {
            out.insert(key.to_owned());
            return;
        }
        let Some(supers) = &self.supers else {
            out.insert(rdf::TYPE.as_str().to_owned());
            return;
        };
        out.insert(key.to_owned());
        for s in supers
            .get(&key[TYPE_KEY.len_utf8()..])
            .into_iter()
            .flatten()
        {
            out.insert(type_key(s));
        }
    }

    fn writes(&self, writes: &BTreeSet<String>) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for w in writes {
            self.expand(w, &mut out);
        }
        out
    }

    fn reads(&self, reads: Vec<Option<BTreeSet<String>>>) -> Vec<Option<BTreeSet<String>>> {
        if self.supers.is_some() {
            return reads;
        }
        reads
            .into_iter()
            .map(|r| {
                r.map(|r| {
                    r.into_iter()
                        .map(|k| {
                            if is_type_key(&k) {
                                rdf::TYPE.as_str().to_owned()
                            } else {
                                k
                            }
                        })
                        .collect()
                })
            })
            .collect()
    }

    /// The keys an inferred triple changes: its predicate, and for a type the keys of its
    /// class and superclasses.
    fn keys(&self, t: &Triple) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        out.insert(t.predicate.as_str().to_owned());
        if t.predicate == rdf::TYPE
            && let Term::NamedNode(c) = &t.object
        {
            self.expand(&type_key(c.as_str()), &mut out);
        }
        out
    }
}

/// [`Classes::keys`] per predicate, and per class for types: the same few keys come up for
/// every inferred triple.
#[derive(Default)]
struct Keys {
    predicates: rustc_hash::FxHashMap<String, Vec<String>>,
    classes: rustc_hash::FxHashMap<String, Vec<String>>,
}

impl Keys {
    fn of(&mut self, classes: &Classes, t: &Triple) -> &[String] {
        let (map, name) = match &t.object {
            Term::NamedNode(c) if t.predicate == rdf::TYPE => (&mut self.classes, c.as_str()),
            _ => (&mut self.predicates, t.predicate.as_str()),
        };
        if !map.contains_key(name) {
            map.insert(name.to_owned(), classes.keys(t).into_iter().collect());
        }
        &map[name]
    }
}

/// The triple patterns every solution of a pattern matches, with its variables bound as in
/// the solution: the basic graph patterns outside `UNION`, `OPTIONAL`, `MINUS`, `GRAPH` and
/// aggregates, and through projections that keep their variables.
fn required_triples<'a>(p: &'a GraphPattern, out: &mut Vec<&'a TriplePattern>) {
    match p {
        GraphPattern::Bgp { patterns } => out.extend(patterns),
        GraphPattern::Join { left, right } => {
            required_triples(left, out);
            required_triples(right, out);
        }
        GraphPattern::LeftJoin { left, .. } | GraphPattern::Minus { left, .. } => {
            required_triples(left, out)
        }
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Extend { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Slice { inner, .. } => required_triples(inner, out),
        // A triple stays bound through a projection that keeps all its variables.
        GraphPattern::Project { inner, variables } => {
            let mut inside = Vec::new();
            required_triples(inner, &mut inside);
            let kept = |t: &TermPattern| match t {
                TermPattern::Variable(v) => variables.contains(v),
                _ => true,
            };
            out.extend(inside.into_iter().filter(|t| {
                kept(&t.subject)
                    && kept(&t.object)
                    && match &t.predicate {
                        NamedNodePattern::Variable(v) => variables.contains(v),
                        NamedNodePattern::NamedNode(_) => true,
                    }
            }));
        }
        _ => {}
    }
}

/// The triples of the right of each `OPTIONAL` that it must match, with a variable bound
/// nowhere else in `root`: when the `OPTIONAL` matches nothing, that variable is unbound and
/// the template triple is not constructed, so a copy of the triple is already in the graph.
fn optional_triples<'a>(
    root: &GraphPattern,
    p: &'a GraphPattern,
    out: &mut Vec<&'a TriplePattern>,
) {
    match p {
        GraphPattern::LeftJoin { left, right, .. } => {
            optional_triples(root, left, out);
            optional_triples(root, right, out);
            let mut outside = FxHashSet::default();
            binding_vars(root, right, &mut outside);
            let mut inside = Vec::new();
            required_triples(right, &mut inside);
            let only_inside = |t: &TermPattern| matches!(t, TermPattern::Variable(v) if !outside.contains(v.as_str()));
            out.extend(inside.into_iter().filter(|t| {
                only_inside(&t.subject)
                    || only_inside(&t.object)
                    || matches!(&t.predicate,
                        NamedNodePattern::Variable(v) if !outside.contains(v.as_str()))
            }));
        }
        GraphPattern::Join { left, right } => {
            optional_triples(root, left, out);
            optional_triples(root, right, out);
        }
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Extend { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::OrderBy { inner, .. }
        | GraphPattern::Slice { inner, .. } => optional_triples(root, inner, out),
        _ => {}
    }
}

/// The variables `p` may bind outside `skip` (and more: a variable a projection drops too).
fn binding_vars<'a>(p: &'a GraphPattern, skip: &GraphPattern, out: &mut FxHashSet<&'a str>) {
    if std::ptr::eq(p, skip) {
        return;
    }
    let term = |t: &'a TermPattern, out: &mut FxHashSet<&'a str>| {
        if let TermPattern::Variable(v) = t {
            out.insert(v.as_str());
        }
    };
    match p {
        GraphPattern::Bgp { patterns } => {
            for t in patterns {
                term(&t.subject, out);
                term(&t.object, out);
                if let NamedNodePattern::Variable(v) = &t.predicate {
                    out.insert(v.as_str());
                }
            }
        }
        GraphPattern::Path {
            subject, object, ..
        } => {
            term(subject, out);
            term(object, out);
        }
        GraphPattern::Extend {
            inner, variable, ..
        } => {
            out.insert(variable.as_str());
            binding_vars(inner, skip, out);
        }
        GraphPattern::Values { variables, .. } => {
            out.extend(variables.iter().map(|v| v.as_str()));
        }
        GraphPattern::Project { inner, variables } => {
            out.extend(variables.iter().map(|v| v.as_str()));
            binding_vars(inner, skip, out);
        }
        GraphPattern::Group {
            inner,
            variables,
            aggregates,
        } => {
            out.extend(variables.iter().map(|v| v.as_str()));
            out.extend(aggregates.iter().map(|(v, _)| v.as_str()));
            binding_vars(inner, skip, out);
        }
        GraphPattern::Graph { name, inner } => {
            if let NamedNodePattern::Variable(v) = name {
                out.insert(v.as_str());
            }
            binding_vars(inner, skip, out);
        }
        GraphPattern::Service { name, inner, .. } => {
            if let NamedNodePattern::Variable(v) = name {
                out.insert(v.as_str());
            }
            binding_vars(inner, skip, out);
        }
        GraphPattern::Join { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right }
        | GraphPattern::LeftJoin { left, right, .. }
        | GraphPattern::Lateral { left, right } => {
            binding_vars(left, skip, out);
            binding_vars(right, skip, out);
        }
        GraphPattern::Filter { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. }
        | GraphPattern::OrderBy { inner, .. } => binding_vars(inner, skip, out),
    }
}

fn has_blank_node(t: &TriplePattern) -> bool {
    matches!(t.subject, TermPattern::BlankNode(_)) || matches!(t.object, TermPattern::BlankNode(_))
}

/// What a rule writes (see [`TYPE_KEY`]), or `None` when it may write any predicate.
pub(crate) fn rule_writes(rule: &Rule) -> Option<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    match &rule.kind {
        RuleKind::Triple {
            predicate, object, ..
        } => match predicate {
            NodeExpr::Constant(Term::NamedNode(n)) => {
                let class = match object {
                    NodeExpr::Constant(Term::NamedNode(c)) => Some(c.as_str()),
                    _ => None,
                };
                out.insert(key_of(n.as_str(), class));
            }
            _ => return None,
        },
        RuleKind::Sparql(q) => {
            let Query::Construct {
                template, pattern, ..
            } = &q.query
            else {
                return Some(out);
            };
            let mut bound = rustc_hash::FxHashMap::default();
            bound_iris(pattern, &mut bound);
            let mut matched = Vec::new();
            required_triples(pattern, &mut matched);
            // The WHERE clause, under the projection of all its variables.
            let mut body = pattern;
            while let GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner } = body
            {
                body = inner;
            }
            optional_triples(body, body, &mut matched);
            for t in template {
                // A copy of a triple the WHERE clause must match is already in the graph, so it
                // never adds a triple and feeds no rule (it is still constructed, as for
                // `--all-constructed`).
                if !has_blank_node(t) && matched.contains(&t) {
                    continue;
                }
                let class = match &t.object {
                    TermPattern::NamedNode(c) => Some(c.as_str()),
                    _ => None,
                };
                match &t.predicate {
                    NamedNodePattern::NamedNode(n) => {
                        out.insert(key_of(n.as_str(), class));
                    }
                    // A predicate a BIND picks among constants.
                    NamedNodePattern::Variable(v) => match bound.get(v.as_str()) {
                        Some(Some(iris)) => {
                            for p in iris {
                                out.insert(key_of(p, class));
                            }
                        }
                        _ => return None,
                    },
                }
            }
        }
    }
    Some(out)
}

/// The rules (by index, in `sh:order`) in strata: groups of rules that depend on each other,
/// in an order where every group comes after the groups it depends on (and otherwise in
/// `sh:order`), each with whether it depends on itself (and so must run to a fixpoint).
/// Rule `b` depends on rule `a` when `a` may write a predicate `b` reads; `positive` are the
/// predicates each rule reads where more triples may give it more results.
fn strata(
    reads: &[Option<BTreeSet<String>>],
    positive: &[Option<BTreeSet<String>>],
    writes: &[Option<BTreeSet<String>>],
) -> Vec<(Vec<usize>, bool)> {
    let n = reads.len();
    let feeds = |reads: &Option<BTreeSet<String>>, a: usize| match (&writes[a], reads) {
        (None, _) | (_, None) => true,
        (Some(w), Some(r)) => meets(w, r),
    };
    let depends = |b: usize, a: usize| feeds(&reads[b], a);
    // Tarjan's strongly connected components over "b depends on a" edges b -> a.
    struct Tarjan<'f> {
        edges: &'f dyn Fn(usize, usize) -> bool,
        n: usize,
        index: Vec<Option<usize>>,
        low: Vec<usize>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
        next: usize,
        components: Vec<Vec<usize>>,
    }
    impl Tarjan<'_> {
        fn visit(&mut self, v: usize) {
            self.index[v] = Some(self.next);
            self.low[v] = self.next;
            self.next += 1;
            self.stack.push(v);
            self.on_stack[v] = true;
            for w in 0..self.n {
                if !(self.edges)(v, w) {
                    continue;
                }
                match self.index[w] {
                    None => {
                        self.visit(w);
                        self.low[v] = self.low[v].min(self.low[w]);
                    }
                    Some(i) if self.on_stack[w] => self.low[v] = self.low[v].min(i),
                    Some(_) => {}
                }
            }
            if Some(self.low[v]) == self.index[v] {
                let mut c = Vec::new();
                while let Some(w) = self.stack.pop() {
                    self.on_stack[w] = false;
                    c.push(w);
                    if w == v {
                        break;
                    }
                }
                c.sort_unstable();
                self.components.push(c);
            }
        }
    }
    let mut t = Tarjan {
        edges: &depends,
        n,
        index: vec![None; n],
        low: vec![0; n],
        on_stack: vec![false; n],
        stack: Vec::new(),
        next: 0,
        components: Vec::new(),
    };
    for v in 0..n {
        if t.index[v].is_none() {
            t.visit(v);
        }
    }
    let components = t.components;
    let mut of = vec![0; n];
    for (c, members) in components.iter().enumerate() {
        for &m in members {
            of[m] = c;
        }
    }
    // Kahn's order over the groups, the group with the first rule in `sh:order` first.
    let k = components.len();
    let mut needs: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); k];
    for b in 0..n {
        for a in 0..n {
            if of[a] != of[b] && depends(b, a) {
                needs[of[b]].insert(of[a]);
            }
        }
    }
    let mut done = vec![false; k];
    let mut out = Vec::with_capacity(k);
    while out.len() < k {
        let next = (0..k)
            .filter(|&c| !done[c] && needs[c].iter().all(|&d| done[d]))
            .min_by_key(|&c| components[c][0])
            .expect("the groups form an acyclic graph");
        done[next] = true;
        let group = components[next].clone();
        // A group runs to a fixpoint when one of its rules may get new results from what a
        // rule of the group infers (not only fewer, through a negation).
        let recursive = group
            .iter()
            .any(|&b| group.iter().any(|&a| feeds(&positive[b], a)));
        out.push((group, recursive));
    }
    out
}

/// What a rule reads in the data graph (to find its focus nodes, test its conditions and
/// compute its triples): every predicate, and those it reads where more triples may give it
/// more results (not only under `NOT EXISTS` or on the right of `MINUS`). `None` when it may
/// read any predicate.
pub(crate) struct RuleReads {
    pub all: BTreeSet<String>,
    pub positive: BTreeSet<String>,
}

pub(crate) fn rule_reads(shapes: &Shapes, rule: &Rule) -> Option<RuleReads> {
    let mut r = Reads::new(shapes);
    r.shape(rule.shape)?;
    for &c in &rule.conditions {
        r.shape(c)?;
    }
    match &rule.kind {
        RuleKind::Triple {
            subject,
            predicate,
            object,
        } => {
            r.node_expr(subject)?;
            r.node_expr(predicate)?;
            r.node_expr(object)?;
        }
        RuleKind::Sparql(q) => r.query(&q.query)?,
    }
    Some(RuleReads {
        all: r.out,
        positive: r.positive,
    })
}

/// What a rule's focus nodes depend on: the predicates its shape's targets and its
/// conditions read (under negations too: more triples there can remove focus nodes). `None`
/// when they may read any predicate.
pub(crate) fn focus_reads(shapes: &Shapes, rule: &Rule) -> Option<BTreeSet<String>> {
    let mut r = Reads::new(shapes);
    r.targets(rule.shape)?;
    for &c in &rule.conditions {
        r.shape(c)?;
    }
    Some(r.out)
}

/// `f` of each item, in their order, on up to `threads` threads at once (#891).
fn par_map<T: Send, R: Send>(threads: usize, items: Vec<T>, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let threads = threads.min(items.len());
    if threads <= 1 {
        return items.into_iter().map(f).collect();
    }
    let slots: Vec<std::sync::Mutex<Option<T>>> = items
        .into_iter()
        .map(|t| std::sync::Mutex::new(Some(t)))
        .collect();
    let out: Vec<std::sync::Mutex<Option<R>>> =
        slots.iter().map(|_| std::sync::Mutex::new(None)).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(slot) = slots.get(k) else { break };
                    let item = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                    if let Some(item) = item {
                        let r = f(item);
                        *out[k].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                    }
                }
            });
        }
    });
    out.into_iter()
        .map(|r| {
            r.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .expect("every item mapped")
        })
        .collect()
}

/// Focus nodes found per shape and conditions, with how many triples were inferred then.
type Found = rustc_hash::FxHashMap<(ShapeId, Vec<ShapeId>), (usize, Vec<Term>)>;

/// What [`Inference::foci`] needs to reuse focus nodes found before.
struct Reuse<'m> {
    /// Shared by the rules of a pass that run at once (#891).
    found: &'m std::sync::Mutex<Found>,
    /// What the rule's focus nodes depend on ([`focus_reads`]); `None`: anything.
    reads: Option<&'m BTreeSet<String>>,
    /// Per key, how many triples were inferred when one with it was last inferred.
    latest: &'m rustc_hash::FxHashMap<String, usize>,
    /// How many triples are inferred now.
    now: usize,
}

struct Reads<'a> {
    shapes: &'a Shapes,
    out: BTreeSet<String>,
    positive: BTreeSet<String>,
    /// Inside a negation (`NOT EXISTS`, the right of `MINUS`): more triples there can only
    /// remove results, so the rule need not run again for them.
    negated: bool,
    /// Inside a construct where more triples may add and remove results (`OPTIONAL`,
    /// aggregates, `IF`, ...): every predicate counts as positive.
    opaque: bool,
    /// Variables a FILTER around the pattern being walked keeps to a few constant IRIs.
    filtered: rustc_hash::FxHashMap<String, BTreeSet<String>>,
    seen: FxHashSet<ShapeId>,
}

impl<'a> Reads<'a> {
    fn new(shapes: &'a Shapes) -> Self {
        Self {
            shapes,
            out: BTreeSet::new(),
            positive: BTreeSet::new(),
            negated: false,
            opaque: false,
            filtered: Default::default(),
            seen: FxHashSet::default(),
        }
    }

    /// What the targets of a shape read.
    fn targets(&mut self, id: ShapeId) -> Option<()> {
        let shapes = self.shapes;
        for t in &shapes.get(id).targets {
            match t {
                Target::Node(_) => {}
                // Implicit class targets and `sh:targetClass`.
                Target::Class(c) => self.class(c),
                Target::SubjectsOf(p) | Target::ObjectsOf(p) => self.add(p.as_str()),
                Target::Sparql(i) => self.query(&shapes.target_queries[*i as usize].query)?,
            }
        }
        Some(())
    }

    fn add(&mut self, predicate: &str) {
        if !self.negated || self.opaque {
            self.positive.insert(predicate.to_owned());
        }
        self.out.insert(predicate.to_owned());
    }

    fn add_path(&mut self, path: &Path) {
        let mut predicates = BTreeSet::new();
        path.predicates(&mut predicates);
        for p in &predicates {
            self.add(p);
        }
    }

    /// Walks `f` inside a negation.
    fn negate(&mut self, f: impl FnOnce(&mut Self) -> Option<()>) -> Option<()> {
        let before = self.negated;
        self.negated = !before;
        let out = f(self);
        self.negated = before;
        out
    }

    /// Walks `f` inside a construct where more triples may add and remove results.
    fn opaque(&mut self, f: impl FnOnce(&mut Self) -> Option<()>) -> Option<()> {
        let before = self.opaque;
        self.opaque = true;
        let out = f(self);
        self.opaque = before;
        out
    }

    /// The instances of `class` (and of its subclasses).
    fn class(&mut self, class: &Term) {
        match class {
            Term::NamedNode(c) => self.add(&type_key(c.as_str())),
            _ => self.add(rdf::TYPE.as_str()),
        }
        self.add(rdfs::SUB_CLASS_OF.as_str());
    }

    fn shape(&mut self, id: ShapeId) -> Option<()> {
        if !self.seen.insert(id) {
            return Some(());
        }
        let shapes = self.shapes;
        let s = shapes.get(id);
        if let Some(p) = &s.path {
            self.add_path(p);
        }
        self.targets(id)?;
        for c in &s.components {
            match c {
                Component::Class(c) => self.class(c),
                Component::Equals(p)
                | Component::Disjoint(p)
                | Component::LessThan(p)
                | Component::LessThanOrEquals(p) => self.add(p.as_str()),
                Component::Closed { .. } => return None,
                Component::Not(x) | Component::Node(x) | Component::Property(x) => {
                    self.shape(*x)?
                }
                Component::And(xs) | Component::Or(xs) | Component::Xone(xs) => {
                    for x in xs {
                        self.shape(*x)?;
                    }
                }
                Component::Qualified {
                    shape, siblings, ..
                } => {
                    self.shape(*shape)?;
                    for x in siblings {
                        self.shape(*x)?;
                    }
                }
                Component::Sparql(i) => self.query(&shapes.checks[*i as usize].query.query)?,
                _ => {}
            }
        }
        Some(())
    }

    fn node_expr(&mut self, e: &NodeExpr) -> Option<()> {
        match e {
            NodeExpr::This | NodeExpr::Constant(_) => {}
            NodeExpr::Path { path, nodes } => {
                self.add_path(path);
                self.node_expr(nodes)?;
            }
            NodeExpr::Filter { shape, nodes } => {
                self.shape(*shape)?;
                self.node_expr(nodes)?;
            }
            NodeExpr::Intersection(list) | NodeExpr::Union(list) => {
                for e in list {
                    self.node_expr(e)?;
                }
            }
        }
        Some(())
    }

    fn query(&mut self, q: &Query) -> Option<()> {
        match q {
            Query::Select { pattern, .. }
            | Query::Construct { pattern, .. }
            | Query::Describe { pattern, .. }
            | Query::Ask { pattern, .. } => self.pattern(pattern),
        }
    }

    fn path(&mut self, p: &PropertyPathExpression) -> Option<()> {
        match p {
            PropertyPathExpression::NamedNode(n) => self.add(n.as_str()),
            PropertyPathExpression::Reverse(p)
            | PropertyPathExpression::ZeroOrMore(p)
            | PropertyPathExpression::OneOrMore(p)
            | PropertyPathExpression::ZeroOrOne(p)
            | PropertyPathExpression::Bounded { path: p, .. } => self.path(p)?,
            PropertyPathExpression::Sequence(a, b) | PropertyPathExpression::Alternative(a, b) => {
                self.path(a)?;
                self.path(b)?;
            }
            PropertyPathExpression::NegatedPropertySet(_) => return None,
        }
        Some(())
    }

    /// What an expression reads: the patterns of its `EXISTS` and `NOT EXISTS`.
    fn expression(&mut self, e: &Expression) -> Option<()> {
        match e {
            Expression::NamedNode(_)
            | Expression::Literal(_)
            | Expression::Variable(_)
            | Expression::Bound(_) => {}
            Expression::Or(a, b) | Expression::And(a, b) => {
                self.expression(a)?;
                self.expression(b)?;
            }
            Expression::Not(a) => self.negate(|r| r.expression(a))?,
            Expression::Exists(p) => self.pattern(p)?,
            // Elsewhere an EXISTS may count either way.
            _ => self.opaque(|r| r.operands(e))?,
        }
        Some(())
    }

    fn operands(&mut self, e: &Expression) -> Option<()> {
        match e {
            Expression::NamedNode(_)
            | Expression::Literal(_)
            | Expression::Variable(_)
            | Expression::Bound(_) => {}
            Expression::Or(a, b)
            | Expression::And(a, b)
            | Expression::Equal(a, b)
            | Expression::SameTerm(a, b)
            | Expression::Greater(a, b)
            | Expression::GreaterOrEqual(a, b)
            | Expression::Less(a, b)
            | Expression::LessOrEqual(a, b)
            | Expression::Add(a, b)
            | Expression::Subtract(a, b)
            | Expression::Multiply(a, b)
            | Expression::Divide(a, b) => {
                self.expression(a)?;
                self.expression(b)?;
            }
            Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
                self.expression(a)?
            }
            Expression::Exists(p) => self.pattern(p)?,
            Expression::In(a, list) => {
                self.expression(a)?;
                for e in list {
                    self.expression(e)?;
                }
            }
            Expression::If(a, b, c) => {
                self.expression(a)?;
                self.expression(b)?;
                self.expression(c)?;
            }
            Expression::Coalesce(list) | Expression::FunctionCall(_, list) => {
                for e in list {
                    self.expression(e)?;
                }
            }
            #[allow(unreachable_patterns)]
            _ => return None,
        }
        Some(())
    }

    #[allow(unreachable_patterns)]
    fn pattern(&mut self, p: &GraphPattern) -> Option<()> {
        match p {
            GraphPattern::Bgp { patterns } => {
                for t in patterns {
                    match &t.predicate {
                        NamedNodePattern::NamedNode(n) => {
                            let class = match &t.object {
                                TermPattern::NamedNode(c) => Some(c.as_str()),
                                _ => None,
                            };
                            self.add(&key_of(n.as_str(), class));
                        }
                        // A predicate a FILTER around it keeps to a few constants.
                        NamedNodePattern::Variable(v) => {
                            let iris = self.filtered.get(v.as_str())?.clone();
                            for p in &iris {
                                self.add(p);
                            }
                        }
                    }
                    // Triple terms name predicates of their own.
                    if !matches!(
                        t.subject,
                        TermPattern::NamedNode(_)
                            | TermPattern::BlankNode(_)
                            | TermPattern::Variable(_)
                    ) || !matches!(
                        t.object,
                        TermPattern::NamedNode(_)
                            | TermPattern::BlankNode(_)
                            | TermPattern::Literal(_)
                            | TermPattern::Variable(_)
                    ) {
                        return None;
                    }
                }
            }
            GraphPattern::Path { path, .. } => self.path(path)?,
            GraphPattern::Join { left, right } | GraphPattern::Union { left, right } => {
                self.pattern(left)?;
                self.pattern(right)?;
            }
            GraphPattern::Minus { left, right } => {
                self.pattern(left)?;
                self.negate(|r| r.pattern(right))?;
            }
            GraphPattern::LeftJoin {
                left,
                right,
                expression,
            } => {
                self.pattern(left)?;
                self.opaque(|r| {
                    // The OPTIONAL's filter keeps its right side to the predicates it allows.
                    let before = r.filtered.clone();
                    if let Some(e) = expression {
                        filter_iris(e, &mut r.filtered);
                    }
                    let out = r.pattern(right);
                    r.filtered = before;
                    out?;
                    match expression {
                        Some(e) => r.expression(e),
                        None => Some(()),
                    }
                })?;
            }
            GraphPattern::Filter { expr, inner } => {
                self.expression(expr)?;
                let before = self.filtered.clone();
                filter_iris(expr, &mut self.filtered);
                let out = self.pattern(inner);
                self.filtered = before;
                out?;
            }
            GraphPattern::Extend {
                inner, expression, ..
            } => {
                self.opaque(|r| r.expression(expression))?;
                self.pattern(inner)?;
            }
            GraphPattern::OrderBy { inner, expression } => {
                self.opaque(|r| {
                    for e in expression {
                        match e {
                            OrderExpression::Asc(e) | OrderExpression::Desc(e) => {
                                r.expression(e)?
                            }
                        }
                    }
                    Some(())
                })?;
                self.pattern(inner)?;
            }
            GraphPattern::Group {
                inner, aggregates, ..
            } => self.opaque(|r| {
                for (_, a) in aggregates {
                    if let AggregateExpression::FunctionCall { expr, .. } = a {
                        r.expression(expr)?;
                    }
                }
                r.pattern(inner)
            })?,
            GraphPattern::Slice { inner, .. } => self.opaque(|r| r.pattern(inner))?,
            GraphPattern::Graph { inner, .. }
            | GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::Service { inner, .. } => self.pattern(inner)?,
            GraphPattern::Values { .. } => {}
            _ => return None,
        }
        Some(())
    }
}
