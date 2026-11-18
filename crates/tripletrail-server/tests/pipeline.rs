//! Pipelines (#404): `pipeline.yaml` and `tripletrail pipeline run`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use tripletrail::Engine;
use tripletrail_server::pipeline::{Options, Pipeline, Status, run};

fn scratch(name: &str) -> tripletrail_testdir::TestDir {
    let dir = tripletrail_testdir::tempdir(&format!("pipeline-{name}"));
    for sub in ["data", "lift", "updates", "rules", "shapes"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let write = |path: &str, text: &str| std::fs::write(dir.join(path), text).unwrap();
    write(
        "data/stations.json",
        r#"[{"code":"OSL","name":"Oslo S"},{"code":"LLS","name":"Lillestrøm"}]"#,
    );
    write(
        "lift/stations.rq",
        "PREFIX xyz: <http://sparql.xyz/facade-x/data/>
         PREFIX fx: <http://sparql.xyz/facade-x/ns/>
         CONSTRUCT { ?st a <http://e/Station> ; <http://e/name> ?name } WHERE {
           SERVICE <x-sparql-anything:location=data/stations.json> { ?s xyz:code ?c ; xyz:name ?name }
           BIND(fx:entity(\"http://e/\", ?c) AS ?st) }",
    );
    write(
        "codes.ttl",
        "<http://e/OSL> <http://e/km> 0 . <http://e/LLS> <http://e/km> 21 .",
    );
    // Run in name order: the second needs what the first made.
    write(
        "updates/01-country.ru",
        "INSERT { ?s <http://e/country> ?_country } WHERE { ?s a <http://e/Station> }",
    );
    write(
        "updates/02-norwegian.ru",
        "INSERT { ?s a <http://e/NorwegianStation> } WHERE { ?s <http://e/country> \"NO\" }",
    );
    write(
        "rules/far.ttl",
        "@prefix sh: <http://www.w3.org/ns/shacl#> .
         <http://e/FarShape> a sh:NodeShape ; sh:targetClass <http://e/Station> ;
           sh:rule [ a sh:SPARQLRule ; sh:construct \"\"\"
             CONSTRUCT { $this a <http://e/FarStation> } WHERE {
               GRAPH <http://e/codes> { $this <http://e/km> ?km } FILTER(?km > 10) }\"\"\" ] .",
    );
    write(
        "shapes/named.ttl",
        "@prefix sh: <http://www.w3.org/ns/shacl#> .
         <http://e/NamedShape> a sh:NodeShape ; sh:targetClass <http://e/Station> ;
           sh:property [ sh:path <http://e/name> ; sh:minCount 1 ] .",
    );
    write(
        "shapes/coded.ttl",
        "@prefix sh: <http://www.w3.org/ns/shacl#> .
         <http://e/CodedShape> a sh:NodeShape ; sh:targetClass <http://e/Station> ;
           sh:property [ sh:path <http://e/code> ; sh:minCount 1 ; sh:severity sh:Warning ] .",
    );
    write(
        "pipeline.yaml",
        "name: stations
parameters:
  country: SE
steps:
  - lift: lift/
  - load: codes.ttl
    graph: http://e/codes
  - name: enrich
    update: updates/
  - infer: rules/far.ttl
  - validate: shapes/named.ttl
    report: out/report.ttl
  - export: out/kg.nt
  - export: out/all.nq
",
    );
    dir
}

fn tripletrail(dir: &Path, args: &[&str]) -> (bool, Value, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tripletrail"))
        .current_dir(dir)
        .arg("pipeline")
        .arg("run")
        .args(args)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // No report when the pipeline could not start.
    let report = if out.stdout.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}\n{stderr}", String::from_utf8_lossy(&out.stdout)))
    };
    (out.status.success(), report, stderr)
}

fn sorted_lines(path: &Path) -> Vec<String> {
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

#[test]
fn runs_every_kind_of_step_and_reports_it() {
    let dir = scratch("all");
    let (ok, report, stderr) = tripletrail(&dir, &["pipeline.yaml", "-v", "country=NO"]);
    assert!(ok, "{report:#}\n{stderr}");
    assert_eq!(report["pipeline"], "stations");
    let steps = report["steps"].as_array().unwrap();
    let names: Vec<&str> = steps.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "1 lift",
            "2 load",
            "enrich",
            "4 infer",
            "5 validate",
            "6 export",
            "7 export"
        ]
    );
    assert!(steps.iter().all(|s| s["status"] == "ok"), "{report:#}");
    assert_eq!(steps[0]["files"][0]["file"], "lift/stations.rq");
    assert_eq!(steps[0]["files"][0]["triples"], 4);
    assert_eq!(steps[0]["quadsAfter"], 4);
    assert_eq!(steps[1]["quadsAfter"], 6);
    let updates: Vec<&str> = steps[2]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["file"].as_str().unwrap())
        .collect();
    assert_eq!(
        updates,
        ["updates/01-country.ru", "updates/02-norwegian.ru"]
    );
    assert_eq!(steps[3]["triples"], 1, "only LLS is far: {report:#}");
    assert_eq!(steps[4]["failOn"], "violation");
    assert_eq!(steps[4]["results"], serde_json::json!({}));
    assert_eq!(steps[5]["triples"], 9);
    // The rules' and shapes' graph is gone once their step is done.
    assert_eq!(steps[6]["triples"], 11);
    assert!(
        std::fs::read_to_string(dir.join("out/report.ttl"))
            .unwrap()
            .contains("conforms")
    );
    let kg = sorted_lines(&dir.join("out/kg.nt"));
    assert!(kg.contains(&"<http://e/LLS> <http://e/country> \"NO\" .".to_owned()));
    assert!(kg.contains(&"<http://e/LLS> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://e/NorwegianStation> .".to_owned()));
    assert!(kg.contains(&"<http://e/LLS> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://e/FarStation> .".to_owned()));
    assert!(!kg.iter().any(|l| {
        l.contains("OSL> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://e/FarStation>")
    }));
    assert!(
        !kg.iter().any(|l| l.contains("http://e/km")),
        "codes stay in their graph"
    );
    assert!(
        sorted_lines(&dir.join("out/all.nq"))
            .contains(&"<http://e/LLS> <http://e/km> \"21\"^^<http://www.w3.org/2001/XMLSchema#integer> <http://e/codes> .".to_owned())
    );
    // The parameter's default when no -v is given.
    let (ok, _, stderr) = tripletrail(&dir, &["pipeline.yaml"]);
    assert!(ok, "{stderr}");
    assert!(
        sorted_lines(&dir.join("out/kg.nt"))
            .contains(&"<http://e/LLS> <http://e/country> \"SE\" .".to_owned())
    );
}

#[test]
fn a_validation_gate_stops_the_pipeline() {
    let dir = scratch("gate");
    std::fs::write(
        dir.join("gate.yaml"),
        "steps:
  - lift: lift/
  - validate: shapes/coded.ttl
    fail_on: violation
  - validate: [shapes/coded.ttl, shapes/named.ttl]
    fail_on: warning
  - export: out/kg.nt
",
    )
    .unwrap();
    let (ok, report, _) = tripletrail(&dir, &["gate.yaml"]);
    assert!(!ok);
    assert!(!report["ok"].as_bool().unwrap());
    let steps = report["steps"].as_array().unwrap();
    // Warnings pass a violation gate, and are counted.
    assert_eq!(steps[1]["status"], "ok");
    assert_eq!(steps[1]["results"], serde_json::json!({"warning": 2}));
    assert_eq!(steps[2]["status"], "stopped");
    assert_eq!(steps[2]["files"].as_array().unwrap().len(), 2);
    assert_eq!(steps[3]["status"], "skipped");
    assert!(!dir.join("out/kg.nt").exists());
}

#[test]
fn runs_part_of_a_pipeline_on_a_dataset_on_disk() {
    let dir = scratch("disk");
    let (ok, report, stderr) =
        tripletrail(&dir, &["pipeline.yaml", "--location", "db", "--to", "2"]);
    assert!(ok, "{report:#}\n{stderr}");
    assert_eq!(report["steps"].as_array().unwrap().len(), 2);
    // The rest runs on what the first steps left in the dataset.
    let (ok, report, stderr) = tripletrail(
        &dir,
        &[
            "pipeline.yaml",
            "--location",
            "db",
            "--from",
            "enrich",
            "-v",
            "country=NO",
        ],
    );
    assert!(ok, "{report:#}\n{stderr}");
    let steps = report["steps"].as_array().unwrap();
    assert_eq!(steps[0]["step"], 3);
    assert_eq!(steps[0]["quadsBefore"], 6);
    assert_eq!(steps[1]["triples"], 1);
    let (ok, _, stderr) = tripletrail(&dir, &["pipeline.yaml", "--from", "nope"]);
    assert!(!ok && stderr.contains("no step named nope"), "{stderr}");
}

#[test]
fn a_failing_or_slow_step_stops_the_pipeline() {
    let dir = scratch("fail");
    let mut chain = String::new();
    for i in 0..3000 {
        chain.push_str(&format!(
            "<http://e/n{i}> <http://e/next> <http://e/n{}> .\n",
            i + 1
        ));
    }
    std::fs::write(dir.join("chain.nt"), chain).unwrap();
    std::fs::write(
        dir.join("slow.ru"),
        "INSERT { ?a <http://e/reach> ?b } WHERE { ?a <http://e/next>* ?b }",
    )
    .unwrap();
    std::fs::write(dir.join("bad.ru"), "INSERT DATA { oops }").unwrap();
    std::fs::write(
        dir.join("fail.yaml"),
        "steps:
  - load: chain.nt
  - update: slow.ru
    timeout: 0.3
  - update: bad.ru
",
    )
    .unwrap();
    let started = std::time::Instant::now();
    let (ok, report, _) = tripletrail(&dir, &["fail.yaml"]);
    assert!(!ok);
    assert!(started.elapsed().as_secs() < 30);
    let steps = report["steps"].as_array().unwrap();
    assert_eq!(steps[1]["status"], "failed");
    assert_eq!(
        steps[1]["error"], "the step took longer than its timeout of 0.3 s",
        "{report:#}"
    );
    assert_eq!(steps[2]["status"], "skipped");
    let (ok, report, _) = tripletrail(&dir, &["fail.yaml", "--from", "3"]);
    assert!(!ok);
    let error = report["steps"][0]["error"].as_str().unwrap();
    assert!(error.starts_with("bad.ru: "), "{error}");
}

#[test]
fn a_pipeline_file_is_checked_before_it_runs() {
    let parse = |text: &str| Pipeline::parse(text, PathBuf::from("."), "p").map(|_| ());
    for (text, error) in [
        ("steps: []", "no steps"),
        ("steps:\n  - lod: x.ttl", "no step kind"),
        (
            "steps:\n  - load: x.ttl\n    update: y.ru",
            "more than one step kind",
        ),
        (
            "steps:\n  - load: x.ttl\n    grpah: http://e/g",
            "unknown key grpah",
        ),
        (
            "steps:\n  - update: y.ru\n    graph: http://e/g",
            "names its graphs",
        ),
        ("steps:\n  - reason: owl.ttl", "#187"),
        ("steps:\n  - reason: owl-dl", "owl-rl"),
        (
            "steps:\n  - validate: s.ttl\n    fail_on: sometimes",
            "fail_on",
        ),
        ("steps:\n  - load: x.ttl\n    timeout: soon", "timeout"),
        (
            "steps:\n  - load: x.ttl\n    graph: not an iri",
            "not an IRI",
        ),
        (
            "steps:\n  - load: x.ttl\n    name: a\n  - load: y.ttl\n    name: a",
            "two steps",
        ),
        ("stpes: []", "unknown key stpes"),
    ] {
        let e = parse(text).unwrap_err();
        assert!(e.contains(error), "{text}: {e}");
    }
}

#[test]
fn a_pipeline_stays_inside_its_root() {
    let dir = scratch("root");
    let root = dir.join("ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(dir.join("secret.ttl"), "<http://e/s> <http://e/p> 1 .").unwrap();
    for (text, error) in [
        ("steps:\n  - load: ../secret.ttl", "outside the workspace"),
        ("steps:\n  - export: ../out.nt", "outside the workspace"),
        (
            "steps:\n  - export: sub/../../out.nt",
            "outside the workspace",
        ),
    ] {
        let pipeline = Pipeline::parse(text, root.to_path_buf(), "p").unwrap();
        let options = Options {
            root: Some(&root),
            ..Options::default()
        };
        let report = run(&pipeline, &Engine::new_tripletrail(), &options, &|_, _| {});
        assert_eq!(report.steps[0].status, Status::Failed);
        let e = report.steps[0].error.as_deref().unwrap();
        assert!(e.contains(error), "{text}: {e}");
    }
    assert!(!dir.join("out.nt").exists());
}

fn chain_engine(n: usize) -> Engine {
    let engine = Engine::new_tripletrail();
    let mut chain = String::new();
    for i in 0..n {
        chain.push_str(&format!(
            "<http://e/n{i}> <http://e/next> <http://e/n{}> .\n",
            i + 1
        ));
    }
    engine
        .load(
            tripletrail::oxigraph::io::RdfFormat::NTriples,
            chain.as_bytes(),
        )
        .unwrap();
    engine
}

fn has_shapes_graph(engine: &Engine) -> bool {
    matches!(
        engine
            .query("ASK { GRAPH <urn:x-tripletrail:pipeline:shapes> { ?s ?p ?o } }")
            .unwrap(),
        tripletrail::oxigraph::sparql::QueryResults::Boolean(true)
    )
}

#[test]
fn a_step_that_runs_out_of_time_leaves_no_shapes_behind() {
    let dir = scratch("shapes-timeout");
    std::fs::write(
        dir.join("rules/reach.ttl"),
        "@prefix sh: <http://www.w3.org/ns/shacl#> .
         <http://e/ReachShape> a sh:NodeShape ; sh:targetSubjectsOf <http://e/next> ;
           sh:rule [ a sh:SPARQLRule ; sh:construct \"\"\"
             CONSTRUCT { $this <http://e/reach> ?b } WHERE { $this <http://e/next>* ?b }\"\"\" ] .",
    )
    .unwrap();
    let pipeline = Pipeline::parse(
        "steps:\n  - infer: rules/reach.ttl\n    graph: http://e/inferred\n    timeout: 0.3\n",
        dir.to_path_buf(),
        "p",
    )
    .unwrap();
    let engine = chain_engine(3000);
    let started = std::time::Instant::now();
    let report = run(&pipeline, &engine, &Options::default(), &|_, _| {});
    assert!(started.elapsed().as_secs() < 30);
    assert_eq!(report.steps[0].status, Status::Failed);
    assert_eq!(
        report.steps[0].error.as_deref(),
        Some("the step took longer than its timeout of 0.3 s")
    );
    assert!(!has_shapes_graph(&engine));
    // The same when the server's cap is the shorter.
    let pipeline = Pipeline::parse(
        "steps:\n  - infer: rules/reach.ttl\n    graph: http://e/inferred\n",
        dir.to_path_buf(),
        "p",
    )
    .unwrap();
    let options = Options {
        timeout: Some(std::time::Duration::from_millis(300)),
        ..Options::default()
    };
    let report = run(&pipeline, &engine, &options, &|_, _| {});
    assert_eq!(report.steps[0].status, Status::Failed);
    assert!(!has_shapes_graph(&engine));
}

/// An export's `query` is the query itself when it has a `{`, else the file that holds it,
/// which an error names when it is missing.
#[test]
fn an_export_query_is_inline_or_a_file() {
    let dir = scratch("export-query");
    let pipeline = Pipeline::parse(
        "steps:\n  - load: codes.ttl\n  - export: out/first.nt\n    query: 'CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o } LIMIT 1'\n  - export: out/none.nt\n    query: queries/missing.rq\n",
        dir.to_path_buf(),
        "p",
    )
    .unwrap();
    assert_eq!(pipeline.steps[1].kind.files(), ["out/first.nt"]);
    let report = run(
        &pipeline,
        &Engine::new_tripletrail(),
        &Options::default(),
        &|_, _| {},
    );
    assert_eq!(report.steps[1].status, Status::Ok, "{:#}", report.to_json());
    let first = std::fs::read_to_string(dir.join("out/first.nt")).unwrap();
    assert_eq!(first.lines().count(), 1);
    let error = report.steps[2].error.as_deref().unwrap();
    assert!(error.contains("query file queries/missing.rq"), "{error}");
}

#[test]
fn a_failed_export_keeps_the_last_good_file() {
    let dir = scratch("export");
    let pipeline = Pipeline::parse(
        "steps:\n  - load: codes.ttl\n  - export: out/kg.nt\n  - validate: shapes/named.ttl\n    report: out/report.ttl\n",
        dir.to_path_buf(),
        "p",
    )
    .unwrap();
    let report = run(
        &pipeline,
        &Engine::new_tripletrail(),
        &Options::default(),
        &|_, _| {},
    );
    assert!(report.ok(), "{:#}", report.to_json());
    let kg = std::fs::read_to_string(dir.join("out/kg.nt")).unwrap();
    assert_eq!(kg.lines().count(), 2);
    // Room for less than the export: it fails, the old file stays, no part of the new one.
    std::fs::write(
        dir.join("more.ttl"),
        "<http://e/a> <http://e/b> <http://e/c> .",
    )
    .unwrap();
    let pipeline = Pipeline::parse(
        "steps:\n  - load: [codes.ttl, more.ttl]\n  - export: out/kg.nt\n",
        dir.to_path_buf(),
        "p",
    )
    .unwrap();
    let options = Options {
        max_output: Some(40),
        ..Options::default()
    };
    let report = run(&pipeline, &Engine::new_tripletrail(), &options, &|_, _| {});
    assert_eq!(report.steps[1].status, Status::Failed);
    assert!(
        report.steps[1]
            .error
            .as_deref()
            .unwrap()
            .contains("more than the 40 bytes"),
        "{:#}",
        report.to_json()
    );
    assert_eq!(std::fs::read_to_string(dir.join("out/kg.nt")).unwrap(), kg);
    let left: Vec<_> = std::fs::read_dir(dir.join("out"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(!left.iter().any(|n| n.ends_with(".partial")), "{left:?}");
    // And past the most quads.
    let options = Options {
        max_quads: Some(2),
        ..Options::default()
    };
    let report = run(&pipeline, &Engine::new_tripletrail(), &options, &|_, _| {});
    assert_eq!(report.steps[0].status, Status::Failed);
    assert!(
        report.steps[0]
            .error
            .as_deref()
            .unwrap()
            .contains("more than the 2 it may hold")
    );
}

#[cfg(unix)]
#[test]
fn a_partial_file_left_as_a_link_is_not_followed() {
    let dir = scratch("partial-link");
    let root = dir.join("ws");
    std::fs::create_dir_all(root.join("out")).unwrap();
    std::fs::write(
        root.join("a.ttl"),
        "<http://e/a> <http://e/b> <http://e/c> .",
    )
    .unwrap();
    std::os::unix::fs::symlink(dir.join("outside.nt"), root.join("out/kg.nt.partial")).unwrap();
    let pipeline = Pipeline::parse(
        "steps:\n  - load: a.ttl\n  - export: out/kg.nt\n",
        root.to_path_buf(),
        "p",
    )
    .unwrap();
    let options = Options {
        root: Some(&root),
        ..Options::default()
    };
    let report = run(&pipeline, &Engine::new_tripletrail(), &options, &|_, _| {});
    assert!(report.ok(), "{:#}", report.to_json());
    assert!(!dir.join("outside.nt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("out/kg.nt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

fn count(engine: &Engine, query: &str) -> usize {
    match engine.query(query).unwrap() {
        tripletrail::oxigraph::sparql::QueryResults::Solutions(rows) => rows
            .map(|r| {
                let r = r.unwrap();
                match r.get("n") {
                    Some(tripletrail::oxigraph::model::Term::Literal(l)) => {
                        l.value().parse().unwrap()
                    }
                    _ => 0,
                }
            })
            .next()
            .unwrap_or(0),
        _ => panic!("expected rows"),
    }
}

fn has_python() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn ordered_rules_run_once_in_order_into_the_step_graph() {
    let dir = scratch("ordered");
    // `first` (order 1) reads what `last` (order 3) writes: run in order, it sees none of it;
    // run to a fixpoint, it does. `second` reads what `first` and the data in another graph give.
    std::fs::write(
        dir.join("rules/chain.ttl"),
        "@prefix sh: <http://www.w3.org/ns/shacl#> .
         <http://e/S> a sh:NodeShape ; sh:targetSubjectsOf <http://e/km> ;
           sh:rule <http://e/first>, <http://e/second>, <http://e/last> .
         <http://e/first> a sh:SPARQLRule ; sh:order 1 ; sh:construct \"\"\"
           CONSTRUCT { $this <http://e/a> true } WHERE { $this <http://e/km> ?km FILTER NOT EXISTS { $this <http://e/c> true } }\"\"\" .
         <http://e/second> a sh:SPARQLRule ; sh:order 2 ; sh:construct \"\"\"
           CONSTRUCT { $this <http://e/b> ?km } WHERE { $this <http://e/a> true ; <http://e/km> ?km }\"\"\" .
         <http://e/last> a sh:SPARQLRule ; sh:order 3 ; sh:construct \"\"\"
           CONSTRUCT { $this <http://e/c> true } WHERE { $this <http://e/b> ?km }\"\"\" .",
    )
    .unwrap();
    let yaml = |ordered: bool| {
        format!(
            "union_default_graph: true
steps:
  - load: codes.ttl
    graph: http://e/codes
  - infer: rules/chain.ttl
    graph: http://e/step
    clear: true
    ordered: {ordered}
"
        )
    };
    let engine = Engine::new_tripletrail();
    let pipeline = Pipeline::parse(&yaml(true), dir.to_path_buf(), "p").unwrap();
    let report = run(&pipeline, &engine, &Options::default(), &|_, _| {});
    assert!(report.ok(), "{:?}", report.steps);
    let in_step = "SELECT (COUNT(*) AS ?n) { GRAPH <http://e/step> { ?s ?p ?o } }";
    // a, b and c for both stations, only in the step's graph.
    assert_eq!(count(&engine, in_step), 6);
    // Each rule's run is in the report, in order.
    let rules = report.to_json()["steps"][1]["rules"].clone();
    let names: Vec<&str> = rules
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["rule"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["<http://e/first>", "<http://e/second>", "<http://e/last>"]
    );
    assert_eq!(rules[0]["new"], 2);
    assert_eq!(
        count(
            &engine,
            "SELECT (COUNT(*) AS ?n) { GRAPH <http://e/codes> { ?s ?p ?o } }"
        ),
        2
    );
    assert!(!engine.union_default_graph());
    // Again: the step's graph is replaced, not added to. The data now hold `c`, so `first`
    // would find nothing, but clearing comes first.
    let report = run(&pipeline, &engine, &Options::default(), &|_, _| {});
    assert!(report.ok(), "{:?}", report.steps);
    assert_eq!(count(&engine, in_step), 6);
    // The rules' own order without `ordered`: the same here, as each rule reads only what
    // the rules before it write.
    let pipeline = Pipeline::parse(&yaml(false), dir.to_path_buf(), "p").unwrap();
    let report = run(&pipeline, &engine, &Options::default(), &|_, _| {});
    assert!(report.ok(), "{:?}", report.steps);
    assert_eq!(count(&engine, in_step), 6);
}

#[test]
fn ordered_rules_follow_sh_order_not_dependencies() {
    use tripletrail::oxigraph::model::NamedNode;
    let engine = Engine::new_tripletrail();
    engine
        .load(
            tripletrail::oxigraph::io::RdfFormat::Turtle,
            "<http://e/x> <http://e/km> 1 .".as_bytes(),
        )
        .unwrap();
    let shapes = NamedNode::new("http://e/shapes").unwrap();
    // `early` reads what `late` writes: in sh:order it runs first and finds nothing.
    engine
        .load_into(
            shapes.clone(),
            tripletrail::oxigraph::io::RdfFormat::Turtle,
            "@prefix sh: <http://www.w3.org/ns/shacl#> .
             <http://e/S> a sh:NodeShape ; sh:targetSubjectsOf <http://e/km> ;
               sh:rule <http://e/early>, <http://e/late> .
             <http://e/early> a sh:SPARQLRule ; sh:order 1 ; sh:construct \"\"\"
               CONSTRUCT { $this <http://e/seen> true } WHERE { $this <http://e/late> true }\"\"\" .
             <http://e/late> a sh:SPARQLRule ; sh:order 2 ; sh:construct \"\"\"
               CONSTRUCT { $this <http://e/late> true } WHERE { $this <http://e/km> ?km }\"\"\" ."
                .as_bytes(),
        )
        .unwrap();
    let infer = |sequential: bool| {
        let options = tripletrail::InferOptions {
            sequential,
            max_passes: if sequential { 1 } else { 64 },
            ..Default::default()
        };
        engine
            .infer_with(
                &shapes,
                tripletrail::oxigraph::model::GraphNameRef::DefaultGraph,
                None,
                &options,
                &|_| {},
            )
            .unwrap()
            .len()
    };
    assert_eq!(infer(false), 2);
    assert_eq!(infer(true), 1);
}

#[test]
fn python_steps_work_on_the_served_dataset() {
    if !has_python() {
        eprintln!("no python3: skipped");
        return;
    }
    let dir = scratch("python");
    std::fs::create_dir_all(dir.join("steps")).unwrap();
    // As the rail KG steps: run_*(base_url, dataset), plain HTTP, reading the union default graph
    // and writing with the Graph Store Protocol and form updates.
    std::fs::write(
        dir.join("steps/geometry.py"),
        r#"import json, os, urllib.parse, urllib.request

def post(url, data, content_type, accept=None):
    request = urllib.request.Request(url, data=data, headers={"Content-Type": content_type})
    if accept:
        request.add_header("Accept", accept)
    with urllib.request.urlopen(request) as response:
        return response.status, response.read()

def run_geometry(base_url, dataset, graph, factor="1"):
    query = "SELECT ?s ?km WHERE { ?s <http://e/km> ?km } ORDER BY ?s"
    status, body = post(f"{base_url}/{dataset}/sparql", urllib.parse.urlencode({"query": query}).encode(),
                        "application/x-www-form-urlencoded", "application/sparql-results+json")
    rows = json.loads(body)["results"]["bindings"]
    nt = "".join(f'<{r["s"]["value"]}> <http://e/m> "{int(r["km"]["value"]) * 1000 * int(factor)}" .\n' for r in rows)
    status, _ = post(f"{base_url}/{dataset}/data?graph={urllib.parse.quote(graph)}", nt.encode(), "application/n-triples")
    assert 200 <= status < 300, status
    update = f"INSERT DATA {{ GRAPH <{graph}> {{ <http://e/run> <http://e/rows> {len(rows)} }} }}"
    status, _ = post(f"{base_url}/{dataset}/update", urllib.parse.urlencode({"update": update}).encode(),
                     "application/x-www-form-urlencoded")
    assert status == 200, status
    print(f"{len(rows)} rows")
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("steps/tag.py"),
        r#"import os, sys, urllib.parse, urllib.request
update = f"INSERT DATA {{ GRAPH <{os.environ['TRIPLETRAIL_GRAPH']}> {{ <http://e/run> <http://e/tag> \"{sys.argv[1]}\" }} }}"
request = urllib.request.Request(os.environ["TRIPLETRAIL_ENDPOINT"] + "/update", data=update.encode(),
                                 headers={"Content-Type": "application/sparql-update"})
urllib.request.urlopen(request)
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("steps/broken.py"),
        "def run(base_url, dataset):\n    raise ValueError('no geometry for station 7')\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("pipeline.yaml"),
        "union_default_graph: true
parameters:
  tag: v1
steps:
  - load: codes.ttl
    graph: http://e/codes
  - name: geometry
    python: steps.geometry:run_geometry
    graph: http://e/geometry
    clear: true
    kwargs: {graph: 'http://e/geometry', factor: 2}
  - name: tag
    python: steps/tag.py
    args: ['${tag}']
    graph: http://e/tag
  - name: broken
    python: steps.broken:run
",
    )
    .unwrap();
    let pipeline = Pipeline::read(&dir.join("pipeline.yaml")).unwrap();
    let engine = Engine::new_tripletrail();
    let options = Options {
        python: Some("python3".to_owned()),
        ..Options::default()
    };
    let report = run(&pipeline, &engine, &options, &|_, _| {});
    let steps = &report.steps;
    assert_eq!(steps[1].status, Status::Ok, "{:?}", steps[1].error);
    assert_eq!(steps[2].status, Status::Ok, "{:?}", steps[2].error);
    assert_eq!(steps[3].status, Status::Failed);
    let error = steps[3].error.as_deref().unwrap();
    assert!(error.contains("no geometry for station 7"), "{error}");
    assert_eq!(
        count(
            &engine,
            "SELECT (COUNT(*) AS ?n) { GRAPH <http://e/geometry> { ?s <http://e/m> \"42000\" } }"
        ),
        1
    );
    assert_eq!(
        count(
            &engine,
            "SELECT (COUNT(*) AS ?n) { GRAPH <http://e/geometry> { <http://e/run> <http://e/rows> 2 } }"
        ),
        1
    );
    assert_eq!(
        count(
            &engine,
            "SELECT (COUNT(*) AS ?n) { GRAPH <http://e/tag> { <http://e/run> <http://e/tag> \"v1\" } }"
        ),
        1
    );
    // Run again from the python step: its graph is cleared first.
    let options = Options {
        from: 1,
        to: Some(2),
        python: Some("python3".to_owned()),
        ..Options::default()
    };
    let report = run(&pipeline, &engine, &options, &|_, _| {});
    assert!(report.ok(), "{:?}", report.steps);
    assert_eq!(
        count(
            &engine,
            "SELECT (COUNT(*) AS ?n) { GRAPH <http://e/geometry> { ?s ?p ?o } }"
        ),
        3
    );
    // Without an interpreter (as on a server), python steps are refused.
    let report = run(
        &pipeline,
        &engine,
        &Options {
            from: 1,
            to: Some(2),
            ..Options::default()
        },
        &|_, _| {},
    );
    assert!(
        report.steps[0]
            .error
            .as_deref()
            .unwrap()
            .contains("not on the server")
    );
    // From the command line too.
    let (ok, report, stderr) = tripletrail(&dir, &["pipeline.yaml", "--to", "tag"]);
    assert!(ok, "{report}\n{stderr}");
    assert!(stderr.contains("2 rows"), "{stderr}");
}

/// A pipeline's `datasets` (a lineage dataset the rail KG scripts write next to the data, as
/// Fuseki's `/metadata`): python steps reach them on the loopback server, and what one step
/// writes there the next one reads.
#[test]
fn python_steps_reach_the_pipelines_other_datasets() {
    if !has_python() {
        eprintln!("no python3: skipped");
        return;
    }
    let dir = scratch("python-datasets");
    std::fs::create_dir_all(dir.join("steps")).unwrap();
    std::fs::write(
        dir.join("steps/lineage.py"),
        r#"import json, urllib.parse, urllib.request

def post(url, form, accept=None):
    request = urllib.request.Request(url, data=urllib.parse.urlencode(form).encode(),
                                     headers={"Content-Type": "application/x-www-form-urlencoded"})
    if accept:
        request.add_header("Accept", accept)
    with urllib.request.urlopen(request) as response:
        return response.status, response.read()

def write(base_url, dataset, step):
    update = f"INSERT DATA {{ GRAPH <http://e/lineage> {{ <http://e/{step}> <http://e/from> \"{dataset}\" }} }}"
    status, _ = post(f"{base_url}/metadata/update", {"update": update})
    assert status == 200, status

def check(base_url, dataset):
    status, body = post(f"{base_url}/metadata/sparql",
                        {"query": "SELECT (COUNT(*) AS ?n) { GRAPH ?g { ?s ?p ?o } }"},
                        "application/sparql-results+json")
    n = int(json.loads(body)["results"]["bindings"][0]["n"]["value"])
    assert n == 2, n
    update = f"INSERT DATA {{ <http://e/lineage> <http://e/count> {n} }}"
    status, _ = post(f"{base_url}/{dataset}/update", {"update": update})
    assert status == 200, status
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("pipeline.yaml"),
        "datasets: [metadata]
steps:
  - name: first
    python: steps.lineage:write
    kwargs: {step: a}
  - name: second
    python: steps.lineage:write
    kwargs: {step: b}
  - name: check
    python: steps.lineage:check
",
    )
    .unwrap();
    let pipeline = Pipeline::read(&dir.join("pipeline.yaml")).unwrap();
    assert_eq!(pipeline.datasets, ["metadata"]);
    let engine = Engine::new_tripletrail();
    let options = Options {
        python: Some("python3".to_owned()),
        ..Options::default()
    };
    let report = run(&pipeline, &engine, &options, &|_, _| {});
    for s in &report.steps {
        assert_eq!(s.status, Status::Ok, "{}: {:?}", s.name, s.error);
    }
    // The lineage stayed out of the pipeline's own dataset.
    assert_eq!(
        count(&engine, "SELECT (COUNT(*) AS ?n) { GRAPH ?g { ?s ?p ?o } }"),
        0
    );
    assert_eq!(
        count(
            &engine,
            "SELECT (COUNT(*) AS ?n) { <http://e/lineage> <http://e/count> 2 }"
        ),
        1
    );
}

#[test]
fn python_steps_are_checked_when_read() {
    let dir = PathBuf::from(".");
    let parse = |yaml: &str| {
        Pipeline::parse(yaml, dir.to_path_buf(), "p")
            .map(|_| ())
            .unwrap_err()
    };
    assert!(parse("steps:\n  - python: 'os; rm'\n").contains("module:function"));
    assert!(parse("steps:\n  - python: a.py\n    kwargs: {a: 1}\n").contains("kwargs"));
    assert!(parse("steps:\n  - python: a:b\n    args: [x]\n").contains("args"));
    assert!(parse("steps:\n  - python: a:b\n    dataset: 'a b'\n").contains("dataset"));
    assert!(parse("datasets: ['a b']\nsteps:\n  - python: a:b\n").contains("dataset name"));
    assert!(parse("datasets: [m, m]\nsteps:\n  - python: a:b\n").contains("twice"));
    assert!(parse("steps:\n  - load: a.ttl\n    clear: true\n").contains("clear"));
    Pipeline::parse(
        "union_default_graph: true\nsteps:\n  - python: pkg.mod:run\n    path: [src]\n    graph: http://e/g\n    clear: true\n",
        dir,
        "p",
    )
    .unwrap();
}

#[test]
fn with_a_union_default_graph_rules_and_shapes_see_every_graph() {
    let dir = scratch("union");
    // The stations are in a graph of their own; the default graph is empty.
    std::fs::write(
        dir.join("stations.ttl"),
        "<http://e/OSL> a <http://e/Station> . <http://e/LLS> a <http://e/Station> ; <http://e/name> \"Lillestrøm\" .",
    )
    .unwrap();
    let pipeline = Pipeline::parse(
        "union_default_graph: true
steps:
  - load: stations.ttl
    graph: http://e/stations
  - infer: rules/far.ttl
    graph: http://e/far
  - load: codes.ttl
    graph: http://e/codes
  - infer: rules/far.ttl
    graph: http://e/far
    clear: true
  - validate: shapes/named.ttl
",
        dir.to_path_buf(),
        "p",
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    let report = run(&pipeline, &engine, &Options::default(), &|_, _| {});
    // Without the codes, no station is far; with them, LLS (21 km).
    assert_eq!(report.steps[1].triples, Some(0));
    assert_eq!(report.steps[3].triples, Some(1));
    // OSL has no name: the gate sees the stations' graph.
    assert_eq!(report.steps[4].status, Status::Stopped);
    assert_eq!(report.steps[4].results.as_ref().unwrap()["violation"], 1);
}

#[test]
fn a_pipeline_exports_as_an_airflow_dag_and_a_python_script() {
    let dir = scratch("export-code");
    let out = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_tripletrail"))
            .current_dir(&dir)
            .args(["pipeline", "export", "pipeline.yaml"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let dag = out(&["--to", "airflow", "--location", "/data/kg"]);
    assert!(dag.contains("LOCATION = \"/data/kg\""), "{dag}");
    assert!(dag.contains("dag_id=\"stations\""));
    // One task per step, in a chain, named by the step; the parameters are DAG params.
    assert!(
        dag.contains("t1 = BashOperator(task_id=\"lift_1\", bash_command=command(\"1\", \"1\")")
    );
    assert!(dag.contains("t3 = BashOperator(task_id=\"enrich\""));
    assert!(dag.contains("t1 >> t2 >> t3 >> t4 >> t5 >> t6 >> t7"));
    assert!(dag.contains("\"country\": \"SE\""));
    let one = out(&["--to", "airflow", "--one-task"]);
    assert!(one.contains("run = BashOperator(task_id=\"pipeline\", bash_command=command()"));
    // The script binds the parameters of the updates that use them, and reads the rest.
    let script = out(&["--to", "python", "-v", "country=NO"]);
    assert!(script.contains("?s <http://e/country> \"NO\""), "{script}");
    assert!(script.contains("e.load(path=f, to_graph=NamedNode(\"http://e/codes\"))"));
    assert!(script.contains("\"tripletrail\", \"lift\", path(\"lift/\")"));
    assert!(script.contains("raise SystemExit(\"5 validate\" + f\": validation found"));
}

/// Names and parameters from a `pipeline.yaml` end up in generated code: they must stay data.
#[test]
fn exported_code_keeps_hostile_names_as_data() {
    if !has_python() {
        eprintln!("no python3: skipped");
        return;
    }
    let dir = scratch("export-hostile");
    std::fs::write(
        dir.join("pipeline.yaml"),
        r#"name: "bad\\x name\n\"\"\" import os"
parameters:
  country: NO
  "x$(touch PWNED_SHELL)": "1"
steps:
  - load: data/
  - name: "enrich\n    import os\n    os.system('touch PWNED_PY')"
    update: updates/
  - name: import
    infer: rules/r.ttl
  - name: a-b
    export: out/1.nq
  - name: "a.b\\"
    validate: shapes/
  - name: "py\n    os.system('x')"
    python: "x\nimport os.py"
"#,
    )
    .unwrap();
    let export = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_tripletrail"))
            .current_dir(&dir)
            .args(["pipeline", "export", "pipeline.yaml"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    std::fs::write(dir.join("dag.py"), export(&["--to", "airflow"])).unwrap();
    std::fs::write(dir.join("run.py"), export(&["--to", "python"])).unwrap();
    // Both compile, and neither calls anything a name smuggled in; the DAG's command line,
    // run by bash, does not run the parameter's name.
    let check = r#"
import ast, os, subprocess, sys, types
for f in ["dag.py", "run.py"]:
    tree = ast.parse(open(f).read(), f)
    calls = {n.func.attr for n in ast.walk(tree) if isinstance(n, ast.Call) and isinstance(n.func, ast.Attribute)}
    assert "system" not in calls, f
    imports = [n for n in ast.walk(tree) if isinstance(n, ast.Import) and any(a.name == "os" for a in n.names)]
    assert len(imports) == (1 if f == "run.py" else 0), f
tasks = []
class Op:
    def __init__(self, task_id, bash_command, **k):
        tasks.append((task_id, bash_command))
    def __rshift__(self, other):
        return other
class Dag:
    def __init__(self, **k): pass
    def __enter__(self): return self
    def __exit__(self, *a): pass
sys.modules["airflow"] = types.SimpleNamespace(DAG=Dag)
sys.modules["airflow.providers.standard.operators.bash"] = types.SimpleNamespace(BashOperator=Op)
sys.modules["pendulum"] = types.SimpleNamespace(datetime=lambda *a, **k: None)
code = open("dag.py").read().replace('TRIPLETRAIL = "tripletrail"', 'TRIPLETRAIL = "true"')
exec(compile(code, "dag.py", "exec"), {})
assert len({t for t, _ in tasks}) == len(tasks) == 6, tasks
for _, command in tasks:
    subprocess.run(["bash", "-c", command], check=True)
assert not os.path.exists("PWNED_SHELL")
"#;
    let out = Command::new("python3")
        .current_dir(&dir)
        .args(["-c", check])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn rules_into_another_graph_still_read_the_named_graphs() {
    let dir = scratch("graph-rules");
    std::fs::write(
        dir.join("stations.ttl"),
        "<http://e/OSL> a <http://e/Station> . <http://e/LLS> a <http://e/Station> .",
    )
    .unwrap();
    // `rules/far.ttl` reads `GRAPH <http://e/codes>`; the inferred triples go to another graph,
    // so the rules run on a copy, which must hold that graph too.
    let pipeline = Pipeline::parse(
        "steps:
  - load: stations.ttl
  - load: codes.ttl
    graph: http://e/codes
  - infer: rules/far.ttl
    graph: http://e/far
",
        dir.to_path_buf(),
        "p",
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    let report = run(&pipeline, &engine, &Options::default(), &|_, _| {});
    assert!(report.ok(), "{:?}", report.steps);
    assert_eq!(report.steps[2].triples, Some(1));
    assert!(matches!(
        engine
            .query("ASK { GRAPH <http://e/far> { <http://e/LLS> a <http://e/FarStation> } }")
            .unwrap(),
        tripletrail::oxigraph::sparql::QueryResults::Boolean(true)
    ));
}

#[test]
fn a_lift_runs_once_per_input_file_with_its_variables_and_then_updates() {
    let dir = scratch("for-each");
    std::fs::create_dir_all(dir.join("tms")).unwrap();
    std::fs::write(
        dir.join("tms/a.json"),
        r#"[{"code":"OSL","wkt":"POINT M (1 2 3)"}]"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("tms/b.json"),
        r#"[{"code":"LLS","wkt":"POINT M (4 5 6)"}]"#,
    )
    .unwrap();
    std::fs::write(dir.join("tms/notes.txt"), "not a match").unwrap();
    std::fs::write(
        dir.join("lift/tms.rq"),
        "PREFIX xyz: <http://sparql.xyz/facade-x/data/>
         PREFIX fx: <http://sparql.xyz/facade-x/ns/>
         CONSTRUCT { ?st <http://e/wkt> ?wkt ; <http://e/from> ?_file_stem ; <http://e/source> ?_source } WHERE {
           SERVICE <x-sparql-anything:> { fx:properties fx:location ?_file . ?s xyz:code ?c ; xyz:wkt ?wkt }
           BIND(fx:entity(\"http://e/\", ?c) AS ?st) }",
    )
    .unwrap();
    // The hook sees only what one lift gave: the M ordinate goes.
    std::fs::write(
        dir.join("updates/drop-m.ru"),
        "DELETE { ?s <http://e/wkt> ?w } INSERT { ?s <http://e/wkt> ?flat } WHERE {
           ?s <http://e/wkt> ?w BIND(REPLACE(REPLACE(?w, \" M \", \" \"), \" [0-9.]+\\\\)$\", \")\") AS ?flat) }",
    )
    .unwrap();
    let pipeline = Pipeline::parse(
        "parameters:
  system: tms
steps:
  - lift: lift/tms.rq
    for_each: tms/*.json
    then: updates/drop-m.ru
    graph: http://e/lifted
    variables:
      source: ${system} export
",
        dir.to_path_buf(),
        "p",
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    let report = run(&pipeline, &engine, &Options::default(), &|_, _| {});
    assert!(report.ok(), "{:?}", report.steps);
    let rows = |q: &str| match engine.query(q).unwrap() {
        tripletrail::oxigraph::sparql::QueryResults::Solutions(rows) => rows
            .map(|r| {
                let r = r.unwrap();
                r.iter()
                    .map(|(_, t)| t.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>(),
        _ => panic!(),
    };
    let mut got = rows(
        "SELECT ?s ?w ?f ?src { GRAPH <http://e/lifted> { ?s <http://e/wkt> ?w ; <http://e/from> ?f ; <http://e/source> ?src } }",
    );
    got.sort();
    assert_eq!(
        got,
        [
            "<http://e/LLS> \"POINT (4 5)\" \"b\" \"tms export\"",
            "<http://e/OSL> \"POINT (1 2)\" \"a\" \"tms export\"",
        ]
    );
    // Two lifts, each of one file.
    assert_eq!(report.steps[0].files.len(), 2);

    let bad = |yaml: &str| Pipeline::parse(yaml, dir.to_path_buf(), "p").unwrap_err();
    assert!(bad("steps:\n  - lift: lift/tms.rq\n    as: f\n").contains("as:"));
    assert!(bad("steps:\n  - load: x.ttl\n    variables:\n      a-b: 1\n").contains("variables"));
    let missing = Pipeline::parse(
        "steps:\n  - lift: lift/tms.rq\n    for_each: tms/*.xml\n",
        dir.to_path_buf(),
        "p",
    )
    .unwrap();
    let report = run(
        &missing,
        &Engine::new_tripletrail(),
        &Options::default(),
        &|_, _| {},
    );
    assert!(
        report.steps[0]
            .error
            .as_deref()
            .unwrap()
            .contains("no files")
    );
}

#[test]
fn a_publish_step_writes_the_graphs_as_a_new_dataset() {
    let dir = scratch("publish");
    std::fs::write(
        dir.join("data/g.trig"),
        "<http://e/a> <http://e/p> 1 .
         GRAPH <http://bnd/_graph_lift> { <http://e/b> <http://e/p> 2 }
         GRAPH <http://bnd/_graph_rules> { <http://e/c> <http://e/p> 3 }
         GRAPH <http://other/g> { <http://e/d> <http://e/p> 4 }",
    )
    .unwrap();
    let pipeline = Pipeline::parse(
        "steps:
  - load: data/g.trig
  - publish: out/kg
    graphs: [\"http://bnd/_graph_*\"]
    merge: true
  - publish: out/all
",
        dir.to_path_buf(),
        "p",
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    for _ in 0..2 {
        // The second run replaces the dataset the first wrote.
        let report = run(&pipeline, &engine, &Options::default(), &|_, _| {});
        assert!(report.ok(), "{:?}", report.steps);
        assert_eq!(report.steps[1].triples, Some(2));
        assert_eq!(report.steps[2].triples, Some(4));
    }
    let published = Engine::open_read_only(dir.join("out/kg")).unwrap();
    assert_eq!(published.store().len().unwrap(), 2);
    assert_eq!(
        count(&published, "SELECT (COUNT(*) AS ?n) { ?s <http://e/p> ?o }"),
        2
    );
    drop(published);
    let all = Engine::open_read_only(dir.join("out/all")).unwrap();
    assert_eq!(all.store().len().unwrap(), 4);
    drop(all);
    let leftovers: Vec<_> = std::fs::read_dir(dir.join("out"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers.len(), 2, "{leftovers:?}");

    // A folder that holds files but is not a dataset is not replaced: its files stay.
    let refused = |yaml: &str, options: &Options| {
        let p = Pipeline::parse(yaml, dir.to_path_buf(), "p").unwrap();
        let report = run(&p, &Engine::new_tripletrail(), options, &|_, _| {});
        assert!(!report.ok());
        report.steps.last().unwrap().error.clone().unwrap()
    };
    let error = refused("steps:\n  - publish: data\n", &Options::default());
    assert!(error.contains("not a dataset"), "{error}");
    assert!(dir.join("data/g.trig").exists());
    // Nor the store the pipeline runs on, or a folder holding it.
    let on_store = Options {
        store: Some(dir.join("st/ds")),
        ..Options::default()
    };
    for target in ["st/ds", "st"] {
        let error = refused(&format!("steps:\n  - publish: {target}\n"), &on_store);
        assert!(error.contains("runs on the store"), "{error}");
    }
}

#[test]
fn a_reason_step_writes_rdfs_entailment_to_its_graph() {
    let dir = scratch("reason");
    std::fs::write(
        dir.join("onto.ttl"),
        "@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
         <http://e/Station> rdfs:subClassOf <http://e/Place> .
         <http://e/km> rdfs:domain <http://e/Located> .
         <http://e/OSL> a <http://e/Station> .",
    )
    .unwrap();
    let yaml = |graph: &str| {
        format!(
            "steps:
  - load: onto.ttl
  - load: codes.ttl
    graph: http://e/codes
  - reason: rdfs{graph}
"
        )
    };
    let engine = Engine::new_tripletrail();
    let pipeline = Pipeline::parse(&yaml(""), dir.to_path_buf(), "p").unwrap();
    let report = run(&pipeline, &engine, &Options::default(), &|_, _| {});
    assert!(report.ok(), "{:?}", report.steps);
    // OSL a Place; OSL and LLS a Located (from the km in the codes graph).
    assert_eq!(report.to_json()["steps"][2]["triples"], 3);
    assert_eq!(
        count(
            &engine,
            "SELECT (COUNT(*) AS ?n) { GRAPH <urn:tripletrail:inferred> { ?s a ?c } }"
        ),
        3
    );
    let pipeline = Pipeline::parse(
        &yaml("\n    graph: http://e/entailed"),
        dir.to_path_buf(),
        "p",
    )
    .unwrap();
    let report = run(&pipeline, &engine, &Options::default(), &|_, _| {});
    assert!(report.ok(), "{:?}", report.steps);
    // The other step graph is data now: what it holds is not entailed again.
    assert_eq!(
        count(
            &engine,
            "SELECT (COUNT(*) AS ?n) { GRAPH <http://e/entailed> { ?s ?p ?o } }"
        ),
        0
    );
}

/// Lift steps read URLs only those `--allow-url` allows, as `tripletrail lift` does.
#[test]
fn lift_steps_read_urls_on_the_allowlist() {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let body = r#"[{"code":"BRG","name":"Bergen"}]"#;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let dir = scratch("urls");
    std::fs::write(
        dir.join("lift/stations.rq"),
        format!(
            "PREFIX xyz: <http://sparql.xyz/facade-x/data/>
             CONSTRUCT {{ <http://e/q> <http://e/name> ?name }} WHERE {{
               SERVICE <x-sparql-anything:location={base}/stations.json> {{ ?s xyz:name ?name }} }}"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("urls.yaml"),
        "steps:\n  - lift: lift/\n  - export: out/kg.nt\n",
    )
    .unwrap();
    let (ok, report, stderr) = tripletrail(&dir, &["urls.yaml"]);
    assert!(!ok, "{report:#}");
    assert!(
        format!("{report}{stderr}").contains("reading URLs is off"),
        "{report:#}\n{stderr}"
    );
    let allow = format!("{base}/");
    let (ok, report, stderr) = tripletrail(&dir, &["urls.yaml", "--allow-url", &allow]);
    assert!(ok, "{report:#}\n{stderr}");
    assert_eq!(
        sorted_lines(&dir.join("out/kg.nt")),
        ["<http://e/q> <http://e/name> \"Bergen\" ."]
    );
}

#[test]
fn a_reason_step_stops_at_its_timeout_and_leaves_its_graph_as_it_was() {
    let dir = scratch("reason-timeout");
    // Each round of someValuesFrom reaches one node further back: 10,000 rounds, each
    // reading the whole chain.
    let mut data = String::from(
        "<http://e/R> <http://www.w3.org/2002/07/owl#onProperty> <http://e/next> .
         <http://e/R> <http://www.w3.org/2002/07/owl#someValuesFrom> <http://e/R> .
         <http://e/n10000> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://e/R> .\n",
    );
    for i in 0..10_000 {
        data.push_str(&format!(
            "<http://e/n{i}> <http://e/next> <http://e/n{}> .\n",
            i + 1
        ));
    }
    std::fs::write(dir.join("chain.nt"), data).unwrap();
    let engine = Engine::new_tripletrail();
    let pipeline = Pipeline::parse(
        "steps:\n  - load: chain.nt\n  - reason: owl-rl\n    timeout: 0.3\n",
        dir.to_path_buf(),
        "p",
    )
    .unwrap();
    let started = std::time::Instant::now();
    let report = run(&pipeline, &engine, &Options::default(), &|_, _| {});
    assert!(started.elapsed().as_secs() < 5, "{:?}", started.elapsed());
    let steps = report.to_json()["steps"].clone();
    assert_eq!(steps[1]["status"], "failed");
    assert_eq!(
        steps[1]["error"],
        "the step took longer than its timeout of 0.3 s"
    );
    assert_eq!(
        count(
            &engine,
            "SELECT (COUNT(*) AS ?n) { GRAPH <urn:tripletrail:inferred> { ?s ?p ?o } }"
        ),
        0
    );
}

#[test]
fn an_owl_rl_reason_step_counts_contradictions_and_fails_on_them_when_asked() {
    let dir = scratch("reason-inconsistent");
    std::fs::write(
        dir.join("data.nt"),
        "<http://e/Halt> <http://www.w3.org/2000/01/rdf-schema#subClassOf> <http://e/Station> .
         <http://e/Station> <http://www.w3.org/2002/07/owl#disjointWith> <http://e/Track> .
         <http://e/h1> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://e/Halt> .
         <http://e/h1> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://e/Track> .\n",
    )
    .unwrap();
    let parse = |yaml: &str| Pipeline::parse(yaml, dir.to_path_buf(), "p");
    let base = "steps:\n  - load: data.nt\n  - reason: owl-rl\n";
    // Like Jena: the entailment is written, and the step reports what contradicts it.
    let engine = Engine::new_tripletrail();
    let report = run(
        &parse(base).unwrap(),
        &engine,
        &Options::default(),
        &|_, _| {},
    );
    let steps = report.to_json()["steps"].clone();
    assert_eq!(steps[1]["status"], "ok", "{steps}");
    assert_eq!(steps[1]["results"]["inconsistencies"], 1);
    // Asked to, it fails, with the entailment still written.
    let engine = Engine::new_tripletrail();
    let failing = format!("{base}    fail_on_inconsistency: true\n");
    let report = run(
        &parse(&failing).unwrap(),
        &engine,
        &Options::default(),
        &|_, _| {},
    );
    let steps = report.to_json()["steps"].clone();
    assert_eq!(steps[1]["status"], "failed");
    assert_eq!(
        steps[1]["error"],
        "the data contradicts the OWL 2 RL rules 1 times, first cax-dw: <http://e/h1> \
          <http://e/Station> <http://e/Track>"
    );
    assert!(
        count(
            &engine,
            "SELECT (COUNT(*) AS ?n) { GRAPH <urn:tripletrail:inferred> { ?s ?p ?o } }"
        ) > 0
    );
    // RDFS has no contradictions to fail on.
    let rdfs = "steps:\n  - load: data.nt\n  - reason: rdfs\n    fail_on_inconsistency: true\n";
    assert!(
        parse(rdfs)
            .unwrap_err()
            .contains("RDFS has no inconsistency")
    );
}
