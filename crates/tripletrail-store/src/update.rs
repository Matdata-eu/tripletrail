//! SPARQL 1.1 Update on a [`Store`].
//!
//! The whole request is one transaction. `DELETE`/`INSERT ... WHERE` evaluates its pattern with
//! the evaluator the caller passes (so custom functions and SERVICE handlers apply), on the
//! data as the earlier operations of the request left it.
//!
//! Graphs exist here as long as they hold quads: `CREATE GRAPH` records nothing, and only
//! fails (without `SILENT`) when the graph already has quads.

use crate::{Changes, Store, StoreError, Transaction};
use oxigraph::model::{
    BlankNode, GraphName, GraphNameRef, Literal, NamedNode, NamedOrBlankNode, Quad, Term, Triple,
};
use oxigraph::sparql::{QueryEvaluationError, QueryResults, QuerySolution, SparqlEvaluator};
use rustc_hash::FxHashMap;
use spargebra::algebra::{GraphPattern, GraphTarget};
use spargebra::term::Variable;
use spargebra::term::{
    GraphNamePattern, GroundQuadPattern, GroundTerm, GroundTermPattern, GroundTriple,
    NamedNodePattern, QuadPattern, TermPattern,
};
use spargebra::{GraphUpdateOperation, Query, Update};

/// The IRI that names the stored default graph in the `USING` of an update, as in Jena:
/// `USING <urn:x-arq:DefaultGraph>` reads that graph even when the store reads the union of
/// all graphs as its default graph. TripleTrail gives it to `COPY`, `MOVE` and `ADD DEFAULT` on
/// such a store, which read the stored default graph as Fuseki does (#782).
pub const STORED_DEFAULT_GRAPH: &str = "urn:x-arq:DefaultGraph";

/// Reads [`STORED_DEFAULT_GRAPH`] in the default graphs of `dataset` as the stored default
/// graph.
pub fn read_stored_default_graph(dataset: &mut oxigraph::sparql::QueryDataset) {
    let Some(graphs) = dataset.default_graph_graphs() else {
        return;
    };
    if !graphs
        .iter()
        .any(|g| matches!(g, GraphName::NamedNode(n) if n.as_str() == STORED_DEFAULT_GRAPH))
    {
        return;
    }
    let graphs = graphs
        .iter()
        .map(|g| match g {
            GraphName::NamedNode(n) if n.as_str() == STORED_DEFAULT_GRAPH => {
                GraphName::DefaultGraph
            }
            g => g.clone(),
        })
        .collect();
    dataset.set_default_graph(graphs);
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Query(#[from] QueryEvaluationError),
    #[error("{0}")]
    Graph(String),
    /// `CREATE` of a graph that exists, without `SILENT`: the request's fault.
    #[error("graph {0} already exists")]
    GraphAlreadyExists(String),
    /// `CLEAR` or `DROP` of a graph that does not exist, without `SILENT`.
    #[error("graph {0} does not exist")]
    GraphDoesNotExist(String),
    /// A write row rules do not allow (#202).
    #[error("{0}")]
    Forbidden(String),
}

/// Compiles row rules on a view of the data.
/// A filter of the rules on a view of the data that judges at least the given resources
/// (sorted): what it says of other resources is not looked at (#700).
pub type RowsOn<'a> = dyn Fn(&crate::Snapshot, &[u64]) -> Result<crate::RowFilter, String> + 'a;

/// Row rules on an update (#202). Its `WHERE` patterns read the data without what the rules
/// hide on the data before the update (`before`), so a wildcard `DELETE` never matches a hidden
/// quad. Of what the update would change, a removed quad those rules hide is kept (deleting
/// what one cannot see changes nothing, as for a quad that is not there), and an added quad they
/// would hide (into a hidden resource, linking to one, or of a hidden property) refuses the
/// update. And every subject the update changes must still be visible on the data after it
/// (`after`, judged on the transaction's view), or the update is refused: an edit cannot move
/// a resource out of the caller's own view.
pub struct RowGuard<'a> {
    /// The filter of the rules on the data before the update, made in the transaction.
    pub before: &'a dyn Fn() -> Result<std::sync::Arc<crate::RowFilter>, String>,
    /// The filter of the rules on a view of the data, for the subjects the update changes.
    pub after: &'a RowsOn<'a>,
    /// With `sg:editableIf`: a filter on a view of the data whose hidden resources the caller
    /// may not change. Every subject the update changes must pass it on the data before and
    /// after the update.
    pub editable: Option<&'a RowsOn<'a>>,
}

impl Store {
    /// Runs a SPARQL update as one transaction and returns what it changed. `evaluator` makes
    /// the evaluator for the `WHERE` patterns.
    pub fn update(
        &self,
        update: &Update,
        evaluator: impl Fn() -> SparqlEvaluator + Sync,
    ) -> Result<Changes, UpdateError> {
        self.update_with(update, evaluator, false)
    }

    /// As [`Store::update`]; with `union_default_graph`, the `WHERE` patterns of operations
    /// without `USING` read the union of all graphs as their default graph (inserts and
    /// deletes without `GRAPH` still go to the default graph).
    pub fn update_with(
        &self,
        update: &Update,
        evaluator: impl Fn() -> SparqlEvaluator + Sync,
        union_default_graph: bool,
    ) -> Result<Changes, UpdateError> {
        self.update_prepared(update, evaluator, union_default_graph, None)
    }

    /// As [`Store::update_with`]; `prepare` rewrites the `WHERE` pattern of each
    /// `DELETE`/`INSERT` operation before it is evaluated, given the variables its templates
    /// read and a way to evaluate patterns as that operation's `WHERE` is evaluated (on the
    /// data as the earlier operations left it, with its dataset and base IRI).
    pub fn update_prepared(
        &self,
        update: &Update,
        evaluator: impl Fn() -> SparqlEvaluator + Sync,
        union_default_graph: bool,
        prepare: Option<&PrepareWhere<'_>>,
    ) -> Result<Changes, UpdateError> {
        self.update_guarded(update, evaluator, union_default_graph, prepare, None)
    }

    /// As [`Store::update_prepared`], under row rules when `guard` is given (see
    /// [`RowGuard`]).
    pub fn update_guarded(
        &self,
        update: &Update,
        evaluator: impl Fn() -> SparqlEvaluator + Sync,
        union_default_graph: bool,
        prepare: Option<&PrepareWhere<'_>>,
        guard: Option<RowGuard<'_>>,
    ) -> Result<Changes, UpdateError> {
        self.write_guarded(guard, |t, before| {
            apply_all(t, update, &evaluator, union_default_graph, prepare, before)
        })
    }

    /// Applies `update` in transaction `t` (of this store), its `WHERE` patterns reading
    /// what `rows` does not hide: for [`Store::write_guarded`].
    pub fn apply_update(
        &self,
        t: &mut Transaction<'_>,
        update: &Update,
        evaluator: impl Fn() -> SparqlEvaluator + Sync,
        union_default_graph: bool,
        rows: Option<&std::sync::Arc<crate::RowFilter>>,
    ) -> Result<(), UpdateError> {
        apply_all(t, update, &evaluator, union_default_graph, None, rows)
    }

    /// Runs `write` in one transaction, under row rules when `guard` is given (see
    /// [`RowGuard`]): `write` gets the filter of the rules on the data before it, and what it
    /// changed is checked against them before the transaction commits.
    pub fn write_guarded(
        &self,
        guard: Option<RowGuard<'_>>,
        write: impl FnOnce(
            &mut Transaction<'_>,
            Option<&std::sync::Arc<crate::RowFilter>>,
        ) -> Result<(), UpdateError>,
    ) -> Result<Changes, UpdateError> {
        let ((), changes) = self.transaction_with_changes(|t| {
            let before = match &guard {
                Some(g) => Some((g.before)().map_err(UpdateError::Forbidden)?),
                None => None,
            };
            if let Some(before) = &before {
                t.judge_adds(std::sync::Arc::clone(before));
            }
            write(t, before.as_ref())?;
            if let (Some(guard), Some(before)) = (&guard, &before) {
                check_rows(t, before, guard)?;
            }
            Ok::<_, UpdateError>(())
        })?;
        Ok(changes)
    }
}

fn apply_all(
    t: &mut Transaction<'_>,
    update: &Update,
    evaluator: &(impl Fn() -> SparqlEvaluator + Sync),
    union_default_graph: bool,
    prepare: Option<&PrepareWhere<'_>>,
    rows: Option<&std::sync::Arc<crate::RowFilter>>,
) -> Result<(), UpdateError> {
    for op in &update.operations {
        apply(t, op, update, evaluator, union_default_graph, prepare, rows)?;
    }
    Ok(())
}

/// Rows of a pattern's variables, in the order asked for, as `VALUES` holds them.
pub type WhereRows = Vec<Vec<Option<GroundTerm>>>;

/// What [`RunWhere`] read of a pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhereRead {
    /// All its rows.
    Rows(WhereRows),
    /// It had more than the given number of rows; reading stopped there.
    TooMany,
    /// A value was a blank node or a triple term, which `VALUES` can't hold; reading stopped
    /// there.
    BlankNode,
}

/// Evaluates `SELECT <variables> WHERE { <pattern> }` as an operation's `WHERE` is evaluated,
/// reading at most the given number of rows.
pub type RunWhere<'a> =
    dyn Fn(GraphPattern, &[Variable], usize) -> Result<WhereRead, UpdateError> + Sync + 'a;

/// Rewrites an operation's `WHERE` pattern (see [`Store::update_prepared`]).
pub type PrepareWhere<'a> =
    dyn Fn(GraphPattern, &[Variable], &RunWhere<'_>) -> Result<GraphPattern, UpdateError> + 'a;

/// An `INSERT … WHERE` without `DELETE` puts its quads into the transaction this many at a time.
const INSERT_BATCH: usize = if cfg!(test) { 64 } else { 1 << 14 };

/// The instantiated `INSERT` quads are rid of duplicates once there are this many, and again
/// whenever they double.
const DEDUP_AT: usize = if cfg!(test) { 64 } else { 1 << 20 };

/// Removes repeated quads, keeping the first of each and the order: quads are hashed and the
/// hashes sorted on every core, and only quads with equal hashes compared.
fn dedup(quads: &mut Vec<Quad>) {
    use rayon::prelude::*;
    use std::hash::BuildHasher;
    let hasher = rustc_hash::FxBuildHasher;
    let mut keyed: Vec<(u64, usize)> = quads
        .par_iter()
        .enumerate()
        .map(|(i, q)| (hasher.hash_one(q), i))
        .collect();
    keyed.par_sort_unstable();
    let mut keep = vec![true; quads.len()];
    let mut kept = Vec::new();
    for run in keyed.chunk_by(|a, b| a.0 == b.0) {
        kept.clear();
        // In index order: the first of equal quads is kept.
        for &(_, i) in run {
            if kept.iter().any(|&k: &usize| quads[k] == quads[i]) {
                keep[i] = false;
            } else {
                kept.push(i);
            }
        }
    }
    let mut keep = keep.into_iter();
    quads.retain(|_| keep.next().unwrap_or(true));
}

/// What a guarded update changed is checked against the rules (see [`RowGuard`]).
fn check_rows(
    t: &mut Transaction<'_>,
    before: &crate::RowFilter,
    guard: &RowGuard<'_>,
) -> Result<(), UpdateError> {
    let mut kept = Vec::new();
    let mut touched = rustc_hash::FxHashSet::default();
    // Every quad asked to be added, there already or not, so that the answer does not tell
    // whether a hidden quad exists.
    if let Some(q) = t.refused() {
        let subject = t.view().term(crate::TermId(q[0]))?;
        return Err(UpdateError::Forbidden(format!(
            "Row-level rules do not let you write this about {subject}: it, a resource \
             it links to or the property is hidden from you"
        )));
    }
    for (q, added) in t.pending_ids() {
        if !before.hides(&q) {
            touched.insert(q[0]);
            continue;
        }
        // Added quads were judged above.
        debug_assert!(!added);
        kept.push(q);
    }
    for q in kept {
        t.keep(q);
    }
    if touched.is_empty() {
        return Ok(());
    }
    let mut touched: Vec<u64> = touched.into_iter().collect();
    touched.sort_unstable();
    if let Some(editable) = guard.editable {
        // Of what the caller sees: hidden quads were judged above, so this tells nothing new.
        for view in [t.base_view(), t.view()] {
            let filter = editable(&view, &touched).map_err(UpdateError::Forbidden)?;
            if let Some(&s) = touched
                .iter()
                .find(|&&s| filter.hides_resource(crate::TermId(s)))
            {
                let subject = t.view().term(crate::TermId(s))?;
                return Err(UpdateError::Forbidden(format!(
                    "Row-level rules do not let you edit {subject} (sg:editableIf)"
                )));
            }
        }
    }
    let after = (guard.after)(&t.view(), &touched).map_err(UpdateError::Forbidden)?;
    for s in touched {
        if after.hides_resource(crate::TermId(s)) {
            let subject = t.view().term(crate::TermId(s))?;
            return Err(UpdateError::Forbidden(format!(
                "Row-level rules would hide {subject} from you after this update: an update \
                 may not move a resource out of what you see"
            )));
        }
    }
    Ok(())
}

fn apply(
    t: &mut Transaction<'_>,
    op: &GraphUpdateOperation,
    update: &Update,
    evaluator: &(impl Fn() -> SparqlEvaluator + Sync),
    union_default_graph: bool,
    prepare: Option<&PrepareWhere<'_>>,
    rows: Option<&std::sync::Arc<crate::RowFilter>>,
) -> Result<(), UpdateError> {
    match op {
        GraphUpdateOperation::InsertData { data } => {
            let mut bnodes = FxHashMap::default();
            let quads = data
                .iter()
                .map(|q| {
                    let subject = match &q.subject {
                        NamedOrBlankNode::BlankNode(b) => fresh(b, &mut bnodes).into(),
                        s => s.clone(),
                    };
                    let object = match &q.object {
                        Term::BlankNode(b) => fresh(b, &mut bnodes).into(),
                        o => o.clone(),
                    };
                    Quad::new(subject, q.predicate.clone(), object, graph(&q.graph_name))
                })
                .collect();
            // Looked up in parallel, not one by one: on a store just opened, each lookup can
            // wait on the disk (#437).
            t.insert_all(quads);
        }
        GraphUpdateOperation::DeleteData { data } => {
            for q in data {
                let object: Term = match &q.object {
                    GroundTerm::NamedNode(n) => n.clone().into(),
                    GroundTerm::Literal(l) => l.clone().into(),
                    GroundTerm::Triple(t) => Triple::from(GroundTriple::clone(t)).into(),
                };
                t.remove(&Quad::new(
                    q.subject.clone(),
                    q.predicate.clone(),
                    object,
                    graph(&q.graph_name),
                ));
            }
        }
        GraphUpdateOperation::DeleteInsert {
            delete,
            insert,
            using,
            pattern,
        } => {
            // Only the variables the templates use, each distinct row once when the INSERT
            // template has no blank nodes (each row then makes the same quads however often it
            // comes): the solutions are not kept, only the quads, without duplicates.
            let mut variables = Vec::new();
            for p in delete {
                ground_variables(p, &mut variables);
            }
            let mut bnodes_in_template = false;
            for p in insert {
                bnodes_in_template |= template_variables(p, &mut variables);
            }
            let variable_count = variables.len();
            let union = union_default_graph && using.is_none();
            let pattern = match prepare {
                Some(prepare) => {
                    let base = t.view();
                    let run = |p: GraphPattern,
                               vars: &[Variable],
                               limit: usize|
                     -> Result<WhereRead, UpdateError> {
                        let query = Query::Select {
                            dataset: using.clone(),
                            pattern: GraphPattern::Project {
                                inner: Box::new(p),
                                variables: vars.to_vec(),
                            },
                            base_iri: update.base_iri.clone(),
                        };
                        let mut prepared = evaluator().for_query(query);
                        read_stored_default_graph(prepared.dataset_mut());
                        if union {
                            prepared.dataset_mut().set_default_graph_as_union();
                        }
                        let view =
                            crate::dataset::merged_view(base.clone(), prepared.dataset_mut())
                                .with_rows(rows.cloned());
                        let QueryResults::Solutions(solutions) =
                            crate::dataset::with_estimates(&view, || {
                                prepared.on_queryable_dataset(view.clone()).execute()
                            })?
                        else {
                            return Err(UpdateError::Graph("WHERE did not give solutions".into()));
                        };
                        let mut rows = Vec::new();
                        for s in solutions {
                            let s = s?;
                            if rows.len() == limit {
                                return Ok(WhereRead::TooMany);
                            }
                            let Some(row) = ground_row(vars.iter().map(|v| s.get(v))) else {
                                return Ok(WhereRead::BlankNode);
                            };
                            rows.push(row);
                        }
                        Ok(WhereRead::Rows(rows))
                    };
                    Box::new(prepare((**pattern).clone(), &variables, &run)?)
                }
                None => pattern.clone(),
            };
            // A WHERE that `prepare` computed (rows of `VALUES`) is read as it is.
            let solutions: Box<dyn Iterator<Item = Result<QuerySolution, QueryEvaluationError>>> =
                match static_rows(&pattern, &variables) {
                    Some(rows) => Box::new(rows.into_iter().map(Ok)),
                    None => {
                        let projected = GraphPattern::Project {
                            inner: pattern,
                            variables,
                        };
                        let query = Query::Select {
                            dataset: using.clone(),
                            pattern: if bnodes_in_template {
                                projected
                            } else {
                                GraphPattern::Distinct {
                                    inner: Box::new(projected),
                                }
                            },
                            base_iri: update.base_iri.clone(),
                        };
                        let mut prepared = evaluator().for_query(query);
                        read_stored_default_graph(prepared.dataset_mut());
                        if union {
                            prepared.dataset_mut().set_default_graph_as_union();
                        }
                        let view = crate::dataset::merged_view(t.view(), prepared.dataset_mut())
                            .with_rows(rows.cloned());
                        let QueryResults::Solutions(solutions) =
                            crate::dataset::with_estimates(&view, || {
                                prepared.on_queryable_dataset(view.clone()).execute()
                            })?
                        else {
                            return Err(UpdateError::Graph("WHERE did not give solutions".into()));
                        };
                        Box::new(solutions)
                    }
                };
            if delete.is_empty() {
                // Without a DELETE template, the quads go into the transaction a batch at a
                // time as the rows come (#382): the WHERE reads the view taken before, and the
                // transaction keeps them as ids, so the update holds a few batches, not all.
                return insert_streaming(t, insert, solutions);
            }
            let mut deleted = rustc_hash::FxHashSet::default();
            // Kept in a Vec, which is cheaper to fill than a set, and rid of duplicates
            // whenever it doubles when some row can repeat quads: its size follows the
            // distinct quads.
            let repeats =
                repeating_rows(insert, variable_count, bnodes_in_template).contains(&true);
            let mut inserted = Vec::new();
            let mut dedup_at = DEDUP_AT;
            for s in solutions {
                let s = s?;
                deleted.extend(delete.iter().filter_map(|p| ground(p, &s)));
                let mut bnodes = FxHashMap::default();
                inserted.extend(
                    insert
                        .iter()
                        .filter_map(|p| instantiate(p, &s, &mut bnodes)),
                );
                if repeats && inserted.len() >= dedup_at {
                    dedup(&mut inserted);
                    dedup_at = (inserted.len() * 2).max(DEDUP_AT);
                }
            }
            for q in &deleted {
                t.remove(q);
            }
            t.insert_all(inserted);
        }
        GraphUpdateOperation::Load { silent, .. } => {
            if !silent {
                return Err(UpdateError::Graph(
                    "LOAD is not supported: the store has no HTTP client".into(),
                ));
            }
        }
        GraphUpdateOperation::Create { graph, silent } => {
            if !silent && has_graph(t, graph) {
                return Err(UpdateError::GraphAlreadyExists(graph.to_string()));
            }
        }
        GraphUpdateOperation::Clear { graph, silent }
        | GraphUpdateOperation::Drop { graph, silent } => match graph {
            GraphTarget::NamedNode(g) => {
                if !has_graph(t, g) {
                    if *silent {
                        return Ok(());
                    }
                    return Err(UpdateError::GraphDoesNotExist(g.to_string()));
                }
                remove_all(t, Some(GraphNameRef::NamedNode(g.as_ref())), false);
            }
            GraphTarget::DefaultGraph => remove_all(t, Some(GraphNameRef::DefaultGraph), false),
            GraphTarget::NamedGraphs => remove_all(t, None, true),
            GraphTarget::AllGraphs => remove_all(t, None, false),
        },
    }
    Ok(())
}

/// A row of terms as `VALUES` holds them; `None` for a blank node or a triple term.
fn ground_row<'t>(
    values: impl Iterator<Item = Option<&'t Term>>,
) -> Option<Vec<Option<GroundTerm>>> {
    values
        .map(|t| match t {
            None => Some(None),
            Some(Term::NamedNode(n)) => Some(Some(n.clone().into())),
            Some(Term::Literal(l)) => Some(Some(l.clone().into())),
            Some(_) => None,
        })
        .collect()
}

/// The rows of `p` as solutions of `variables`, when `p` is only `VALUES` blocks (and unions
/// of them): what [`Store::update_prepared`]'s `prepare` gives for a `WHERE` it computed. A
/// user's own `WHERE { VALUES … }` is read this way too; the `DISTINCT` the rows would get is
/// left out, which changes nothing, as the quads are a set.
fn static_rows(p: &GraphPattern, variables: &[Variable]) -> Option<Vec<QuerySolution>> {
    fn walk(
        p: &GraphPattern,
        variables: &std::sync::Arc<[Variable]>,
        out: &mut Vec<QuerySolution>,
    ) -> Option<()> {
        match p {
            GraphPattern::Union { left, right } => {
                walk(left, variables, out)?;
                walk(right, variables, out)
            }
            GraphPattern::Values {
                variables: own,
                bindings,
            } => {
                let at: Vec<Option<usize>> = variables
                    .iter()
                    .map(|v| own.iter().position(|o| o == v))
                    .collect();
                out.extend(bindings.iter().map(|row| {
                    let values: Vec<Option<Term>> = at
                        .iter()
                        .map(|i| {
                            i.and_then(|i| row[i].clone()).map(|t| match t {
                                GroundTerm::NamedNode(n) => n.into(),
                                GroundTerm::Literal(l) => l.into(),
                                GroundTerm::Triple(t) => Triple::from(*t).into(),
                            })
                        })
                        .collect();
                    QuerySolution::from((std::sync::Arc::clone(variables), values))
                }));
                Some(())
            }
            _ => None,
        }
    }
    if !matches!(p, GraphPattern::Values { .. } | GraphPattern::Union { .. }) {
        return None;
    }
    let variables: std::sync::Arc<[Variable]> = variables.into();
    let mut out = Vec::new();
    walk(p, &variables, &mut out)?;
    Some(out)
}

/// Batches of rows the WHERE may run ahead of the inserts of an `INSERT … WHERE`.
const ROWS_AHEAD: usize = 4;

/// Puts the quads of `INSERT … WHERE` rows into the transaction while the WHERE goes on (#292):
/// this thread takes the rows, and a second one makes their quads and looks their terms up and
/// adds them (on every core), a batch at a time. The WHERE no longer waits for the inserts,
/// which for an update that writes a million quads took a fifth of its time.
///
/// The rows are sent only between calls to the WHERE's iterator, which holds no lock on the
/// dictionary then, so the inserts can take its write lock while this thread waits to send.
fn insert_streaming(
    t: &mut Transaction<'_>,
    insert: &[QuadPattern],
    solutions: impl Iterator<Item = Result<QuerySolution, QueryEvaluationError>>,
) -> Result<(), UpdateError> {
    let rows_per_batch = (INSERT_BATCH / insert.len().max(1)).max(1);
    let (send, receive) = std::sync::mpsc::sync_channel::<Vec<QuerySolution>>(ROWS_AHEAD);
    std::thread::scope(|scope| {
        let inserter = scope.spawn(move || {
            for rows in receive {
                let mut quads = Vec::with_capacity(rows.len() * insert.len());
                for s in &rows {
                    let mut bnodes = FxHashMap::default();
                    quads.extend(insert.iter().filter_map(|p| instantiate(p, s, &mut bnodes)));
                }
                drop(rows);
                t.insert_all(quads);
            }
        });
        let mut result = Ok(());
        let mut rows = Vec::with_capacity(rows_per_batch);
        for s in solutions {
            match s {
                Ok(s) => rows.push(s),
                Err(e) => {
                    result = Err(e.into());
                    break;
                }
            }
            if rows.len() == rows_per_batch {
                let full = std::mem::replace(&mut rows, Vec::with_capacity(rows_per_batch));
                if send.send(full).is_err() {
                    break; // the inserter is gone: its panic is raised below
                }
            }
        }
        if result.is_ok() && !rows.is_empty() {
            let _ = send.send(rows);
        }
        drop(send);
        if let Err(panic) = inserter.join() {
            std::panic::resume_unwind(panic);
        }
        result
    })
}

fn graph(g: &spargebra::term::GraphName) -> GraphName {
    match g {
        spargebra::term::GraphName::NamedNode(n) => n.clone().into(),
        spargebra::term::GraphName::DefaultGraph => GraphName::DefaultGraph,
    }
}

fn fresh(b: &BlankNode, bnodes: &mut FxHashMap<BlankNode, BlankNode>) -> BlankNode {
    bnodes.entry(b.clone()).or_default().clone()
}

fn has_graph(t: &Transaction<'_>, g: &NamedNode) -> bool {
    t.view()
        .quads_for_pattern(None, None, None, Some(GraphNameRef::NamedNode(g.as_ref())))
        .next()
        .is_some()
}

/// Removes the quads of `graph` (`None`: every graph, or only named graphs).
fn remove_all(t: &mut Transaction<'_>, graph: Option<GraphNameRef<'_>>, named_only: bool) {
    let quads: Vec<Quad> = t
        .view()
        .quads_for_pattern(None, None, None, graph)
        .filter(|q| !(named_only && q.graph_name.is_default_graph()))
        .collect();
    for q in &quads {
        t.remove(q);
    }
}

fn named_node(p: &NamedNodePattern, s: &QuerySolution) -> Option<NamedNode> {
    match p {
        NamedNodePattern::NamedNode(n) => Some(n.clone()),
        NamedNodePattern::Variable(v) => match s.get(v)? {
            Term::NamedNode(n) => Some(n.clone()),
            _ => None,
        },
    }
}

fn graph_name(p: &GraphNamePattern, s: &QuerySolution) -> Option<GraphName> {
    match p {
        GraphNamePattern::NamedNode(n) => Some(n.clone().into()),
        GraphNamePattern::DefaultGraph => Some(GraphName::DefaultGraph),
        GraphNamePattern::Variable(v) => match s.get(v)? {
            Term::NamedNode(n) => Some(n.clone().into()),
            Term::BlankNode(b) => Some(b.clone().into()),
            _ => None,
        },
    }
}

fn subject(t: Term) -> Option<NamedOrBlankNode> {
    match t {
        Term::NamedNode(n) => Some(n.into()),
        Term::BlankNode(b) => Some(b.into()),
        _ => None,
    }
}

/// A `DELETE` template term for one solution.
fn ground_term(t: &GroundTermPattern, s: &QuerySolution) -> Option<Term> {
    match t {
        GroundTermPattern::NamedNode(n) => Some(n.clone().into()),
        GroundTermPattern::Literal(l) => Some(Literal::clone(l).into()),
        GroundTermPattern::Variable(v) => s.get(v).cloned(),
        GroundTermPattern::Triple(t) => Some(
            Triple::new(
                subject(ground_term(&t.subject, s)?)?,
                named_node(&t.predicate, s)?,
                ground_term(&t.object, s)?,
            )
            .into(),
        ),
    }
}

fn push_variable(v: &Variable, out: &mut Vec<Variable>) {
    if !out.contains(v) {
        out.push(v.clone());
    }
}

fn ground_term_variables(t: &GroundTermPattern, out: &mut Vec<Variable>) {
    match t {
        GroundTermPattern::Variable(v) => push_variable(v, out),
        GroundTermPattern::Triple(t) => {
            ground_term_variables(&t.subject, out);
            if let NamedNodePattern::Variable(v) = &t.predicate {
                push_variable(v, out);
            }
            ground_term_variables(&t.object, out);
        }
        _ => {}
    }
}

/// The variables of a `DELETE` template row.
fn ground_variables(p: &GroundQuadPattern, out: &mut Vec<Variable>) {
    ground_term_variables(&p.subject, out);
    if let NamedNodePattern::Variable(v) = &p.predicate {
        push_variable(v, out);
    }
    ground_term_variables(&p.object, out);
    if let GraphNamePattern::Variable(v) = &p.graph_name {
        push_variable(v, out);
    }
}

/// Adds the variables of an `INSERT` template term; returns whether it has a blank node.
fn term_variables(t: &TermPattern, out: &mut Vec<Variable>) -> bool {
    match t {
        TermPattern::Variable(v) => {
            push_variable(v, out);
            false
        }
        TermPattern::BlankNode(_) => true,
        TermPattern::Triple(t) => {
            let mut b = term_variables(&t.subject, out);
            if let NamedNodePattern::Variable(v) = &t.predicate {
                push_variable(v, out);
            }
            b |= term_variables(&t.object, out);
            b
        }
        _ => false,
    }
}

/// Which `INSERT` template rows can give the same quad for several solutions. Rows with a
/// blank node give new quads for every solution, and so do rows with all `variable_count`
/// variables when the solutions are distinct (no blank nodes in the template): their quads
/// are kept as they come. Other rows repeat quads (as often as there are solutions, for a row
/// without variables), so theirs are kept in a set.
fn repeating_rows(
    insert: &[QuadPattern],
    variable_count: usize,
    bnodes_in_template: bool,
) -> Vec<bool> {
    insert
        .iter()
        .map(|p| {
            let mut own = Vec::new();
            let bnode = template_variables(p, &mut own);
            !bnode && (bnodes_in_template || own.len() < variable_count)
        })
        .collect()
}

/// Adds the variables of an `INSERT` template row; returns whether it has a blank node.
fn template_variables(p: &QuadPattern, out: &mut Vec<Variable>) -> bool {
    let mut b = term_variables(&p.subject, out);
    if let NamedNodePattern::Variable(v) = &p.predicate {
        push_variable(v, out);
    }
    b |= term_variables(&p.object, out);
    if let GraphNamePattern::Variable(v) = &p.graph_name {
        push_variable(v, out);
    }
    b
}

/// A `DELETE` template row for one solution; `None` if a variable is unbound or misplaced.
fn ground(p: &GroundQuadPattern, s: &QuerySolution) -> Option<Quad> {
    Some(Quad::new(
        subject(ground_term(&p.subject, s)?)?,
        named_node(&p.predicate, s)?,
        ground_term(&p.object, s)?,
        graph_name(&p.graph_name, s)?,
    ))
}

/// An `INSERT` template term for one solution, with fresh blank nodes per solution.
fn instantiate_term(
    t: &TermPattern,
    s: &QuerySolution,
    bnodes: &mut FxHashMap<BlankNode, BlankNode>,
) -> Option<Term> {
    match t {
        TermPattern::NamedNode(n) => Some(n.clone().into()),
        TermPattern::Literal(l) => Some(l.clone().into()),
        TermPattern::BlankNode(b) => Some(fresh(b, bnodes).into()),
        TermPattern::Variable(v) => s.get(v).cloned(),
        TermPattern::Triple(t) => {
            let subj = subject(instantiate_term(&t.subject, s, bnodes)?)?;
            let obj = instantiate_term(&t.object, s, bnodes)?;
            Some(Triple::new(subj, named_node(&t.predicate, s)?, obj).into())
        }
    }
}

/// An `INSERT` template row for one solution, with fresh blank nodes per solution.
fn instantiate(
    p: &QuadPattern,
    s: &QuerySolution,
    bnodes: &mut FxHashMap<BlankNode, BlankNode>,
) -> Option<Quad> {
    let subj = subject(instantiate_term(&p.subject, s, bnodes)?)?;
    let obj = instantiate_term(&p.object, s, bnodes)?;
    Some(Quad::new(
        subj,
        named_node(&p.predicate, s)?,
        obj,
        graph_name(&p.graph_name, s)?,
    ))
}

#[cfg(test)]
mod tests {
    use crate::Store;
    use oxigraph::sparql::SparqlEvaluator;

    #[test]
    fn streamed_inserts_cover_every_batch_and_keep_blank_nodes_per_row() {
        // Many batches of rows go to the inserter thread; each row gets its own blank node.
        let store = Store::new();
        let mut data = String::from("PREFIX ex: <http://example.com/> INSERT DATA {");
        for i in 0..2000 {
            data += &format!(" ex:s{i} ex:p {i} .");
        }
        data += " }";
        for u in [
            data.as_str(),
            "PREFIX ex: <http://example.com/> INSERT { ?s ex:q [ ex:v ?v ] . ?s ex:r ex:T } WHERE { ?s ex:p ?v }",
        ] {
            let parsed = spargebra::SparqlParser::new().parse_update(u).unwrap();
            store.update(&parsed, SparqlEvaluator::new).unwrap();
        }
        assert_eq!(store.len(), 2000 * 4);
    }

    #[test]
    fn repeated_template_quads_are_inserted_once() {
        let store = Store::new();
        let mut data = String::from("PREFIX ex: <http://example.com/> INSERT DATA {");
        for i in 0..1000 {
            data += &format!(" ex:s{i} ex:p ex:o{} .", i % 3);
        }
        data += " }";
        // The constant row and `?o ex:b ex:T` repeat across the 1000 solutions, many more
        // times than there are distinct quads.
        for u in [
            data.as_str(),
            "PREFIX ex: <http://example.com/> INSERT { ?s ex:a ex:T . ?o ex:b ex:T . ex:x ex:y ex:z } WHERE { ?s ex:p ?o }",
        ] {
            let parsed = spargebra::SparqlParser::new().parse_update(u).unwrap();
            store.update(&parsed, SparqlEvaluator::new).unwrap();
        }
        assert_eq!(store.len(), 1000 + 1000 + 3 + 1);
        // Rows with a subset of the variables repeat quads (`?s ex:a ex:T` once per `?o`).
        let rows = |u: &str| {
            let parsed = spargebra::SparqlParser::new().parse_update(u).unwrap();
            let spargebra::GraphUpdateOperation::DeleteInsert { insert, .. } =
                &parsed.operations[0]
            else {
                unreachable!()
            };
            let mut variables = Vec::new();
            let bnodes = insert.iter().fold(false, |b, p| {
                super::template_variables(p, &mut variables) | b
            });
            super::repeating_rows(insert, variables.len(), bnodes)
        };
        assert_eq!(
            rows(
                "PREFIX ex: <http://example.com/> INSERT { ?s ex:a ex:T . ?o ex:b ex:T . ex:x ex:y ex:z } WHERE { ?s ex:p ?o }"
            ),
            [true, true, true]
        );
        assert_eq!(
            rows(
                "PREFIX ex: <http://example.com/> INSERT { ?s ex:a [ ex:b ?o ] } WHERE { ?s ex:p ?o }"
            ),
            [false, false]
        );
    }

    #[test]
    fn dedup_keeps_the_first_of_equal_quads_in_order() {
        use oxigraph::model::{GraphName, NamedNode, Quad};
        let q = |i: usize| {
            let n = NamedNode::new_unchecked(format!("http://example.com/{i}"));
            Quad::new(n.clone(), n.clone(), n, GraphName::DefaultGraph)
        };
        let mut quads: Vec<Quad> = [3, 1, 3, 2, 1, 3, 0].into_iter().map(q).collect();
        super::dedup(&mut quads);
        assert_eq!(quads, [3, 1, 2, 0].map(q));
        let mut same = vec![q(7); 10_000];
        super::dedup(&mut same);
        assert_eq!(same, [q(7)]);
    }
}
