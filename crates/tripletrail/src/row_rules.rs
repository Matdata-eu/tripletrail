//! Row rules of [sparql-guard](https://matdata-eu.github.io/sparql-guard/reference/policy-language)
//! policies (#202): `sg:visibleIf` hides the targets of a shape that do not conform to it,
//! `sg:hideProperties` hides the predicates of a shape's property paths on its targets
//! (everywhere without a target), `sg:editableIf` limits what a caller may change.
//!
//! Row-level policies are a feature of TripleTrail Enterprise (#818). This crate holds what a
//! query needs to apply them: the rules, the [`RowFilter`] the store scans with, and the cache
//! of compiled filters. Compiling and judging rules is a [`RowCompiler`], which the enterprise
//! build installs ([`install_row_compiler`]). Without one, a request under row rules is
//! refused: a policy is never enforced in part, nor ignored.

use crate::{Engine, Error, RowFilter};
use oxigraph::model::{Graph, GraphName, NamedNode, Term, TermRef};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tripletrail_geo::sync::LockUnpoisoned;
use tripletrail_store::{Dataset, Snapshot};

/// `sg:requestingAgent`: the agent of the request, in `sh:hasValue`, `sh:in` and `sh:targetNode`.
pub const REQUESTING_AGENT: &str = "https://matdata.eu/ns/sparql-guard#requestingAgent";
/// The agent of an anonymous request, for `sg:requestingAgent`.
pub const ANONYMOUS: &str = "urn:sparql-guard:anonymous";

/// What a request under row rules, or a policy file with them, answers without a
/// [`RowCompiler`].
pub const NEEDS_ENTERPRISE: &str = "Row-level policies (sg:visibleIf, sg:hideProperties, sg:editableIf) need TripleTrail Enterprise";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowRule {
    /// `sg:visibleIf`: the targets of `shape` that do not conform to it are hidden, as subject
    /// and, with `references` (`sg:hideReferences`, true by default), as object.
    VisibleIf { shape: Term, references: bool },
    /// `sg:hideProperties`: the predicates of the property paths of `shape`, on its targets, or
    /// everywhere when it has none.
    HideProperties { shape: Term },
    /// `sg:editableIf` (proposed term): the caller may change only the resources that conform
    /// to `shape` among its targets (of what it sees); it hides nothing from reads.
    EditableIf { shape: Term },
}

impl RowRule {
    pub fn shape(&self) -> &Term {
        match self {
            Self::VisibleIf { shape, .. }
            | Self::HideProperties { shape }
            | Self::EditableIf { shape } => shape,
        }
    }
}

/// Whether the shapes graph mentions `sg:requestingAgent`, so its rules differ per agent.
pub fn depends_on_agent(shapes: &Graph) -> bool {
    shapes
        .iter()
        .any(|t| matches!(t.object, TermRef::NamedNode(n) if n.as_str() == REQUESTING_AGENT))
}

/// Compiles and judges row rules (#202, #818): TripleTrail Enterprise has the one implementation.
/// Every method judges over the quads of `snapshot`, which must not hide anything itself, and
/// stops with [`QueryEvaluationError::Cancelled`] once `cancelled` says so.
///
/// [`QueryEvaluationError::Cancelled`]: oxigraph::sparql::QueryEvaluationError::Cancelled
pub trait RowCompiler: Send + Sync {
    /// Refuses shapes outside the spec's SHACL subset, recursive ones, and `sg:hideProperties`
    /// paths that are not plain IRIs, so a policy is never enforced in part.
    fn check(&self, shapes: &Graph, rules: &[RowRule]) -> Result<(), String>;

    /// The filter of `rules`: what they hide.
    fn compile(
        &self,
        snapshot: &Snapshot,
        rules: &RowRules,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<RowFilter, Error>;

    /// [`RowCompiler::compile`] from the filter of the same rules on an earlier generation
    /// (`before`) and the nodes (`resources`) and `predicates` the writes since touched;
    /// `None` when the rules must be compiled anew.
    fn compile_incremental(
        &self,
        snapshot: &Snapshot,
        rules: &RowRules,
        before: &RowFilter,
        resources: &[u64],
        predicates: &[u64],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<RowFilter>, Error>;

    /// What the `sg:visibleIf` rules hide of `resources` (term ids): the verdicts of
    /// [`RowCompiler::compile`] for those resources only (the update path, #700).
    fn judge(
        &self,
        snapshot: &Snapshot,
        rules: &RowRules,
        resources: &[u64],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<RowFilter, Error>;

    /// What the `sg:editableIf` rules keep the caller from changing of `resources`, as hidden
    /// resources of a filter; `None` without such rules.
    fn judge_editable(
        &self,
        snapshot: &Snapshot,
        rules: &RowRules,
        resources: &[u64],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<RowFilter>, Error>;
}

static COMPILER: OnceLock<Box<dyn RowCompiler>> = OnceLock::new();

/// Installs the compiler of row rules, once per process; a second call keeps the first one.
pub fn install_row_compiler(compiler: Box<dyn RowCompiler>) {
    let _ = COMPILER.set(compiler);
}

/// Whether this process can enforce row-level policies (a [`RowCompiler`] is installed).
pub fn row_policies_available() -> bool {
    COMPILER.get().is_some()
}

fn compiler() -> Result<&'static dyn RowCompiler, Error> {
    COMPILER
        .get()
        .map(|c| &**c)
        .ok_or_else(|| Error::Request(NEEDS_ENTERPRISE.to_owned()))
}

/// Refuses row rule shapes the installed [`RowCompiler`] cannot enforce, and every row rule
/// without one.
pub fn check_row_shapes(shapes: &Graph, rules: &[RowRule]) -> Result<(), String> {
    match COMPILER.get() {
        Some(c) => c.check(shapes, rules),
        None if rules.is_empty() => Ok(()),
        None => Err(NEEDS_ENTERPRISE.to_owned()),
    }
}

/// [`RowCompiler::judge`] of the installed compiler.
pub(crate) fn judge(
    snapshot: &Snapshot,
    rules: &RowRules,
    resources: &[u64],
    cancelled: &dyn Fn() -> bool,
) -> Result<RowFilter, Error> {
    compiler()?.judge(snapshot, rules, resources, cancelled)
}

/// [`RowCompiler::judge_editable`] of the installed compiler.
pub(crate) fn judge_editable(
    snapshot: &Snapshot,
    rules: &RowRules,
    resources: &[u64],
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<RowFilter>, Error> {
    compiler()?.judge_editable(snapshot, rules, resources, cancelled)
}

/// The shapes of a set of policy files (#202), read once when the files are loaded. Each gets
/// its own number, so filters compiled from older files are never taken for theirs.
#[derive(Debug)]
pub struct RowShapes {
    graph: Graph,
    id: u64,
    by_agent: bool,
}

static SHAPES: AtomicU64 = AtomicU64::new(0);

impl RowShapes {
    pub fn new(graph: Graph) -> Self {
        Self {
            by_agent: depends_on_agent(&graph),
            id: SHAPES.fetch_add(1, Ordering::Relaxed),
            graph,
        }
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// A number unique to these shapes in this process: a reload gives a new one. Anything
    /// cached from them can take it as its key.
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl PartialEq for RowShapes {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for RowShapes {}

/// What [`Engine::explain_resource`] found.
#[derive(Debug, Default, Clone)]
pub struct ResourceExplained {
    pub hidden_by: Vec<Term>,
    pub hidden_properties: Vec<NamedNode>,
    pub not_editable_by: Vec<Term>,
}

/// The row rules that apply to a caller (#202): the rules of its policies, with their shapes,
/// its agent (`None`: anonymous) and the graphs it may read (`None`: every graph; a
/// [`GraphName::DefaultGraph`] for the default graph). Visibility is judged over those graphs.
#[derive(Clone, Debug)]
pub struct RowRules {
    pub shapes: Arc<RowShapes>,
    pub rules: Vec<RowRule>,
    pub agent: Option<String>,
    pub graphs: Option<Vec<GraphName>>,
}

impl RowRules {
    /// A key for what these rules hide: anything cached from a read under them (a profile,
    /// a tile) takes it as part of its key, so callers with other rules never share an entry.
    /// A reload of the shapes gives a new one.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.key().hash(&mut hasher);
        hasher.finish()
    }

    fn key(&self) -> CacheKey {
        let mut graphs = self.graphs.clone();
        if let Some(g) = &mut graphs {
            g.sort_by_key(ToString::to_string);
            g.dedup();
        }
        (
            self.shapes.id,
            self.rules.clone(),
            self.shapes
                .by_agent
                .then(|| self.agent.clone().unwrap_or_else(|| ANONYMOUS.to_owned())),
            graphs,
        )
    }
}

/// Row filters compiled from the quads of one store generation, by shapes, rules, agent (for
/// shapes that use `sg:requestingAgent`) and graphs.
#[derive(Default)]
pub(crate) struct Cache {
    generation: u64,
    filters: HashMap<CacheKey, Arc<RowFilter>>,
    /// The filters of the last earlier generation that had any: brought up to date from the
    /// writes since ([`RowCompiler::compile_incremental`]) rather than compiled anew.
    previous: Option<(u64, HashMap<CacheKey, Arc<RowFilter>>)>,
}

type CacheKey = (u64, Vec<RowRule>, Option<String>, Option<Vec<GraphName>>);

/// Filters kept per generation, at most.
const CACHED: usize = 256;

impl Engine {
    /// Compiles row rules (#202) to a [`RowFilter`] over the store as it is now.
    pub fn compile_row_rules(&self, rules: &RowRules) -> Result<RowFilter, Error> {
        let Dataset::TripleTrail(store) = self.store() else {
            return Err(need_tripletrail());
        };
        compiler()?.compile(&store.snapshot(), rules, &|| self.is_cancelled())
    }

    /// Which of `rules` keep `resource` from the caller, on the data as it is now (#202,
    /// the access explain page): the `sg:visibleIf` shapes that hide it, the properties of it
    /// that `sg:hideProperties` hides, and the `sg:editableIf` shapes that keep it from being
    /// changed. Each rule is judged on its own.
    pub fn explain_resource(
        &self,
        rules: &RowRules,
        resource: oxigraph::model::NamedNodeRef<'_>,
    ) -> Result<ResourceExplained, Error> {
        let Dataset::TripleTrail(store) = self.store() else {
            return Err(need_tripletrail());
        };
        let snapshot = store.snapshot();
        let mut out = ResourceExplained::default();
        let Some(id) = snapshot.term_id(resource.into()) else {
            return Ok(out);
        };
        let quads: Vec<_> = snapshot
            .quads_for_pattern(Some(resource.into()), None, None, None)
            .collect();
        for rule in &rules.rules {
            let single = RowRules {
                shapes: Arc::clone(&rules.shapes),
                rules: vec![match rule {
                    RowRule::EditableIf { shape } => RowRule::VisibleIf {
                        shape: shape.clone(),
                        references: false,
                    },
                    rule => rule.clone(),
                }],
                agent: rules.agent.clone(),
                graphs: rules.graphs.clone(),
            };
            let filter = compiler()?.compile(&snapshot, &single, &|| self.is_cancelled())?;
            match rule {
                RowRule::VisibleIf { shape, .. } if filter.hides_resource(id) => {
                    out.hidden_by.push(shape.clone());
                }
                RowRule::EditableIf { shape } if filter.hides_resource(id) => {
                    out.not_editable_by.push(shape.clone());
                }
                RowRule::HideProperties { .. } => {
                    // A `sg:hideProperties` filter hides predicates only.
                    for q in &quads {
                        if let Some(p) = snapshot.term_id(q.predicate.as_ref().into())
                            && filter.hides(&[id.0, p.0, u64::MAX, 0])
                            && !out.hidden_properties.contains(&q.predicate)
                        {
                            out.hidden_properties.push(q.predicate.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// A view of the store as it is now without what `rules` hide, for the readers that read
    /// the store directly rather than through a query (describe, Linked Data, …): the filter
    /// is judged on the quads of this same view.
    pub fn row_view(&self, rules: &RowRules) -> Result<Snapshot, Error> {
        let Dataset::TripleTrail(store) = self.store() else {
            return Err(need_tripletrail());
        };
        let snapshot = store.snapshot();
        let filter = self.row_filter_for(&snapshot, rules)?;
        Ok(snapshot.with_rows(Some(filter)))
    }

    /// The filter of `rules` over the quads of `snapshot`: the one compiled before for the same
    /// generation of the store, or compiled now (and kept). The filter and the query that uses
    /// it must read the same snapshot, or a write between them would be shown unjudged.
    pub fn row_filter_for(
        &self,
        snapshot: &Snapshot,
        rules: &RowRules,
    ) -> Result<Arc<RowFilter>, Error> {
        let Some(generation) = snapshot.generation() else {
            return Ok(Arc::new(
                compiler()?.compile(snapshot, rules, &|| self.is_cancelled())?,
            ));
        };
        let key = rules.key();
        let base = {
            let cache = self.spatial.row_filters.lock_unpoisoned();
            if cache.generation == generation
                && let Some(filter) = cache.filters.get(&key)
            {
                return Ok(Arc::clone(filter));
            }
            // The filter of an earlier generation, to bring up to date.
            let earlier = |g: u64, filters: &HashMap<CacheKey, Arc<RowFilter>>| {
                (g < generation)
                    .then(|| filters.get(&key).map(|f| (g, Arc::clone(f))))
                    .flatten()
            };
            earlier(cache.generation, &cache.filters).or_else(|| {
                cache
                    .previous
                    .as_ref()
                    .and_then(|(g, filters)| earlier(*g, filters))
            })
        };
        let cancelled = || self.is_cancelled();
        let incremental = match (&base, self.store()) {
            (Some((from, before)), Dataset::TripleTrail(store)) => {
                match store.touched_between(*from, generation) {
                    Some((resources, predicates)) => compiler()?.compile_incremental(
                        snapshot,
                        rules,
                        before,
                        &resources,
                        &predicates,
                        &cancelled,
                    )?,
                    None => None,
                }
            }
            _ => None,
        };
        let filter = Arc::new(match incremental {
            Some(filter) => filter,
            None => compiler()?.compile(snapshot, rules, &cancelled)?,
        });
        let mut cache = self.spatial.row_filters.lock_unpoisoned();
        // Only a newer generation replaces what is kept: a query on an older snapshot does
        // not throw away the filters of the current one.
        if generation > cache.generation || cache.filters.is_empty() {
            if !cache.filters.is_empty() {
                let filters = std::mem::take(&mut cache.filters);
                cache.previous = Some((cache.generation, filters));
            }
            cache.generation = generation;
        }
        if cache.generation == generation {
            if cache.filters.len() >= CACHED {
                cache.filters.clear();
            }
            cache.filters.insert(key, Arc::clone(&filter));
        }
        Ok(filter)
    }
}

fn need_tripletrail() -> Error {
    Error::Request("Row-level access rules need the tripletrail store".to_owned())
}
