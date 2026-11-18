//! The Workbench's Pipeline tab (#404): runs a workspace's `pipeline.yaml` step by step,
//! keeping a copy of the store after each step (a snapshot) so that a step can be run again
//! from its input without starting over, and what each step added and removed (a diff).

use crate::pipeline::{self, Pipeline, Status};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};
use tripletrail::Engine;
use tripletrail::oxigraph::model::vocab::rdf;
use tripletrail::oxigraph::model::{NamedOrBlankNodeRef, Quad, Term};

/// Changed quads listed as samples in a step's diff, added and removed each.
const SAMPLES: usize = 20;
/// Predicates and classes listed in a step's diff, most changed first.
const TOP: usize = 50;

/// What the last runs of a workspace's pipeline left.
#[derive(Default)]
pub(crate) struct State {
    /// The `pipeline.yaml` the steps ran from: when it changes, they are run again.
    text: String,
    steps: Vec<StepState>,
    /// A python step waiting for its code to run from a notebook: the store it works on,
    /// served read-write at `/$/workspaces/{id}/pipeline/{N}/…` until `complete`.
    pending: Option<Pending>,
}

struct Pending {
    index: usize,
    /// What the step started from, to diff against.
    input: Engine,
    work: Engine,
    files: Vec<(String, u64, u64)>,
    values: HashMap<String, String>,
    begun: std::time::Instant,
}

#[derive(Default, Clone)]
struct StepState {
    report: Option<Value>,
    /// The store after the step, while it is small enough to keep.
    snapshot: Option<Engine>,
    diff: Option<Value>,
    /// The step's files (path, size, modified) when it ran.
    files: Vec<(String, u64, u64)>,
    /// The parameter values it ran with.
    values: HashMap<String, String>,
}

/// How a run is bounded.
pub(crate) struct Limits<'a> {
    /// The longest a step may take.
    pub timeout: Option<Duration>,
    /// The most quads the store may hold after a step.
    pub max_quads: usize,
    /// The largest store kept as a snapshot.
    pub max_snapshot: usize,
    /// The bytes the workspace has room for, now (asked again before each step).
    pub room: &'a dyn Fn() -> u64,
    /// The store the first step starts from, never written (an overlay's base, #406);
    /// `None`: an empty one.
    pub start: Option<Engine>,
}

/// The step's files as they are now: each file, and each file of a folder.
fn fingerprint(dir: &Path, step: &pipeline::Step) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    let mut add = |path: &Path| {
        if let Ok(meta) = path.metadata() {
            let modified = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as u64);
            out.push((path.display().to_string(), meta.len(), modified));
        }
    };
    let reads: Vec<&str> = match &step.kind {
        // What an export writes is not its input.
        pipeline::Kind::Export { query, .. } => query
            .as_deref()
            .filter(|q| !pipeline::is_inline_query(q))
            .into_iter()
            .collect(),
        pipeline::Kind::Publish { .. } => Vec::new(),
        // A pattern is not a file: its folder is what it reads.
        pipeline::Kind::Lift {
            files,
            for_each,
            then,
            ..
        } => files
            .iter()
            .chain(then)
            .map(String::as_str)
            .chain(for_each.as_deref().map(|p| match p.rsplit_once('/') {
                Some((d, l)) if l.contains(['*', '?']) => d,
                None if p.contains(['*', '?']) => ".",
                _ => p,
            }))
            .collect(),
        kind => kind.files(),
    };
    for file in reads {
        let path = dir.join(file);
        if path.is_dir() {
            let mut entries: Vec<_> = walk(&path);
            entries.sort();
            for e in entries {
                add(&e);
            }
        } else {
            add(&path);
        }
    }
    out
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// The pipeline of a workspace, read from its `pipeline.yaml`, with the file's text.
pub(crate) fn read(root: &Path) -> Result<(Pipeline, String), String> {
    let path = root.join("pipeline.yaml");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| "The workspace has no pipeline.yaml".to_owned())?;
    let pipeline = Pipeline::parse(&text, root.to_path_buf(), "pipeline")
        .map_err(|e| format!("pipeline.yaml: {e}"))?;
    Ok((pipeline, text))
}

impl State {
    /// Forgets the runs when the pipeline is not the one they ran.
    fn sync(&mut self, pipeline: &Pipeline, text: &str) {
        if self.text != text || self.steps.len() != pipeline.steps.len() {
            self.text = text.to_owned();
            self.steps = vec![StepState::default(); pipeline.steps.len()];
            self.pending = None;
        }
    }

    /// The store python step `i` works on while it waits for its code, read-write.
    pub(crate) fn pending(&self, i: usize) -> Option<Engine> {
        self.pending
            .as_ref()
            .filter(|p| p.index == i)
            .map(|p| p.work.clone())
    }

    /// Takes what the code of waiting python step `i` left as the step's result: the steps
    /// after it then run from there.
    pub(crate) fn complete(
        &mut self,
        root: &Path,
        i: usize,
        limits: &Limits<'_>,
    ) -> Result<(), String> {
        let (pipeline, text) = read(root)?;
        self.sync(&pipeline, &text);
        let Some(p) = self.pending.take_if(|p| p.index == i) else {
            return Err(format!(
                "step {} is not a python step waiting for its code",
                i + 1
            ));
        };
        let quads_before = p.input.store().len().unwrap_or(0);
        let quads_after = p.work.store().len().unwrap_or(0);
        if quads_after > limits.max_quads {
            let error = format!(
                "the store holds {quads_after} quads after this step, more than the {} it may hold here",
                limits.max_quads
            );
            self.pending = Some(p);
            return Err(error);
        }
        let step = &pipeline.steps[i];
        let state = &mut self.steps[i];
        state.report = Some(json!({
            "step": i + 1,
            "name": step.name,
            "kind": step.kind.name(),
            "status": "ok",
            "seconds": (p.begun.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
            "quadsBefore": quads_before,
            "quadsAfter": quads_after,
            "files": [],
            "completed": "by its code, from a client",
        }));
        state.files = p.files;
        state.values = p.values;
        // Kept whatever its size (it is within `max_quads`): without it, the next run would go
        // back before this step and lose what its code did.
        state.diff = Some(diff(&p.input, &p.work));
        state.snapshot = Some(p.work);
        Ok(())
    }

    /// The snapshot after step `i`, to query.
    pub(crate) fn snapshot(&self, i: usize) -> Option<Engine> {
        self.steps.get(i).and_then(|s| s.snapshot.clone())
    }

    /// The pipeline's steps with what their last run did, as JSON.
    pub(crate) fn describe(&mut self, root: &Path) -> Value {
        let (pipeline, text) = match read(root) {
            Ok(p) => p,
            Err(error) => return json!({ "error": error, "steps": [] }),
        };
        self.sync(&pipeline, &text);
        // A step whose files changed since it ran, and every step after it, is out of date.
        let mut outdated = false;
        let steps: Vec<Value> = pipeline
            .steps
            .iter()
            .zip(&self.steps)
            .enumerate()
            .map(|(i, (step, state))| {
                let changed = state.report.is_some() && fingerprint(root, step) != state.files;
                outdated |= changed;
                json!({
                    "step": i + 1,
                    "name": step.name,
                    "kind": step.kind.name(),
                    "files": step.kind.files(),
                    "target": match &step.kind {
                        pipeline::Kind::Python { target, .. } => Some(target.as_str()),
                        _ => None,
                    },
                    "graph": step.graph.as_ref().map(|g| g.as_str()),
                    "timeout": step.timeout.map(|t| t.as_secs_f64()),
                    "report": state.report,
                    "snapshot": state.snapshot.is_some(),
                    "diff": state.diff,
                    "changed": changed,
                    "outdated": outdated && state.report.is_some(),
                })
            })
            .collect();
        json!({
            "name": pipeline.name,
            "parameters": pipeline.parameters,
            "steps": steps,
        })
    }

    /// Runs the steps `from..=to` (indexes; `from` defaults to the first step not run or out
    /// of date, `to` to the last), starting from the snapshot before `from`, or from the
    /// latest earlier snapshot there is.
    pub(crate) fn run(
        &mut self,
        root: &Path,
        from: Option<&str>,
        to: Option<&str>,
        values: HashMap<String, String>,
        limits: &Limits<'_>,
    ) -> Result<(), String> {
        let (pipeline, text) = read(root)?;
        self.sync(&pipeline, &text);
        let last = pipeline.steps.len() - 1;
        let to = to
            .map(|s| pipeline.step_index(s))
            .transpose()?
            .unwrap_or(last);
        let mut from = match from {
            Some(s) => pipeline.step_index(s)?,
            None => pipeline
                .steps
                .iter()
                .zip(&self.steps)
                .position(|(step, state)| {
                    state.report.as_ref().is_none_or(|r| r["status"] != "ok")
                        || fingerprint(root, step) != state.files
                })
                .unwrap_or(0),
        };
        if from > to {
            return Err(format!(
                "step {} comes after step {}: nothing to run",
                from + 1,
                to + 1
            ));
        }
        // Back to the latest step there is a snapshot after.
        while from > 0 && self.steps[from - 1].snapshot.is_none() {
            from -= 1;
        }
        let mut input = match from {
            0 => limits.start.clone().unwrap_or_else(Engine::new_tripletrail),
            n => self.steps[n - 1].snapshot.clone().expect("checked above"),
        };
        for s in &mut self.steps[from..] {
            *s = StepState::default();
        }
        self.pending = None;
        for i in from..=to {
            let work = copy(&input)?;
            let step = &pipeline.steps[i];
            if matches!(step.kind, pipeline::Kind::Python { .. }) {
                // The server does not run it: its code runs from a notebook against the
                // store the step works on, served for it, and `complete` takes the result.
                if step.clear
                    && let Some(g) = &step.graph
                {
                    work.update(&format!("DROP SILENT GRAPH {g}"))
                        .map_err(|e| e.to_string())?;
                }
                let quads = work.store().len().unwrap_or(0);
                self.steps[i].report = Some(json!({
                    "step": i + 1,
                    "name": step.name,
                    "kind": "python",
                    "status": "waiting",
                    "seconds": 0,
                    "quadsBefore": quads,
                    "quadsAfter": quads,
                    "files": [],
                    "error": format!(
                        "waiting for its code: run it against base_url …/pipeline, dataset \"{}\" \
                         (ws.run_step({}, function) in Python), then complete the step",
                        i + 1,
                        i + 1
                    ),
                }));
                self.pending = Some(Pending {
                    index: i,
                    input,
                    work,
                    files: fingerprint(root, step),
                    values: values.clone(),
                    begun: std::time::Instant::now(),
                });
                break;
            }
            let options = pipeline::Options {
                root: Some(root),
                values: values.clone(),
                from: i,
                to: Some(i + 1),
                allow: Vec::new(),
                timeout: limits.timeout,
                max_quads: Some(limits.max_quads),
                max_output: Some((limits.room)()),
                scratch: None,
                // A server does not run its callers' code.
                python: None,
                store: None,
                // Nor does it read URLs for them.
                remote: Default::default(),
            };
            let files = fingerprint(root, &pipeline.steps[i]);
            let report = pipeline::run(&pipeline, &work, &options, &|_, _| {});
            let step = &report.steps[0];
            let ok = step.status == Status::Ok;
            let keep = step.quads_after <= limits.max_snapshot;
            let state = &mut self.steps[i];
            state.report = Some(step.to_json());
            state.files = files;
            state.values = values.clone();
            if keep {
                state.diff = Some(diff(&input, &work));
                state.snapshot = Some(work.clone());
            }
            if !ok {
                break;
            }
            input = work;
        }
        Ok(())
    }
}

impl State {
    /// Which steps of the last run added or removed a triple (in any graph), and from which
    /// file, for the steps there are snapshots of (from the first): blame.
    pub(crate) fn blame(
        &self,
        root: &Path,
        subject: &str,
        predicate: &str,
        object: &str,
        limits: &Limits<'_>,
    ) -> Result<Value, String> {
        use std::str::FromStr;
        use tripletrail::oxigraph::model::{NamedNode, NamedOrBlankNode};
        let (pipeline, text) = read(root)?;
        if text != self.text {
            return Err("pipeline.yaml changed since the pipeline ran: run it again".to_owned());
        }
        let term = |t: &str, what: &str| {
            Term::from_str(t.trim())
                .map_err(|e| format!("{what}: {t} is not an N-Triples term: {e}"))
        };
        let subject = match term(subject, "subject")? {
            Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n),
            Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b),
            _ => return Err("subject: an IRI or a blank node".to_owned()),
        };
        let predicate = match term(predicate, "predicate")? {
            Term::NamedNode(n) => n,
            _ => return Err("predicate: an IRI".to_owned()),
        };
        let object = term(object, "object")?;
        let has = |engine: &Engine| {
            engine
                .store()
                .quads_for_pattern(
                    Some(subject.as_ref()),
                    Some(NamedNode::as_ref(&predicate)),
                    Some(object.as_ref()),
                    None,
                )
                .next()
                .is_some()
        };
        let mut events = Vec::new();
        let mut before = false;
        let mut checked = 0;
        for (i, state) in self.steps.iter().enumerate() {
            let Some(snapshot) = &state.snapshot else {
                break;
            };
            checked = i + 1;
            let now = has(snapshot);
            if now != before {
                let step = &pipeline.steps[i];
                let file = self.file_of(root, &pipeline, i, now, &has, limits)?;
                events.push(json!({
                    "step": i + 1,
                    "name": step.name,
                    "kind": step.kind.name(),
                    "change": if now { "added" } else { "removed" },
                    "file": file,
                    "files": step.kind.files(),
                }));
            }
            before = now;
        }
        Ok(json!({
            "events": events,
            "present": before,
            "checkedSteps": checked,
            "steps": pipeline.steps.len(),
        }))
    }

    /// The file of step `i` that added (or removed) the triple: the step's only file, or the
    /// first file after which the triple is there (or gone) when the step is run again file by
    /// file on its input. `None` for steps whose files are rules or shapes.
    fn file_of(
        &self,
        root: &Path,
        pipeline: &Pipeline,
        i: usize,
        added: bool,
        has: &dyn Fn(&Engine) -> bool,
        limits: &Limits<'_>,
    ) -> Result<Option<String>, String> {
        let step = &pipeline.steps[i];
        let extensions: &[&str] = match &step.kind {
            pipeline::Kind::Load { .. } => &[
                "ttl", "nt", "trig", "nq", "rdf", "owl", "jsonld", "n3", "xml", "gz",
            ],
            pipeline::Kind::Update { .. } => &["ru"],
            pipeline::Kind::Lift { .. } => &[
                "rq", "sparql", "ttl", "rml", "r2rml", "yarrrml", "yml", "yaml",
            ],
            _ => return Ok(None),
        };
        let mut files = Vec::new();
        let named = match &step.kind {
            // The queries and mappings, not the inputs of `for_each` or the updates of `then`.
            pipeline::Kind::Lift { files, .. } => files.iter().map(String::as_str).collect(),
            kind => kind.files(),
        };
        for f in named {
            let path = crate::testing::inside(root, f, Some(root))?;
            if path.is_dir() {
                let mut found: Vec<_> = walk(&path)
                    .into_iter()
                    .filter(|p| {
                        p.extension()
                            .is_some_and(|e| extensions.contains(&e.to_string_lossy().as_ref()))
                    })
                    .collect();
                found.sort();
                files.extend(found);
            } else {
                files.push(path);
            }
        }
        let real_root = root.canonicalize().map_err(|e| e.to_string())?;
        let relative = |p: &Path| {
            p.strip_prefix(&real_root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        };
        if files.len() == 1 {
            return Ok(Some(relative(&files[0])));
        }
        let work = match i {
            0 => Engine::new_tripletrail(),
            n => match &self.steps[n - 1].snapshot {
                Some(s) => copy(s)?,
                None => return Ok(None),
            },
        };
        for file in &files {
            let name = relative(file);
            let mut one = step.clone();
            one.timeout = None;
            one.kind = match &step.kind {
                pipeline::Kind::Load { .. } => pipeline::Kind::Load {
                    files: vec![name.clone()],
                },
                pipeline::Kind::Update { .. } => pipeline::Kind::Update {
                    files: vec![name.clone()],
                },
                pipeline::Kind::Lift {
                    base,
                    jobs,
                    for_each,
                    each_as,
                    then,
                    ..
                } => pipeline::Kind::Lift {
                    files: vec![name.clone()],
                    base: base.clone(),
                    jobs: *jobs,
                    for_each: for_each.clone(),
                    each_as: each_as.clone(),
                    then: then.clone(),
                },
                _ => return Ok(None),
            };
            let replay = Pipeline {
                dir: pipeline.dir.clone(),
                name: pipeline.name.clone(),
                parameters: pipeline.parameters.clone(),
                union_default_graph: pipeline.union_default_graph,
                datasets: pipeline.datasets.clone(),
                steps: vec![one],
            };
            let options = pipeline::Options {
                root: Some(root),
                values: self.steps[i].values.clone(),
                timeout: limits.timeout,
                max_quads: Some(limits.max_quads),
                ..pipeline::Options::default()
            };
            let report = pipeline::run(&replay, &work, &options, &|_, _| {});
            if report.steps[0].status != Status::Ok {
                return Ok(None);
            }
            if has(&work) == added {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }
}

/// A copy of a store, to run a step on while the original stays the step's input.
/// A copy of the store to run a step on: a fork, which costs no decoding and shares the quad
/// index until it writes, on the tripletrail store.
fn copy(engine: &Engine) -> Result<Engine, String> {
    if let tripletrail::store::Dataset::TripleTrail(store) = engine.store() {
        return Ok(Engine::from_store(store.fork()));
    }
    let out = Engine::new_tripletrail();
    let quads: Result<Vec<Quad>, _> = engine
        .store()
        .quads_for_pattern(None, None, None, None)
        .collect();
    out.load_quads(quads.map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

fn contains(engine: &Engine, q: &Quad) -> bool {
    engine
        .store()
        .quads_for_pattern(
            Some(q.subject.as_ref()),
            Some(q.predicate.as_ref()),
            Some(q.object.as_ref()),
            Some(q.graph_name.as_ref()),
        )
        .next()
        .is_some()
}

/// The classes of a subject in a store.
fn classes(engine: &Engine, subject: NamedOrBlankNodeRef<'_>) -> Vec<String> {
    engine
        .store()
        .quads_for_pattern(Some(subject), Some(rdf::TYPE), None, None)
        .filter_map(Result::ok)
        .filter_map(|q| match q.object {
            Term::NamedNode(n) => Some(n.into_string()),
            _ => None,
        })
        .collect()
}

/// What changed from `before` to `after`: counts of quads added and removed by predicate and
/// by class of their subject (as typed in the store they are in), with samples.
fn diff(before: &Engine, after: &Engine) -> Value {
    use tripletrail::store::Dataset;
    match (before.store(), after.store()) {
        (Dataset::TripleTrail(b), Dataset::TripleTrail(a)) => diff_forked(b, a),
        _ => diff_scanned(before, after),
    }
}

/// [`diff`] of a store forked from `before` (see [`copy`]): one merge of the two quad indexes
/// by id, and terms decoded only for the predicates, classes and samples it reports.
fn diff_forked(before: &tripletrail::store::Store, after: &tripletrail::store::Store) -> Value {
    use tripletrail::store::{GraphSelector, TermId};
    let (added, removed) = after.changes_from(before);
    let views = [after.snapshot(), before.snapshot()];
    // Ids mean the same in both; `after` knows every term of either.
    let term = |id: TermId| views[0].term(id).map(|t| t.to_string()).unwrap_or_default();
    let iri = |id: TermId| match views[0].term(id) {
        Ok(Term::NamedNode(n)) => n.into_string(),
        Ok(t) => t.to_string(),
        Err(_) => String::new(),
    };
    let rdf_type = views[0].term_id(rdf::TYPE.into());
    let mut by_predicate: HashMap<TermId, [usize; 2]> = HashMap::new();
    let mut by_class: HashMap<Option<TermId>, [usize; 2]> = HashMap::new();
    let mut samples: [Vec<String>; 2] = [Vec::new(), Vec::new()];
    let mut class_cache: [HashMap<TermId, Vec<TermId>>; 2] = [HashMap::new(), HashMap::new()];
    for (side, quads) in [&added, &removed].into_iter().enumerate() {
        for q in quads {
            by_predicate.entry(q[1]).or_default()[side] += 1;
            let cs = class_cache[side]
                .entry(q[0])
                .or_insert_with(|| match rdf_type {
                    Some(t) => views[side]
                        .quad_ids_for_pattern(Some(q[0]), Some(t), None, GraphSelector::All)
                        .filter(|c| matches!(views[0].term(c[2]), Ok(Term::NamedNode(_))))
                        .map(|c| c[2])
                        .collect(),
                    None => Vec::new(),
                });
            if cs.is_empty() {
                by_class.entry(None).or_default()[side] += 1;
            }
            for c in cs.iter() {
                by_class.entry(Some(*c)).or_default()[side] += 1;
            }
            if samples[side].len() < SAMPLES {
                let mut line = format!("{} {} {}", term(q[0]), term(q[1]), term(q[2]));
                if q[3].0 != 0 {
                    line.push(' ');
                    line.push_str(&term(q[3]));
                }
                samples[side].push(line);
            }
        }
    }
    let by_predicate: BTreeMap<String, [usize; 2]> =
        by_predicate.into_iter().map(|(p, n)| (iri(p), n)).collect();
    let mut classes: BTreeMap<String, [usize; 2]> = BTreeMap::new();
    for (c, [a, r]) in by_class {
        let e = classes.entry(c.map(iri).unwrap_or_default()).or_default();
        e[0] += a;
        e[1] += r;
    }
    summary([added.len(), removed.len()], by_predicate, classes, samples)
}

/// [`diff`] of any two stores: each quad of one looked up in the other.
fn diff_scanned(before: &Engine, after: &Engine) -> Value {
    let mut by_predicate: BTreeMap<String, [usize; 2]> = BTreeMap::new();
    let mut by_class: BTreeMap<String, [usize; 2]> = BTreeMap::new();
    let mut samples: [Vec<String>; 2] = [Vec::new(), Vec::new()];
    let mut totals = [0usize; 2];
    let mut class_cache: [HashMap<String, Vec<String>>; 2] = [HashMap::new(), HashMap::new()];
    for (side, (from, other)) in [(after, before), (before, after)].into_iter().enumerate() {
        for q in from.store().quads_for_pattern(None, None, None, None) {
            let Ok(q) = q else { continue };
            if contains(other, &q) {
                continue;
            }
            totals[side] += 1;
            by_predicate
                .entry(q.predicate.as_str().to_owned())
                .or_default()[side] += 1;
            let key = q.subject.to_string();
            let cs = class_cache[side]
                .entry(key)
                .or_insert_with(|| classes(from, q.subject.as_ref()));
            if cs.is_empty() {
                by_class.entry(String::new()).or_default()[side] += 1;
            }
            for c in cs.iter() {
                by_class.entry(c.clone()).or_default()[side] += 1;
            }
            if samples[side].len() < SAMPLES {
                samples[side].push(q.to_string());
            }
        }
    }
    summary(totals, by_predicate, by_class, samples)
}

fn summary(
    totals: [usize; 2],
    by_predicate: BTreeMap<String, [usize; 2]>,
    by_class: BTreeMap<String, [usize; 2]>,
    samples: [Vec<String>; 2],
) -> Value {
    let top = |map: BTreeMap<String, [usize; 2]>, key: &str| {
        let mut rows: Vec<(String, [usize; 2])> = map.into_iter().collect();
        rows.sort_by(|a, b| {
            (b.1[0] + b.1[1])
                .cmp(&(a.1[0] + a.1[1]))
                .then(a.0.cmp(&b.0))
        });
        rows.into_iter()
            .take(TOP)
            .map(|(k, [added, removed])| {
                let mut v = json!({ "added": added, "removed": removed });
                v[key] = if k.is_empty() { Value::Null } else { k.into() };
                v
            })
            .collect::<Vec<_>>()
    };
    json!({
        "added": totals[0],
        "removed": totals[1],
        "predicates": top(by_predicate, "predicate"),
        "classes": top(by_class, "class"),
        "addedSamples": samples[0],
        "removedSamples": samples[1],
    })
}
