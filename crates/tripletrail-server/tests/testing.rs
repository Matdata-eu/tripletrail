//! Workbench tests (#403): folders with an input, a target and the expected output, as
//! `tripletrail test` and `/$/workspaces/{id}/tests` run them.

use std::path::Path;
use tripletrail_server::testing::{self, Limits, Outcome};

fn temp_dir(name: &str) -> tripletrail_testdir::TestDir {
    tripletrail_testdir::tempdir(&format!("tests-{name}"))
}

fn write(dir: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

const ANY: Limits<'static> = Limits {
    root: None,
    timeout: None,
    record: false,
    max_bytes: None,
    max_input: None,
    max_rows: None,
};

fn run_all(dir: &Path, limits: Limits<'_>) -> Vec<Outcome> {
    let base = dir.join("tests");
    testing::discover(dir)
        .iter()
        .map(|t| testing::run(t, &base, limits))
        .collect()
}

fn by_name<'a>(outcomes: &'a [Outcome], name: &str) -> &'a Outcome {
    outcomes.iter().find(|o| o.name == name).unwrap()
}

const INPUT: &str = "@prefix ex: <http://example.com/> .\n\
    ex:a ex:p 1 . ex:b ex:p 2 . ex:c ex:p 3 .\n";
const INT: &str = "http://www.w3.org/2001/XMLSchema#integer";

fn rows_srj(rows: &[(&str, i32)]) -> String {
    let bindings: Vec<String> = rows
        .iter()
        .map(|(s, o)| {
            format!(
                r#"{{"s":{{"type":"uri","value":"http://example.com/{s}"}},"o":{{"type":"literal","value":"{o}","datatype":"{INT}"}}}}"#
            )
        })
        .collect();
    format!(
        r#"{{"head":{{"vars":["s","o"]}},"results":{{"bindings":[{}]}}}}"#,
        bindings.join(",")
    )
}

const SELECT: &str = "PREFIX ex: <http://example.com/> SELECT ?s ?o { ?s ex:p ?o } ORDER BY ?o";
const RULES: &str = r#"@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.com/> .
ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:p ;
  sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate ex:q ; sh:object "x" ] .
"#;
const SHAPES: &str = r#"@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.com/> .
ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:p ;
  sh:property [ sh:path ex:p ; sh:maxInclusive 2 ] .
"#;

#[test]
fn each_kind_of_test_passes_and_fails() {
    let dir = temp_dir("kinds");
    write(
        &dir,
        &[
            ("rules/r.ttl", RULES),
            // A SELECT: rows in any order, unless `ordered`.
            ("tests/select/input.ttl", INPUT),
            ("tests/select/query.rq", SELECT),
            (
                "tests/select/expected.srj",
                &rows_srj(&[("c", 3), ("a", 1), ("b", 2)]),
            ),
            ("tests/ordered/input.ttl", INPUT),
            ("tests/ordered/query.rq", SELECT),
            ("tests/ordered/test.yaml", "ordered: true\n"),
            (
                "tests/ordered/expected.srj",
                &rows_srj(&[("c", 3), ("a", 1), ("b", 2)]),
            ),
            ("tests/csv/input.ttl", INPUT),
            ("tests/csv/query.rq", SELECT),
            (
                "tests/csv/expected.csv",
                "s,o\r\nhttp://example.com/b,2\r\nhttp://example.com/a,1\r\nhttp://example.com/c,3\r\n",
            ),
            // A CONSTRUCT with blank nodes: up to isomorphism.
            ("tests/construct/input.ttl", INPUT),
            (
                "tests/construct/query.rq",
                "PREFIX ex: <http://example.com/> CONSTRUCT { [] ex:of ?s } { ?s ex:p ?o FILTER(?o > 1) }",
            ),
            (
                "tests/construct/expected.ttl",
                "@prefix ex: <http://example.com/> . [] ex:of ex:c . [] ex:of ex:b .",
            ),
            ("tests/construct-wrong/input.ttl", INPUT),
            (
                "tests/construct-wrong/query.rq",
                "PREFIX ex: <http://example.com/> CONSTRUCT { [] ex:of ?s } { ?s ex:p ?o FILTER(?o > 1) }",
            ),
            (
                "tests/construct-wrong/expected.ttl",
                "@prefix ex: <http://example.com/> . [] ex:of ex:a . [] ex:of ex:b .",
            ),
            // Updates, in name order: the store afterwards.
            ("tests/update/input.ttl", INPUT),
            (
                "tests/update/updates/1.ru",
                "PREFIX ex: <http://example.com/> DELETE WHERE { ex:a ?p ?o }",
            ),
            (
                "tests/update/updates/2.ru",
                "PREFIX ex: <http://example.com/> INSERT DATA { GRAPH ex:g { ex:z ex:p 9 } }",
            ),
            (
                "tests/update/expected.trig",
                "@prefix ex: <http://example.com/> . ex:b ex:p 2 . ex:c ex:p 3 . ex:g { ex:z ex:p 9 }",
            ),
            // The workspace's rules: what they infer.
            ("tests/rules/input.ttl", INPUT),
            ("tests/rules/test.yaml", "rules: ../../rules\n"),
            (
                "tests/rules/expected.ttl",
                "@prefix ex: <http://example.com/> . ex:a ex:q \"x\" . ex:b ex:q \"x\" . ex:c ex:q \"x\" .",
            ),
            // Shapes: a count, a list (prefixed names, short component names), or none.
            ("tests/count/input.ttl", INPUT),
            ("tests/count/shapes.ttl", SHAPES),
            ("tests/count/test.yaml", "violations: 1\n"),
            ("tests/list/input.ttl", INPUT),
            ("tests/list/shapes.ttl", SHAPES),
            (
                "tests/list/test.yaml",
                "violations:\n  - focus: ex:c\n    shape: ex:S\n    component: MaxInclusive\n    path: ex:p\n    severity: Violation\n",
            ),
            ("tests/conforms/input.ttl", INPUT),
            ("tests/conforms/shapes.ttl", SHAPES),
            ("tests/list-wrong/input.ttl", INPUT),
            ("tests/list-wrong/shapes.ttl", SHAPES),
            (
                "tests/list-wrong/test.yaml",
                "violations:\n  - focus: ex:b\n",
            ),
            // Rules then shapes: the shapes see what the rules infer.
            ("tests/rules-shapes/input.ttl", INPUT),
            (
                "tests/rules-shapes/shapes.ttl",
                "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://example.com/> .\n\
                 ex:Q a sh:NodeShape ; sh:targetSubjectsOf ex:q ; sh:property [ sh:path ex:q ; sh:maxCount 0 ] .",
            ),
            (
                "tests/rules-shapes/test.yaml",
                "rules: ../../rules\nviolations: 3\n",
            ),
            // Nested, and broken.
            ("tests/deep/one/input.ttl", INPUT),
            ("tests/deep/one/query.rq", "ASK { ?s ?p 3 }"),
            (
                "tests/deep/one/expected.srj",
                r#"{"head":{},"boolean":true}"#,
            ),
            ("tests/syntax/query.rq", "SELECT * { ?s ?p }"),
            ("tests/syntax/expected.srj", "{}"),
            ("tests/no-target/expected.ttl", ""),
        ],
    );
    let outcomes = run_all(&dir, ANY);
    let names: Vec<&str> = outcomes.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "conforms",
            "construct",
            "construct-wrong",
            "count",
            "csv",
            "deep/one",
            "list",
            "list-wrong",
            "no-target",
            "ordered",
            "rules",
            "rules-shapes",
            "select",
            "syntax",
            "update"
        ]
    );
    for name in [
        "construct",
        "count",
        "csv",
        "deep/one",
        "list",
        "rules",
        "rules-shapes",
        "select",
        "update",
    ] {
        let o = by_name(&outcomes, name);
        assert!(o.passed, "{}", o.report());
    }
    let o = by_name(&outcomes, "construct-wrong");
    assert!(!o.passed);
    assert_eq!((o.missing_count, o.unexpected_count), (1, 1));
    assert!(o.missing[0].ends_with("<http://example.com/of> <http://example.com/a>"));
    let o = by_name(&outcomes, "ordered");
    assert!(!o.passed);
    assert_eq!(o.missing.len(), 3, "{}", o.report());
    assert!(o.missing[0].starts_with("row 1: ?s=<http://example.com/c>"));
    let o = by_name(&outcomes, "conforms");
    assert_eq!((o.passed, o.unexpected_count), (false, 1));
    let o = by_name(&outcomes, "list-wrong");
    assert_eq!((o.missing_count, o.unexpected_count), (1, 1));
    assert!(o.report().contains("+ component: <http://www.w3.org/ns/shacl#MaxInclusiveConstraintComponent>, focus: <http://example.com/c>"));
    assert!(
        by_name(&outcomes, "syntax")
            .error
            .as_deref()
            .unwrap()
            .contains("error at 1:")
    );
    assert!(
        by_name(&outcomes, "no-target")
            .error
            .as_deref()
            .unwrap()
            .starts_with("no target")
    );

    // JUnit: one testcase each, failures with their report.
    let xml = testing::junit(&outcomes);
    assert!(xml.contains(r#"<testsuite name="tripletrail" tests="15" failures="6""#));
    assert!(xml.contains(r#"<testcase classname="query" name="select""#));
    assert!(xml.contains("<failure message=\"1 missing, 1 unexpected\">"));

    // One test folder on its own.
    let one = testing::discover(&dir.join("tests/select"));
    assert_eq!(one, [dir.join("tests/select")]);
}

#[test]
fn record_writes_the_expected_output() {
    let dir = temp_dir("record");
    write(
        &dir,
        &[
            ("rules/r.ttl", RULES),
            ("tests/select/input.ttl", INPUT),
            ("tests/select/query.rq", SELECT),
            ("tests/construct/input.ttl", INPUT),
            (
                "tests/construct/query.rq",
                "PREFIX ex: <http://example.com/> CONSTRUCT { [] ex:of ?s } { ?s ex:p ?o }",
            ),
            // Recording replaces the expected output there is.
            ("tests/rules/input.ttl", INPUT),
            ("tests/rules/test.yaml", "rules: ../../rules\n"),
            (
                "tests/rules/expected.ttl",
                "<urn:old> <urn:old> <urn:old> .",
            ),
            ("tests/shapes/input.ttl", INPUT),
            ("tests/shapes/shapes.ttl", SHAPES),
            ("tests/fixed/input.ttl", INPUT),
            ("tests/fixed/shapes.ttl", SHAPES),
            ("tests/fixed/test.yaml", "violations: 0\n"),
        ],
    );
    let record = Limits {
        record: true,
        ..ANY
    };
    let outcomes = run_all(&dir, record);
    for name in ["select", "construct", "rules", "shapes"] {
        assert!(by_name(&outcomes, name).passed, "{name}");
    }
    // Not over an expected output test.yaml gives.
    assert!(
        by_name(&outcomes, "fixed")
            .error
            .as_deref()
            .unwrap()
            .contains("test.yaml sets the expected output")
    );
    let tests = dir.join("tests");
    assert!(tests.join("select/expected.srj").is_file());
    assert!(tests.join("construct/expected.nt").is_file());
    assert!(!tests.join("rules/expected.ttl").exists());
    assert_eq!(
        std::fs::read_to_string(tests.join("rules/expected.nt"))
            .unwrap()
            .lines()
            .count(),
        3
    );
    let violations = std::fs::read_to_string(tests.join("shapes/violations.yaml")).unwrap();
    assert!(
        violations.contains("- component: \"<http://www.w3.org/ns/shacl#MaxInclusiveConstraintComponent>\"\n  focus: \"<http://example.com/c>\""),
        "{violations}"
    );
    // What was recorded passes, and fails once the data changes.
    std::fs::remove_dir_all(tests.join("fixed")).unwrap();
    assert!(run_all(&dir, ANY).iter().all(|o| o.passed));
    for t in ["select", "construct", "rules", "shapes"] {
        write(
            &tests.join(t),
            &[(
                "input.ttl",
                "<http://example.com/a> <http://example.com/p> 3 .",
            )],
        );
    }
    let outcomes = run_all(&dir, ANY);
    for o in &outcomes {
        assert!(!o.passed, "{}", o.name);
    }
    // A limit on what may be written.
    let small = Limits {
        record: true,
        max_bytes: Some(10),
        ..ANY
    };
    let o = testing::run(&tests.join("select"), &tests, small);
    assert!(o.error.unwrap().contains("more than the 10"));
}

#[test]
fn paths_stay_inside_the_root() {
    let dir = temp_dir("root");
    let outside = temp_dir("root-outside");
    write(&outside, &[("rules.ttl", RULES)]);
    write(
        &dir,
        &[
            ("rules/r.ttl", RULES),
            ("tests/in/input.ttl", INPUT),
            ("tests/in/test.yaml", "rules: ../../rules\n"),
            ("tests/in/expected.nt", ""),
            ("tests/out/input.ttl", INPUT),
            (
                "tests/out/test.yaml",
                &format!(
                    "rules: ../../../{}/rules.ttl\n",
                    outside.file_name().unwrap().to_string_lossy()
                ),
            ),
            ("tests/out/expected.nt", ""),
            ("tests/missing/input.ttl", INPUT),
            (
                "tests/missing/test.yaml",
                "rules: ../../../../../../../../no/such/file\n",
            ),
            ("tests/missing/expected.nt", ""),
        ],
    );
    let limits = Limits {
        root: Some(&dir),
        ..ANY
    };
    let outcomes = run_all(&dir, limits);
    assert!(by_name(&outcomes, "in").error.is_none());
    for name in ["out", "missing"] {
        assert_eq!(
            by_name(&outcomes, name).error.as_deref(),
            Some(
                format!(
                    "{} is outside the workspace",
                    if name == "out" {
                        format!(
                            "../../../{}/rules.ttl",
                            outside.file_name().unwrap().to_string_lossy()
                        )
                    } else {
                        "../../../../../../../../no/such/file".to_owned()
                    }
                )
                .as_str()
            )
        );
    }
    // Without a root (`tripletrail test`), anywhere.
    let outcomes = run_all(&dir, ANY);
    assert!(by_name(&outcomes, "out").error.is_none());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(bytes).unwrap();
    gz.finish().unwrap()
}

#[test]
fn tests_stay_within_their_limits() {
    let dir = temp_dir("limits");
    // 8 MB of blank lines unpack from a few KB.
    let bomb = gzip(&vec![b'\n'; 8 << 20]);
    assert!(bomb.len() < 1 << 20);
    std::fs::create_dir_all(dir.join("tests/bomb")).unwrap();
    std::fs::write(dir.join("tests/bomb/input.nt.gz"), &bomb).unwrap();
    let mut many = String::new();
    for i in 0..30 {
        many.push_str(&format!("<http://e/{i}> <http://e/p> {i} .\n"));
    }
    write(
        &dir,
        &[
            ("tests/bomb/test.yaml", "input: input.nt.gz\n"),
            ("tests/bomb/query.rq", "ASK {}"),
            ("tests/bomb/expected.srj", r#"{"head":{},"boolean":true}"#),
            ("tests/rows/input.ttl", &many),
            ("tests/rows/query.rq", "SELECT * { ?s ?p ?o }"),
            (
                "tests/rows/expected.srj",
                r#"{"head":{"vars":["s","p","o"]},"results":{"bindings":[]}}"#,
            ),
            ("tests/triples/input.ttl", &many),
            ("tests/triples/query.rq", "CONSTRUCT WHERE { ?s ?p ?o }"),
            ("tests/triples/expected.nt", ""),
            // A chain of 3000 nodes: its closure has 4.5 million pairs, each step a store lookup
            // (where cancellation is checked), without holding many rows.
            ("tests/slow/input.nt", &chain()),
            (
                "tests/slow/query.rq",
                "SELECT (COUNT(*) AS ?n) { ?a <http://e/next>* ?b }",
            ),
            (
                "tests/slow/expected.srj",
                r#"{"head":{"vars":["n"]},"results":{"bindings":[]}}"#,
            ),
        ],
    );
    let limits = Limits {
        max_input: Some(1 << 20),
        max_rows: Some(10),
        timeout: Some(std::time::Duration::from_millis(300)),
        ..ANY
    };
    let started = std::time::Instant::now();
    let outcomes = run_all(&dir, limits);
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    let error = |name: &str| by_name(&outcomes, name).error.clone().unwrap_or_default();
    assert!(
        error("bomb").contains("more than 1048576 bytes (unpacked)"),
        "{}",
        error("bomb")
    );
    assert!(
        error("rows").contains("more than 10 rows"),
        "{}",
        error("rows")
    );
    assert!(
        error("triples").contains("more than 10 rows"),
        "{}",
        error("triples")
    );
    assert_eq!(error("slow"), "the test took longer than its timeout");
    // Recording stops once the output is too large to keep.
    let record = Limits {
        record: true,
        max_bytes: Some(200),
        ..ANY
    };
    let o = testing::run(&dir.join("tests/rows"), &dir.join("tests"), record);
    assert!(o.error.unwrap().contains("more than the 200 bytes"));
    // Without limits (the CLI), the bomb's blank lines load.
    let o = testing::run(&dir.join("tests/bomb"), &dir.join("tests"), ANY);
    assert!(o.passed, "{}", o.report());
}

fn chain() -> String {
    (0..3000)
        .map(|i| format!("<http://e/n{i}> <http://e/next> <http://e/n{}> .\n", i + 1))
        .collect()
}
