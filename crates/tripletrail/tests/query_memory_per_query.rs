//! A memory limit for one engine's queries (#189), below the process's: a query whose hash
//! join holds more rows than it allows fails, while the same query on the engine it was made
//! from runs. A test file of its own: it relies on the process having no limit of its own, so
//! no test here sets one.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::{CancellationToken, QueryResults};
use tripletrail::Engine;

/// Subqueries joined on ?k (7 values over 140 subjects): hash joins in the evaluator.
const QUERY: &str = "SELECT (COUNT(*) AS ?n) { { SELECT ?k ?a { ?a <urn:r> ?k } } { SELECT ?k ?b { ?b <urn:r> ?k } } { SELECT ?k ?c { ?c <urn:r> ?k } } FILTER(STR(?a) != STR(?c)) }";
const EXPECTED: &str = "\"53200\"^^<http://www.w3.org/2001/XMLSchema#integer>";

fn count(engine: &Engine) -> Result<String, String> {
    let QueryResults::Solutions(mut s) = engine.query(QUERY).map_err(|e| e.to_string())? else {
        panic!()
    };
    let row = s.next().unwrap().map_err(|e| e.to_string())?;
    Ok(row.get("n").unwrap().to_string())
}

#[test]
fn one_engines_queries_get_their_own_memory_limit() {
    let d: String = (0..140)
        .map(|i| format!("<urn:s{i}> <urn:r> <urn:k{}> .\n", i % 7))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let limited = engine.with_query_memory(64 << 10);
    let error = count(&limited).unwrap_err();
    assert!(error.contains("more memory than its limit"), "{error}");
    // The engine it was made from has no limit; a larger one lets the query through.
    assert_eq!(count(&engine).unwrap(), EXPECTED);
    assert_eq!(
        count(&engine.with_query_memory(64 << 20)).unwrap(),
        EXPECTED
    );
    // A timeout's token set afterwards keeps the limit.
    let timed = limited.with_cancellation_token(CancellationToken::new());
    assert!(count(&timed).is_err());
    // The lower of an engine's and the process's limit applies: in evaluator_memory.rs,
    // where the tests may set the process's limit.
}

/// Joins on term ids under one engine's limit are evaluated row by row, with the same answer
/// (measured on 200 subjects: 412 MB peak without the limit, 104 MB with 64 MiB).
#[test]
fn one_engines_id_joins_keep_to_its_limit() {
    let d: String = (0..60)
        .map(|i| format!("<urn:s{i}> <urn:p> {i} .\n"))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let q = "SELECT (COUNT(*) AS ?n) { ?a <urn:p> ?x . ?b <urn:p> ?y . ?c <urn:p> ?z FILTER(?x + ?y + ?z < 50) }";
    let n = |e: &Engine| {
        let QueryResults::Solutions(mut s) = e.query(q).unwrap() else {
            panic!()
        };
        s.next().unwrap().unwrap().get("n").unwrap().to_string()
    };
    let expected = "\"22100\"^^<http://www.w3.org/2001/XMLSchema#integer>";
    assert_eq!(n(&engine), expected);
    // 32 KiB: the evaluator's two hash joins of 60 rows hold about 20 KB together (#874),
    // while the id join would take megabytes.
    assert_eq!(n(&engine.with_query_memory(32 << 10)), expected);
}

/// An update whose `WHERE` runs past the limit fails and changes nothing (#189).
#[test]
fn an_update_past_the_limit_changes_nothing() {
    let d: String = (0..140)
        .map(|i| format!("<urn:s{i}> <urn:r> <urn:k{}> .\n", i % 7))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let update = "INSERT { ?a <urn:same> ?c } WHERE { { SELECT ?k ?a { ?a <urn:r> ?k } } { SELECT ?k ?b { ?b <urn:r> ?k } } { SELECT ?k ?c { ?c <urn:r> ?k } } }";
    let error = engine
        .with_query_memory(64 << 10)
        .update(update)
        .unwrap_err()
        .to_string();
    assert!(error.contains("more memory than its limit"), "{error}");
    let QueryResults::Solutions(mut s) = engine
        .query("SELECT (COUNT(*) AS ?n) { ?s ?p ?o }")
        .unwrap()
    else {
        panic!()
    };
    let n = s.next().unwrap().unwrap().get("n").unwrap().to_string();
    assert_eq!(n, "\"140\"^^<http://www.w3.org/2001/XMLSchema#integer>");
}

/// The rows ORDER BY sorts, the rows DISTINCT has seen and the groups of GROUP BY count
/// against the limit too (#189): past it the query fails, below it the answer is as before.
#[test]
fn sorted_distinct_and_grouped_rows_keep_to_the_limit() {
    let d: String = (0..5000)
        .map(|i| format!("<urn:s{i}> <urn:p> \"v{i}\" .\n"))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let rows = |e: &Engine, q: &str| -> Result<usize, String> {
        let QueryResults::Solutions(s) = e.query(q).map_err(|e| e.to_string())? else {
            panic!()
        };
        s.map(|r| r.map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()
            .map(|v| v.len())
    };
    for q in [
        "SELECT ?s ?k { ?s <urn:p> ?o BIND(CONCAT(?o, \"x\") AS ?k) } ORDER BY ?k",
        "SELECT DISTINCT ?k { ?s <urn:p> ?o BIND(CONCAT(?o, \"x\") AS ?k) }",
        "SELECT ?k (COUNT(*) AS ?n) { ?s <urn:p> ?o BIND(CONCAT(?o, \"x\") AS ?k) } GROUP BY ?k",
    ] {
        assert_eq!(rows(&engine, q), Ok(5000), "{q}");
        let error = rows(&engine.with_query_memory(64 << 10), q).unwrap_err();
        assert!(error.contains("more memory than its limit"), "{q}: {error}");
        assert_eq!(
            rows(&engine.with_query_memory(64 << 20), q),
            Ok(5000),
            "{q}"
        );
    }
}

/// The terms a property path's closure reaches count against the limit too (#874).
#[test]
fn path_closures_keep_to_the_limit() {
    // A chain of 300 nodes: 300 × 301 / 2 pairs for `+`.
    let d: String = (0..300)
        .map(|i| format!("<urn:n{i}> <urn:next> <urn:n{}> .\n", i + 1))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let rows = |e: &Engine, q: &str| -> Result<usize, String> {
        let QueryResults::Solutions(s) = e.query(q).map_err(|e| e.to_string())? else {
            panic!()
        };
        s.map(|r| r.map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()
            .map(|v| v.len())
    };
    for (q, n) in [
        ("SELECT ?a ?b { ?a <urn:next>+ ?b }", 45150),
        ("SELECT ?b { <urn:n0> <urn:next>* ?b }", 301),
        ("SELECT ?a { ?a <urn:next>+ <urn:n300> }", 300),
    ] {
        assert_eq!(rows(&engine, q), Ok(n), "{q}");
        let error = rows(&engine.with_query_memory(1 << 10), q).unwrap_err();
        assert!(error.contains("more memory than its limit"), "{q}: {error}");
        assert_eq!(rows(&engine.with_query_memory(64 << 20), q), Ok(n), "{q}");
    }
    // Path counts answered from the id tables keep to it too (#874).
    let count = |e: &Engine, q: &str| -> Result<String, String> {
        let QueryResults::Solutions(mut s) = e.query(q).map_err(|e| e.to_string())? else {
            panic!()
        };
        let row = s.next().unwrap().map_err(|e| e.to_string())?;
        Ok(row.get("n").unwrap().to_string())
    };
    let d: String = (0..300)
        .map(|i| format!("<urn:start{i}> <urn:first> <urn:n{i}> .\n"))
        .collect();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    for (q, n) in [
        ("SELECT (COUNT(*) AS ?n) { ?a <urn:next>+ ?b }", 45150),
        (
            "SELECT (COUNT(*) AS ?n) { ?a <urn:first>/<urn:next>+ ?b }",
            45150,
        ),
        (
            "SELECT (COUNT(*) AS ?n) { ?s <urn:first> ?a . ?a <urn:next>+ ?b }",
            45150,
        ),
    ] {
        let n = format!("\"{n}\"^^<http://www.w3.org/2001/XMLSchema#integer>");
        assert_eq!(count(&engine, q), Ok(n.clone()), "{q}");
        let error = count(&engine.with_query_memory(1 << 10), q).expect_err(q);
        assert!(error.contains("more memory than its limit"), "{q}: {error}");
        assert_eq!(count(&engine.with_query_memory(64 << 20), q), Ok(n), "{q}");
    }
}

/// GROUP BY answered from the id tables keeps to the limit too (#874): past it, the groups
/// are left to the evaluator, which fails the query.
#[test]
fn groups_on_ids_keep_to_the_limit() {
    let d: String = (0..5000)
        .map(|i| format!("<urn:s{i}> <urn:p> {i} ; <urn:q> <urn:k{}> .\n", i % 7))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let rows = |e: &Engine, q: &str| -> Result<usize, String> {
        let QueryResults::Solutions(s) = e.query(q).map_err(|e| e.to_string())? else {
            panic!()
        };
        s.map(|r| r.map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()
            .map(|v| v.len())
    };
    for (q, n) in [
        ("SELECT ?s (COUNT(*) AS ?n) { ?s ?p ?o } GROUP BY ?s", 5000),
        (
            "SELECT ?o (COUNT(?s) AS ?n) { ?s <urn:p> ?o } GROUP BY ?o",
            5000,
        ),
        (
            "SELECT ?s (COUNT(*) AS ?n) { ?s <urn:p> ?v ; <urn:q> ?k } GROUP BY ?s",
            5000,
        ),
        (
            "SELECT ?n (COUNT(*) AS ?c) { { ?n ?p ?o } UNION { ?x ?p ?n } } GROUP BY ?n",
            10007,
        ),
    ] {
        assert_eq!(rows(&engine, q), Ok(n), "{q}");
        let error = rows(&engine.with_query_memory(64 << 10), q).unwrap_err();
        assert!(error.contains("more memory than its limit"), "{q}: {error}");
        assert_eq!(rows(&engine.with_query_memory(64 << 20), q), Ok(n), "{q}");
    }
    // A few groups fit a small limit, on ids.
    let q = "SELECT ?k (COUNT(*) AS ?n) { ?s <urn:q> ?k } GROUP BY ?k";
    assert_eq!(rows(&engine.with_query_memory(64 << 10), q), Ok(7));
}

/// The values an aggregate keeps count against the limit (#874): the text of `GROUP_CONCAT`
/// and the values `COUNT(DISTINCT …)` has seen, in one group, where the groups themselves
/// take almost nothing.
#[test]
fn aggregate_values_keep_to_the_limit() {
    let d: String = (0..5000)
        .map(|i| format!("<urn:s{i}> <urn:p> \"value number {i}\" .\n"))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let first = |e: &Engine, q: &str| -> Result<String, String> {
        let QueryResults::Solutions(mut s) = e.query(q).map_err(|e| e.to_string())? else {
            panic!()
        };
        let row = s.next().unwrap().map_err(|e| e.to_string())?;
        Ok(row.get("n").unwrap().to_string())
    };
    for q in [
        "SELECT (GROUP_CONCAT(?o) AS ?n) { ?s <urn:p> ?o }",
        "SELECT (COUNT(DISTINCT ?o) AS ?n) { ?s <urn:p> ?o }",
        "SELECT (COUNT(DISTINCT STR(?o)) AS ?n) { ?s <urn:p> ?o }",
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> SELECT (AVG(DISTINCT xsd:integer(STRAFTER(?o, \"number \"))) AS ?n) { ?s <urn:p> ?o }",
        "SELECT (COUNT(DISTINCT *) AS ?n) { ?s <urn:p> ?o BIND(STR(?o) AS ?x) }",
    ] {
        let all = first(&engine, q).unwrap();
        let error = first(&engine.with_query_memory(32 << 10), q).expect_err(q);
        assert!(error.contains("more memory than its limit"), "{q}: {error}");
        assert_eq!(
            first(&engine.with_query_memory(64 << 20), q),
            Ok(all),
            "{q}"
        );
    }
    // A few values fit a small limit.
    let q = "SELECT (GROUP_CONCAT(?o) AS ?n) { ?s <urn:p> ?o FILTER(?s = <urn:s7>) }";
    assert_eq!(
        first(&engine.with_query_memory(32 << 10), q),
        Ok("\"value number 7\"".into())
    );
}

/// The operators of one query count against the limit together (#874): a DISTINCT under an
/// ORDER BY holds its rows twice, once in each, and fails where either alone fits. Queries
/// on the same engine at once each have their own total.
#[test]
fn a_querys_operators_share_one_total() {
    let d: String = (0..1000)
        .map(|i| format!("<urn:s{i}> <urn:p> \"v{i}\" .\n"))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let rows = |e: &Engine, q: &str| -> Result<usize, String> {
        let QueryResults::Solutions(s) = e.query(q).map_err(|e| e.to_string())? else {
            panic!()
        };
        s.map(|r| r.map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()
            .map(|v| v.len())
    };
    // Each about 136 KB of held rows, both together about 272 KB.
    let sorted = "SELECT ?s ?x { ?s <urn:p> ?o BIND(STR(?o) AS ?x) } ORDER BY ?x";
    let distinct = "SELECT DISTINCT ?s ?x { ?s <urn:p> ?o BIND(STR(?o) AS ?x) }";
    let both = "SELECT DISTINCT ?s ?x { ?s <urn:p> ?o BIND(STR(?o) AS ?x) } ORDER BY ?x";
    let limited = engine.with_query_memory(200_000);
    assert_eq!(rows(&limited, sorted), Ok(1000));
    assert_eq!(rows(&limited, distinct), Ok(1000));
    let error = rows(&limited, both).unwrap_err();
    assert!(error.contains("more memory than its limit"), "{error}");
    assert_eq!(rows(&engine.with_query_memory(64 << 20), both), Ok(1000));
    // Four at once on one engine, each within the limit alone.
    std::thread::scope(|s| {
        let runs: Vec<_> = (0..4).map(|_| s.spawn(|| rows(&limited, sorted))).collect();
        for run in runs {
            assert_eq!(run.join().unwrap(), Ok(1000));
        }
    });
}

/// The text `GROUP_CONCAT` builds per group on ids (#11) counts against the limit too: all
/// the groups together, where the table of ids they come from fits.
#[test]
fn group_concat_on_ids_keeps_to_the_limit() {
    let pad = "x".repeat(100);
    let d: String = (0..5000)
        .map(|i| format!("<urn:s{i}> <urn:p> \"{pad} {i}\" ; <urn:k> {} .\n", i % 50))
        .collect();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let count = |e: &Engine, q: &str| -> Result<usize, String> {
        let QueryResults::Solutions(s) = e.query(q).map_err(|e| e.to_string())? else {
            panic!("not a SELECT")
        };
        let mut n = 0;
        for row in s {
            row.map_err(|e| e.to_string())?;
            n += 1;
        }
        Ok(n)
    };
    let q = "SELECT ?k (GROUP_CONCAT(?o) AS ?g) { ?s <urn:p> ?o ; <urn:k> ?k } GROUP BY ?k";
    for hints in ["", "#pragma tt.executor ids\n"] {
        let q = format!("{hints}{q}");
        let error = count(&engine.with_query_memory(256 << 10), &q).expect_err(&q);
        assert!(error.contains("more memory than its limit"), "{q}: {error}");
        assert_eq!(
            count(&engine.with_query_memory(64 << 20), &q),
            Ok(50),
            "{q}"
        );
    }
}
