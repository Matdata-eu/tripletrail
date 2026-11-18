//! Workbench tests (#403): regression tests for knowledge-graph code, kept as folders under a
//! workspace's `tests/` and run by `tripletrail test` or the Workbench's Tests tab.
//!
//! A test is a folder with an input, one target and the expected output. By convention:
//!
//! - **input**: `input.ttl`, `input.nt`, `input.trig` or `input.nq` (or none: an empty store);
//! - **target**: `query.rq`, `update.ru` or a folder `updates/` of `.ru` files run in name
//!   order, `rules.ttl` (SHACL-AF rules) or `shapes.ttl` (SHACL shapes; with rules too, the rules
//!   run first and validation sees what they infer);
//! - **expected**: `expected.ttl`/`.nt`/`.trig`/`.nq`, a graph compared up to blank-node
//!   isomorphism (what a CONSTRUCT gives, what an update leaves in the store, what rules infer),
//!   `expected.srj`/`.srx`/`.tsv`/`.csv`, a result table (unordered unless `ordered: true`), or for
//!   shapes `violations` in `test.yaml`: a count, or a list of `{focus, shape, component, path,
//!   severity}`, each field optional.
//!
//! `test.yaml` may set any of `input`, `query`, `update`, `rules`, `shapes` and `expected` to
//! other paths (a file or a folder, relative to the test, such as `../../rules` for the
//! workspace's own rules), and `ordered`, `output` (`inferred`, the default, or `all` for the data
//! plus what rules infer), `max_iterations`, `violations` and `prefixes` (for the IRIs in
//! `violations`; those declared in the input and target files count too).

use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tripletrail::Engine;
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::dataset::CanonicalizationAlgorithm;
use tripletrail::oxigraph::model::{
    Dataset, GraphName, GraphNameRef, NamedNode, Quad, Term, Triple,
};
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail::oxigraph::sparql::results::{
    QueryResultsFormat, QueryResultsParser, QueryResultsSerializer, SliceQueryResultsParserOutput,
};
use yaml_rust2::{Yaml, YamlLoader};

/// The named graph the target's shapes or rules are loaded into.
const SHAPES: &str = "urn:tripletrail:test:shapes";
/// The expected output files a test may have, by convention.
const EXPECTED: &[&str] = &[
    "expected.ttl",
    "expected.nt",
    "expected.trig",
    "expected.nq",
    "expected.srj",
    "expected.json",
    "expected.srx",
    "expected.tsv",
    "expected.csv",
];
/// Lines of a diff shown at most, each way.
const DIFF_LINES: usize = 50;

/// What a test checked and how it went.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The test folder's path relative to the folder the tests were found in.
    pub name: String,
    /// `query`, `update`, `rules` or `shapes`.
    pub kind: String,
    pub passed: bool,
    /// Why it could not run (a missing file, a syntax error, a timeout), when it did not.
    pub error: Option<String>,
    /// Expected but not there: triples, rows or violations.
    pub missing: Vec<String>,
    /// There but not expected.
    pub unexpected: Vec<String>,
    /// How many of each there were in all (the lists are cut at [`DIFF_LINES`]).
    pub missing_count: usize,
    pub unexpected_count: usize,
    pub elapsed: Duration,
}

impl Outcome {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "kind": self.kind,
            "passed": self.passed,
            "error": self.error,
            "missing": self.missing,
            "unexpected": self.unexpected,
            "missingCount": self.missing_count,
            "unexpectedCount": self.unexpected_count,
            "ms": self.elapsed.as_secs_f64() * 1000.0,
        })
    }

    /// The text `tripletrail test` prints for it: a line, and the diff of a failure.
    pub fn report(&self) -> String {
        let ms = self.elapsed.as_secs_f64() * 1000.0;
        let kind = if self.kind.is_empty() {
            String::new()
        } else {
            format!("{}, ", self.kind)
        };
        let mut out = format!(
            "{} {} ({kind}{ms:.0} ms)\n",
            if self.passed { "PASS" } else { "FAIL" },
            self.name
        );
        if let Some(e) = &self.error {
            let _ = writeln!(out, "     {e}");
        }
        for (sign, lines, total, what) in [
            ("-", &self.missing, self.missing_count, "missing"),
            ("+", &self.unexpected, self.unexpected_count, "unexpected"),
        ] {
            if total > 0 {
                let _ = writeln!(out, "     {total} {what}:");
            }
            for line in lines {
                let _ = writeln!(out, "     {sign} {line}");
            }
            if total > lines.len() {
                let _ = writeln!(out, "     … {} more", total - lines.len());
            }
        }
        out
    }
}

/// The test folders under `dir`: `dir` itself when it is one, else the folders of its `tests/`
/// (or of `dir`, when it is a `tests/` folder) that hold a test, in name order, nested too.
pub fn discover(dir: &Path) -> Vec<PathBuf> {
    if is_test(dir) {
        return vec![dir.to_owned()];
    }
    let base = if dir.join("tests").is_dir() {
        dir.join("tests")
    } else {
        dir.to_owned()
    };
    let mut out = Vec::new();
    walk(&base, &mut out, 0);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if is_test(&path) {
            out.push(path);
        } else if depth < 6 {
            walk(&path, out, depth + 1);
        }
    }
}

/// A folder is a test when it has a `test.yaml`, an expected output or a target.
fn is_test(dir: &Path) -> bool {
    const FILES: &[&str] = &[
        "test.yaml",
        "violations.yaml",
        "query.rq",
        "query.sparql",
        "update.ru",
        "rules.ttl",
        "rules.nt",
        "shapes.ttl",
        "shapes.nt",
    ];
    FILES.iter().any(|f| dir.join(f).is_file())
        || dir.join("updates").is_dir()
        || std::fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("expected."))
        })
}

/// Where a test may read files: anywhere (`tripletrail test`), or only inside a root (a
/// workspace, for the server).
#[derive(Clone, Copy)]
pub struct Limits<'a> {
    pub root: Option<&'a Path>,
    pub timeout: Option<Duration>,
    /// Write what the target gives as the expected output (`expected.nt`, `expected.srj`
    /// or `violations.yaml`), replacing the test's expected files, instead of comparing.
    pub record: bool,
    /// The largest expected output file to record.
    pub max_bytes: Option<u64>,
    /// The most bytes the files a test loads may hold, all together and unpacked.
    pub max_input: Option<u64>,
    /// The most rows, triples or validation results a target may give.
    pub max_rows: Option<usize>,
}

/// Runs the test in `dir`, naming it by its path relative to `base`.
pub fn run(dir: &Path, base: &Path, limits: Limits<'_>) -> Outcome {
    let started = Instant::now();
    let name = dir
        .strip_prefix(base)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(dir)
        .to_string_lossy()
        .replace('\\', "/");
    let mut outcome = Outcome {
        name,
        kind: String::new(),
        passed: false,
        error: None,
        missing: Vec::new(),
        unexpected: Vec::new(),
        missing_count: 0,
        unexpected_count: 0,
        elapsed: Duration::ZERO,
    };
    match Test::read(dir, limits).and_then(|t| {
        outcome.kind = t.kind().to_owned();
        t.check(limits)
    }) {
        Ok((missing, unexpected)) => {
            outcome.passed = missing.is_empty() && unexpected.is_empty();
            outcome.missing_count = missing.len();
            outcome.unexpected_count = unexpected.len();
            outcome.missing = missing.into_iter().take(DIFF_LINES).collect();
            outcome.unexpected = unexpected.into_iter().take(DIFF_LINES).collect();
        }
        // Cancelled by the deadline, wherever it was noticed.
        Err(e) if e.contains("has been cancelled") => {
            outcome.error = Some("the test took longer than its timeout".to_owned());
        }
        Err(e) => outcome.error = Some(e),
    }
    outcome.elapsed = started.elapsed();
    outcome
}

/// JUnit XML for CI.
pub fn junit(outcomes: &[Outcome]) -> String {
    let failures = outcomes.iter().filter(|o| !o.passed).count();
    let time: f64 = outcomes.iter().map(|o| o.elapsed.as_secs_f64()).sum();
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites tests=\"{}\" failures=\"{failures}\" time=\"{time:.3}\">\n<testsuite name=\"tripletrail\" tests=\"{}\" failures=\"{failures}\" time=\"{time:.3}\">\n",
        outcomes.len(),
        outcomes.len()
    );
    for o in outcomes {
        let _ = write!(
            out,
            "  <testcase classname=\"{}\" name=\"{}\" time=\"{:.3}\"",
            xml(&o.kind),
            xml(&o.name),
            o.elapsed.as_secs_f64()
        );
        if o.passed {
            out.push_str("/>\n");
        } else {
            let message = o.error.clone().unwrap_or_else(|| {
                format!(
                    "{} missing, {} unexpected",
                    o.missing_count, o.unexpected_count
                )
            });
            let _ = write!(
                out,
                ">\n    <failure message=\"{}\">{}</failure>\n  </testcase>\n",
                xml(&message),
                xml(&o.report())
            );
        }
    }
    out.push_str("</testsuite>\n</testsuites>\n");
    out
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// What is under test.
enum Target {
    Query(String),
    Updates(Vec<String>),
    Rules(Vec<PathBuf>),
    Shapes {
        shapes: Vec<PathBuf>,
        rules: Vec<PathBuf>,
    },
}

enum Expected {
    Graph(PathBuf),
    Table(PathBuf),
    Violations(Violations),
    /// Shapes without `violations`: the data must conform.
    Conforms,
    /// Nothing yet: only to record.
    Nothing,
}

enum Violations {
    Count(usize),
    List(Vec<HashMap<String, String>>),
}

struct Test {
    dir: PathBuf,
    record: bool,
    max_bytes: Option<u64>,
    max_rows: Option<usize>,
    max_input: u64,
    /// What the files loaded so far may still hold (see [`Limits::max_input`]).
    input_left: std::cell::Cell<Option<u64>>,
    input: Vec<PathBuf>,
    target: Target,
    expected: Expected,
    ordered: bool,
    all: bool,
    max_iterations: Option<usize>,
    prefixes: BTreeMap<String, String>,
}

/// The first of `names` in `dir`.
fn first(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names.iter().map(|n| dir.join(n)).find(|p| p.exists())
}

/// A path of `test.yaml`, relative to the test, which must stay inside `limits.root`.
fn resolve(dir: &Path, path: &str, limits: Limits<'_>) -> Result<PathBuf, String> {
    inside(dir, path, limits.root)
}

/// `path` relative to `dir`, which must stay inside `root` (when given), lexically and once
/// links are followed. The path must exist.
pub(crate) fn inside(dir: &Path, path: &str, root: Option<&Path>) -> Result<PathBuf, String> {
    let joined = dir.join(path);
    if let Some(root) = root {
        // Lexically first, so nothing outside the root is even looked at.
        let outside = || format!("{path} is outside the workspace");
        let normal = lexical(&joined).ok_or_else(outside)?;
        if !normal.starts_with(lexical(root).ok_or_else(outside)?) {
            return Err(outside());
        }
    }
    let real = joined.canonicalize().map_err(|e| format!("{path}: {e}"))?;
    if let Some(root) = root {
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        if !real.starts_with(&root) {
            return Err(format!("{path} is outside the workspace"));
        }
    }
    Ok(real)
}

/// `path` made absolute (against the working directory) with its `.` and `..` resolved
/// lexically, without looking at the file system; `None` when `..` goes above the top.
pub(crate) fn lexical(path: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(path).ok()?;
    let mut normal = PathBuf::new();
    for part in absolute.components() {
        match part {
            std::path::Component::ParentDir => {
                if !normal.pop() {
                    return None;
                }
            }
            std::path::Component::CurDir => {}
            other => normal.push(other),
        }
    }
    Some(normal)
}

/// The files a path names: itself, or the files of a folder in name order (not hidden ones).
pub(crate) fn files_of(path: &Path, extensions: &[&str]) -> Result<Vec<PathBuf>, String> {
    if path.is_file() {
        return Ok(vec![path.to_owned()]);
    }
    let mut out: Vec<PathBuf> = std::fs::read_dir(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                && (extensions.is_empty()
                    || p.extension()
                        .is_some_and(|e| extensions.contains(&e.to_string_lossy().as_ref())))
        })
        .collect();
    out.sort();
    Ok(out)
}

const RDF: &[&str] = &[
    "ttl", "nt", "trig", "nq", "rdf", "owl", "jsonld", "n3", "xml", "gz",
];

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

impl Test {
    fn read(dir: &Path, limits: Limits<'_>) -> Result<Self, String> {
        let spec = match dir.join("test.yaml") {
            p if p.is_file() => {
                let docs =
                    YamlLoader::load_from_str(&read(&p)?).map_err(|e| format!("test.yaml: {e}"))?;
                docs.into_iter().next().unwrap_or(Yaml::Null)
            }
            _ => Yaml::Null,
        };
        let text = |key: &str| spec[key].as_str().map(str::to_owned);
        let paths = |key: &str, extensions: &[&str]| -> Result<Option<Vec<PathBuf>>, String> {
            let given: Vec<String> = match &spec[key] {
                Yaml::String(s) => vec![s.clone()],
                Yaml::Array(a) => a
                    .iter()
                    .filter_map(|y| y.as_str().map(str::to_owned))
                    .collect(),
                _ => return Ok(None),
            };
            let mut out = Vec::new();
            for p in given {
                out.extend(files_of(&resolve(dir, &p, limits)?, extensions)?);
            }
            Ok(Some(out))
        };
        let input = match paths("input", RDF)? {
            Some(p) => p,
            None => ["input.ttl", "input.nt", "input.trig", "input.nq"]
                .iter()
                .map(|n| dir.join(n))
                .filter(|p| p.is_file())
                .collect(),
        };
        let rules = match paths("rules", RDF)? {
            Some(p) => p,
            None => first(dir, &["rules.ttl", "rules.nt", "rules"])
                .map(|p| files_of(&p, RDF))
                .transpose()?
                .unwrap_or_default(),
        };
        let shapes = match paths("shapes", RDF)? {
            Some(p) => p,
            None => first(dir, &["shapes.ttl", "shapes.nt", "shapes"])
                .map(|p| files_of(&p, RDF))
                .transpose()?
                .unwrap_or_default(),
        };
        let query = match paths("query", &["rq", "sparql"])? {
            Some(p) => p.into_iter().next(),
            None => first(dir, &["query.rq", "query.sparql"]),
        };
        let updates = match paths("update", &["ru", "sparql"])? {
            Some(p) => p,
            None => first(dir, &["update.ru", "updates"])
                .map(|p| files_of(&p, &["ru"]))
                .transpose()?
                .unwrap_or_default(),
        };
        let target = if let Some(q) = query {
            Target::Query(read(&q)?)
        } else if !updates.is_empty() {
            Target::Updates(updates.iter().map(|p| read(p)).collect::<Result<_, _>>()?)
        } else if !shapes.is_empty() {
            Target::Shapes { shapes, rules }
        } else if !rules.is_empty() {
            Target::Rules(rules)
        } else {
            return Err(
                "no target: add query.rq, update.ru, updates/, rules.ttl or shapes.ttl, or name \
                 one in test.yaml"
                    .to_owned(),
            );
        };
        let expected = match text("expected") {
            Some(p) => Some(resolve(dir, &p, limits)?),
            None => first(dir, EXPECTED),
        };
        let from_file;
        let violations = match &spec["violations"] {
            Yaml::Null | Yaml::BadValue if dir.join("violations.yaml").is_file() => {
                from_file = YamlLoader::load_from_str(&read(&dir.join("violations.yaml"))?)
                    .map_err(|e| format!("violations.yaml: {e}"))?
                    .into_iter()
                    .next()
                    .unwrap_or(Yaml::Null);
                &from_file
            }
            v => v,
        };
        if limits.record && (!spec["violations"].is_badvalue() || !spec["expected"].is_badvalue()) {
            return Err(
                "test.yaml sets the expected output: take `expected` or `violations` out of it \
                 to record"
                    .to_owned(),
            );
        }
        let violations = match violations {
            Yaml::Integer(n) => Some(Violations::Count(*n as usize)),
            Yaml::Array(list) => Some(Violations::List(
                list.iter()
                    .map(|entry| {
                        entry
                            .as_hash()
                            .map(|h| {
                                h.iter()
                                    .filter_map(|(k, v)| {
                                        let value = match v {
                                            Yaml::String(s) => s.clone(),
                                            Yaml::Integer(n) => n.to_string(),
                                            _ => return None,
                                        };
                                        Some((k.as_str()?.to_owned(), value))
                                    })
                                    .collect()
                            })
                            .ok_or_else(|| {
                                "violations: each entry is a map of focus, shape, component, \
                                 path, severity"
                                    .to_owned()
                            })
                    })
                    .collect::<Result<_, _>>()?,
            )),
            Yaml::Null | Yaml::BadValue => None,
            _ => return Err("violations: a count or a list".to_owned()),
        };
        let expected = match (&target, violations, expected) {
            _ if limits.record => Expected::Nothing,
            (Target::Shapes { .. }, Some(v), _) => Expected::Violations(v),
            (Target::Shapes { .. }, None, _) => Expected::Conforms,
            (_, Some(_), _) => return Err("violations are for a shapes test".to_owned()),
            (_, None, None) => {
                return Err(
                    "no expected output: add expected.ttl, expected.srj or expected.csv".to_owned(),
                );
            }
            (_, None, Some(p)) => {
                let ext = p
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                if ["srj", "json", "srx", "xml", "tsv", "csv"].contains(&ext.as_str()) {
                    Expected::Table(p)
                } else {
                    Expected::Graph(p)
                }
            }
        };
        let mut prefixes = BTreeMap::new();
        if let Some(h) = spec["prefixes"].as_hash() {
            for (k, v) in h {
                if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
                    prefixes.insert(k.to_owned(), v.to_owned());
                }
            }
        }
        Ok(Self {
            dir: dir.to_owned(),
            record: limits.record,
            max_bytes: limits.max_bytes,
            max_rows: limits.max_rows,
            max_input: limits.max_input.unwrap_or(u64::MAX),
            input_left: std::cell::Cell::new(limits.max_input),
            input,
            target,
            expected,
            ordered: spec["ordered"].as_bool().unwrap_or(false),
            all: text("output").as_deref() == Some("all"),
            max_iterations: spec["max_iterations"].as_i64().map(|n| n.max(1) as usize),
            prefixes,
        })
    }

    fn kind(&self) -> &'static str {
        match self.target {
            Target::Query(_) => "query",
            Target::Updates(_) => "update",
            Target::Rules(_) => "rules",
            Target::Shapes { .. } => "shapes",
        }
    }

    /// Runs the test: what was expected and is missing, and what is there unexpected.
    fn check(&self, limits: Limits<'_>) -> Result<(Vec<String>, Vec<String>), String> {
        let engine = Engine::new_tripletrail();
        for file in &self.input {
            self.load(&engine, file, None)?;
        }
        let (engine, _deadline) = crate::with_deadline(&engine, limits.timeout);
        let err = |e: tripletrail::Error| -> String {
            let text = e.to_string();
            if text.contains("cancelled") {
                "the test took longer than its timeout".to_owned()
            } else {
                text
            }
        };
        let shapes = NamedNode::new_unchecked(SHAPES);
        match &self.target {
            Target::Query(q) => match engine.query(q).map_err(err)? {
                QueryResults::Graph(triples) => {
                    let mut kept: Vec<Triple> = Vec::new();
                    for t in triples {
                        kept.push(t.map_err(|e| err(e.into()))?);
                        self.cap(kept.len())?;
                    }
                    let triples = kept;
                    self.compare_graph(
                        triples
                            .into_iter()
                            .map(|t| t.in_graph(GraphName::DefaultGraph)),
                    )
                }
                results => self.compare_table(results),
            },
            Target::Updates(updates) => {
                for u in updates {
                    engine.update(u).map_err(err)?;
                }
                let mut quads: Vec<Quad> = Vec::new();
                for q in engine.store().quads_for_pattern(None, None, None, None) {
                    quads.push(q.map_err(|e| e.to_string())?);
                    self.cap(quads.len())?;
                }
                self.compare_graph(quads.into_iter())
            }
            Target::Rules(rules) => {
                for file in rules {
                    self.load(&engine, file, Some(&shapes))?;
                }
                let options = tripletrail::InferOptions {
                    max_passes: self
                        .max_iterations
                        .unwrap_or(tripletrail::InferOptions::default().max_passes),
                    ..Default::default()
                };
                let inferred = engine
                    .infer_with(&shapes, GraphNameRef::DefaultGraph, None, &options, &|_| {})
                    .map_err(err)?;
                self.cap(inferred.len())?;
                let mut quads: Vec<Quad> = inferred
                    .iter()
                    .map(|t| t.clone().in_graph(GraphName::DefaultGraph))
                    .collect();
                if self.all {
                    for q in engine.store().quads_for_pattern(
                        None,
                        None,
                        None,
                        Some(GraphNameRef::DefaultGraph),
                    ) {
                        quads.push(q.map_err(|e| e.to_string())?);
                        self.cap(quads.len())?;
                    }
                }
                self.compare_graph(quads.into_iter())
            }
            Target::Shapes {
                shapes: files,
                rules,
            } => {
                // The rules first, into the data; then the shapes.
                if !rules.is_empty() {
                    let rules_graph = NamedNode::new_unchecked("urn:tripletrail:test:rules");
                    for file in rules {
                        self.load(&engine, file, Some(&rules_graph))?;
                    }
                    let inferred = engine
                        .infer_with(
                            &rules_graph,
                            GraphNameRef::DefaultGraph,
                            None,
                            &tripletrail::InferOptions::default(),
                            &|_| {},
                        )
                        .map_err(err)?;
                    self.cap(inferred.len())?;
                    engine
                        .load_quads(
                            inferred
                                .iter()
                                .map(|t| t.clone().in_graph(GraphName::DefaultGraph)),
                        )
                        .map_err(err)?;
                }
                for file in files {
                    self.load(&engine, file, Some(&shapes))?;
                }
                self.compare_violations(&engine, &shapes, err)
            }
        }
    }

    /// Fails once a target gives more than [`Limits::max_rows`].
    fn cap(&self, n: usize) -> Result<(), String> {
        match self.max_rows {
            Some(max) if n > max => Err(format!(
                "the target gives more than {max} rows, triples or results, the most a test may \
                 give here"
            )),
            _ => Ok(()),
        }
    }

    /// Loads an RDF file (gzipped too) into the default graph, or into `graph`, within what
    /// the test's files may still hold.
    fn load(&self, engine: &Engine, path: &Path, graph: Option<&NamedNode>) -> Result<(), String> {
        let format = rdf_format(path)?;
        let too_large = |max: u64| {
            format!(
                "{}: the test's files hold more than {max} bytes (unpacked), the most a test may \
                 load here",
                file_name(path)
            )
        };
        let left = self.input_left.get();
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut reader: Box<dyn std::io::Read> = if file_name(path).ends_with(".gz") {
            Box::new(flate2::read::MultiGzDecoder::new(std::io::BufReader::new(
                file,
            )))
        } else {
            Box::new(file)
        };
        let mut bytes = Vec::new();
        match left {
            Some(left) => {
                std::io::Read::read_to_end(
                    &mut std::io::Read::take(&mut reader, left + 1),
                    &mut bytes,
                )
                .map_err(|e| format!("{}: {e}", file_name(path)))?;
                if bytes.len() as u64 > left {
                    return Err(too_large(self.max_input()));
                }
                self.input_left.set(Some(left - bytes.len() as u64));
            }
            None => {
                reader
                    .read_to_end(&mut bytes)
                    .map_err(|e| format!("{}: {e}", file_name(path)))?;
            }
        }
        let result = match graph {
            Some(g) => engine.load_into(g.clone(), format, &bytes[..]),
            None => engine.load(format, &bytes[..]),
        };
        result.map_err(|e| format!("{}: {e}", file_name(path)))
    }

    /// The limit [`Test::load`] was given.
    fn max_input(&self) -> u64 {
        self.max_input
    }

    /// Replaces the test's expected output with `text` in the file `name`.
    fn write_expected(
        &self,
        name: &str,
        text: &[u8],
    ) -> Result<(Vec<String>, Vec<String>), String> {
        if let Some(max) = self.max_bytes
            && text.len() as u64 > max
        {
            return Err(format!(
                "the output is {} bytes, more than the {max} the workspace has room for",
                text.len()
            ));
        }
        for old in EXPECTED.iter().chain(&["violations.yaml"]) {
            let path = self.dir.join(old);
            if path.is_file() {
                std::fs::remove_file(&path).map_err(|e| format!("{old}: {e}"))?;
            }
        }
        std::fs::write(self.dir.join(name), text).map_err(|e| format!("{name}: {e}"))?;
        Ok((Vec::new(), Vec::new()))
    }

    fn compare_graph(
        &self,
        actual: impl Iterator<Item = Quad>,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        if self.record {
            let mut got: Dataset = actual.collect();
            got.canonicalize(CanonicalizationAlgorithm::Unstable);
            let mut lines: Vec<String> = got.iter().map(|q| format!("{q} .\n")).collect();
            lines.sort();
            let named = got.iter().any(|q| !q.graph_name.is_default_graph());
            let name = if named { "expected.nq" } else { "expected.nt" };
            return self.write_expected(name, lines.concat().as_bytes());
        }
        let Expected::Graph(path) = &self.expected else {
            return Err("this target gives triples: expect a graph (expected.ttl)".to_owned());
        };
        let mut want = Dataset::new();
        let format = rdf_format(path)?;
        for quad in RdfParser::from_format(format)
            .for_slice(&std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?)
        {
            want.insert(&quad.map_err(|e| format!("{}: {e}", file_name(path)))?);
        }
        let mut got: Dataset = actual.collect();
        // Isomorphism: the same canonical blank node labels on both sides when they are.
        want.canonicalize(CanonicalizationAlgorithm::Unstable);
        got.canonicalize(CanonicalizationAlgorithm::Unstable);
        let missing = want
            .iter()
            .filter(|q| !got.contains(*q))
            .map(|q| q.to_string())
            .collect();
        let unexpected = got
            .iter()
            .filter(|q| !want.contains(*q))
            .map(|q| q.to_string())
            .collect();
        Ok((sorted(missing), sorted(unexpected)))
    }

    fn compare_table(
        &self,
        results: QueryResults<'static>,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        if self.record {
            // Stops as soon as the output is too large to keep.
            let mut out = Bounded {
                bytes: Vec::new(),
                max: self.max_bytes,
            };
            let serializer = QueryResultsSerializer::from_format(QueryResultsFormat::Json);
            match results {
                QueryResults::Boolean(b) => {
                    serializer
                        .serialize_boolean_to_writer(&mut out, b)
                        .map_err(|e| e.to_string())?;
                }
                QueryResults::Solutions(solutions) => {
                    let variables = solutions.variables().to_vec();
                    let mut w = serializer
                        .serialize_solutions_to_writer(&mut out, variables)
                        .map_err(|e| e.to_string())?;
                    for (n, s) in solutions.enumerate() {
                        w.serialize(&s.map_err(|e| e.to_string())?)
                            .map_err(|e| e.to_string())?;
                        self.cap(n + 1)?;
                    }
                    w.finish().map_err(|e| e.to_string())?;
                }
                QueryResults::Graph(_) => unreachable!("graphs are compared as graphs"),
            }
            return self.write_expected("expected.srj", &out.bytes);
        }
        let Expected::Table(path) = &self.expected else {
            return Err("this query gives a table: expect expected.srj, .tsv or .csv".to_owned());
        };
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if ext == "csv" {
            // CSV has no term types: compare the cells as text.
            let mut out = Vec::new();
            match results {
                QueryResults::Solutions(solutions) => {
                    let variables = solutions.variables().to_vec();
                    let mut w = QueryResultsSerializer::from_format(QueryResultsFormat::Csv)
                        .serialize_solutions_to_writer(&mut out, variables)
                        .map_err(|e| e.to_string())?;
                    for (n, s) in solutions.enumerate() {
                        w.serialize(&s.map_err(|e| e.to_string())?)
                            .map_err(|e| e.to_string())?;
                        self.cap(n + 1)?;
                    }
                    w.finish().map_err(|e| e.to_string())?;
                }
                QueryResults::Boolean(_) => {
                    return Err("expected rows, but the query gives a boolean".to_owned());
                }
                QueryResults::Graph(_) => {
                    return Err(
                        "this query gives triples: expect a graph (expected.ttl)".to_owned()
                    );
                }
            }
            let rows = |text: &str| -> (String, Vec<String>) {
                let mut lines = text.lines().map(|l| l.trim_end_matches('\r').to_owned());
                let head = lines.next().unwrap_or_default();
                (head, lines.filter(|l| !l.is_empty()).collect())
            };
            let (want_head, want) = rows(&String::from_utf8_lossy(&bytes));
            let (got_head, got) = rows(&String::from_utf8_lossy(&out));
            if want_head != got_head {
                return Ok((
                    vec![format!("columns {want_head}")],
                    vec![format!("columns {got_head}")],
                ));
            }
            return Ok(self.diff_rows(want, got));
        }
        let format = match ext.as_str() {
            "srx" | "xml" => QueryResultsFormat::Xml,
            "tsv" => QueryResultsFormat::Tsv,
            _ => QueryResultsFormat::Json,
        };
        let parsed = QueryResultsParser::from_format(format)
            .for_slice(&bytes)
            .map_err(|e| format!("{}: {e}", file_name(path)))?;
        match (parsed, results) {
            (SliceQueryResultsParserOutput::Boolean(want), QueryResults::Boolean(got)) => {
                Ok(if want == got {
                    (Vec::new(), Vec::new())
                } else {
                    (vec![want.to_string()], vec![got.to_string()])
                })
            }
            (SliceQueryResultsParserOutput::Solutions(want), QueryResults::Solutions(got)) => {
                let vars: Vec<String> = want
                    .variables()
                    .iter()
                    .map(|v| v.as_str().to_owned())
                    .collect();
                let row = |get: &dyn Fn(&str) -> Option<Term>| -> String {
                    vars.iter()
                        .map(|v| match get(v) {
                            // Blank node labels are local to each side.
                            Some(Term::BlankNode(_)) => format!("?{v}=_:b"),
                            Some(t) => format!("?{v}={t}"),
                            None => format!("?{v}="),
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                let mut want_rows = Vec::new();
                for s in want {
                    let s = s.map_err(|e| format!("{}: {e}", file_name(path)))?;
                    want_rows.push(row(&|v| s.get(v).cloned()));
                }
                let mut got_rows = Vec::new();
                let got_vars: Vec<String> = got
                    .variables()
                    .iter()
                    .map(|v| v.as_str().to_owned())
                    .collect();
                for s in got {
                    let s = s.map_err(|e| e.to_string())?;
                    got_rows.push(row(&|v| s.get(v).cloned()));
                    self.cap(got_rows.len())?;
                }
                let mut a = vars.clone();
                let mut b = got_vars.clone();
                a.sort();
                b.sort();
                if a != b {
                    return Ok((
                        vec![format!("columns ?{}", vars.join(" ?"))],
                        vec![format!("columns ?{}", got_vars.join(" ?"))],
                    ));
                }
                Ok(self.diff_rows(want_rows, got_rows))
            }
            (SliceQueryResultsParserOutput::Boolean(_), _) => {
                Err("expected a boolean, but the query gives rows".to_owned())
            }
            (_, QueryResults::Boolean(_)) => {
                Err("expected rows, but the query gives a boolean".to_owned())
            }
            _ => Err("this query gives triples: expect a graph (expected.ttl)".to_owned()),
        }
    }

    /// Rows that differ: as multisets, or with `ordered`, position by position.
    fn diff_rows(&self, want: Vec<String>, got: Vec<String>) -> (Vec<String>, Vec<String>) {
        if self.ordered {
            let mut missing = Vec::new();
            let mut unexpected = Vec::new();
            for i in 0..want.len().max(got.len()) {
                match (want.get(i), got.get(i)) {
                    (Some(a), Some(b)) if a == b => {}
                    (a, b) => {
                        if let Some(a) = a {
                            missing.push(format!("row {}: {a}", i + 1));
                        }
                        if let Some(b) = b {
                            unexpected.push(format!("row {}: {b}", i + 1));
                        }
                    }
                }
            }
            return (missing, unexpected);
        }
        let mut counts: BTreeMap<&String, isize> = BTreeMap::new();
        for r in &want {
            *counts.entry(r).or_default() += 1;
        }
        for r in &got {
            *counts.entry(r).or_default() -= 1;
        }
        let mut missing = Vec::new();
        let mut unexpected = Vec::new();
        for (row, n) in counts {
            for _ in 0..n.unsigned_abs() {
                if n > 0 {
                    missing.push(row.clone());
                } else {
                    unexpected.push(row.clone());
                }
            }
        }
        (missing, unexpected)
    }

    fn compare_violations(
        &self,
        engine: &Engine,
        shapes: &NamedNode,
        err: impl Fn(tripletrail::Error) -> String,
    ) -> Result<(Vec<String>, Vec<String>), String> {
        // The node shape of each property shape, to match a blank one by its parent.
        let mut parents: HashMap<Term, Term> = HashMap::new();
        for q in engine
            .store()
            .quads_for_pattern(
                None,
                Some(NamedNode::new_unchecked("http://www.w3.org/ns/shacl#property").as_ref()),
                None,
                Some(shapes.as_ref().into()),
            )
            .flatten()
        {
            parents.insert(q.object, q.subject.into());
        }
        let mut got: Vec<BTreeMap<&'static str, String>> = Vec::new();
        engine
            .validate_results(shapes, GraphNameRef::DefaultGraph, &mut |r| {
                let mut v = BTreeMap::new();
                v.insert("focus", r.focus_node.to_string());
                let shape: Term = r.source_shape.clone();
                let shape = match (&shape, parents.get(&shape)) {
                    (Term::BlankNode(_), Some(parent)) => parent.to_string(),
                    _ => shape.to_string(),
                };
                v.insert("shape", shape);
                v.insert("component", r.source_constraint_component.to_string());
                if let Some(p) = &r.result_path {
                    v.insert("path", p.to_sparql());
                }
                v.insert("severity", r.severity.to_string());
                if self.max_rows.is_none_or(|max| got.len() <= max) {
                    got.push(v);
                }
            })
            .map_err(err)?;
        self.cap(got.len())?;
        if self.record {
            // Blank nodes have no name to match by; sh:Violation is the default.
            let mut text = String::from(
                "# The validation results the shapes give, recorded by `tripletrail test --record`.\n",
            );
            if got.is_empty() {
                text.push_str("[]\n");
            }
            for v in &got {
                let mut first = true;
                for (k, value) in v {
                    if value.starts_with("_:")
                        || (*k == "severity" && value.ends_with("#Violation>"))
                    {
                        continue;
                    }
                    let _ = writeln!(
                        text,
                        "{}{k}: {}",
                        if first { "- " } else { "  " },
                        Value::String(value.clone())
                    );
                    first = false;
                }
                if first {
                    text.push_str("- {}\n");
                }
            }
            return self.write_expected("violations.yaml", text.as_bytes());
        }
        let show = |v: &BTreeMap<&str, String>| {
            v.iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        match &self.expected {
            Expected::Conforms => Ok((Vec::new(), got.iter().map(show).collect())),
            Expected::Violations(Violations::Count(n)) => Ok(if got.len() == *n {
                (Vec::new(), Vec::new())
            } else if got.len() > *n {
                (
                    Vec::new(),
                    vec![format!("{} violations, {n} expected", got.len())],
                )
            } else {
                (
                    vec![format!("{n} violations, {} found", got.len())],
                    Vec::new(),
                )
            }),
            Expected::Violations(Violations::List(list)) => {
                let prefixes = self.prefixes_with_files();
                let mut taken = vec![false; got.len()];
                let mut missing = Vec::new();
                for entry in list {
                    let wanted: Vec<(&str, String)> = entry
                        .iter()
                        .map(|(k, v)| (k.as_str(), expand(v, &prefixes)))
                        .collect();
                    let hit = got.iter().enumerate().position(|(i, g)| {
                        !taken[i]
                            && wanted.iter().all(|(k, v)| match (*k, g.get(k)) {
                                ("component", Some(c)) => component_matches(c, v),
                                ("severity", Some(s)) => {
                                    s == v || s.trim_end_matches('>').ends_with(&format!("#{v}"))
                                }
                                ("path", Some(p)) => {
                                    p == v || p == &format!("<{}>", v.trim_matches(['<', '>']))
                                }
                                (_, Some(g)) => g == v,
                                (_, None) => false,
                            })
                    });
                    match hit {
                        Some(i) => taken[i] = true,
                        None => missing.push(
                            entry
                                .iter()
                                .map(|(k, v)| format!("{k}: {v}"))
                                .collect::<Vec<_>>()
                                .join(", "),
                        ),
                    }
                }
                let unexpected = got
                    .iter()
                    .zip(&taken)
                    .filter(|(_, t)| !**t)
                    .map(|(g, _)| show(g))
                    .collect();
                Ok((missing, unexpected))
            }
            _ => Err("a shapes test expects violations (in test.yaml) or none".to_owned()),
        }
    }

    /// The prefixes of `test.yaml` and those declared in the input, shapes and rules files.
    fn prefixes_with_files(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let files = self.input.iter().chain(match &self.target {
            Target::Shapes { shapes, rules } => {
                Box::new(shapes.iter().chain(rules)) as Box<dyn Iterator<Item = &PathBuf>>
            }
            Target::Rules(r) => Box::new(r.iter()),
            _ => Box::new(std::iter::empty()),
        });
        for f in files {
            let Ok(text) = std::fs::read_to_string(f) else {
                continue;
            };
            for line in text.lines() {
                let line = line.trim();
                let rest = line
                    .strip_prefix("@prefix")
                    .or_else(|| line.strip_prefix("PREFIX"))
                    .or_else(|| line.strip_prefix("prefix"));
                if let Some(rest) = rest
                    && let Some((name, iri)) = rest.trim().split_once(':')
                    && let Some(iri) = iri.trim().strip_prefix('<')
                    && let Some((iri, _)) = iri.split_once('>')
                {
                    out.insert(name.trim().to_owned(), iri.to_owned());
                }
            }
        }
        out.extend(self.prefixes.clone());
        out
    }
}

/// An IRI of `test.yaml` as N-Triples writes it: `<…>` kept, `prefix:name` and bare IRIs
/// expanded; anything else (a literal, a SPARQL path) as given.
fn expand(value: &str, prefixes: &BTreeMap<String, String>) -> String {
    let v = value.trim();
    if v.starts_with('<') || v.starts_with('"') || v.starts_with("_:") {
        return v.to_owned();
    }
    if v.contains("://") || v.starts_with("urn:") {
        return format!("<{v}>");
    }
    if let Some((p, local)) = v.split_once(':')
        && let Some(ns) = prefixes.get(p)
    {
        return format!("<{ns}{local}>");
    }
    v.to_owned()
}

/// `sh:MinCountConstraintComponent`, `MinCountConstraintComponent` or `MinCount` (given) against
/// the full IRI (found).
fn component_matches(found: &str, given: &str) -> bool {
    let local = found
        .trim_end_matches('>')
        .rsplit(['#', '/'])
        .next()
        .unwrap_or_default();
    let given = given.trim_matches(['<', '>']);
    found.trim_matches(['<', '>']) == given
        || local == given.rsplit(['#', '/', ':']).next().unwrap_or(given)
        || local
            == format!(
                "{}ConstraintComponent",
                given.rsplit(['#', '/', ':']).next().unwrap_or(given)
            )
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(crate) fn rdf_format(path: &Path) -> Result<RdfFormat, String> {
    let name = file_name(path);
    let name = name.strip_suffix(".gz").unwrap_or(&name);
    let ext = name.rsplit('.').next().unwrap_or_default();
    RdfFormat::from_extension(ext).ok_or_else(|| format!("{name}: unknown RDF format"))
}

/// A writer that fails once it holds more than `max` bytes.
struct Bounded {
    bytes: Vec<u8>,
    max: Option<u64>,
}

impl std::io::Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(max) = self.max
            && (self.bytes.len() + buf.len()) as u64 > max
        {
            return Err(std::io::Error::other(format!(
                "the output is more than the {max} bytes the workspace has room for"
            )));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
