use crate::constraint::Constraint;
use crate::service::PgrError;
use oxigraph::model::Term;
use oxigraph::sparql::{QueryDataset, QueryResults, SparqlEvaluator};
use spargebra::algebra::{
    AggregateExpression, Expression, GraphPattern, OrderExpression, PropertyPathExpression,
};
use spargebra::term::NamedNodePattern;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tripletrail_geo::sync::LockUnpoisoned;
use tripletrail_routing::{Cch, Graph, GraphBuilder, InternedGraph};
use tripletrail_store::{Dataset, RowView};

/// The state of a built network, as `pgr:networkInfo` reports it.
#[derive(Debug, Clone)]
pub struct NetworkInfo {
    pub vertices: usize,
    /// Distinct edge ids.
    pub edges: usize,
    /// Edge-set rows.
    pub rows: usize,
    pub arcs: usize,
    pub has_cost: bool,
    /// The `?c_*` columns, without `?`.
    pub columns: Vec<String>,
    /// Whether the contraction hierarchy has been built (or loaded).
    pub hierarchy: bool,
    /// Filtered graphs (`pgr:constraint`, `pgr:shapes`, `pgr:vehicle`) cached on the network.
    pub filters: usize,
    /// How many of those have the hierarchy customized for them.
    pub customized: usize,
    pub stats: BuildStats,
    pub hierarchy_time: Option<Duration>,
    /// Whether the network is saved in the cache directory.
    pub persisted: bool,
}

/// Timings and sizes of one network build.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildStats {
    /// Evaluating the edge sub-select.
    pub edge_query: Duration,
    /// Interning terms and building the CSR graph.
    pub build: Duration,
    pub rows: usize,
    pub vertices: usize,
    pub arcs: usize,
    /// Customizing the hierarchy of the network this one replaced, when the write that made
    /// it stale left the vertices as they were (only costs or arcs between them changed).
    pub customize: Option<Duration>,
}

/// A routing graph built from an edge set, keyed by RDF terms.
#[derive(Debug)]
pub struct Network {
    pub graph: InternedGraph<Term>,
    pub stats: BuildStats,
    /// Whether the edge set has a `?cost` column (without one every arc costs 1).
    pub has_cost: bool,
    /// `(capacity, reverse_capacity)` of every row of `graph.rows` (negative for no arc),
    /// when the edge set has a `?capacity` column (the flow functions).
    pub capacities: Option<Vec<(f64, f64)>>,
    /// The extra `?c_*` columns of the edge set, when it projects any.
    pub extras: Option<Extras>,
    /// The `?geom` of every row of `graph.rows`, when the edge set projects `?geom`.
    pub geoms: Option<Vec<Option<Term>>>,
    edge_ids: OnceLock<HashMap<String, u32>>,
    /// The first row of every edge id, built on first use.
    edge_rows: OnceLock<Vec<u32>>,
    /// Vertex coordinates from `?x1 ?y1 ?x2 ?y2`, or else from the ends of the geometries.
    vertex_xy: OnceLock<Vec<Option<(f64, f64)>>>,
    /// Graphs without the edges a filter (`pgr:constraint`, `pgr:shapes`, `pgr:vehicle`)
    /// rejects, by filter key, with what they read in the store; newest last.
    constrained: Mutex<Vec<(String, FilterReads, Arc<Constrained>)>>,
    /// Vertex and edge ids in pgRouting's id order, built on first use (see [`Network::id_order`]).
    id_order: OnceLock<(Vec<u32>, Vec<u32>)>,
    /// Every vertex's position when the vertices are sorted by their text, built on first use.
    text_ranks: OnceLock<Vec<u32>>,
    hierarchy: OnceLock<Cch>,
    /// The hierarchy as read from the network's file, not yet checked and customized: that
    /// takes seconds on a large network, so it is done on first use, by the queries that need
    /// the hierarchy (see [`Network::hierarchy`]).
    saved_hierarchy: Mutex<Option<tripletrail_routing::SavedHierarchy>>,
    /// How long building the hierarchy took (unknown when it was loaded or adopted).
    hierarchy_time: OnceLock<Duration>,
    /// Where the network is saved, when the cache keeps networks on disk.
    file: Option<Persisted>,
    /// The cache generation the network is current for.
    generation: AtomicU64,
}

/// The file a network is saved to, and what it must match to be reused.
#[derive(Debug, Clone)]
struct Persisted {
    path: PathBuf,
    key: String,
    directed: bool,
    /// The cache's [`NetworkCache::files`] flag, set before the file is written.
    files: Arc<AtomicBool>,
}

/// The extra `?c_*` columns of an edge set: their names (without `?`) and, row by row, their
/// values (`values[row * names.len() + column]`).
#[derive(Debug, Clone)]
pub struct Extras {
    pub names: Vec<String>,
    pub values: Vec<Option<Term>>,
}

impl Extras {
    pub fn row(&self, row: usize) -> &[Option<Term>] {
        let n = self.names.len();
        &self.values[row * n..(row + 1) * n]
    }
}

/// A network's graph without the edges a constraint rejects, and the network's contraction
/// hierarchy customized for it (on first use).
#[derive(Debug)]
pub struct Constrained {
    pub graph: Graph,
    /// Rejected edges, by edge id.
    pub disabled: Vec<bool>,
    hierarchy: OnceLock<Cch>,
    /// Evaluating the constraint and filtering the graph.
    pub build: Duration,
}

impl Constrained {
    /// Whether the hierarchy has been customized for this graph yet.
    pub fn is_customized(&self) -> bool {
        self.hierarchy.get().is_some()
    }
}

/// What a filtered graph depends on besides the network's edge rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterReads {
    /// Only the rows (`pgr:constraint`).
    Rows,
    /// Quads with these predicates, in any graph (SHACL shapes and compatibility rules read
    /// the shapes or rules graph and follow paths in the data).
    Predicates(BTreeSet<String>),
    /// Any quad (a closed shape reads every property of its focus nodes).
    Any,
}

impl FilterReads {
    /// Whether a write of quads with `predicates` can change the filter. Writes of SHACL or
    /// compatibility terms always can: they may change the shapes.
    fn affected_by(&self, predicates: &HashSet<&str>) -> bool {
        match self {
            Self::Rows => false,
            Self::Any => true,
            Self::Predicates(reads) => predicates.iter().any(|p| {
                reads.contains(*p)
                    || p.starts_with("http://www.w3.org/ns/shacl#")
                    || p.starts_with(tripletrail_shacl::compat::NS)
            }),
        }
    }
}

/// How many constrained graphs a network keeps.
const CONSTRAINED_CACHE: usize = 8;

/// One usable row of an edge set.
struct EdgeRow {
    id: Term,
    source: Term,
    target: Term,
    cost: f64,
    reverse_cost: Option<f64>,
    source_xy: Option<(f64, f64)>,
    target_xy: Option<(f64, f64)>,
    /// `(capacity, reverse_capacity)`, when the edge set has a `?capacity` column.
    capacity: Option<(f64, f64)>,
    /// Values of the extra `?c_*` columns.
    extra: Vec<Option<Term>>,
    geom: Option<Term>,
}

/// Which optional columns an edge set projects.
#[derive(Clone, Default)]
struct Columns {
    cost: bool,
    capacity: bool,
    geom: bool,
    /// Names of the extra `?c_*` columns.
    extras: Vec<String>,
}

impl Network {
    /// Evaluates an edge sub-select on the store and builds the graph. `evaluator` makes the
    /// evaluator: the branches of a top-level `UNION` are evaluated in parallel, one evaluator
    /// each.
    pub fn build(
        store: &Dataset,
        evaluator: &(dyn Fn() -> SparqlEvaluator + Sync),
        pattern: &GraphPattern,
        directed: bool,
    ) -> Result<Self, PgrError> {
        Self::build_prefixed(store, None, None, evaluator, pattern, directed, "")
    }

    /// Like [`Network::build`], reading the columns whose names start with `prefix`
    /// (`?fine_id ?fine_source ...` for the fine network of hierarchical routing).
    /// `dataset`: the graphs the edge sub-select reads (a caller limited to some graphs,
    /// #203); `None` reads the store's own. `rows`: what a caller under row rules sees
    /// (#202); `None` reads every row.
    pub fn build_prefixed(
        store: &Dataset,
        dataset: Option<&QueryDataset>,
        rows: Option<&RowView>,
        evaluator: &(dyn Fn() -> SparqlEvaluator + Sync),
        pattern: &GraphPattern,
        directed: bool,
        prefix: &str,
    ) -> Result<Self, PgrError> {
        let start = Instant::now();
        let parts = union_branches(pattern);
        let rows: Vec<(Vec<EdgeRow>, Columns)> = if parts.len() == 1 {
            vec![edge_rows(
                store,
                dataset,
                rows,
                evaluator(),
                &parts[0],
                prefix,
            )?]
        } else {
            std::thread::scope(|scope| {
                let workers: Vec<_> = parts
                    .iter()
                    .map(|part| {
                        scope.spawn(move || {
                            edge_rows(store, dataset, rows, evaluator(), part, prefix)
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|w| w.join().expect("edge query thread panicked"))
                    .collect::<Result<_, _>>()
            })?
        };
        let edge_query = start.elapsed();
        let t = Instant::now();
        let columns = rows.first().map(|r| r.1.clone()).unwrap_or_default();
        let mut capacities = columns.capacity.then(Vec::new);
        let mut geoms = columns.geom.then(Vec::new);
        let mut extras = (!columns.extras.is_empty()).then(|| Extras {
            names: columns.extras.clone(),
            values: Vec::new(),
        });
        let mut builder = GraphBuilder::<Term>::default();
        for r in rows.into_iter().flat_map(|r| r.0) {
            if let (Some(list), Some(c)) = (&mut capacities, r.capacity) {
                list.push(c);
            }
            if let Some(list) = &mut geoms {
                list.push(r.geom);
            }
            if let Some(x) = &mut extras {
                x.values.extend(r.extra);
            }
            builder.add(
                r.id,
                &r.source,
                &r.target,
                r.cost,
                r.reverse_cost,
                r.source_xy,
                r.target_xy,
            );
        }
        let rows = builder.len();
        let graph = builder.build(directed);
        let stats = BuildStats {
            edge_query,
            build: t.elapsed(),
            rows,
            vertices: graph.graph.num_vertices(),
            arcs: graph.graph.num_arcs(),
            customize: None,
        };
        Ok(Self {
            graph,
            stats,
            has_cost: columns.cost,
            capacities,
            extras,
            geoms,
            edge_ids: OnceLock::new(),
            edge_rows: OnceLock::new(),
            vertex_xy: OnceLock::new(),
            constrained: Mutex::default(),
            id_order: OnceLock::new(),
            text_ranks: OnceLock::new(),
            hierarchy: OnceLock::new(),
            saved_hierarchy: Mutex::default(),
            hierarchy_time: OnceLock::new(),
            file: None,
            generation: AtomicU64::new(0),
        })
    }
}

/// Splits `SELECT ... { A } UNION { B } ...` into one `SELECT` per branch, which together give
/// the same rows (a union is the concatenation of its branches). Anything else is one part.
fn union_branches(pattern: &GraphPattern) -> Vec<GraphPattern> {
    fn collect(p: &GraphPattern, out: &mut Vec<GraphPattern>) {
        match p {
            GraphPattern::Union { left, right } => {
                collect(left, out);
                collect(right, out);
            }
            other => out.push(other.clone()),
        }
    }
    let mut branches = Vec::new();
    match pattern {
        GraphPattern::Project { inner, variables } => {
            collect(inner, &mut branches);
            if branches.len() > 1 {
                return branches
                    .into_iter()
                    .map(|b| GraphPattern::Project {
                        inner: Box::new(b),
                        variables: variables.clone(),
                    })
                    .collect();
            }
        }
        GraphPattern::Union { .. } => {
            collect(pattern, &mut branches);
            return branches;
        }
        _ => {}
    }
    vec![pattern.clone()]
}

/// Evaluates (part of) an edge sub-select and keeps the rows that make an edge.
fn edge_rows(
    store: &Dataset,
    dataset: Option<&QueryDataset>,
    rows: Option<&RowView>,
    evaluator: SparqlEvaluator,
    pattern: &GraphPattern,
    prefix: &str,
) -> Result<(Vec<EdgeRow>, Columns), PgrError> {
    let query = spargebra::Query::Select {
        dataset: None,
        pattern: pattern.clone(),
        base_iri: None,
    };
    let mut prepared = evaluator.for_query(query);
    if let Some(d) = dataset {
        *prepared.dataset_mut() = d.clone();
    }
    let QueryResults::Solutions(solutions) = store
        .execute_in(prepared, rows)
        .map_err(|e| PgrError::EdgeQuery(Box::new(e)))?
    else {
        return Err(PgrError::Request("the edge query must be a SELECT".into()));
    };
    let position = |name: &str| {
        solutions
            .variables()
            .iter()
            .position(|v| v.as_str().strip_prefix(prefix) == Some(name))
    };
    let required = |name: &str| {
        position(name).ok_or_else(|| {
            PgrError::Request(format!("the edge query must project ?{prefix}{name}"))
        })
    };
    let extra_columns: Vec<(usize, String)> = solutions
        .variables()
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            let name = v.as_str().strip_prefix(prefix)?;
            name.starts_with("c_").then(|| (i, name.to_owned()))
        })
        .collect();
    let geom = position("geom");
    let (id, source, target) = (required("id")?, required("source")?, required("target")?);
    // Without ?cost every arc costs 1, or its capacity for the flow functions (which only
    // read capacities); the request checks that functions needing costs get them.
    let cost = position("cost");
    let capacity = position("capacity");
    let reverse_capacity = position("reverse_capacity");
    let reverse_cost = position("reverse_cost").or(reverse_capacity.filter(|_| cost.is_none()));
    let columns = Columns {
        cost: cost.is_some(),
        capacity: capacity.is_some(),
        geom: geom.is_some(),
        extras: extra_columns.iter().map(|c| c.1.clone()).collect(),
    };
    let coords = match (
        position("x1"),
        position("y1"),
        position("x2"),
        position("y2"),
    ) {
        (Some(a), Some(b), Some(c), Some(d)) => Some((a, b, c, d)),
        _ => None,
    };
    let mut rows = Vec::new();
    for solution in solutions {
        let solution = solution.map_err(|e| PgrError::EdgeQuery(Box::new(e)))?;
        let values = solution.values();
        // pgRouting ignores rows with NULL id/source/target/cost as well.
        let (Some(i), Some(s), Some(t)) = (&values[id], &values[source], &values[target]) else {
            continue;
        };
        let get = |p: usize| values[p].as_ref().and_then(number);
        let capacity = match capacity {
            Some(p) => match get(p) {
                Some(c) => Some((c, reverse_capacity.and_then(get).unwrap_or(-1.0))),
                None => continue,
            },
            None => None,
        };
        let c = match (cost, capacity) {
            (Some(p), _) => match get(p) {
                Some(c) => c,
                None => continue,
            },
            (None, Some(c)) => c.0,
            (None, None) => 1.0,
        };
        let (source_xy, target_xy) = match coords {
            Some((a, b, c2, d)) => (get(a).zip(get(b)), get(c2).zip(get(d))),
            None => (None, None),
        };
        rows.push(EdgeRow {
            id: i.clone(),
            source: s.clone(),
            target: t.clone(),
            cost: c,
            reverse_cost: reverse_cost.and_then(get),
            source_xy,
            target_xy,
            capacity,
            extra: extra_columns
                .iter()
                .map(|&(i, _)| values[i].clone())
                .collect(),
            geom: geom.and_then(|p| values[p].clone()),
        });
    }
    Ok((rows, columns))
}

impl Network {
    /// The customizable contraction hierarchy of the graph, built on first use.
    pub fn hierarchy(&self) -> &Cch {
        let mut built = false;
        let h = self.hierarchy.get_or_init(|| {
            let saved = self.saved_hierarchy.lock_unpoisoned().take();
            if let Some(h) = saved.and_then(|s| s.load(&self.graph.graph)) {
                return h;
            }
            built = true;
            let t = Instant::now();
            let h = Cch::build(&self.graph.graph);
            let _ = self.hierarchy_time.set(t.elapsed());
            h
        });
        if built {
            // Save again, now with the hierarchy.
            self.save();
        }
        h
    }

    /// Whether the network has a contraction hierarchy, built or still as saved.
    fn has_hierarchy(&self) -> bool {
        self.hierarchy.get().is_some() || self.saved_hierarchy.lock_unpoisoned().is_some()
    }

    /// What `pgr:networkInfo` reports about the network.
    pub fn info(&self) -> NetworkInfo {
        let filters = self.constrained.lock_unpoisoned();
        NetworkInfo {
            vertices: self.graph.graph.num_vertices(),
            edges: self.graph.edges.len(),
            rows: self.graph.rows.len(),
            arcs: self.graph.graph.num_arcs(),
            has_cost: self.has_cost,
            columns: self
                .extras
                .as_ref()
                .map(|x| x.names.clone())
                .unwrap_or_default(),
            hierarchy: self.has_hierarchy(),
            filters: filters.len(),
            customized: filters.iter().filter(|f| f.2.is_customized()).count(),
            stats: self.stats,
            hierarchy_time: self.hierarchy_time.get().copied(),
            persisted: self.file.as_ref().is_some_and(|p| p.path.is_file()),
        }
    }

    /// Takes over the hierarchy of `old`, a network built from an earlier version of the data,
    /// when both have the same vertices: the hierarchy's topology still fits, and customizing
    /// it for the new costs is much cheaper than building a new one.
    fn adopt_hierarchy(&mut self, old: &Network) -> bool {
        if !old.has_hierarchy() {
            return false;
        }
        let h = old.hierarchy();
        let t = Instant::now();
        let (new, old) = (&self.graph, &old.graph);
        if new.vertices.len() != old.vertices.len() {
            return false;
        }
        let old_id: Option<Vec<u32>> = if new.vertices == old.vertices {
            Some((0..new.vertices.len() as u32).collect())
        } else {
            new.vertices.iter().map(|v| old.vertex_id(v)).collect()
        };
        let Some(mut h) = old_id.and_then(|ids| h.relabel(&ids)) else {
            return false;
        };
        if !h.customize(&new.graph) {
            return false;
        }
        let _ = self.hierarchy.set(h);
        self.stats.customize = Some(t.elapsed());
        true
    }

    /// Writes the network (and its hierarchy, once built) to its file, if it has one.
    /// Errors are ignored: the file is only a cache.
    fn save(&self) {
        // The file format has no capacities or geometries, and loaded networks are taken to
        // have costs.
        if self.capacities.is_some() || self.geoms.is_some() || !self.has_cost {
            return;
        }
        let Some(p) = &self.file else { return };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.generation.load(Ordering::SeqCst).to_le_bytes());
        put_str(&mut bytes, &p.key);
        for d in [self.stats.edge_query, self.stats.build] {
            bytes.extend_from_slice(&d.as_secs_f64().to_le_bytes());
        }
        // The extra `?c_*` columns (for `pgr:constraint`), row by row.
        match &self.extras {
            None => bytes.push(0),
            Some(x) => {
                bytes.push(1);
                bytes.extend_from_slice(&(x.names.len() as u64).to_le_bytes());
                for n in &x.names {
                    put_str(&mut bytes, n);
                }
                bytes.extend_from_slice(&(x.values.len() as u64).to_le_bytes());
                for v in &x.values {
                    match v {
                        None => bytes.push(0),
                        Some(t) => {
                            bytes.push(1);
                            put_str(&mut bytes, &t.to_string());
                        }
                    }
                }
            }
        }
        self.graph.write(
            &mut bytes,
            p.directed,
            self.hierarchy.get(),
            Term::to_string,
        );
        p.files.store(true, Ordering::SeqCst);
        let tmp = p.path.with_extension("tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &p.path);
        }
    }

    /// Marks the network current for a new cache generation, in its file too.
    fn set_generation(&self, generation: u64) {
        use std::io::Write;
        self.generation.store(generation, Ordering::SeqCst);
        if let Some(p) = &self.file
            && let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&p.path)
        {
            // The generation is the first 8 bytes.
            let _ = f.write_all(&generation.to_le_bytes());
        }
    }

    /// Reads a saved network, if the file exists and matches the generation and key.
    fn load(p: Persisted, generation: u64) -> Option<Self> {
        let bytes = std::fs::read(&p.path).ok()?;
        let (saved, rest) = bytes.split_at_checked(8)?;
        if u64::from_le_bytes(saved.try_into().ok()?) != generation {
            return None;
        }
        let (key, rest) = take_str(rest)?;
        if key != p.key {
            return None;
        }
        let (times, rest) = rest.split_at_checked(16)?;
        let secs =
            |b: &[u8]| Duration::try_from_secs_f64(f64::from_le_bytes(b.try_into().ok()?)).ok();
        let (edge_query, build) = (secs(&times[..8])?, secs(&times[8..])?);
        let (&flag, mut rest) = rest.split_first()?;
        let extras = match flag {
            0 => None,
            1 => {
                let count = |rest: &mut &[u8]| -> Option<usize> {
                    let (n, r) = rest.split_at_checked(8)?;
                    *rest = r;
                    usize::try_from(u64::from_le_bytes(n.try_into().ok()?)).ok()
                };
                let n = count(&mut rest)?;
                let mut names = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    let (name, r) = take_str(rest)?;
                    names.push(name.to_owned());
                    rest = r;
                }
                let m = count(&mut rest)?;
                let mut values = Vec::with_capacity(m.min(rest.len()));
                for _ in 0..m {
                    let (&present, r) = rest.split_first()?;
                    rest = r;
                    values.push(if present == 1 {
                        let (t, r) = take_str(rest)?;
                        rest = r;
                        Some(saved_term(t)?)
                    } else {
                        None
                    });
                }
                Some(Extras { names, values })
            }
            _ => return None,
        };
        let (graph, directed, hierarchy) = InternedGraph::<Term>::read_lazy(rest, saved_term)?;
        if directed != p.directed {
            return None;
        }
        let stats = BuildStats {
            edge_query,
            build,
            rows: graph.rows.len(),
            vertices: graph.graph.num_vertices(),
            arcs: graph.graph.num_arcs(),
            customize: None,
        };
        Some(Self {
            graph,
            stats,
            has_cost: true,
            capacities: None,
            extras,
            geoms: None,
            edge_ids: OnceLock::new(),
            edge_rows: OnceLock::new(),
            vertex_xy: OnceLock::new(),
            constrained: Mutex::default(),
            id_order: OnceLock::new(),
            text_ranks: OnceLock::new(),
            hierarchy: OnceLock::new(),
            saved_hierarchy: Mutex::new(hierarchy),
            hierarchy_time: OnceLock::new(),
            file: Some(p),
            generation: AtomicU64::new(generation),
        })
    }

    /// The vertex ids and the edge ids sorted in pgRouting's id order (numbers by value, then
    /// other terms by their text). Built on first use.
    pub(crate) fn id_order(&self) -> &(Vec<u32>, Vec<u32>) {
        self.id_order.get_or_init(|| {
            (
                crate::tables::ranks(&self.graph.vertices),
                crate::tables::ranks(&self.graph.edges),
            )
        })
    }

    /// Every vertex's position when the vertices are sorted by their N-Triples text. Built on
    /// first use.
    pub(crate) fn text_ranks(&self) -> &[u32] {
        self.text_ranks
            .get_or_init(|| text_ranks(&self.graph.vertices))
    }

    /// The first edge-set row of every edge id.
    pub fn edge_rows(&self) -> &[u32] {
        self.edge_rows.get_or_init(|| {
            let mut rows = vec![u32::MAX; self.graph.edges.len()];
            for (i, r) in self.graph.rows.iter().enumerate().rev() {
                rows[r.id as usize] = i as u32;
            }
            rows
        })
    }

    /// The geometry of edge `e` (of its first row), when the edge set has `?geom`.
    pub fn geom(&self, e: u32) -> Option<&Term> {
        let row = *self.edge_rows().get(e as usize)? as usize;
        self.geoms.as_ref()?.get(row)?.as_ref()
    }

    /// Vertex coordinates: from `?x1 ?y1 ?x2 ?y2` when every vertex has some, else from the
    /// first and last points of the edges' `?geom` (`None` where neither gives any).
    pub fn vertex_xy(&self) -> &[Option<(f64, f64)>] {
        self.vertex_xy.get_or_init(|| {
            if let Some(c) = self.graph.graph.coords() {
                return c.iter().copied().map(Some).collect();
            }
            let mut xy = vec![None; self.graph.vertices.len()];
            if let Some(geoms) = &self.geoms {
                for (r, g) in self.graph.rows.iter().zip(geoms) {
                    let Some((line, _)) = g.as_ref().and_then(crate::geometry::line) else {
                        continue;
                    };
                    let (a, b) = (line[0], line[line.len() - 1]);
                    xy[r.source as usize].get_or_insert((a.x, a.y));
                    xy[r.target as usize].get_or_insert((b.x, b.y));
                }
            }
            xy
        })
    }

    /// The graph without the edges `constraint` rejects (see [`crate::constraint`]), cached
    /// by the constraint's text, and whether it was built now. An edge is rejected when any
    /// of its rows fails.
    pub fn constrained(&self, constraint: &str) -> Result<(Arc<Constrained>, bool), PgrError> {
        self.filtered(constraint, |n| {
            Ok((n.constraint_rejects(constraint)?, FilterReads::Rows))
        })
    }

    /// Which edges (by edge id) `constraint` rejects: those with a row it does not accept.
    pub fn constraint_rejects(&self, constraint: &str) -> Result<Vec<bool>, PgrError> {
        let names = self
            .extras
            .as_ref()
            .map(|x| x.names.as_slice())
            .unwrap_or(&[]);
        let compiled = Constraint::parse(constraint, |n| names.iter().position(|x| x == n))?;
        let mut disabled = vec![false; self.graph.edges.len()];
        for (i, r) in self.graph.rows.iter().enumerate() {
            let values = self.extras.as_ref().map_or(&[][..], |x| x.row(i));
            if !compiled.accepts(values) {
                disabled[r.id as usize] = true;
            }
        }
        Ok(disabled)
    }

    /// The graph without the edges `build` rejects (by edge id), cached under `key` with
    /// what the filter reads, and whether it was built now.
    pub fn filtered(
        &self,
        key: &str,
        build: impl FnOnce(&Self) -> Result<(Vec<bool>, FilterReads), PgrError>,
    ) -> Result<(Arc<Constrained>, bool), PgrError> {
        if let Some((_, _, c)) = self
            .constrained
            .lock_unpoisoned()
            .iter()
            .find(|(k, _, _)| k == key)
        {
            return Ok((Arc::clone(c), false));
        }
        let start = Instant::now();
        let (disabled, reads) = build(self)?;
        let graph = self.graph.graph.without_edges(&disabled);
        let c = Arc::new(Constrained {
            graph,
            disabled,
            hierarchy: OnceLock::new(),
            build: start.elapsed(),
        });
        let mut cache = self.constrained.lock_unpoisoned();
        if cache.len() >= CONSTRAINED_CACHE {
            cache.remove(0);
        }
        cache.push((key.to_owned(), reads, Arc::clone(&c)));
        Ok((c, true))
    }

    /// Drops the filtered graphs a write of quads with `predicates` may change.
    fn forget_filters(&self, predicates: &HashSet<&str>) {
        self.constrained
            .lock_unpoisoned()
            .retain(|(_, reads, _)| !reads.affected_by(predicates));
    }

    /// The network's hierarchy customized for a constrained graph: the same shape, with the
    /// rejected arcs left out of the weights. Built on first use.
    pub fn constrained_hierarchy<'a>(&self, c: &'a Constrained) -> &'a Cch {
        c.hierarchy.get_or_init(|| {
            let mut h = self.hierarchy().clone();
            let ok = h.customize(&c.graph);
            debug_assert!(ok, "a subgraph fits the hierarchy");
            h
        })
    }

    /// The dense id of an edge, by its key: an IRI without brackets, or a literal's value.
    /// Built on first use (only restrictions need it).
    pub fn edge_id(&self, key: &str) -> Option<u32> {
        self.edge_ids
            .get_or_init(|| {
                let mut ids = HashMap::with_capacity(self.graph.edges.len());
                for (i, e) in self.graph.edges.iter().enumerate() {
                    if let Some(k) = edge_key(e) {
                        ids.entry(k.to_owned()).or_insert(i as u32);
                    }
                }
                ids
            })
            .get(key)
            .copied()
    }
}

/// A vertex or edge term as [`Network::save`] wrote it (`Term::to_string`). Most are IRIs,
/// written by this program, so they are taken as they are instead of going through the
/// N-Triples parser, which dominates reading a large network otherwise.
fn saved_term(s: &str) -> Option<Term> {
    if let Some(iri) = s.strip_prefix('<').and_then(|s| s.strip_suffix('>'))
        && !iri.contains(['<', '>', '"', ' ', '\\'])
    {
        return Some(oxigraph::model::NamedNode::new_unchecked(iri).into());
    }
    s.parse::<Term>().ok()
}

/// The version of the saved network format, part of the file name.
const FORMAT: u8 = 2;

/// Removes the saved networks in `dir` whose path `remove` accepts.
fn remove_networks(dir: &std::path::Path, remove: impl Fn(&PathBuf) -> bool) {
    if let Ok(files) = std::fs::read_dir(dir) {
        for f in files.flatten() {
            let path = f.path();
            if path.extension().is_some_and(|e| e == "net") && remove(&path) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn take_str(bytes: &[u8]) -> Option<(&str, &[u8])> {
    let (len, rest) = bytes.split_at_checked(8)?;
    let len = usize::try_from(u64::from_le_bytes(len.try_into().ok()?)).ok()?;
    let (s, rest) = rest.split_at_checked(len)?;
    Some((std::str::from_utf8(s).ok()?, rest))
}

/// How an edge is named in a restriction.
pub(crate) fn edge_key(t: &Term) -> Option<&str> {
    match t {
        Term::NamedNode(n) => Some(n.as_str()),
        Term::Literal(l) => Some(l.value()),
        _ => None,
    }
}

pub(crate) fn number(t: &Term) -> Option<f64> {
    match t {
        Term::Literal(l) => l.value().parse().ok(),
        _ => None,
    }
}

/// Edge-set key (sub-select algebra or network IRI) and `directed` flag.
type CacheKey = (String, bool);

#[derive(Debug)]
struct Cached {
    generation: u64,
    network: Arc<Network>,
    /// The predicates the edge query reads; `None` if it can read any.
    reads: Option<BTreeSet<String>>,
    /// A named network (`pgr:network`): never evicted for the memory budget.
    pinned: bool,
    /// Roughly how many bytes the network took when it was built.
    bytes: usize,
    /// When the network was last used, as a tick of [`NetworkCache::clock`].
    used: AtomicU64,
}

/// Roughly how many bytes a term takes in memory.
fn term_bytes(t: &Term) -> usize {
    std::mem::size_of::<Term>()
        + match t {
            Term::NamedNode(n) => n.as_str().len(),
            Term::BlankNode(b) => b.as_str().len(),
            Term::Literal(l) => l.value().len() + 16,
            #[allow(unreachable_patterns)]
            _ => 64,
        }
}

impl Network {
    /// Roughly how many bytes the network takes in memory (without the hierarchy and the
    /// filtered graphs built later).
    pub fn approx_bytes(&self) -> usize {
        let g = &self.graph;
        // Vertex terms are held twice: in the list and as keys of the id map.
        g.graph.approx_bytes()
            + g.vertices
                .iter()
                .map(|t| 2 * term_bytes(t) + 8)
                .sum::<usize>()
            + g.edges.iter().map(term_bytes).sum::<usize>()
            + g.rows.len() * std::mem::size_of::<tripletrail_routing::EdgeRow>()
            + self.capacities.as_ref().map_or(0, |c| c.len() * 16)
            + self.geoms.as_ref().map_or(0, |gs| {
                gs.iter()
                    .map(|t| t.as_ref().map_or(8, term_bytes))
                    .sum::<usize>()
            })
    }
}

/// The predicates a pattern reads, or `None` if it can read any predicate (a variable or
/// negated property set in predicate position, or a `SERVICE` call).
pub fn predicates_read(pattern: &GraphPattern) -> Option<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    pattern_reads(pattern, &mut out).then_some(out)
}

fn pattern_reads(p: &GraphPattern, out: &mut BTreeSet<String>) -> bool {
    match p {
        GraphPattern::Bgp { patterns } => patterns.iter().all(|t| match &t.predicate {
            NamedNodePattern::NamedNode(n) => {
                out.insert(n.as_str().to_owned());
                true
            }
            NamedNodePattern::Variable(_) => false,
        }),
        GraphPattern::Path { path, .. } => path_reads(path, out),
        GraphPattern::Join { left, right }
        | GraphPattern::Lateral { left, right }
        | GraphPattern::Union { left, right }
        | GraphPattern::Minus { left, right } => {
            pattern_reads(left, out) && pattern_reads(right, out)
        }
        GraphPattern::LeftJoin {
            left,
            right,
            expression,
        } => {
            pattern_reads(left, out)
                && pattern_reads(right, out)
                && expression.as_ref().is_none_or(|e| expression_reads(e, out))
        }
        GraphPattern::Filter { expr, inner } => {
            expression_reads(expr, out) && pattern_reads(inner, out)
        }
        GraphPattern::Extend {
            inner, expression, ..
        } => pattern_reads(inner, out) && expression_reads(expression, out),
        GraphPattern::OrderBy { inner, expression } => {
            pattern_reads(inner, out)
                && expression.iter().all(|e| match e {
                    OrderExpression::Asc(e) | OrderExpression::Desc(e) => expression_reads(e, out),
                })
        }
        GraphPattern::Group {
            inner, aggregates, ..
        } => {
            pattern_reads(inner, out)
                && aggregates.iter().all(|(_, a)| match a {
                    AggregateExpression::CountSolutions { .. } => true,
                    AggregateExpression::FunctionCall { expr, .. } => expression_reads(expr, out),
                })
        }
        GraphPattern::Graph { inner, .. }
        | GraphPattern::Project { inner, .. }
        | GraphPattern::Distinct { inner }
        | GraphPattern::Reduced { inner }
        | GraphPattern::Slice { inner, .. } => pattern_reads(inner, out),
        GraphPattern::Values { .. } => true,
        GraphPattern::Service { .. } => false,
    }
}

fn path_reads(p: &PropertyPathExpression, out: &mut BTreeSet<String>) -> bool {
    match p {
        PropertyPathExpression::NamedNode(n) => {
            out.insert(n.as_str().to_owned());
            true
        }
        PropertyPathExpression::Reverse(p)
        | PropertyPathExpression::ZeroOrMore(p)
        | PropertyPathExpression::OneOrMore(p)
        | PropertyPathExpression::ZeroOrOne(p)
        | PropertyPathExpression::Bounded { path: p, .. } => path_reads(p, out),
        PropertyPathExpression::Sequence(a, b) | PropertyPathExpression::Alternative(a, b) => {
            path_reads(a, out) && path_reads(b, out)
        }
        PropertyPathExpression::NegatedPropertySet(_) => false,
    }
}

fn expression_reads(e: &Expression, out: &mut BTreeSet<String>) -> bool {
    match e {
        Expression::NamedNode(_)
        | Expression::Literal(_)
        | Expression::Variable(_)
        | Expression::Bound(_) => true,
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
        | Expression::Divide(a, b) => expression_reads(a, out) && expression_reads(b, out),
        Expression::UnaryPlus(a) | Expression::UnaryMinus(a) | Expression::Not(a) => {
            expression_reads(a, out)
        }
        Expression::In(a, list) => {
            expression_reads(a, out) && list.iter().all(|e| expression_reads(e, out))
        }
        Expression::If(a, b, c) => {
            expression_reads(a, out) && expression_reads(b, out) && expression_reads(c, out)
        }
        Expression::Coalesce(list) | Expression::FunctionCall(_, list) => {
            list.iter().all(|e| expression_reads(e, out))
        }
        Expression::Exists(p) => pattern_reads(p, out),
    }
}

/// Built networks, reused until the store changes.
///
/// Runtime edge queries are keyed by the algebra of their sub-select, named networks by IRI;
/// both by the `directed` flag. Any write to the store must call [`NetworkCache::invalidate`],
/// or [`NetworkCache::invalidate_for`] with the predicates it changed.
///
/// With [`NetworkCache::persistent`], built networks (and their contraction hierarchies) are
/// also saved to a directory, so a restarted server reuses them. A generation counter, saved in
/// the same directory and advanced by [`NetworkCache::invalidate`], tells current files from
/// stale ones. It only works if every write to the store goes through something that calls
/// `invalidate` (as [`tripletrail::Engine`](../tripletrail/struct.Engine.html) does).
#[derive(Debug)]
pub struct NetworkCache {
    generation: AtomicU64,
    entries: Mutex<HashMap<CacheKey, Cached>>,
    /// Stale networks that had a hierarchy, kept until their key is built again so the new
    /// network can reuse the hierarchy (see [`Network::adopt_hierarchy`]).
    previous: Mutex<HashMap<CacheKey, Arc<Network>>>,
    dir: Option<PathBuf>,
    /// The generation last written to the generation file in `dir`.
    saved_generation: AtomicU64,
    /// Whether `dir` may hold network files. Writes check it, so that with no network on
    /// disk they neither write the generation file nor list the directory (#709): the
    /// generation file is only brought up to date before a network is saved.
    files: Arc<AtomicBool>,
    /// Most bytes the networks built from inline edge sub-selects may take together; the
    /// least recently used are dropped past it. `usize::MAX` for no limit.
    budget: AtomicUsize,
    /// Ticks once per lookup, to tell which network was used last.
    clock: AtomicU64,
}

/// The default [`NetworkCache::set_budget`]: 2 GiB.
pub const DEFAULT_NETWORK_BUDGET: usize = 2 << 30;

/// The budget new caches start with (see [`NetworkCache::set_default_budget`]).
static DEFAULT_BUDGET: AtomicUsize = AtomicUsize::new(DEFAULT_NETWORK_BUDGET);

const GENERATION_FILE: &str = "generation";

impl Default for NetworkCache {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            entries: Mutex::default(),
            previous: Mutex::default(),
            dir: None,
            saved_generation: AtomicU64::new(0),
            files: Arc::default(),
            budget: AtomicUsize::new(DEFAULT_BUDGET.load(Ordering::Relaxed)),
            clock: AtomicU64::new(0),
        }
    }
}

impl NetworkCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets how many bytes the networks built from inline edge sub-selects may take
    /// together (`usize::MAX` for no limit); past it, the least recently used are dropped.
    /// Named networks (`pgr:network`) are kept whatever their size. The network a query
    /// has just built is kept too, even alone over the budget.
    pub fn set_budget(&self, bytes: usize) {
        self.budget.store(bytes, Ordering::Relaxed);
        let mut entries = self.entries.lock_unpoisoned();
        self.evict(&mut entries, None);
    }

    /// The budget caches created from now on start with ([`DEFAULT_NETWORK_BUDGET`] unless
    /// changed), e.g. set once by a server for every dataset it opens.
    pub fn set_default_budget(bytes: usize) {
        DEFAULT_BUDGET.store(bytes, Ordering::Relaxed);
    }

    /// The budget of [`NetworkCache::set_budget`].
    pub fn budget(&self) -> usize {
        self.budget.load(Ordering::Relaxed)
    }

    /// Roughly how many bytes the cached networks built from inline sub-selects take.
    pub fn unpinned_bytes(&self) -> usize {
        self.entries
            .lock_unpoisoned()
            .values()
            .filter(|c| !c.pinned)
            .map(|c| c.bytes)
            .sum()
    }

    /// Roughly how many bytes all cached networks take, named ones included, and the stale
    /// ones kept for their contraction hierarchy (counted once if also cached).
    pub fn bytes(&self) -> usize {
        let entries = self.entries.lock_unpoisoned();
        let cached: usize = entries.values().map(|c| c.bytes).sum();
        let stale: usize = self
            .previous
            .lock_unpoisoned()
            .values()
            .filter(|n| !entries.values().any(|c| Arc::ptr_eq(&c.network, n)))
            .map(|n| n.approx_bytes())
            .sum();
        cached + stale
    }

    /// How many networks are cached.
    pub fn len(&self) -> usize {
        self.entries.lock_unpoisoned().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops the least recently used unpinned networks, except `keep`, until those left fit
    /// in the budget.
    fn evict(&self, entries: &mut HashMap<CacheKey, Cached>, keep: Option<&CacheKey>) {
        let budget = self.budget.load(Ordering::Relaxed);
        let mut total: usize = entries
            .values()
            .filter(|c| !c.pinned)
            .map(|c| c.bytes)
            .sum();
        if total <= budget {
            return;
        }
        let mut order: Vec<(u64, CacheKey, usize)> = entries
            .iter()
            .filter(|(k, c)| !c.pinned && Some(*k) != keep)
            .map(|(k, c)| (c.used.load(Ordering::Relaxed), k.clone(), c.bytes))
            .collect();
        order.sort_unstable_by_key(|(used, _, _)| *used);
        for (_, key, bytes) in order {
            if total <= budget {
                break;
            }
            if let Some(c) = entries.remove(&key) {
                total -= bytes;
                if let Some(p) = &c.network.file {
                    let _ = std::fs::remove_file(&p.path);
                }
            }
        }
    }

    /// The directory networks are kept in, if any: other derived data of the dataset (such
    /// as vector graphs) is kept there too.
    pub fn dir(&self) -> Option<&std::path::Path> {
        self.dir.as_deref()
    }

    /// A cache that also keeps networks in `dir` (created if needed).
    pub fn persistent(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let generation = std::fs::read_to_string(dir.join(GENERATION_FILE))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        Ok(Self {
            generation: AtomicU64::new(generation),
            dir: Some(dir),
            saved_generation: AtomicU64::new(generation),
            // Files from an earlier run may be there.
            files: Arc::new(AtomicBool::new(true)),
            ..Self::default()
        })
    }

    /// Drops the cached networks whose edge query reads one of `predicates` (or could read
    /// any predicate), and keeps the others; call after a write that changed only quads with
    /// these predicates.
    pub fn invalidate_for(&self, predicates: &HashSet<&str>) {
        let mut entries = self.entries.lock_unpoisoned();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        // Taken before the generation file is written, so a network saved meanwhile sets
        // it again and is removed by a later write.
        let files = self.dir.is_some() && self.files.swap(false, Ordering::SeqCst);
        if files {
            self.save_generation(generation);
        }
        let mut previous = self.previous.lock_unpoisoned();
        entries.retain(|key, c| {
            let keep = c.generation + 1 == generation
                && c.reads
                    .as_ref()
                    .is_some_and(|r| r.iter().all(|p| !predicates.contains(p.as_str())));
            if keep {
                c.generation = generation;
                c.network.set_generation(generation);
                c.network.forget_filters(predicates);
            } else if c.network.has_hierarchy() {
                previous.insert(key.clone(), Arc::clone(&c.network));
            }
            keep
        });
        drop(previous);
        if let Some(dir) = &self.dir
            && files
        {
            let kept: HashSet<&PathBuf> = entries
                .values()
                .filter_map(|c| c.network.file.as_ref().map(|p| &p.path))
                .collect();
            if !kept.is_empty() {
                self.files.store(true, Ordering::SeqCst);
            }
            remove_networks(dir, |p| !kept.contains(p));
        }
    }

    /// Writes `generation` to the generation file. Networks saved with an older one are
    /// not read again.
    fn save_generation(&self, generation: u64) {
        if let Some(dir) = &self.dir {
            let _ = std::fs::write(dir.join(GENERATION_FILE), generation.to_string());
            self.saved_generation.store(generation, Ordering::SeqCst);
        }
    }

    /// Drops every cached network; call after any change to the data.
    pub fn invalidate(&self) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let stale = std::mem::take(&mut *self.entries.lock_unpoisoned());
        self.previous.lock_unpoisoned().extend(
            stale
                .into_iter()
                .filter(|(_, c)| c.network.has_hierarchy())
                .map(|(k, c)| (k, c.network)),
        );
        if let Some(dir) = &self.dir
            && self.files.swap(false, Ordering::SeqCst)
        {
            // Advance the saved generation first, so a crash cannot leave stale files current.
            self.save_generation(generation);
            remove_networks(dir, |_| true);
        }
    }

    /// The cached network for `key`, or one made by `build`. `reads` are the predicates its
    /// edge query reads (see [`predicates_read`]), so that writes to other predicates keep it.
    /// A `pinned` network (a named one) is never dropped for the memory budget.
    pub fn get_or_build(
        &self,
        key: String,
        directed: bool,
        reads: Option<BTreeSet<String>>,
        pinned: bool,
        build: impl FnOnce() -> Result<Network, PgrError>,
    ) -> Result<(Arc<Network>, bool), PgrError> {
        let generation = self.generation.load(Ordering::SeqCst);
        let tick = self.clock.fetch_add(1, Ordering::Relaxed);
        if let Some(c) = self.entries.lock_unpoisoned().get(&(key.clone(), directed))
            && c.generation == generation
        {
            c.used.store(tick, Ordering::Relaxed);
            return Ok((Arc::clone(&c.network), true));
        }
        let persisted = self.dir.as_ref().map(|dir| {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            // With the file format's version: files of another format are not read.
            (&key, directed, FORMAT).hash(&mut h);
            Persisted {
                path: dir.join(format!("{:016x}.net", h.finish())),
                key: key.clone(),
                directed,
                files: Arc::clone(&self.files),
            }
        });
        let cached = |network: &Arc<Network>| Cached {
            generation,
            network: Arc::clone(network),
            reads: reads.clone(),
            pinned,
            bytes: network.approx_bytes(),
            used: AtomicU64::new(tick),
        };
        if let Some(network) = persisted.clone().and_then(|p| Network::load(p, generation)) {
            let network = Arc::new(network);
            let mut entries = self.entries.lock_unpoisoned();
            if self.generation.load(Ordering::SeqCst) == generation {
                let k = (key, directed);
                entries.insert(k.clone(), cached(&network));
                self.evict(&mut entries, Some(&k));
            }
            return Ok((network, true));
        }
        let mut network = build()?;
        network.generation = AtomicU64::new(generation);
        let old = self
            .previous
            .lock_unpoisoned()
            .remove(&(key.clone(), directed));
        if let Some(old) = old {
            network.adopt_hierarchy(&old);
        }
        let network = {
            // Under the lock, so an invalidation cannot slip in between the check and the insert.
            let mut entries = self.entries.lock_unpoisoned();
            if self.generation.load(Ordering::SeqCst) == generation {
                if persisted.is_some() && self.saved_generation.load(Ordering::SeqCst) != generation
                {
                    // Brought up to date only now (see `files`): a network saved with the
                    // current generation must be read back after a restart.
                    self.save_generation(generation);
                }
                network.file = persisted;
                network.save();
                let network = Arc::new(network);
                let k = (key, directed);
                entries.insert(k.clone(), cached(&network));
                self.evict(&mut entries, Some(&k));
                network
            } else {
                Arc::new(network)
            }
        };
        Ok((network, false))
    }
}

/// Every term's position when the terms are sorted by their N-Triples text.
pub(crate) fn text_ranks(terms: &[Term]) -> Vec<u32> {
    let keys: Vec<String> = terms.iter().map(Term::to_string).collect();
    let mut order: Vec<u32> = (0..terms.len() as u32).collect();
    order.sort_unstable_by(|&a, &b| keys[a as usize].cmp(&keys[b as usize]));
    let mut rank = vec![0; terms.len()];
    for (i, v) in order.into_iter().enumerate() {
        rank[v as usize] = i as u32;
    }
    rank
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reads(q: &str) -> Option<Vec<String>> {
        let Ok(spargebra::Query::Select { pattern, .. }) =
            spargebra::SparqlParser::new().parse_query(q)
        else {
            panic!("{q}")
        };
        predicates_read(&pattern).map(|r| r.into_iter().collect())
    }

    #[test]
    fn unions_split_into_branches() {
        let pattern = |q: &str| {
            let Ok(spargebra::Query::Select { pattern, .. }) =
                spargebra::SparqlParser::new().parse_query(q)
            else {
                panic!("{q}")
            };
            pattern
        };
        let parts = union_branches(&pattern(
            "SELECT ?id WHERE { { ?id <http://e/a> 1 } UNION { ?id <http://e/b> 2 } UNION { ?id <http://e/c> 3 } }",
        ));
        assert_eq!(parts.len(), 3);
        assert!(
            parts.iter().all(
                |p| matches!(p, GraphPattern::Project { variables, .. } if variables.len() == 1)
            )
        );
        // DISTINCT over the union is not the concatenation of its branches.
        let distinct = pattern(
            "SELECT DISTINCT ?id WHERE { { ?id <http://e/a> 1 } UNION { ?id <http://e/b> 2 } }",
        );
        assert_eq!(union_branches(&distinct).len(), 1);
    }

    #[test]
    fn predicates_of_edge_queries() {
        assert_eq!(
            reads(
                "PREFIX ex: <http://e/> SELECT * WHERE { ?e a ex:E ; ex:from/ex:id ?s .
                 OPTIONAL { ?e ex:back ?r } FILTER NOT EXISTS { ?e ex:closed true }
                 BIND(IF(EXISTS { ?e ex:slow ?x }, 2, 1) AS ?f) }"
            ),
            Some(
                [
                    "http://e/back",
                    "http://e/closed",
                    "http://e/from",
                    "http://e/id",
                    "http://e/slow",
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                ]
                .map(String::from)
                .to_vec()
            )
        );
        assert_eq!(reads("SELECT * WHERE { ?s ?p ?o }"), None);
        assert_eq!(reads("SELECT * WHERE { ?s !<http://e/a> ?o }"), None);
        assert_eq!(
            reads("SELECT * WHERE { SERVICE <http://e/s> { ?s <http://e/a> ?o } }"),
            None
        );
    }
}
