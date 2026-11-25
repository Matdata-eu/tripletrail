//! Runs the W3C SPARQL and RDF test suites from a local checkout of
//! [w3c/rdf-tests](https://github.com/w3c/rdf-tests), the SHACL suites from one of
//! [w3c/data-shapes](https://github.com/w3c/data-shapes) and the GeoSPARQL Compliance
//! Benchmark from one of
//! [OpenLinkSoftware/GeoSPARQLBenchmark](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark),
//! against TripleTrail, and writes an EARL report and a Markdown summary. With `--known`, it fails on any change against a list of
//! known failures, so CI catches regressions (and tests that start to pass).

mod eval;
mod geosparql;
mod manifest;
mod protocol;
mod semantics;
mod shacl;

use clap::Parser;
use eval::Outcome;
use manifest::{BASE, Files, Kind, SHACL_BASE, SHACL_DIR, Test};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

/// The suites, by name, with their top-level manifest in the rdf-tests repository.
const SUITES: &[(&str, &str, &str)] = &[
    (
        "sparql10",
        "SPARQL 1.0 Query",
        "sparql/sparql10/manifest.ttl",
    ),
    (
        "sparql11-query",
        "SPARQL 1.1 Query",
        "sparql/sparql11/manifest-sparql11-query.ttl",
    ),
    (
        "sparql11-update",
        "SPARQL 1.1 Update",
        "sparql/sparql11/manifest-sparql11-update.ttl",
    ),
    (
        "sparql11-results",
        "SPARQL 1.1 Query Results CSV, TSV and JSON",
        "sparql/sparql11/manifest-sparql11-results.ttl",
    ),
    (
        "sparql11-fed",
        "SPARQL 1.1 Federated Query",
        "sparql/sparql11/manifest-sparql11-fed.ttl",
    ),
    (
        "sparql11-protocol",
        "SPARQL 1.1 Protocol",
        "sparql/sparql11/protocol/manifest.ttl",
    ),
    (
        "sparql11-gsp",
        "SPARQL 1.1 Graph Store Protocol",
        "sparql/sparql11/graph-store-protocol/manifest.ttl",
    ),
    (
        "sparql11-sd",
        "SPARQL 1.1 Service Description",
        "sparql/sparql11/service-description/manifest.ttl",
    ),
    (
        "sparql11-entailment",
        "SPARQL 1.1 Entailment Regimes",
        "sparql/sparql11/entailment/manifest.ttl",
    ),
    ("sparql12", "SPARQL 1.2", "sparql/sparql12/manifest.ttl"),
    (
        "rdf11-ntriples",
        "RDF 1.1 N-Triples",
        "rdf/rdf11/rdf-n-triples/manifest.ttl",
    ),
    (
        "rdf11-nquads",
        "RDF 1.1 N-Quads",
        "rdf/rdf11/rdf-n-quads/manifest.ttl",
    ),
    (
        "rdf11-turtle",
        "RDF 1.1 Turtle",
        "rdf/rdf11/rdf-turtle/manifest.ttl",
    ),
    (
        "rdf11-trig",
        "RDF 1.1 TriG",
        "rdf/rdf11/rdf-trig/manifest.ttl",
    ),
    (
        "rdf11-xml",
        "RDF 1.1 RDF/XML",
        "rdf/rdf11/rdf-xml/manifest.ttl",
    ),
    (
        "rdf11-mt",
        "RDF 1.1 Semantics",
        "rdf/rdf11/rdf-mt/manifest.ttl",
    ),
    (
        "rdf12-ntriples",
        "RDF 1.2 N-Triples",
        "rdf/rdf12/rdf-n-triples/manifest.ttl",
    ),
    (
        "rdf12-nquads",
        "RDF 1.2 N-Quads",
        "rdf/rdf12/rdf-n-quads/manifest.ttl",
    ),
    (
        "rdf12-turtle",
        "RDF 1.2 Turtle",
        "rdf/rdf12/rdf-turtle/manifest.ttl",
    ),
    (
        "rdf12-trig",
        "RDF 1.2 TriG",
        "rdf/rdf12/rdf-trig/manifest.ttl",
    ),
    (
        "rdf12-xml",
        "RDF 1.2 RDF/XML",
        "rdf/rdf12/rdf-xml/manifest.ttl",
    ),
];

/// The SHACL suites, with their manifest in the data-shapes test suite.
const SHACL_SUITES: &[(&str, &str, &str)] = &[
    ("shacl-core", "SHACL Core", "core/manifest.ttl"),
    ("shacl-sparql", "SHACL-SPARQL", "sparql/manifest.ttl"),
];

/// The GeoSPARQL benchmark's repository.
const GSB_REPO: &str = "https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark";

/// The GeoSPARQL compliance benchmark, read from a checkout of its repository.
const GEOSPARQL_SUITE: (&str, &str) = ("geosparql", "GeoSPARQL 1.0 Compliance Benchmark");

#[derive(Parser)]
#[command(about = "Runs the W3C SPARQL, RDF and SHACL test suites against TripleTrail")]
struct Args {
    /// A checkout of https://github.com/w3c/rdf-tests.
    #[arg(long)]
    tests: PathBuf,
    /// A checkout of https://github.com/w3c/data-shapes, for the SHACL suites. Without it
    /// they are not run.
    #[arg(long)]
    shacl_tests: Option<PathBuf>,
    /// A checkout of https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark, for the
    /// GeoSPARQL compliance benchmark. Without it, it is not run.
    #[arg(long)]
    geosparql_tests: Option<PathBuf>,
    /// Suites to run (default: all): sparql10, sparql11-query, sparql11-update,
    /// sparql11-results, sparql11-fed, sparql11-protocol, sparql11-gsp, sparql11-sd, sparql11-entailment, sparql12, rdf11-/rdf12- ntriples, nquads,
    /// turtle, trig and xml, shacl-core, shacl-sparql and geosparql.
    #[arg(long = "suite")]
    suites: Vec<String>,
    /// Only tests whose IRI contains this text.
    #[arg(long)]
    filter: Option<String>,
    /// Writes the EARL report (Turtle) here.
    #[arg(long)]
    earl: Option<PathBuf>,
    /// Writes the Markdown summary here.
    #[arg(long)]
    report: Option<PathBuf>,
    /// The known failures, one test IRI per line (`#` starts a comment). Any other failure,
    /// or a listed test that passes, makes the run fail.
    #[arg(long)]
    known: Option<PathBuf>,
    /// Writes the current failures in the known-failures format here.
    #[arg(long)]
    write_known: Option<PathBuf>,
    /// The `tripletrail` binary, to run the SPARQL Protocol and Graph Store Protocol suites
    /// against `tripletrail serve`. Without it those suites are not run.
    #[arg(long)]
    server: Option<PathBuf>,
    /// The executor the queries run on (`--executor` of the server, #11): `ids`, the
    /// default, or `spareval`. The known failures are the same for both.
    #[arg(long, default_value = "ids", value_parser = ["ids", "spareval"])]
    executor: String,
    /// Seconds a test may take.
    #[arg(long, default_value_t = 30)]
    timeout: u64,
    /// Prints every test's outcome.
    #[arg(long, short)]
    verbose: bool,
}

/// The GeoSPARQL benchmark's checkout: its commit and query weights.
struct Benchmark {
    commit: String,
    weights: BTreeMap<String, f64>,
}

impl Benchmark {
    /// The benchmark's compliance percentage for the runs.
    fn compliance(&self, runs: &[Run]) -> f64 {
        let passed: Vec<&str> = runs
            .iter()
            .filter(|r| geosparql::is_benchmark(&r.test) && r.outcome == Outcome::Passed)
            .map(|r| geosparql::file_name(&r.test))
            .collect();
        geosparql::compliance(&self.weights, &passed)
    }
}

struct Run {
    test: Test,
    suite_title: String,
    outcome: Outcome,
}

fn main() -> ExitCode {
    let args = Args::parse();
    tripletrail::Engine::set_default_executor_ids(args.executor == "ids");
    let _ = protocol::EXECUTOR.set(args.executor.clone());
    let rdf_tests = Arc::new(Files::new(&args.tests));
    let mut suites: Vec<(&str, &str, Arc<Files>, String)> = SUITES
        .iter()
        .map(|&(name, title, path)| (name, title, rdf_tests.clone(), format!("{BASE}{path}")))
        .collect();
    if let Some(dir) = &args.shacl_tests {
        let shapes_tests = Arc::new(Files::with_base(dir.join(SHACL_DIR), SHACL_BASE));
        suites.extend(SHACL_SUITES.iter().map(|&(name, title, path)| {
            (
                name,
                title,
                shapes_tests.clone(),
                format!("{SHACL_BASE}{path}"),
            )
        }));
    }
    if let Some(dir) = &args.geosparql_tests {
        suites.push((
            GEOSPARQL_SUITE.0,
            GEOSPARQL_SUITE.1,
            Arc::new(Files::with_base(
                dir.join(geosparql::GSB_DIR),
                geosparql::GSB_BASE,
            )),
            String::new(),
        ));
    }
    let mut runs = Vec::new();
    let mut server: Option<Result<protocol::Server, String>> = None;
    for (name, title, files, manifest) in suites {
        if !args.suites.is_empty() && !args.suites.iter().any(|s| s == name) {
            continue;
        }
        let tests = if name == GEOSPARQL_SUITE.0 {
            geosparql::tests(&files)
        } else {
            manifest::read(&files, &manifest)
        };
        let tests = match tests {
            Ok(tests) => tests,
            Err(e) => {
                eprintln!("{name}: {e}");
                return ExitCode::FAILURE;
            }
        };
        for test in tests {
            if args
                .filter
                .as_ref()
                .is_some_and(|f| !test.id.contains(f.as_str()))
            {
                continue;
            }
            let timeout = Duration::from_secs(args.timeout);
            let mut outcome = if matches!(
                test.kind,
                Kind::Protocol | Kind::GraphStore | Kind::ServiceDescription
            ) {
                http_test(&args, &mut server, &files, &test, timeout)
            } else if !test.service_data.is_empty() {
                federated_test(&args, &files, &test, timeout)
            } else {
                eval::run(&files, &test, timeout)
            };
            if geosparql::is_benchmark(&test)
                && let Outcome::Failed(reason) = &mut outcome
            {
                *reason = geosparql::without_values(reason);
            }
            if args.verbose {
                println!("{} {}", label(&outcome), test.id);
                if let Outcome::Failed(reason) = &outcome {
                    println!("    {reason}");
                }
            }
            runs.push(Run {
                test,
                suite_title: title.to_owned(),
                outcome,
            });
        }
    }
    let commit = commit_of(&args.tests);
    let shacl_commit = args.shacl_tests.as_deref().map(commit_of);
    let benchmark = match args.geosparql_tests.as_deref() {
        Some(dir) => match geosparql::weights(dir) {
            Ok(weights) => Some(Benchmark {
                commit: commit_of(dir),
                weights,
            }),
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };
    print_summary(&runs);
    if let Some(benchmark) = &benchmark {
        println!(
            "GeoSPARQL compliance (the benchmark's weighting): {:.2} %",
            benchmark.compliance(&runs)
        );
    }
    if let Some(path) = &args.earl
        && let Err(e) = std::fs::write(path, earl(&runs))
    {
        eprintln!("{}: {e}", path.display());
        return ExitCode::FAILURE;
    }
    if let Some(path) = &args.report
        && let Err(e) = std::fs::write(
            path,
            markdown(&runs, &commit, shacl_commit.as_deref(), benchmark.as_ref()),
        )
    {
        eprintln!("{}: {e}", path.display());
        return ExitCode::FAILURE;
    }
    if let Some(path) = &args.write_known
        && let Err(e) = std::fs::write(path, known_failures(&runs))
    {
        eprintln!("{}: {e}", path.display());
        return ExitCode::FAILURE;
    }
    if let Some(path) = &args.known {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        };
        if !check_known(&runs, &text) {
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

/// A protocol test, against a server started for the first one.
fn http_test(
    args: &Args,
    server: &mut Option<Result<protocol::Server, String>>,
    files: &Files,
    test: &Test,
    timeout: Duration,
) -> Outcome {
    if let Some(approval) = &test.not_approved {
        return Outcome::Untested(format!("not approved ({approval})"));
    }
    if let Some(reason) = protocol::untested(test) {
        return Outcome::Untested(reason);
    }
    let Some(binary) = &args.server else {
        return Outcome::Untested("needs a server (--server)".into());
    };
    let server = server.get_or_insert_with(|| protocol::Server::start(binary, timeout));
    match server {
        Ok(server) => match if test.kind == Kind::ServiceDescription {
            protocol::service_description(server, test)
        } else {
            protocol::run(server, files, test)
        } {
            Ok(()) => Outcome::Passed,
            Err(e) => Outcome::Failed(e),
        },
        Err(e) => Outcome::Failed(format!("starting the server: {e}")),
    }
}

/// A federated query test: a server of its own stands in for the test's endpoints (a
/// dataset for each, holding its `qt:data`), and the endpoint IRIs in the test's files are
/// replaced by the server's URLs.
fn federated_test(
    args: &Args,
    files: &std::sync::Arc<Files>,
    test: &Test,
    timeout: Duration,
) -> Outcome {
    if let Some(approval) = &test.not_approved {
        return Outcome::Untested(format!("not approved ({approval})"));
    }
    let Some(binary) = &args.server else {
        return Outcome::Untested("needs a server for its SERVICE endpoints (--server)".into());
    };
    let mut endpoints: Vec<&str> = test.service_data.iter().map(|(e, _)| e.as_str()).collect();
    endpoints.sort_unstable();
    endpoints.dedup();
    let names: Vec<String> = (0..endpoints.len()).map(|i| format!("ep{i}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let server = match protocol::Server::start_with(binary, timeout, &names, true) {
        Ok(server) => server,
        Err(e) => return Outcome::Failed(format!("starting the server: {e}")),
    };
    let base = format!("http://127.0.0.1:{}/", server.port());
    let mut rewrites: Vec<(String, String)> = endpoints
        .iter()
        .zip(&names)
        .map(|(endpoint, name)| ((*endpoint).to_owned(), format!("{base}{name}/sparql")))
        .collect();
    // The longest first: one endpoint IRI may start another.
    rewrites.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    let files = std::sync::Arc::new(files.rewriting(rewrites));
    for (endpoint, data) in &test.service_data {
        let name = names[endpoints
            .binary_search(&endpoint.as_str())
            .unwrap_or_default()];
        let loaded = files
            .read_to_string(data)
            .and_then(|text| server.put_default(name, data, &text));
        if let Err(e) = loaded {
            return Outcome::Failed(format!("loading endpoint {endpoint}: {e}"));
        }
    }
    eval::run_with(&files, test, timeout, Some(base))
}

fn label(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Passed => "pass",
        Outcome::Failed(_) => "FAIL",
        Outcome::Untested(_) => "skip",
    }
}

fn commit_of(dir: &std::path::Path) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into())
}

#[derive(Default)]
struct Counts {
    passed: usize,
    failed: usize,
    untested: usize,
}

impl Counts {
    fn add(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::Passed => self.passed += 1,
            Outcome::Failed(_) => self.failed += 1,
            Outcome::Untested(_) => self.untested += 1,
        }
    }

    fn rate(&self) -> String {
        let run = self.passed + self.failed;
        if run == 0 {
            return "–".into();
        }
        format!("{:.1} %", 100.0 * self.passed as f64 / run as f64)
    }
}

/// A manifest's label and its tests' runs.
type Group<'a> = (String, Vec<&'a Run>);

/// Per suite in the order run, then per manifest in the order read.
fn by_suite(runs: &[Run]) -> Vec<(String, Vec<Group<'_>>)> {
    let mut suites: Vec<(String, Vec<Group<'_>>)> = Vec::new();
    for run in runs {
        if suites.last().is_none_or(|(s, _)| *s != run.suite_title) {
            suites.push((run.suite_title.clone(), Vec::new()));
        }
        let groups = &mut suites.last_mut().expect("pushed above").1;
        if groups.last().is_none_or(|(g, _)| *g != run.test.group) {
            groups.push((run.test.group.clone(), Vec::new()));
        }
        groups.last_mut().expect("pushed above").1.push(run);
    }
    suites
}

fn print_summary(runs: &[Run]) {
    for (suite, groups) in by_suite(runs) {
        let mut counts = Counts::default();
        for run in groups.iter().flat_map(|(_, r)| r) {
            counts.add(&run.outcome);
        }
        println!(
            "{suite}: {} passed, {} failed, {} not run",
            counts.passed, counts.failed, counts.untested
        );
    }
}

fn one_line(text: &str, max: usize) -> String {
    let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let cut: String = line.chars().take(max).collect();
    format!("{cut}…")
}

fn markdown(
    runs: &[Run],
    commit: &str,
    shacl_commit: Option<&str>,
    benchmark: Option<&Benchmark>,
) -> String {
    let mut out = String::new();
    let suites = by_suite(runs);
    let _ = writeln!(out, "# Conformance\n");
    let _ = writeln!(
        out,
        "TripleTrail's results on the W3C SPARQL, RDF and SHACL test suites and the GeoSPARQL \
         Compliance Benchmark, run against TripleTrail's own store, planner, operators, geometry \
         functions and SHACL validator (not upstream Oxigraph). The SPARQL and RDF tests come \
         from [w3c/rdf-tests](https://github.com/w3c/rdf-tests) at commit \
         [`{short}`](https://github.com/w3c/rdf-tests/tree/{commit}){shacl}{geo}. CI runs them \
         on every pull request and fails on any change against \
         [`known-failures.txt`](../crates/tripletrail-testsuite/known-failures.txt).\n",
        short = &commit[..commit.len().min(12)],
        shacl = shacl_commit
            .map(|c| format!(
                ", the SHACL tests from [w3c/data-shapes](https://github.com/w3c/data-shapes) at \
                 commit [`{}`](https://github.com/w3c/data-shapes/tree/{c}/{SHACL_DIR})",
                &c[..c.len().min(12)]
            ))
            .unwrap_or_default(),
        geo = benchmark
            .map(|b| format!(
                ", and the GeoSPARQL benchmark from \
                 [OpenLinkSoftware/GeoSPARQLBenchmark]({GSB_REPO}) at commit \
                 [`{}`]({GSB_REPO}/tree/{})",
                &b.commit[..b.commit.len().min(12)],
                b.commit
            ))
            .unwrap_or_default()
    );
    let _ = writeln!(
        out,
        "To run them yourself, and to update this page and the known failures after a fix:\n\n\
         ```sh\ngit clone https://github.com/w3c/rdf-tests\n\
         git -C rdf-tests checkout {commit}\n\
         git clone https://github.com/w3c/data-shapes\n\
         git -C data-shapes checkout {shapes}\n\
         git clone {GSB_REPO}\n\
         git -C GeoSPARQLBenchmark checkout {geo}\n\
         cargo run -p tripletrail-testsuite -- --tests rdf-tests --shacl-tests data-shapes \\\n  \
         --geosparql-tests GeoSPARQLBenchmark \\\n  \
         --report docs/conformance.md \\\n  \
         --write-known crates/tripletrail-testsuite/known-failures.txt --earl tripletrail-earl.ttl \\\n  \
         --server target/debug/tripletrail   # after cargo build -p tripletrail-server\n```\n\n\
         CI runs the commits in `RDF_TESTS_COMMIT`, `DATA_SHAPES_COMMIT` and \
         `GEOSPARQL_BENCHMARK_COMMIT` of `.github/workflows/ci.yml`; bump them with the page \
         and the known failures. It runs them twice, under each executor (`--executor \
         ids`, the default, and `--executor spareval`, #11), and fails unless both give this \
         page.\n",
        shapes = shacl_commit.unwrap_or("gh-pages"),
        geo = benchmark.map_or("master", |b| b.commit.as_str())
    );
    let _ = writeln!(
        out,
        "Each test runs on a new in-memory store. Results compare up to blank node renaming, \
         and numeric literals compare by value (`\"1.0\"` and `\"1\"` are the same \
         `xsd:decimal`), as in Jena's and Oxigraph's runners: the expected results come from \
         several implementations that write numbers differently. Rows compare in order only \
         when the query has `ORDER BY`. \"Not run\" counts tests the runner can't run yet \
         (entailment regimes TripleTrail lacks, graphs named by server path, which Fuseki doesn't \
         serve either) and tests the working group didn't approve; a pass rate counts only the tests that ran. The RDF suites load \
         each file with `Engine::load`, the loader the server and the CLI use, and read the \
         statements back from the store: they test the parsers and what the store keeps \
         (literals exactly as written, language directions, triple terms).\n"
    );
    let _ = writeln!(
        out,
        "The Protocol and Graph Store Protocol suites replay their HTTP requests against \
         `tripletrail serve` (with `--server target/debug/tripletrail`), on one in-memory dataset \
         emptied before each test; `/sparql/` maps to the dataset's `/ds/sparql` and `/gsp` \
         to `/ds/data`. The Service Description suite reads the description at `/ds/sparql`. \
         In the Federated Query suite, a `tripletrail serve` of its own stands in for each test's \
         endpoints, a dataset per endpoint holding its `qt:data`; the endpoint IRIs in the \
         test's files become that server's URLs, and the query runs in the store with SERVICE \
         calls allowed to it.\n"
    );
    let _ = writeln!(
        out,
        "The SHACL suites load each test's data and shapes graphs into the store and validate \
         with `Engine::validate`, the validator of `tripletrail validate` and `tripletrail infer`. \
         The report must be the expected one up to blank node renaming, with the blank nodes \
         it reaches (paths, blank node shapes) and leaving out `sh:resultMessage`, as the \
         suite asks; a test that expects `sht:Failure` passes when validation fails. Test IRIs \
         are those of the W3C SHACL implementation report (`urn:x-shacl-test:/…`).\n"
    );
    let _ = writeln!(
        out,
        "The [GeoSPARQL Compliance Benchmark]({GSB_REPO}) (Jovanovik, Homburg and Spasić, \
         [2021](https://www.mdpi.com/2220-9964/10/7/487)) has 206 queries on one dataset that \
         cover the requirements of GeoSPARQL 1.0, and is what GeoSPARQL support of RDF stores \
         is usually measured with; OGC's executable test suite for GeoSPARQL 1.1 tests only \
         the core so far. The dataset loads into the default graph, and a query passes when \
         its answer is the expected one or one of the benchmark's alternatives. As in the \
         benchmark, WKT literals compare without white space and case; GML, KML and GeoJSON \
         literals compare without the white space between elements. The benchmark's own score, \
         the compliance percentage, weighs each of the 30 requirements equally and shares a \
         requirement's weight among its queries.\n"
    );
    let _ = writeln!(
        out,
        "The Entailment Regimes suite answers a test under the regime it lists that TripleTrail \
         has the most rules of: OWL 2 RDF-Based semantics as the OWL 2 RL rules \
         (`tripletrail entail --regime owl-rl`), else RDFS as Jena's default RDFS rules \
         (`--regime rdfs`), which leave out the axiomatic and reflexive triples. The \
         default graph's entailment is materialized and added to it before the query runs. \
         Tests under only simple, D, RDF, OWL 2 Direct or RIF entailment are not run.\n"
    );
    let _ = writeln!(
        out,
        "The RDF 1.1 semantics tests load the action and, under RDFS, close it with the \
         same RDFS rules. The runner then checks the entailment: the result graph, its blank \
         nodes as variables, maps into the closure, with literals of a recognized datatype \
         compared by value; a graph is inconsistent when it holds an ill-typed literal of a \
         recognized datatype or, under RDFS, a literal outside a recognized datatype in the \
         range of its property. Simple and RDF entailment add no rules.\n"
    );
    let _ = writeln!(out, "| Suite | Passed | Failed | Not run | Pass rate |");
    let _ = writeln!(out, "| --- | ---: | ---: | ---: | ---: |");
    let mut total = Counts::default();
    for (suite, groups) in &suites {
        let mut counts = Counts::default();
        for run in groups.iter().flat_map(|(_, r)| r) {
            counts.add(&run.outcome);
            total.add(&run.outcome);
        }
        let _ = writeln!(
            out,
            "| {suite} | {} | {} | {} | {} |",
            counts.passed,
            counts.failed,
            counts.untested,
            counts.rate()
        );
    }
    let _ = writeln!(
        out,
        "| **All** | **{}** | **{}** | **{}** | **{}** |\n",
        total.passed,
        total.failed,
        total.untested,
        total.rate()
    );
    for (suite, groups) in &suites {
        let _ = writeln!(out, "## {suite}\n");
        if let Some(benchmark) = benchmark
            && suite == GEOSPARQL_SUITE.1
        {
            let _ = writeln!(
                out,
                "GeoSPARQL compliance percentage (the benchmark's weighting): **{:.2} %**\n",
                benchmark.compliance(runs)
            );
        }
        let _ = writeln!(out, "| Tests | Passed | Failed | Not run |");
        let _ = writeln!(out, "| --- | ---: | ---: | ---: |");
        for (group, runs) in groups {
            let mut counts = Counts::default();
            for run in runs {
                counts.add(&run.outcome);
            }
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                group.replace('|', "\\|"),
                counts.passed,
                counts.failed,
                counts.untested
            );
        }
        let failures: Vec<&&Run> = groups
            .iter()
            .flat_map(|(_, r)| r)
            .filter(|r| matches!(r.outcome, Outcome::Failed(_)))
            .collect();
        if !failures.is_empty() {
            let _ = writeln!(out, "\n### Failures\n");
            for run in failures {
                let Outcome::Failed(reason) = &run.outcome else {
                    continue;
                };
                let _ = writeln!(
                    out,
                    "- [{}]({}) ({}): {}",
                    short_name(&run.test),
                    browse(&run.test),
                    run.test.group,
                    one_line(reason, 300).replace('|', "\\|")
                );
            }
        }
        let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
        for run in groups.iter().flat_map(|(_, r)| r) {
            if let Outcome::Untested(reason) = &run.outcome {
                *skipped.entry(reason.clone()).or_default() += 1;
            }
        }
        if !skipped.is_empty() {
            let _ = writeln!(out, "\n### Not run\n");
            for (reason, count) in skipped {
                let _ = writeln!(out, "- {count} × {reason}");
            }
        }
        let _ = writeln!(out);
    }
    out
}

/// The test's name, or the fragment of its IRI.
fn short_name(test: &Test) -> String {
    let fragment = test.id.rsplit(['#', '/']).next().unwrap_or(&test.id);
    if test.name.is_empty() {
        fragment.to_owned()
    } else {
        format!("{fragment}: {}", one_line(&test.name, 80))
    }
}

/// A link to the test's files in the rdf-tests or data-shapes repository.
fn browse(test: &Test) -> String {
    let action = test.action.as_deref().unwrap_or_default();
    if let Some(path) = action.strip_prefix(BASE) {
        format!("https://github.com/w3c/rdf-tests/blob/main/{path}")
    } else if let Some(path) = action.strip_prefix(SHACL_BASE) {
        format!("https://github.com/w3c/data-shapes/blob/gh-pages/{SHACL_DIR}/{path}")
    } else {
        test.id.clone()
    }
}

fn earl(runs: &[Run]) -> String {
    let mut out = String::new();
    let project = "https://github.com/Matdata-eu/tripletrail";
    let _ = writeln!(
        out,
        "@prefix earl: <http://www.w3.org/ns/earl#> .\n\
         @prefix doap: <http://usefulinc.com/ns/doap#> .\n\
         @prefix dc: <http://purl.org/dc/terms/> .\n\
         @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n"
    );
    let _ = writeln!(
        out,
        "<{project}> a doap:Project, earl:TestSubject, earl:Software ;\n  \
         doap:name \"TripleTrail\" ;\n  \
         doap:release [ doap:revision \"{}\" ] ;\n  \
         doap:programming-language \"Rust\" ;\n  \
         doap:homepage <{project}> .\n",
        env!("CARGO_PKG_VERSION")
    );
    for run in runs {
        let outcome = match run.outcome {
            Outcome::Passed => "earl:passed",
            Outcome::Failed(_) => "earl:failed",
            Outcome::Untested(_) => "earl:untested",
        };
        let test = if run.test.id.starts_with("_:") {
            continue;
        } else {
            format!("<{}>", run.test.id)
        };
        let info = match &run.outcome {
            Outcome::Failed(reason) | Outcome::Untested(reason) => {
                format!(
                    " ;\n    earl:info {}",
                    turtle_string(&one_line(reason, 500))
                )
            }
            Outcome::Passed => String::new(),
        };
        let _ = writeln!(
            out,
            "[] a earl:Assertion ;\n  earl:assertedBy <{project}> ;\n  earl:subject <{project}> ;\n  \
             earl:test {test} ;\n  earl:mode earl:automatic ;\n  \
             earl:result [ a earl:TestResult ; earl:outcome {outcome}{info} ] .\n"
        );
    }
    out
}

fn turtle_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn known_failures(runs: &[Run]) -> String {
    let mut out = String::from(
        "# Tests of the W3C SPARQL, RDF and SHACL suites and the GeoSPARQL benchmark TripleTrail fails, one IRI per line, with the reason\n\
         # after `#`. Generated: CI fails when another test fails or one of these passes.\n\
         # See docs/conformance.md for how to update it.\n",
    );
    for (suite, groups) in by_suite(runs) {
        let failures: Vec<&&Run> = groups
            .iter()
            .flat_map(|(_, r)| r)
            .filter(|r| matches!(r.outcome, Outcome::Failed(_)))
            .collect();
        if failures.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n# {suite}");
        for run in failures {
            let Outcome::Failed(reason) = &run.outcome else {
                continue;
            };
            let _ = writeln!(out, "{}  # {}", run.test.id, one_line(reason, 160));
        }
    }
    out
}

fn check_known(runs: &[Run], text: &str) -> bool {
    // Test IRIs have `#` in them: a line is the IRI up to the first space, then the comment.
    let known: BTreeSet<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    let mut ok = true;
    for run in runs {
        let listed = known.contains(run.test.id.as_str());
        match (&run.outcome, listed) {
            (Outcome::Failed(reason), false) => {
                eprintln!(
                    "new failure: {}\n    {}",
                    run.test.id,
                    one_line(reason, 300)
                );
                ok = false;
            }
            (Outcome::Passed, true) => {
                eprintln!(
                    "now passes, remove it from the known failures: {}",
                    run.test.id
                );
                ok = false;
            }
            _ => {}
        }
    }
    if ok {
        println!("no change against the known failures");
    }
    ok
}
