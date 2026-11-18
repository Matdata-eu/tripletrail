//! Workbench pipelines (#404): an ETL as an ordered list of steps in a `pipeline.yaml`, run
//! headless by `tripletrail pipeline run` (from Airflow, CI or cron) and, step by step, in the
//! Workbench.
//!
//! ```yaml
//! name: network-etl
//! parameters:          # defaults; `-v name=value` overrides them
//!   date: 2026-01-01
//! steps:
//!   - lift: 01-lift/                 # Facade-X queries and RML/R2RML/YARRRML, as `tripletrail lift`
//!     base: tabular/
//!   - load: [extra.ttl, codes.nt]    # files or folders into a graph
//!     graph: http://example.org/codes
//!   - update: 03-post-process/       # a .ru file, or a folder of them in name order
//!   - infer: rules/                  # SHACL-AF rules: what they infer goes into `graph`
//!   - validate: shapes/era.ttl       # a gate: stops when violations remain
//!     fail_on: violation
//!     report: out/report.ttl
//!   - export: out/kg.nt              # the default graph, a `graph`, or a CONSTRUCT `query`
//! ```
//!
//! Each step is a map with one kind key (`lift`, `load`, `update`, `infer`, `validate`, `export`)
//! naming its files, and optionally `name`, `graph` (the graph it writes to or exports; the
//! default graph if omitted) and `timeout` (seconds). Paths are relative to the
//! `pipeline.yaml`. Parameters bind as `tripletrail lift -v` does (`?_name`, `?_name_iri`,
//! `?_name_number`), in lift queries, updates and export queries.

use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfSerializer;
use tripletrail::oxigraph::model::{GraphName, GraphNameRef, NamedNode};
use tripletrail::oxigraph::sparql::QueryResults;
use yaml_rust2::{Yaml, YamlLoader};

use crate::testing::{files_of, inside, rdf_format};

/// What the interpreter runs for a python step given as `module:function`.
const PYTHON_CALL: &str = r#"import importlib, json, os
module, _, name = os.environ["TRIPLETRAIL_CALLABLE"].partition(":")
function = importlib.import_module(module)
for part in name.split("."):
    function = getattr(function, part)
function(os.environ["TRIPLETRAIL_BASE_URL"], os.environ["TRIPLETRAIL_DATASET"], **json.loads(os.environ["TRIPLETRAIL_KWARGS"]))
"#;

/// `${name}` in `text` replaced by the parameter's value; `$$` is a `$`.
fn bind_values(text: &str, values: &HashMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(r) = after.strip_prefix('$') {
            out.push('$');
            rest = r;
        } else if let Some(r) = after.strip_prefix('{')
            && let Some(end) = r.find('}')
        {
            let name = &r[..end];
            let value = values
                .get(name)
                .ok_or_else(|| format!("${{{name}}}: no parameter {name}"))?;
            out.push_str(value);
            rest = &r[end + 1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// A variable or parameter name: letters, digits and `_`.
fn is_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Whether `name` matches `pattern`, where `*` is any text and `?` any one character.
fn wildcard(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < n.len() {
        if i < p.len() && (p[i] == '?' || p[i] == n[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == '*')
}

/// The named graph a step's shapes or rules are loaded into while it runs.
const SHAPES: &str = "urn:x-tripletrail:pipeline:shapes";
const SH: &str = "http://www.w3.org/ns/shacl#";
/// The step kinds a pipeline knows, in the order of the issue.
/// The graph of a `reason` step without one.
const INFERRED_GRAPH: &str = "urn:tripletrail:inferred";
const KINDS: &[&str] = &[
    "lift", "load", "update", "infer", "reason", "validate", "export", "python", "publish",
];
const RDF: &[&str] = &[
    "ttl", "nt", "trig", "nq", "rdf", "owl", "jsonld", "n3", "xml", "gz",
];

/// A pipeline, as read from its `pipeline.yaml`.
#[derive(Debug, Clone)]
pub struct Pipeline {
    /// The folder of the `pipeline.yaml`: paths are relative to it.
    pub dir: PathBuf,
    pub name: String,
    /// Parameter defaults.
    pub parameters: HashMap<String, String>,
    /// The default graph of queries, updates and rules is the union of all graphs while the
    /// pipeline runs (Fuseki's `unionDefaultGraph`).
    pub union_default_graph: bool,
    /// More datasets, empty and in memory, that python steps reach next to the pipeline's
    /// own on the loopback server (as `{base}/{name}/update`): those a script also writes to
    /// on Fuseki, such as a lineage dataset. They last for the run.
    pub datasets: Vec<String>,
    pub steps: Vec<Step>,
}

/// One step of a pipeline.
#[derive(Debug, Clone)]
pub struct Step {
    pub name: String,
    pub kind: Kind,
    /// The graph it writes to (or exports); the default graph if `None`.
    pub graph: Option<NamedNode>,
    /// Drop `graph` before the step runs, so that running it again replaces what it wrote.
    pub clear: bool,
    /// Variables of this step, over the pipeline's parameters (`${name}` in a value is a
    /// parameter's value).
    pub variables: Vec<(String, String)>,
    pub timeout: Option<Duration>,
}

/// What a step does.
#[derive(Debug, Clone)]
pub enum Kind {
    /// Query and mapping files or folders, run as `tripletrail lift`, their triples into the graph.
    Lift {
        files: Vec<String>,
        /// Relative file locations in the queries resolve against it (the pipeline's folder by
        /// default).
        base: Option<String>,
        jobs: Option<usize>,
        /// Lift once per file this matches (a folder, or a file pattern with `*` and `?` in its
        /// last part), with the file's path bound to `as` (`file` by default), its name to
        /// `{as}_name` and its name without extension to `{as}_stem`.
        for_each: Option<String>,
        each_as: String,
        /// SPARQL updates run on what each lift gave, on its own, before it is added to the
        /// graph (e.g. to drop the M ordinate of WKT literals).
        then: Vec<String>,
    },
    /// RDF files or folders loaded into the graph.
    Load { files: Vec<String> },
    /// SPARQL update files, or folders of `.ru` files run in name order.
    Update { files: Vec<String> },
    /// SHACL-AF rules run on `data`, what they infer written to the step's graph (`data` if
    /// none).
    Infer {
        files: Vec<String>,
        data: Option<NamedNode>,
        max_passes: usize,
        all_constructed: bool,
        /// Each rule once, in `sh:order` (then by rule IRI), each seeing what the rules before
        /// it inferred: one pass, as a list of SPARQL INSERTs run in that order.
        ordered: bool,
    },
    /// Entailment (#187): what the regime's rules derive from the other graphs, written to the
    /// step's graph (`urn:tripletrail:inferred` if none), replacing what it held. With `owl-rl`
    /// the OWL 2 RL consistency rules then run: the step reports the contradictions, and with
    /// `fail_on_inconsistency` fails on one (with the entailment written, like Jena).
    Reason {
        regime: String,
        fail_on_inconsistency: bool,
    },
    /// Validation of `data`: the pipeline stops when results of `fail_on` or worse remain.
    Validate {
        files: Vec<String>,
        data: Option<NamedNode>,
        fail_on: Severity,
        report: Option<String>,
    },
    /// The step's graph (the whole dataset for `.nq` and `.trig` when no graph is given), or
    /// what a CONSTRUCT `query` gives, written to `file`.
    Export { file: String, query: Option<String> },
    /// Python run against the dataset, served for the step on a loopback port with the
    /// SPARQL Protocol and the Graph Store Protocol as Fuseki serves them.
    Python {
        /// `module:function`, called as `function(base_url, dataset, **kwargs)`; or a `.py`
        /// script, run with `args` and the endpoint in its environment.
        target: String,
        /// Folders put first on the module search path (the pipeline's folder by default).
        path: Vec<String>,
        args: Vec<String>,
        kwargs: Vec<(String, String)>,
        /// The dataset name the endpoint is served under.
        dataset: String,
    },
    /// The graphs given (all of them by default) written as a new on-disk dataset at
    /// `location`, a dataset folder (`tripletrail serve --attach NAME=DIR`); it replaces the dataset there
    /// when the step succeeds.
    Publish {
        location: String,
        /// Graph IRIs; one ending in `*` stands for every graph whose IRI starts with the rest.
        /// `default` is the default graph.
        graphs: Vec<String>,
        /// All of them into the new dataset's default graph instead of under their names.
        merge: bool,
    },
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Lift { .. } => "lift",
            Kind::Load { .. } => "load",
            Kind::Update { .. } => "update",
            Kind::Infer { .. } => "infer",
            Kind::Reason { .. } => "reason",
            Kind::Validate { .. } => "validate",
            Kind::Export { .. } => "export",
            Kind::Python { .. } => "python",
            Kind::Publish { .. } => "publish",
        }
    }

    /// The files the step names, as written.
    pub fn files(&self) -> Vec<&str> {
        match self {
            Kind::Lift {
                files,
                for_each,
                then,
                ..
            } => files
                .iter()
                .chain(then)
                .map(String::as_str)
                .chain(for_each.as_deref())
                .collect(),
            Kind::Load { files }
            | Kind::Update { files }
            | Kind::Infer { files, .. }
            | Kind::Validate { files, .. } => files.iter().map(String::as_str).collect(),
            Kind::Export { file, query } => std::iter::once(file.as_str())
                .chain(query.as_deref().filter(|q| !is_inline_query(q)))
                .collect(),
            // A script, or the folders of the module.
            Kind::Python { target, path, .. } if !target.ends_with(".py") => {
                path.iter().map(String::as_str).collect()
            }
            Kind::Python { target, .. } => vec![target.as_str()],
            Kind::Publish { location, .. } => vec![location.as_str()],
            Kind::Reason { .. } => Vec::new(),
        }
    }
}

/// The severity a validation gate stops at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info = 1,
    Warning = 2,
    Violation = 3,
    /// Never stop: the step only reports.
    Never = 4,
}

impl Severity {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "info" => Self::Info,
            "warning" => Self::Warning,
            "violation" => Self::Violation,
            "never" | "none" => Self::Never,
            _ => return None,
        })
    }

    /// The severity of a `sh:resultSeverity`; one SHACL does not define counts as a violation.
    fn of(iri: &str) -> Self {
        match iri.strip_prefix(SH) {
            Some("Info") => Self::Info,
            Some("Warning") => Self::Warning,
            _ => Self::Violation,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Violation => "violation",
            Self::Never => "never",
        }
    }
}

fn yaml_str(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(s) | Yaml::Real(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn yaml_strings(y: &Yaml, what: &str) -> Result<Vec<String>, String> {
    match y {
        Yaml::Array(items) => items
            .iter()
            .map(|i| yaml_str(i).ok_or_else(|| format!("{what}: expected a path")))
            .collect(),
        other => Ok(vec![yaml_str(other).ok_or_else(|| {
            format!("{what}: expected a path or a list of paths")
        })?]),
    }
}

fn graph_iri(value: &str, what: &str) -> Result<NamedNode, String> {
    let value = value.trim();
    let value = value
        .strip_prefix('<')
        .and_then(|v| v.strip_suffix('>'))
        .unwrap_or(value);
    NamedNode::new(value).map_err(|e| format!("{what}: {value} is not an IRI: {e}"))
}

impl Pipeline {
    /// Reads a `pipeline.yaml`.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        let fallback = path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self::parse(&text, dir, &fallback)
            .map_err(|e| format!("{}: {e}", path.file_name().unwrap_or_default().display()))
    }

    /// Parses the text of a `pipeline.yaml` whose paths are relative to `dir`.
    pub fn parse(text: &str, dir: PathBuf, fallback_name: &str) -> Result<Self, String> {
        let doc = YamlLoader::load_from_str(text)
            .map_err(|e| e.to_string())?
            .into_iter()
            .next()
            .unwrap_or(Yaml::Null);
        let Yaml::Hash(top) = doc else {
            return Err("expected a map with `steps`".to_owned());
        };
        let mut name = fallback_name.to_owned();
        let mut parameters = HashMap::new();
        let mut union_default_graph = false;
        let mut datasets: Vec<String> = Vec::new();
        let mut steps = Vec::new();
        for (key, value) in &top {
            match key.as_str() {
                Some("name") => {
                    name = yaml_str(value).ok_or("name: expected text")?;
                }
                Some("parameters") => match value {
                    Yaml::Hash(map) => {
                        for (k, v) in map {
                            let k = k.as_str().ok_or("parameters: names are text")?;
                            let v = yaml_str(v)
                                .ok_or_else(|| format!("parameters: {k}: expected a value"))?;
                            parameters.insert(k.to_owned(), v);
                        }
                    }
                    Yaml::Null => {}
                    _ => return Err("parameters: expected a map of name: value".to_owned()),
                },
                Some("union_default_graph") => {
                    union_default_graph = value
                        .as_bool()
                        .ok_or("union_default_graph: expected true or false")?;
                }
                Some("datasets") => {
                    let names: Vec<&Yaml> = match value {
                        Yaml::Array(items) => items.iter().collect(),
                        Yaml::Null => Vec::new(),
                        other => vec![other],
                    };
                    for n in names {
                        let n = yaml_str(n).ok_or("datasets: expected a list of names")?;
                        if n.is_empty()
                            || !n
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                        {
                            return Err(format!(
                                "datasets: {n:?} is no dataset name (letters, digits, - and _)"
                            ));
                        }
                        if datasets.contains(&n) {
                            return Err(format!("datasets: {n} twice"));
                        }
                        datasets.push(n);
                    }
                }
                Some("steps") => {
                    let Yaml::Array(items) = value else {
                        return Err("steps: expected a list".to_owned());
                    };
                    for (i, item) in items.iter().enumerate() {
                        steps.push(
                            Step::parse(item, i + 1).map_err(|e| format!("step {}: {e}", i + 1))?,
                        );
                    }
                }
                other => {
                    return Err(format!(
                        "unknown key {} (expected name, parameters, union_default_graph, datasets and steps)",
                        other.unwrap_or("?")
                    ));
                }
            }
        }
        if steps.is_empty() {
            return Err("no steps".to_owned());
        }
        let mut names = std::collections::HashSet::new();
        for s in &steps {
            if !names.insert(s.name.as_str()) {
                return Err(format!("two steps are named {}", s.name));
            }
        }
        Ok(Self {
            dir,
            name,
            parameters,
            union_default_graph,
            datasets,
            steps,
        })
    }

    /// The index of a step given by its number (from 1) or its name.
    pub fn step_index(&self, step: &str) -> Result<usize, String> {
        if let Ok(n) = step.parse::<usize>() {
            if (1..=self.steps.len()).contains(&n) {
                return Ok(n - 1);
            }
            return Err(format!(
                "there is no step {n}: the pipeline has {} steps",
                self.steps.len()
            ));
        }
        self.steps
            .iter()
            .position(|s| s.name == step)
            .ok_or_else(|| format!("there is no step named {step}"))
    }
}

impl Step {
    fn parse(item: &Yaml, number: usize) -> Result<Self, String> {
        let Yaml::Hash(map) = item else {
            return Err("expected a map such as `update: file.ru`".to_owned());
        };
        let get = |k: &str| map.get(&Yaml::String(k.to_owned()));
        let kinds: Vec<&str> = KINDS.iter().copied().filter(|k| get(k).is_some()).collect();
        let kind = match kinds.as_slice() {
            [one] => *one,
            [] => {
                return Err(format!("no step kind: give one of {}", KINDS.join(", ")));
            }
            more => return Err(format!("more than one step kind: {}", more.join(", "))),
        };
        let allowed: &[&str] = match kind {
            "lift" => &["base", "jobs", "for_each", "as", "then"],
            "publish" => &["graphs", "merge"],
            "infer" => &["data", "max_passes", "all_constructed", "ordered"],
            "python" => &["path", "args", "kwargs", "dataset"],
            "validate" => &["data", "fail_on", "report"],
            "export" => &["query"],
            "reason" => &["fail_on_inconsistency"],
            _ => &[],
        };
        for k in map.keys() {
            let k = k.as_str().unwrap_or("?");
            if k != kind
                && !["name", "graph", "clear", "timeout", "variables"].contains(&k)
                && !allowed.contains(&k)
            {
                return Err(format!("{kind}: unknown key {k}"));
            }
        }
        let files = || yaml_strings(get(kind).unwrap_or(&Yaml::Null), kind);
        let text = |k: &str| -> Result<Option<String>, String> {
            get(k)
                .map(|v| yaml_str(v).ok_or_else(|| format!("{k}: expected text")))
                .transpose()
        };
        let graph = |k: &str| -> Result<Option<NamedNode>, String> {
            text(k)?.map(|g| graph_iri(&g, k)).transpose()
        };
        let number_of = |k: &str| -> Result<Option<usize>, String> {
            get(k)
                .map(|v| match v {
                    Yaml::Integer(i) if *i >= 0 => Ok(*i as usize),
                    _ => Err(format!("{k}: expected a whole number")),
                })
                .transpose()
        };
        let flag = |k: &str| match get(k) {
            None => Ok(false),
            Some(Yaml::Boolean(b)) => Ok(*b),
            Some(_) => Err(format!("{k}: expected true or false")),
        };
        let kind = match kind {
            "lift" => Kind::Lift {
                files: files()?,
                base: text("base")?,
                jobs: number_of("jobs")?,
                for_each: text("for_each")?,
                each_as: match (text("as")?, get("for_each")) {
                    (Some(_), None) => {
                        return Err("as: names the variable of `for_each`".to_owned());
                    }
                    (Some(a), _) if !is_name(&a) => {
                        return Err(format!("as: {a}: letters, digits and _ only"));
                    }
                    (Some(a), _) => a,
                    (None, _) => "file".to_owned(),
                },
                then: match get("then") {
                    Some(t) => yaml_strings(t, "then")?,
                    None => Vec::new(),
                },
            },
            "publish" => Kind::Publish {
                location: match files()?.as_slice() {
                    [one] => one.clone(),
                    _ => return Err("publish: expected one dataset folder".to_owned()),
                },
                graphs: match get("graphs") {
                    Some(g) => {
                        let graphs = yaml_strings(g, "graphs")?;
                        for g in &graphs {
                            let iri = g.strip_suffix('*').unwrap_or(g);
                            if g != "default" && !g.ends_with('*') {
                                graph_iri(iri, "graphs")?;
                            }
                        }
                        graphs
                    }
                    None => Vec::new(),
                },
                merge: flag("merge")?,
            },
            "load" => Kind::Load { files: files()? },
            "update" => Kind::Update { files: files()? },
            "infer" => Kind::Infer {
                files: files()?,
                data: graph("data")?,
                max_passes: number_of("max_passes")?.unwrap_or(64),
                all_constructed: flag("all_constructed")?,
                ordered: flag("ordered")?,
            },
            "validate" => Kind::Validate {
                files: files()?,
                data: graph("data")?,
                fail_on: match text("fail_on")? {
                    None => Severity::Violation,
                    Some(s) => Severity::parse(&s).ok_or_else(|| {
                        format!("fail_on: {s} (expected violation, warning, info or never)")
                    })?,
                },
                report: text("report")?,
            },
            "export" => Kind::Export {
                file: match files()?.as_slice() {
                    [one] => one.clone(),
                    _ => return Err("export: expected one file".to_owned()),
                },
                query: text("query")?,
            },
            "reason" => match text("reason")?.as_deref().map(str::trim) {
                Some(regime @ ("rdfs" | "owl-rl")) => Kind::Reason {
                    regime: regime.to_owned(),
                    fail_on_inconsistency: match flag("fail_on_inconsistency")? {
                        true if regime == "rdfs" => {
                            return Err(
                                "fail_on_inconsistency: RDFS has no inconsistency (use owl-rl)"
                                    .to_owned(),
                            );
                        }
                        f => f,
                    },
                },
                Some(other) => {
                    return Err(format!(
                        "reason: {other} (expected rdfs or owl-rl, the OWL 2 RL subset of #187)"
                    ));
                }
                None => return Err("reason: expected rdfs or owl-rl".to_owned()),
            },
            _ => {
                let target = text("python")?.unwrap_or_default();
                let target = target.trim().to_owned();
                let script = target.ends_with(".py");
                if !script {
                    let (module, function) = target.split_once(':').unwrap_or_default();
                    let name = |s: &str| {
                        !s.is_empty()
                            && s.split('.').all(|p| {
                                !p.is_empty()
                                    && !p.starts_with(|c: char| c.is_ascii_digit())
                                    && p.chars().all(|c| c.is_alphanumeric() || c == '_')
                            })
                    };
                    if !name(module) || !name(function) {
                        return Err(format!(
                            "python: {target}: expected module:function (e.g. steps.geometry:run) or a .py script"
                        ));
                    }
                }
                let kwargs = match get("kwargs") {
                    None | Some(Yaml::Null) => Vec::new(),
                    Some(Yaml::Hash(map)) => map
                        .iter()
                        .map(|(k, v)| {
                            let k = k.as_str().ok_or("kwargs: names are text")?;
                            let v = yaml_str(v)
                                .ok_or_else(|| format!("kwargs: {k}: expected a value"))?;
                            Ok((k.to_owned(), v))
                        })
                        .collect::<Result<_, String>>()?,
                    Some(_) => return Err("kwargs: expected a map of name: value".to_owned()),
                };
                if script && !kwargs.is_empty() {
                    return Err(
                        "python: kwargs are for module:function; a script takes args".to_owned(),
                    );
                }
                if !script && get("args").is_some() {
                    return Err(
                        "python: args are for a .py script; module:function takes kwargs"
                            .to_owned(),
                    );
                }
                let dataset = text("dataset")?.unwrap_or_else(|| "ds".to_owned());
                if dataset.is_empty()
                    || !dataset
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
                {
                    return Err(format!("dataset: {dataset}: letters, digits, _ and - only"));
                }
                Kind::Python {
                    target,
                    path: match get("path") {
                        Some(p) => yaml_strings(p, "path")?,
                        None => Vec::new(),
                    },
                    args: match get("args") {
                        Some(a) => yaml_strings(a, "args")?,
                        None => Vec::new(),
                    },
                    kwargs,
                    dataset,
                }
            }
        };
        if matches!(kind, Kind::Update { .. }) && get("graph").is_some() {
            return Err("update: an update names its graphs itself (no `graph`)".to_owned());
        }
        if matches!(&kind, Kind::Python { target, .. } if target.is_empty())
            || !matches!(kind, Kind::Python { .. } | Kind::Reason { .. })
                && kind.files().first().is_none_or(|f| f.is_empty())
        {
            return Err(format!("{}: no files", kind.name()));
        }
        let timeout = get("timeout")
            .map(|v| {
                let s = match v {
                    Yaml::Integer(i) => *i as f64,
                    Yaml::Real(r) => r.parse().unwrap_or(-1.0),
                    _ => -1.0,
                };
                Duration::try_from_secs_f64(s)
                    .ok()
                    .filter(|d| !d.is_zero())
                    .ok_or_else(|| "timeout: expected a number of seconds".to_owned())
            })
            .transpose()?;
        let graph = graph("graph")?;
        let variables = match get("variables") {
            None | Some(Yaml::Null) => Vec::new(),
            Some(Yaml::Hash(map)) => map
                .iter()
                .map(|(k, v)| {
                    let k = k.as_str().filter(|k| is_name(k)).ok_or_else(|| {
                        format!("variables: {k:?}: names are letters, digits and _")
                    })?;
                    let v =
                        yaml_str(v).ok_or_else(|| format!("variables: {k}: expected a value"))?;
                    Ok((k.to_owned(), v))
                })
                .collect::<Result<_, String>>()?,
            Some(_) => return Err("variables: expected a map of name: value".to_owned()),
        };
        let clear = flag("clear")?;
        if clear && graph.is_none() {
            return Err("clear: drops the step's `graph`, and it has none".to_owned());
        }
        Ok(Self {
            name: text("name")?.unwrap_or_else(|| format!("{number} {}", kind.name())),
            graph,
            clear,
            variables,
            timeout,
            kind,
        })
    }
}

/// How to run a pipeline.
#[derive(Default)]
pub struct Options<'a> {
    /// Every file the pipeline reads or writes must be inside this folder (a workspace).
    pub root: Option<&'a Path>,
    /// Parameter values, over the pipeline's defaults.
    pub values: HashMap<String, String>,
    /// The first step to run (an index), 0 by default.
    pub from: usize,
    /// The step after the last to run; all of them by default.
    pub to: Option<usize>,
    /// The folders lift steps may read (any file if empty).
    pub allow: Vec<PathBuf>,
    /// The longest any step may take, and how long a step without a `timeout` may take.
    pub timeout: Option<Duration>,
    /// The most quads the dataset may hold after a step: past it the step fails.
    pub max_quads: Option<usize>,
    /// The most bytes the files the steps write (exports, validation reports) may take.
    pub max_output: Option<u64>,
    /// Where lift steps spool their triples before loading them (the system's temporary
    /// folder if `None`).
    pub scratch: Option<PathBuf>,
    /// The Python interpreter python steps run with; `None` refuses python steps (a server
    /// does not run its callers' code).
    pub python: Option<String>,
    /// The folder of the store the pipeline runs on, which a publish step may not replace.
    pub store: Option<PathBuf>,
    /// The URLs lift steps may read (none by default).
    pub remote: tripletrail::fx::remote::RemoteLocations,
}

/// What a file of a step did.
#[derive(Debug, Clone)]
pub struct FileReport {
    /// Relative to the pipeline's folder when inside it.
    pub file: String,
    pub seconds: f64,
    pub triples: Option<usize>,
    pub error: Option<String>,
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Failed,
    /// A validation gate found results of its severity.
    Stopped,
    /// Not run, because an earlier step failed.
    Skipped,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Failed => "failed",
            Status::Stopped => "stopped",
            Status::Skipped => "skipped",
        }
    }
}

/// What a step did.
#[derive(Debug, Clone)]
pub struct StepReport {
    /// From 1.
    pub number: usize,
    pub name: String,
    pub kind: &'static str,
    pub status: Status,
    pub seconds: f64,
    /// The process's peak resident memory while the step ran (Linux), in bytes.
    pub peak_memory: Option<u64>,
    /// Quads in the dataset before and after.
    pub quads_before: usize,
    pub quads_after: usize,
    pub files: Vec<FileReport>,
    /// For infer: the triples inferred. For export: the triples or quads written.
    pub triples: Option<usize>,
    /// For validate: results by severity, and the severity it stops at.
    pub results: Option<BTreeMap<&'static str, usize>>,
    pub fail_on: Option<Severity>,
    pub error: Option<String>,
    /// For infer: each rule run, in order, to find the slow ones.
    pub rules: Vec<RuleReport>,
}

/// One run of a rule in an infer step.
#[derive(Debug, Clone)]
pub struct RuleReport {
    /// The rule node (`_:` for a blank one).
    pub rule: String,
    pub pass: usize,
    pub seconds: f64,
    pub focus_nodes: usize,
    /// Triples it constructed, and those that were new.
    pub constructed: usize,
    pub new: usize,
}

impl StepReport {
    pub fn to_json(&self) -> Value {
        let mut out = json!({
            "step": self.number,
            "name": self.name,
            "kind": self.kind,
            "status": self.status.label(),
            "seconds": round(self.seconds),
            "quadsBefore": self.quads_before,
            "quadsAfter": self.quads_after,
            "files": self.files.iter().map(|f| {
                let mut v = json!({"file": f.file, "seconds": round(f.seconds)});
                if let Some(t) = f.triples { v["triples"] = t.into(); }
                if let Some(e) = &f.error { v["error"] = e.clone().into(); }
                v
            }).collect::<Vec<_>>(),
        });
        if let Some(m) = self.peak_memory {
            out["peakMemory"] = m.into();
        }
        if let Some(t) = self.triples {
            out["triples"] = t.into();
        }
        if let Some(r) = &self.results {
            out["results"] = json!(r);
        }
        if let Some(f) = self.fail_on {
            out["failOn"] = f.label().into();
        }
        if let Some(e) = &self.error {
            out["error"] = e.clone().into();
        }
        if !self.rules.is_empty() {
            out["rules"] = self
                .rules
                .iter()
                .map(|r| {
                    json!({
                        "rule": r.rule,
                        "pass": r.pass,
                        "seconds": round(r.seconds),
                        "focusNodes": r.focus_nodes,
                        "constructed": r.constructed,
                        "new": r.new,
                    })
                })
                .collect::<Vec<_>>()
                .into();
        }
        out
    }
}

/// What a run of a pipeline did.
#[derive(Debug, Clone)]
pub struct Report {
    pub pipeline: String,
    /// Seconds since 1970 when it started.
    pub started: u64,
    pub seconds: f64,
    pub steps: Vec<StepReport>,
}

impl Report {
    /// Whether every step passed (a skipped step means an earlier one did not).
    pub fn ok(&self) -> bool {
        self.steps.iter().all(|s| s.status == Status::Ok)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "pipeline": self.pipeline,
            "started": self.started,
            "seconds": round(self.seconds),
            "ok": self.ok(),
            "steps": self.steps.iter().map(StepReport::to_json).collect::<Vec<_>>(),
        })
    }
}

fn round(seconds: f64) -> f64 {
    (seconds * 1000.0).round() / 1000.0
}

/// Runs the steps `options.from..options.to` of `pipeline` on `engine`, in order, calling
/// `progress` as each step starts (with `None`) and ends. It stops at the first step that fails
/// or whose gate stops it; the steps after it are reported as skipped.
pub fn run(
    pipeline: &Pipeline,
    engine: &Engine,
    options: &Options<'_>,
    progress: &dyn Fn(usize, Option<&StepReport>),
) -> Report {
    let started = Instant::now();
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut values = pipeline.parameters.clone();
    values.extend(options.values.clone());
    let to = options
        .to
        .unwrap_or(pipeline.steps.len())
        .min(pipeline.steps.len());
    let mut steps = Vec::new();
    let mut stopped = false;
    let output_left = std::cell::Cell::new(options.max_output);
    let union = engine.union_default_graph();
    if pipeline.union_default_graph {
        engine.set_union_default_graph(true);
    }
    let datasets: Vec<(String, Engine)> = pipeline
        .datasets
        .iter()
        .map(|n| (n.clone(), Engine::new_tripletrail()))
        .collect();
    for i in options.from..to {
        let step = &pipeline.steps[i];
        let quads = engine.store().len().unwrap_or(0);
        let mut report = StepReport {
            number: i + 1,
            name: step.name.clone(),
            kind: step.kind.name(),
            status: Status::Skipped,
            seconds: 0.0,
            peak_memory: None,
            quads_before: quads,
            quads_after: quads,
            files: Vec::new(),
            triples: None,
            results: None,
            fail_on: None,
            error: None,
            rules: Vec::new(),
        };
        if stopped {
            steps.push(report);
            continue;
        }
        progress(i, None);
        reset_peak_memory();
        let begun = Instant::now();
        let timeout = match (step.timeout, options.timeout) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let (deadline_engine, _deadline) = crate::with_deadline(engine, timeout);
        let mut result = step_values(&values, step).and_then(|values| {
            Run {
                pipeline,
                engine: &deadline_engine,
                base: engine,
                options,
                values: &values,
                output_left: &output_left,
                datasets: &datasets,
                report: &mut report,
            }
            .step(step)
        });
        report.seconds = begun.elapsed().as_secs_f64();
        report.peak_memory = peak_memory();
        report.quads_after = engine.store().len().unwrap_or(0);
        if let Some(max) = options.max_quads
            && report.quads_after > max
            && result.is_ok()
        {
            result = Err(format!(
                "the dataset holds {} quads after this step, more than the {max} it may hold here",
                report.quads_after
            ));
        }
        match result {
            Ok(()) => {
                if report.status == Status::Skipped {
                    report.status = Status::Ok;
                }
            }
            Err(e) => {
                report.status = Status::Failed;
                report.error = Some(match timeout {
                    Some(t) if e.contains("has been cancelled") => format!(
                        "the step took longer than its timeout of {} s",
                        t.as_secs_f64()
                    ),
                    _ => e,
                });
            }
        }
        if report.status != Status::Ok {
            stopped = true;
        }
        progress(i, Some(&report));
        steps.push(report);
    }
    engine.set_union_default_graph(union);
    Report {
        pipeline: pipeline.name.clone(),
        started: epoch,
        seconds: started.elapsed().as_secs_f64(),
        steps,
    }
}

/// The pipeline's parameter values with the step's variables over them.
fn step_values(
    values: &HashMap<String, String>,
    step: &Step,
) -> Result<HashMap<String, String>, String> {
    let mut out = values.clone();
    for (k, v) in &step.variables {
        out.insert(
            k.clone(),
            bind_values(v, values).map_err(|e| format!("variables: {k}: {e}"))?,
        );
    }
    Ok(out)
}

/// One step being run.
struct Run<'a> {
    pipeline: &'a Pipeline,
    /// The engine with the step's deadline.
    engine: &'a Engine,
    /// The same without it, to clean up after a step that ran out of time.
    base: &'a Engine,
    options: &'a Options<'a>,
    values: &'a HashMap<String, String>,
    /// The bytes the steps may still write.
    output_left: &'a std::cell::Cell<Option<u64>>,
    /// The pipeline's other datasets ([`Pipeline::datasets`]).
    datasets: &'a [(String, Engine)],
    report: &'a mut StepReport,
}

impl Run<'_> {
    fn path(&self, path: &str) -> Result<PathBuf, String> {
        inside(&self.pipeline.dir, path, self.options.root)
    }

    /// The files of the paths: each file, and the files of each folder with one of `extensions`
    /// in name order.
    fn files(&self, paths: &[String], extensions: &[&str]) -> Result<Vec<PathBuf>, String> {
        let mut out = Vec::new();
        for p in paths {
            out.extend(files_of(&self.path(p)?, extensions)?);
        }
        Ok(out)
    }

    /// `path` for the report: relative to the pipeline's folder when inside it.
    fn show(&self, path: &Path) -> String {
        let dir = self.pipeline.dir.canonicalize().unwrap_or_default();
        path.strip_prefix(&dir)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// A file a step writes: it may not exist yet, but its folder must be inside the root.
    fn output(&self, path: &str) -> Result<PathBuf, String> {
        let joined = self.pipeline.dir.join(path);
        let name = joined
            .file_name()
            .ok_or_else(|| format!("{path}: not a file name"))?
            .to_owned();
        let parent = joined.parent().unwrap_or(Path::new("."));
        if let Some(root) = self.options.root {
            // Lexically first, so no folder outside the root is made.
            let outside = || format!("{path} is outside the workspace");
            let normal = crate::testing::lexical(parent).ok_or_else(outside)?;
            if !normal.starts_with(crate::testing::lexical(root).ok_or_else(outside)?) {
                return Err(outside());
            }
        }
        std::fs::create_dir_all(parent).map_err(|e| format!("{path}: {e}"))?;
        let parent = inside(parent, ".", self.options.root)?;
        let target = parent.join(name);
        if target.is_symlink() {
            return Err(format!("{path} is a link"));
        }
        Ok(target)
    }

    fn step(mut self, step: &Step) -> Result<(), String> {
        let target = graph_name(step.graph.as_ref());
        if step.clear
            && let Some(g) = &step.graph
        {
            self.engine
                .update(&format!("DROP SILENT GRAPH {g}"))
                .map_err(|e| format!("clearing {g}: {e}"))?;
        }
        match &step.kind {
            Kind::Load { files } => {
                for file in self.files(files, RDF)? {
                    let begun = Instant::now();
                    let result = load(self.engine, &file, &target);
                    self.report.files.push(FileReport {
                        file: self.show(&file),
                        seconds: begun.elapsed().as_secs_f64(),
                        triples: None,
                        error: result.as_ref().err().cloned(),
                    });
                    result?;
                }
                Ok(())
            }
            Kind::Update { files } => {
                for file in self.files(files, &["ru"])? {
                    let begun = Instant::now();
                    let result = std::fs::read_to_string(&file)
                        .map_err(|e| e.to_string())
                        .and_then(|text| {
                            crate::lift::bind_parameters(&text, self.values)
                                .map_err(|e| e.to_string())
                        })
                        .and_then(|text| self.engine.update(&text).map_err(|e| e.to_string()));
                    let name = self.show(&file);
                    self.report.files.push(FileReport {
                        file: name.clone(),
                        seconds: begun.elapsed().as_secs_f64(),
                        triples: None,
                        error: result.as_ref().err().cloned(),
                    });
                    result.map_err(|e| format!("{name}: {e}"))?;
                }
                Ok(())
            }
            Kind::Lift {
                files,
                base,
                jobs,
                for_each,
                each_as,
                then,
            } => {
                let Some(pattern) = for_each else {
                    return self.lift(files, base.as_deref(), *jobs, then, self.values, &target);
                };
                let inputs = self.matching(pattern)?;
                if inputs.is_empty() {
                    return Err(format!("for_each: {pattern}: no files"));
                }
                for input in inputs {
                    let mut values = self.values.clone();
                    values.insert(each_as.clone(), input.display().to_string());
                    let name = input.file_name().unwrap_or_default().to_string_lossy();
                    values.insert(format!("{each_as}_name"), name.to_string());
                    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
                    values.insert(format!("{each_as}_stem"), stem.to_string());
                    let shown = self.show(&input);
                    self.lift(files, base.as_deref(), *jobs, then, &values, &target)
                        .map_err(|e| format!("{each_as} = {shown}: {e}"))?;
                }
                Ok(())
            }
            Kind::Publish {
                location,
                graphs,
                merge,
            } => self.publish(location, graphs, *merge),
            Kind::Infer {
                files,
                data,
                max_passes,
                all_constructed,
                ordered,
            } => {
                let data = graph_name(data.as_ref());
                let into = match &step.graph {
                    Some(g) => GraphName::NamedNode(g.clone()),
                    None => data.clone(),
                };
                let shapes = self.load_shapes(files)?;
                // The rules run on the store itself when what they infer goes back into its
                // default graph (the fast path, #260); otherwise on an in-memory copy of the
                // data and shapes, as fast, writing only `into` here.
                let in_store = into == data
                    && data.is_default_graph()
                    && matches!(
                        self.engine.store(),
                        tripletrail::store::Dataset::TripleTrail(_)
                    )
                    && !self.engine.union_default_graph();
                let options = tripletrail::InferOptions {
                    max_passes: if *ordered { 1 } else { *max_passes },
                    include_existing: *all_constructed,
                    semi_naive: true,
                    in_store,
                    scratch: !in_store,
                    sequential: *ordered,
                    plan: None,
                };
                let rules = std::cell::RefCell::new(Vec::new());
                let result = self.engine.infer_with(
                    &shapes,
                    graph_ref(&data),
                    Some(graph_ref(&into)),
                    &options,
                    &|r| {
                        if r.done {
                            rules.borrow_mut().push(RuleReport {
                                rule: r.rule.to_string(),
                                pass: r.pass,
                                seconds: r.elapsed.as_secs_f64(),
                                focus_nodes: r.focus_nodes,
                                constructed: r.constructed,
                                new: r.new,
                            });
                        }
                    },
                );
                self.report.rules = rules.into_inner();
                let dropped = self.drop_shapes();
                self.report.triples = Some(result.map_err(|e| e.to_string())?.len());
                dropped
            }
            Kind::Reason {
                regime,
                fail_on_inconsistency,
            } => {
                let into = step
                    .graph
                    .clone()
                    .unwrap_or_else(|| NamedNode::new_unchecked(INFERRED_GRAPH));
                let regime = match regime.as_str() {
                    "owl-rl" => tripletrail::store::entail::Regime::OwlRl,
                    _ => tripletrail::store::entail::Regime::Rdfs,
                };
                let done = self
                    .engine
                    .entail(into.as_ref(), regime)
                    .map_err(|e| e.to_string())?;
                self.report.triples = Some(done.triples);
                // Past the deadline while committing or checking consistency: the entailment is
                // written, which the message says (a stop before the commit leaves the graph as
                // it was, with the plain timeout message).
                let written = || {
                    format!(
                        "the step took longer than its timeout of {} s, after its entailment was \
                         written ({} triples in {})",
                        step.timeout.map_or(0.0, |t| t.as_secs_f64()),
                        done.triples,
                        into
                    )
                };
                if self.engine.is_cancelled() {
                    return Err(written());
                }
                if regime != tripletrail::store::entail::Regime::OwlRl {
                    return Ok(());
                }
                let found = self
                    .engine
                    .inconsistencies(1, Some(into.as_ref()))
                    .map_err(|e| e.to_string())?;
                let (count, first) = (found.count, found.first);
                if self.engine.is_cancelled() {
                    return Err(written());
                }
                // Past the cap the count is a lower bound, under its own name.
                let key = if found.at_least {
                    "inconsistencies_at_least"
                } else {
                    "inconsistencies"
                };
                self.report.results = Some(BTreeMap::from([(key, count)]));
                let at_least = if found.at_least { "at least " } else { "" };
                match first.first() {
                    Some(i) if *fail_on_inconsistency => Err(format!(
                        "the data contradicts the OWL 2 RL rules {at_least}{count} times, first {}: {}",
                        i.rule,
                        i.terms
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(" ")
                    )),
                    _ => Ok(()),
                }
            }
            Kind::Validate {
                files,
                data,
                fail_on,
                report,
            } => {
                let data = graph_name(data.as_ref());
                let shapes = self.load_shapes(files)?;
                let result = self.validate(&shapes, &data, report.as_deref());
                let dropped = self.drop_shapes();
                let counts = result?;
                dropped?;
                let stop = counts.iter().any(|(s, n)| *n > 0 && *s >= *fail_on);
                self.report.results = Some(
                    counts
                        .iter()
                        .filter(|(_, n)| **n > 0)
                        .map(|(s, n)| (s.label(), *n))
                        .collect(),
                );
                self.report.fail_on = Some(*fail_on);
                if stop {
                    self.report.status = Status::Stopped;
                    self.report.error = Some(format!(
                        "validation found results of severity {} or worse",
                        fail_on.label()
                    ));
                }
                Ok(())
            }
            Kind::Python {
                target,
                path,
                args,
                kwargs,
                dataset,
            } => self.python(step, target, path, args, kwargs, dataset),
            Kind::Export { file, query } => {
                let out = self.output(file)?;
                let begun = Instant::now();
                let written = self.export(&out, query.as_deref(), step.graph.as_ref())?;
                self.report.files.push(FileReport {
                    file: self.show(&out),
                    seconds: begun.elapsed().as_secs_f64(),
                    triples: Some(written),
                    error: None,
                });
                self.report.triples = Some(written);
                Ok(())
            }
        }
    }

    fn lift(
        &mut self,
        files: &[String],
        base: Option<&str>,
        jobs: Option<usize>,
        then: &[String],
        values: &HashMap<String, String>,
        target: &GraphName,
    ) -> Result<(), String> {
        let queries = files
            .iter()
            .map(|f| self.path(f))
            .collect::<Result<Vec<_>, _>>()?;
        let base = match base {
            Some(b) => self.path(b)?,
            None => self
                .pipeline
                .dir
                .canonicalize()
                .map_err(|e| e.to_string())?,
        };
        let mut allow = self.options.allow.clone();
        if allow.is_empty()
            && let Some(root) = self.options.root
        {
            allow.push(root.to_path_buf());
        }
        let temp = Temp::new(self.options.scratch.as_deref(), "nt");
        let report = crate::lift::lift(
            self.engine,
            crate::lift::LiftArgs {
                queries,
                base: Some(base),
                allow,
                output: Some(temp.0.clone()),
                output_dir: None,
                jobs: jobs
                    .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get())),
                values: values.clone(),
                explain: false,
                quiet: true,
                // A pipeline reruns its steps: its mappings' targets are its own to replace.
                overwrite_targets: true,
                remote: self.options.remote.clone(),
                #[cfg(feature = "postgres")]
                database: std::env::var("TRIPLETRAIL_DATABASE").ok(),
                #[cfg(feature = "postgres")]
                connections: HashMap::new(),
            },
        )
        .map_err(|e| e.to_string())?;
        let mut done = report.files;
        done.sort_by(|a, b| a.file.cmp(&b.file));
        let mut failed = Vec::new();
        for f in done {
            if let Some(e) = &f.error {
                failed.push(format!("{}: {e}", self.show(&f.file)));
            }
            self.report.files.push(FileReport {
                file: self.show(&f.file),
                seconds: f.seconds,
                triples: f.error.is_none().then_some(f.triples),
                error: f.error,
            });
        }
        if !failed.is_empty() {
            return Err(failed.join("; "));
        }
        if then.is_empty() {
            return load(self.engine, &temp.0, target);
        }
        // The updates see only what this lift gave, in the default graph of a store of its own.
        let lifted = Engine::new_tripletrail();
        load(&lifted, &temp.0, &GraphName::DefaultGraph)?;
        for file in self.files(then, &["ru"])? {
            let name = self.show(&file);
            let text = std::fs::read_to_string(&file).map_err(|e| format!("{name}: {e}"))?;
            let text =
                crate::lift::bind_parameters(&text, values).map_err(|e| format!("{name}: {e}"))?;
            if self.engine.is_cancelled() {
                return Err("the step has been cancelled".to_owned());
            }
            lifted.update(&text).map_err(|e| format!("{name}: {e}"))?;
        }
        let quads = lifted
            .store()
            .quads_for_pattern(None, None, None, Some(GraphNameRef::DefaultGraph))
            .map(|q| {
                q.map(|q| {
                    tripletrail::oxigraph::model::Quad::new(
                        q.subject,
                        q.predicate,
                        q.object,
                        target.clone(),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        self.engine.load_quads(quads).map_err(|e| e.to_string())
    }

    /// The files `pattern` names: a file, the files of a folder, or the files of a folder whose
    /// names match the last part of the pattern (`*` and `?`), in name order.
    fn matching(&self, pattern: &str) -> Result<Vec<PathBuf>, String> {
        let (dir, last) = match pattern.rsplit_once('/') {
            Some((d, l)) => (d, l),
            None => (".", pattern),
        };
        if !last.contains(['*', '?']) {
            return files_of(&self.path(pattern)?, &[]);
        }
        let folder = self.path(if dir.is_empty() { "/" } else { dir })?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&folder).map_err(|e| format!("{dir}: {e}"))? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if wildcard(last, &name) && entry.file_type().is_ok_and(|t| t.is_file()) {
                out.push(self.path(&format!("{dir}/{name}"))?);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Writes the graphs as a new dataset at `location`, replacing what is there once it is
    /// complete.
    fn publish(&mut self, location: &str, graphs: &[String], merge: bool) -> Result<(), String> {
        if self.options.root.is_some() {
            return Err(
                "publish steps run with `tripletrail pipeline run`, not on the server".to_owned(),
            );
        }
        let out = self.output(location)?;
        check_publish(&out, location, self.options.store.as_deref())?;
        let wanted = |g: &GraphName| -> bool {
            if g.as_str_opt() == Some(SHAPES) {
                return false;
            }
            graphs.is_empty()
                || graphs.iter().any(|w| match (w.as_str(), g) {
                    ("default", GraphName::DefaultGraph) => true,
                    (w, GraphName::NamedNode(n)) => match w.strip_suffix('*') {
                        Some(prefix) => n.as_str().starts_with(prefix),
                        None => n.as_str() == w,
                    },
                    _ => false,
                })
        };
        let copy = Engine::new_tripletrail();
        let mut count = 0;
        let mut batch = Vec::with_capacity(1 << 16);
        for q in self.base.store().quads_for_pattern(None, None, None, None) {
            let mut q = q.map_err(|e| e.to_string())?;
            if !wanted(&q.graph_name) {
                continue;
            }
            if merge {
                q.graph_name = GraphName::DefaultGraph;
            }
            batch.push(q);
            count += 1;
            if batch.len() == batch.capacity() {
                if self.engine.is_cancelled() {
                    return Err("the step has been cancelled".to_owned());
                }
                copy.load_quads(std::mem::take(&mut batch))
                    .map_err(|e| e.to_string())?;
                batch.reserve(1 << 16);
            }
        }
        copy.load_quads(batch).map_err(|e| e.to_string())?;
        // Written beside it, then swapped in: `location` never holds half a dataset.
        let suffix = format!("{}-{}", std::process::id(), self.report.number);
        let new = out.with_file_name(format!("{}.new-{suffix}", location_name(&out)));
        let old = out.with_file_name(format!("{}.old-{suffix}", location_name(&out)));
        let _ = std::fs::remove_dir_all(&new);
        copy.save(&new).map_err(|e| format!("{location}: {e}"))?;
        drop(copy);
        let had = out.exists();
        if had {
            std::fs::rename(&out, &old).map_err(|e| format!("{location}: {e}"))?;
        }
        if let Err(e) = std::fs::rename(&new, &out) {
            if had {
                let _ = std::fs::rename(&old, &out);
            }
            return Err(format!("{location}: {e}"));
        }
        if had {
            let _ = std::fs::remove_dir_all(&old);
        }
        self.report.files.push(FileReport {
            file: self.show(&out),
            seconds: 0.0,
            triples: Some(count),
            error: None,
        });
        self.report.triples = Some(count);
        Ok(())
    }

    /// Runs a python step: the dataset is served on a loopback port for the step's duration
    /// (the SPARQL Protocol at `{base}/{dataset}/sparql`, `/query` and `/update`, the Graph
    /// Store Protocol at `/data`, as Fuseki), and the interpreter calls `module:function` with
    /// `(base_url, dataset, **kwargs)` or runs the script. Its output goes to standard error.
    fn python(
        &mut self,
        step: &Step,
        target: &str,
        path: &[String],
        args: &[String],
        kwargs: &[(String, String)],
        dataset: &str,
    ) -> Result<(), String> {
        let Some(interpreter) = self.options.python.clone() else {
            return Err(
                "python steps run with `tripletrail pipeline run`, not on the server".to_owned(),
            );
        };
        let script = target.ends_with(".py");
        let script_path = script.then(|| self.path(target)).transpose()?;
        let mut python_path = Vec::new();
        for p in path {
            python_path.push(self.path(p)?);
        }
        if python_path.is_empty() {
            python_path.push(
                self.pipeline
                    .dir
                    .canonicalize()
                    .map_err(|e| e.to_string())?,
            );
        }
        let bind = |text: &str| bind_values(text, self.values);
        let args = args
            .iter()
            .map(|a| bind(a))
            .collect::<Result<Vec<_>, _>>()?;
        let kwargs = kwargs
            .iter()
            .map(|(k, v)| Ok((k.clone(), Value::String(bind(v)?))))
            .collect::<Result<serde_json::Map<String, Value>, String>>()?;
        let options = crate::Options {
            ui: false,
            max_body: None,
            shutdown_grace: Duration::from_secs(5),
            workspaces: crate::WorkspaceOptions {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        if self.datasets.iter().any(|(n, _)| n == dataset) {
            return Err(format!(
                "the pipeline's dataset is named {dataset}, as one of its `datasets`"
            ));
        }
        let served = std::iter::once((dataset.to_owned(), self.engine.clone()))
            .chain(self.datasets.iter().cloned())
            .collect::<Vec<_>>();
        let server = crate::Server::bind("127.0.0.1:0", served, options)
            .map_err(|e| format!("serving the dataset for the step: {e}"))?;
        let base_url = format!("http://{}", server.address());
        let handle = server.handle();
        let serving = std::thread::spawn(move || server.run_until_stopped());
        let stop = || {
            handle.stop();
        };
        let mut command = std::process::Command::new(&interpreter);
        match &script_path {
            Some(p) => {
                command.arg(p).args(&args);
            }
            None => {
                command.arg("-c").arg(PYTHON_CALL);
            }
        }
        let mut search = python_path.clone();
        if let Some(existing) = std::env::var_os("PYTHONPATH") {
            search.extend(std::env::split_paths(&existing));
        }
        let parameters: serde_json::Map<String, Value> = self
            .values
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        command
            .current_dir(&self.pipeline.dir)
            .env(
                "PYTHONPATH",
                std::env::join_paths(&search).map_err(|e| e.to_string())?,
            )
            .env("TRIPLETRAIL_BASE_URL", &base_url)
            .env("TRIPLETRAIL_DATASET", dataset)
            .env("TRIPLETRAIL_ENDPOINT", format!("{base_url}/{dataset}"))
            .env("TRIPLETRAIL_CALLABLE", target)
            .env("TRIPLETRAIL_KWARGS", Value::Object(kwargs).to_string())
            .env(
                "TRIPLETRAIL_PARAMETERS",
                Value::Object(parameters).to_string(),
            )
            .env(
                "TRIPLETRAIL_GRAPH",
                step.graph.as_ref().map(|g| g.as_str()).unwrap_or(""),
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let begun = Instant::now();
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                stop();
                let _ = serving.join();
                return Err(format!("running {interpreter}: {e}"));
            }
        };
        // Both streams go to standard error, and the end of them into the report.
        let tail = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let forward = |stream: Box<dyn std::io::Read + Send>| {
            let tail = tail.clone();
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    eprintln!("  | {line}");
                    let mut t = tail.lock().unwrap_or_else(|p| p.into_inner());
                    t.push_str(&line);
                    t.push('\n');
                    if t.len() > 4000 {
                        let cut = t.len() - 3000;
                        let cut = (cut..t.len()).find(|&i| t.is_char_boundary(i)).unwrap_or(0);
                        t.drain(..cut);
                    }
                }
            })
        };
        let readers = [
            child.stdout.take().map(|s| forward(Box::new(s))),
            child.stderr.take().map(|s| forward(Box::new(s))),
        ];
        let timeout = match (step.timeout, self.options.timeout) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(e) => break Err(e.to_string()),
            }
            if let Some(t) = timeout
                && begun.elapsed() > t
            {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "the step took longer than its timeout of {} s",
                    t.as_secs_f64()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        for r in readers.into_iter().flatten() {
            let _ = r.join();
        }
        stop();
        let _ = serving.join();
        let tail = tail.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let result = match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!(
                "{target} ended with {s}{}",
                if tail.trim().is_empty() {
                    String::new()
                } else {
                    format!(":\n{}", tail.trim_end())
                }
            )),
            Err(e) => Err(e),
        };
        self.report.files.push(FileReport {
            file: match &script_path {
                Some(p) => self.show(p),
                None => target.to_owned(),
            },
            seconds: begun.elapsed().as_secs_f64(),
            triples: None,
            error: result.as_ref().err().cloned(),
        });
        result
    }

    /// Loads the shapes files of a step into [`SHAPES`].
    fn load_shapes(&mut self, files: &[String]) -> Result<NamedNode, String> {
        let shapes = NamedNode::new_unchecked(SHAPES);
        self.drop_shapes()?;
        for file in self.files(files, RDF)? {
            let begun = Instant::now();
            let result = load(self.engine, &file, &GraphName::NamedNode(shapes.clone()));
            self.report.files.push(FileReport {
                file: self.show(&file),
                seconds: begun.elapsed().as_secs_f64(),
                triples: None,
                error: result.as_ref().err().cloned(),
            });
            if let Err(e) = result {
                let _ = self.drop_shapes();
                return Err(e);
            }
        }
        Ok(shapes)
    }

    /// Drops the step's shapes: with the engine without the deadline, as the step's may be
    /// cancelled already.
    fn drop_shapes(&self) -> Result<(), String> {
        self.base
            .update(&format!("DROP SILENT GRAPH <{SHAPES}>"))
            .map_err(|e| format!("dropping the step's shapes: {e}"))
    }

    /// Validates `data`, writing the report to `report` if given; the results by severity.
    fn validate(
        &self,
        shapes: &NamedNode,
        data: &GraphName,
        report: Option<&str>,
    ) -> Result<BTreeMap<Severity, usize>, String> {
        let mut writer = match report {
            Some(path) => {
                let out = self.output(path)?;
                let format = rdf_format(&out)?;
                let file = self.create(&out)?;
                let partial = file.partial.clone();
                Some((
                    RdfSerializer::from_format(format)
                        .with_prefix("sh", SH)
                        .map_err(|e| e.to_string())?
                        .for_writer(file),
                    partial,
                ))
            }
            None => None,
        };
        let mut counts = BTreeMap::new();
        let mut error = None;
        let severity = format!("{SH}resultSeverity");
        let result = self
            .engine
            .validate_report(shapes, graph_ref(data), false, &mut |t| {
                if t.predicate.as_str() == severity {
                    let s = match &t.object {
                        tripletrail::oxigraph::model::Term::NamedNode(n) => {
                            Severity::of(n.as_str())
                        }
                        _ => Severity::Violation,
                    };
                    *counts.entry(s).or_insert(0) += 1;
                }
                if let Some((w, _)) = &mut writer
                    && error.is_none()
                    && let Err(e) = w.serialize_triple(&t)
                {
                    error = Some(e.to_string());
                }
            })
            .map_err(|e| e.to_string());
        let partial = writer.as_ref().map(|(_, p)| p.clone());
        let result = match (result, error) {
            (Err(e), _) | (Ok(_), Some(e)) => Err(e),
            (Ok(_), None) => match writer {
                Some((w, _)) => w
                    .finish()
                    .map_err(|e| e.to_string())
                    .and_then(Limited::commit),
                None => Ok(()),
            },
        };
        if let Err(e) = result {
            // No half-written report, and the last good one stays.
            if let Some(partial) = partial {
                let _ = std::fs::remove_file(partial);
            }
            return Err(e);
        }
        Ok(counts)
    }

    /// Writes the export; how many triples or quads it wrote.
    fn export(
        &self,
        out: &Path,
        query: Option<&str>,
        graph: Option<&NamedNode>,
    ) -> Result<usize, String> {
        let format = rdf_format(out)?;
        let file = self.create(out)?;
        let partial = file.partial.clone();
        let result = self.write_export(
            RdfSerializer::from_format(format).for_writer(file),
            format.supports_datasets(),
            query,
            graph,
        );
        if result.is_err() {
            // No half-written export, and the last good one stays.
            let _ = std::fs::remove_file(partial);
        }
        result
    }

    /// Creates a file the step writes, which counts against [`Options::max_output`]: written
    /// beside it as `NAME.partial`, and renamed to its name by [`Limited::commit`].
    fn create(&self, out: &Path) -> Result<Limited<'_>, String> {
        let mut name = out.file_name().unwrap_or_default().to_owned();
        name.push(".partial");
        let partial = out.with_file_name(name);
        // A new file: a link left at its name is removed, not followed (it could point out of
        // the workspace).
        match std::fs::symlink_metadata(&partial) {
            Ok(m) if m.is_dir() => {
                return Err(format!("{} is a folder", partial.display()));
            }
            Ok(_) => {
                std::fs::remove_file(&partial).map_err(|e| format!("{}: {e}", partial.display()))?
            }
            Err(_) => {}
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .map_err(|e| format!("{}: {e}", out.display()))?;
        Ok(Limited {
            file: std::io::BufWriter::new(file),
            left: self.output_left,
            max: self.options.max_output,
            partial,
            target: out.to_path_buf(),
        })
    }

    fn write_export(
        &self,
        mut writer: tripletrail::oxigraph::io::WriterQuadSerializer<Limited<'_>>,
        datasets: bool,
        query: Option<&str>,
        graph: Option<&NamedNode>,
    ) -> Result<usize, String> {
        let mut count = 0;
        match query {
            // The query itself, or the file that holds it.
            Some(q) => {
                let (text, name) = if is_inline_query(q) {
                    (q.to_owned(), "query")
                } else {
                    let missing =
                        |e: String| format!("query file {q}: {e} (or write the query itself)");
                    let path = self.path(q).map_err(missing)?;
                    let text =
                        std::fs::read_to_string(&path).map_err(|e| missing(e.to_string()))?;
                    (text, q)
                };
                let text =
                    crate::lift::bind_parameters(&text, self.values).map_err(|e| e.to_string())?;
                let QueryResults::Graph(triples) = self
                    .engine
                    .query(&text)
                    .map_err(|e| format!("{name}: {e}"))?
                else {
                    return Err(format!("{name}: not a CONSTRUCT or DESCRIBE query"));
                };
                for t in triples {
                    let t = t.map_err(|e| format!("{name}: {e}"))?;
                    writer.serialize_triple(&t).map_err(|e| e.to_string())?;
                    count += 1;
                }
            }
            None => {
                let whole = graph.is_none() && datasets;
                let from = graph.map(|g| GraphNameRef::NamedNode(g.as_ref()));
                let pattern = if whole {
                    None
                } else {
                    Some(from.unwrap_or(GraphNameRef::DefaultGraph))
                };
                for q in self
                    .engine
                    .store()
                    .quads_for_pattern(None, None, None, pattern)
                {
                    let q = q.map_err(|e| e.to_string())?;
                    if q.graph_name.as_str_opt() == Some(SHAPES) {
                        continue;
                    }
                    if whole {
                        writer.serialize_quad(&q).map_err(|e| e.to_string())?;
                    } else {
                        writer
                            .serialize_triple(tripletrail::oxigraph::model::TripleRef::new(
                                q.subject.as_ref(),
                                q.predicate.as_ref(),
                                q.object.as_ref(),
                            ))
                            .map_err(|e| e.to_string())?;
                    }
                    count += 1;
                }
            }
        }
        writer
            .finish()
            .map_err(|e| e.to_string())
            .and_then(Limited::commit)?;
        Ok(count)
    }
}

/// Whether an export step's `query` is the query itself rather than the path of its file:
/// a query has a `{`, which a file name hardly ever has.
pub(crate) fn is_inline_query(query: &str) -> bool {
    query.contains('{')
}

/// A file a step writes, failing once the files of the run take more than
/// [`Options::max_output`].
struct Limited<'a> {
    file: std::io::BufWriter<std::fs::File>,
    left: &'a std::cell::Cell<Option<u64>>,
    max: Option<u64>,
    /// Where it is written, and the name it gets once done.
    partial: PathBuf,
    target: PathBuf,
}

impl Limited<'_> {
    /// Flushes the file and gives it its name.
    fn commit(mut self) -> Result<(), String> {
        std::io::Write::flush(&mut self.file).map_err(|e| e.to_string())?;
        std::fs::rename(&self.partial, &self.target)
            .map_err(|e| format!("{}: {e}", self.target.display()))
    }
}

impl std::io::Write for Limited<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(left) = self.left.get() {
            if buf.len() as u64 > left {
                return Err(std::io::Error::other(format!(
                    "the files the pipeline writes take more than the {} bytes there is room for",
                    self.max.unwrap_or_default()
                )));
            }
            self.left.set(Some(left - buf.len() as u64));
        }
        self.file.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

trait GraphNameStr {
    fn as_str_opt(&self) -> Option<&str>;
}

impl GraphNameStr for GraphName {
    fn as_str_opt(&self) -> Option<&str> {
        match self {
            GraphName::NamedNode(n) => Some(n.as_str()),
            _ => None,
        }
    }
}

/// The named graph, or the default graph.
fn graph_name(graph: Option<&NamedNode>) -> GraphName {
    graph
        .cloned()
        .map_or(GraphName::DefaultGraph, GraphName::NamedNode)
}

fn graph_ref(g: &GraphName) -> GraphNameRef<'_> {
    g.as_ref()
}

/// Loads an RDF file (gzipped if it ends in `.gz`) into `graph`.
fn load(engine: &Engine, path: &Path, graph: &GraphName) -> Result<(), String> {
    let format = rdf_format(path)?;
    let name = path.display();
    let file = std::fs::File::open(path).map_err(|e| format!("{name}: {e}"))?;
    let reader: Box<dyn std::io::Read> = if path.extension().is_some_and(|e| e == "gz") {
        Box::new(flate2::read::MultiGzDecoder::new(std::io::BufReader::new(
            file,
        )))
    } else {
        Box::new(std::io::BufReader::with_capacity(1 << 20, file))
    };
    let result = match graph {
        GraphName::DefaultGraph => engine.load(format, reader),
        g => engine.load_into(g.clone(), format, reader),
    };
    result.map_err(|e| format!("{name}: {e}"))
}

/// Refuses to publish to a folder that is not empty and not a dataset (its files would go), or
/// to the store the pipeline runs on (or a folder holding it, or one in it).
fn check_publish(out: &Path, location: &str, store: Option<&Path>) -> Result<(), String> {
    if let Some(store) = store {
        let store = store
            .canonicalize()
            .ok()
            .or_else(|| crate::testing::lexical(store));
        if let Some(store) = store
            && (out.starts_with(&store) || store.starts_with(out))
        {
            return Err(format!(
                "{location}: the pipeline runs on the store there, which a publish step may not replace"
            ));
        }
    }
    if !out.exists() {
        return Ok(());
    }
    if !out.is_dir() {
        return Err(format!("{location}: not a folder"));
    }
    let names: Vec<String> = std::fs::read_dir(out)
        .map_err(|e| format!("{location}: {e}"))?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{location}: {e}"))?;
    let dataset =
        names.iter().any(|n| n == "CURRENT") && names.iter().any(|n| n.starts_with("checkpoint-"));
    if names.is_empty() || dataset {
        Ok(())
    } else {
        Err(format!(
            "{location} holds files and is not a dataset: a publish step replaces only a dataset or an empty folder"
        ))
    }
}

fn location_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// A temporary file, removed when dropped.
struct Temp(PathBuf);

impl Temp {
    fn new(dir: Option<&Path>, extension: &str) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = dir.map_or_else(std::env::temp_dir, Path::to_path_buf);
        Self(dir.join(format!(
            "tripletrail-pipeline-{}-{}.{extension}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Resets the peak resident memory the kernel keeps for the process (Linux), so that the next
/// [`peak_memory`] is the peak since.
fn reset_peak_memory() {
    #[cfg(target_os = "linux")]
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// The process's peak resident memory in bytes (Linux).
fn peak_memory() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: u64 = line
        .trim_start_matches("VmHWM:")
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kb * 1024)
}
