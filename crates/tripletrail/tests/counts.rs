//! `COUNT` over `?s ?p ?o` answered from the counts kept with the quad indexes (issue #166)
//! gives the same numbers as counting the rows, before and after writes, in memory and after
//! reopening a store from disk.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

fn values(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = engine.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    solutions
        .map(|s| {
            s.unwrap()
                .iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn data() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..2500 {
        let object = match i % 9 {
            0 => format!("ex:o{}", i % 50),
            1 => format!("\"label {}\"", i % 300),
            2 => format!("\"label {}\"@en", i % 200),
            3 => format!("{}", i % 70),
            4 => "true".to_owned(),
            5 => format!("\"{}.5\"^^xsd:decimal", i % 40),
            6 => format!(
                "\"POINT({} 1)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>",
                i % 30
            ),
            7 => format!("_:b{}", i % 25),
            _ => format!("\"+{}\"^^xsd:integer", i % 10),
        };
        ttl.push_str(&format!("ex:s{} ex:p{} {object} .\n", i % 400, i % 13));
        if i % 5 == 0 {
            ttl.push_str(&format!("_:sub{} ex:q ex:s{} .\n", i % 17, i % 400));
        }
    }
    // Named graphs: not part of the default graph's counts.
    ttl.push_str("GRAPH ex:g1 { ex:s1 ex:p1 \"only in g1\" . ex:n ex:pn ex:m . }\n");
    ttl
}

/// The query, and the same query in a shape the counts don't answer (an extra `true` in the
/// filter), so it is evaluated row by row.
const QUERIES: &[(&str, &str)] = &[
    ("(COUNT(*) AS ?n)", ""),
    ("(COUNT(?s) AS ?n)", ""),
    ("(COUNT(DISTINCT ?s) AS ?n)", ""),
    ("(COUNT(DISTINCT ?p) AS ?n)", ""),
    ("(COUNT(DISTINCT ?o) AS ?n)", ""),
    ("(COUNT(DISTINCT *) AS ?n)", ""),
    ("(COUNT(?o) AS ?n)", "isLiteral(?o)"),
    ("(COUNT(DISTINCT ?o) AS ?n)", "isLiteral(?o)"),
    ("(COUNT(*) AS ?n)", "isBlank(?o)"),
    ("(COUNT(DISTINCT ?o) AS ?n)", "isBlank(?o)"),
    ("(COUNT(DISTINCT ?s) AS ?n)", "isBlank(?s)"),
    ("(COUNT(DISTINCT ?s) AS ?n)", "isIRI(?s)"),
    ("(COUNT(*) AS ?n) (COUNT(DISTINCT ?o) AS ?d)", "isIRI(?o)"),
    // Not kept by the counts (distinct of another position): counted on ids.
    ("(COUNT(DISTINCT ?s) AS ?n)", "isLiteral(?o)"),
];

fn check(engine: &Engine) {
    for (aggregates, filter) in QUERIES {
        let (fast, slow) = if filter.is_empty() {
            (
                format!("SELECT {aggregates} WHERE {{ ?s ?p ?o }}"),
                format!("SELECT {aggregates} WHERE {{ ?s ?p ?o FILTER(true) }}"),
            )
        } else {
            (
                format!("SELECT {aggregates} WHERE {{ ?s ?p ?o FILTER {filter} }}"),
                format!("SELECT {aggregates} WHERE {{ ?s ?p ?o FILTER({filter} && true) }}"),
            )
        };
        assert_eq!(values(engine, &fast), values(engine, &slow), "{fast}");
        let plan = engine.explain(&fast).unwrap();
        // The evaluation on ids (#167) answers when the stored counts can't.
        assert!(!plan.contains("QuadPattern"), "{fast}: {plan}");
    }
    // Around the counts: projection, BIND and HAVING still apply.
    let q = "SELECT (?n * 2 AS ?twice) WHERE { { SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o } HAVING (COUNT(*) > 1) } }";
    let r = "SELECT (?n * 2 AS ?twice) WHERE { { SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o FILTER(true) } HAVING (COUNT(*) > 1) } }";
    assert_eq!(values(engine, q), values(engine, r));
}

#[test]
fn counts_match_the_rows_in_memory_and_after_writes() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::TriG, data().as_bytes()).unwrap();
    check(&engine);
    // Writes go to the delta: the stored counts are adjusted for it (#213).
    engine
        .update(
            "PREFIX ex: <http://example.org/> INSERT DATA { ex:new ex:p1 \"x\" . _:x ex:p2 ex:new }",
        )
        .unwrap();
    engine
        .update("PREFIX ex: <http://example.org/> DELETE WHERE { ex:s3 ?p ?o }")
        .unwrap();
    check(&engine);
    // An empty store counts 0.
    let empty = Engine::new_tripletrail();
    assert_eq!(
        values(&empty, "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }"),
        vec!["?n=\"0\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
}

/// Counts of basic graph patterns joined on ids in the store (#167), against the same pattern
/// evaluated row by row.
#[test]
fn joined_counts_match_the_rows() {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..3000 {
        ttl.push_str(&format!(
            "ex:a{i} a ex:C{} ; ex:pub ex:p{} ; ex:feat ex:f{} , ex:f{} .\n",
            i % 4,
            i % 37,
            i % 11,
            (i + 3) % 11
        ));
        if i % 3 == 0 {
            ttl.push_str(&format!("ex:a{i} ex:vendor ex:p{} .\n", i % 37));
        }
        if i % 7 == 0 {
            ttl.push_str(&format!("ex:p{} ex:loc _:l{} .\n", i % 37, i % 5));
        }
    }
    ttl.push_str("ex:loop ex:feat ex:loop .\n");
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let patterns = [
        "?s a ?c . ?s ex:pub ?p",
        "?s a ex:C1 . ?s ex:pub ?p",
        "?s a ?c . ?s ex:pub ?p . ?s ex:feat ?f",
        "?s ex:vendor ?p . ?s ex:pub ?p",
        "?s ex:pub ?p . ?p ex:loc ?l",
        "?s ex:vendor ?v . ?v ex:loc ?l . ?s ex:feat ?f",
        "?s ex:pub ?p . ?p ex:loc _:b",
        "?s ex:feat ?s",
        "?s ex:feat ?f . ?t ex:feat ?f . ?t a ex:C2",
        "?s a ex:Missing . ?s ex:pub ?p",
        "?s ex:pub ex:p3 . ?s ex:feat ?f",
    ];
    let aggregates = [
        "(COUNT(*) AS ?n)",
        "(COUNT(?s) AS ?n) (COUNT(DISTINCT ?s) AS ?d)",
        "(COUNT(DISTINCT *) AS ?n)",
    ];
    for p in patterns {
        for a in aggregates {
            let fast = format!("PREFIX ex: <http://example.org/> SELECT {a} WHERE {{ {p} }}");
            let slow =
                format!("PREFIX ex: <http://example.org/> SELECT {a} WHERE {{ {p} FILTER(true) }}");
            assert_eq!(values(&engine, &fast), values(&engine, &slow), "{fast}");
            let plan = engine.explain(&fast).unwrap();
            let blank = p.contains("_:");
            assert_eq!(
                !plan.contains("QuadPattern"),
                !(blank && a.contains("DISTINCT *")),
                "{fast}: {plan}"
            );
        }
    }
}

/// Counts of joins, OPTIONAL, MINUS, EXISTS and UNION evaluated on ids (#167), against the
/// same pattern evaluated row by row.
#[test]
fn tree_counts_match_the_rows() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..300 {
        ttl.push_str(&format!(
            "ex:a{i} a ex:C{} ; ex:pub ex:p{} .\n",
            i % 4,
            i % 37
        ));
        if i % 3 == 0 {
            ttl.push_str(&format!(
                "ex:a{i} ex:feat ex:f{} , ex:f{} .\n",
                i % 11,
                i % 5
            ));
        }
        if i % 5 == 0 {
            ttl.push_str(&format!("ex:p{} a ex:Publisher .\n", i % 37));
        }
        if i % 60 == 0 {
            ttl.push_str(&format!("ex:a{i} ex:rare ex:r{} .\n", i % 120));
        }
        if i % 7 == 0 {
            ttl.push_str(&format!(
                "ex:a{i} ex:val {} , \"{}.0\"^^xsd:decimal .\n",
                i % 9,
                i % 9
            ));
        }
    }
    ttl.push_str("ex:same ex:pub ex:same . ex:same a ex:same .\n");
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    // (pattern, answered on ids)
    let patterns = [
        ("?s a ?c OPTIONAL { ?s ex:feat ?f }", true),
        ("?s a ?c . ?s ex:pub ?p OPTIONAL { ?s ex:feat ?f }", true),
        ("?s ex:pub ?p MINUS { ?p a ex:Publisher }", true),
        ("?s ex:pub ?p MINUS { ?x a ex:Publisher }", true),
        (
            "?s ex:pub ?p . ?s a ?c FILTER EXISTS { ?s ex:feat ?f }",
            true,
        ),
        ("?s ex:pub ?p FILTER NOT EXISTS { ?s ex:feat ?f }", true),
        ("{ ?s a ?o } UNION { ?s ex:pub ?o }", true),
        ("{ ?s a ?o } UNION { ?s ex:pub ?o } FILTER(?s = ?o)", true),
        ("{ ?s a ?o } UNION { ?s ex:feat ?o } ?s ex:pub ?p", true),
        ("{ ?s a ?o } UNION { ?s ex:pub ?x } ?s ex:pub ?p", true),
        (
            "?s ex:pub ?p { ?p a ex:Publisher } UNION { ?p a ex:Other }",
            true,
        ),
        ("?s a ?c OPTIONAL { ?s ex:feat ?f } FILTER isIRI(?f)", true),
        // Counted per shared variable (#185): a chain on the left, a few left values looked up
        // in a large right side, several triples on the right, a union keyed by its variable,
        // blank nodes, a repeated variable, nested operators and MINUS without shared variables.
        ("?s ex:pub ?p . ?p a ?c OPTIONAL { ?c a ?d }", true),
        ("?x ex:pub ?s OPTIONAL { ?s a ?c }", true),
        ("?s ex:rare ?r OPTIONAL { ?s a ?c }", true),
        ("?s ex:rare ?r MINUS { ?s ex:feat ?f }", true),
        ("?s ex:rare ?r FILTER EXISTS { ?s a ?c }", true),
        ("?s ex:rare ?r OPTIONAL { ?s a ?c . ?s ex:feat ?f }", true),
        ("?s a ?c MINUS { ?s ex:pub ?p . ?p a ex:Publisher }", true),
        (
            "?s a ?c FILTER NOT EXISTS { ?s ex:pub ?p . ?p a ex:Publisher }",
            true,
        ),
        ("?s ex:pub ?p FILTER EXISTS { ?s ex:feat [] }", true),
        (
            "{ ?s a ?o } UNION { ?s ex:feat ?o } OPTIONAL { ?s ex:pub ?p }",
            true,
        ),
        ("?s ex:pub ?p OPTIONAL { ?s ex:pub ?s }", true),
        ("?s a ?c OPTIONAL { ?s ex:feat ?f MINUS { ?f a ?x } }", true),
        (
            "?s a ?c OPTIONAL { ?s ex:feat ?f } OPTIONAL { ?s ex:val ?v }",
            true,
        ),
        ("?s ex:pub ?p MINUS { ?x ex:feat ?y }", true),
        ("?s a ?c OPTIONAL { ?s ex:pub ?p . ?p a ?c }", true),
        ("?s ex:val ?a . ?s ex:val ?b FILTER sameTerm(?a, ?b)", true),
        // Different literals may be equal values (1 and 1.0): inline numbers compare by
        // value on their ids (#170).
        ("?s ex:val ?a . ?s ex:val ?b FILTER(?a = ?b)", true),
        // A variable that may be unbound, shared with another pattern: the evaluator decides.
        ("?s a ?c OPTIONAL { ?s ex:feat ?f } ?f ex:pub ?r", false),
        // A filter on the right side's own variables is the right side's (#11).
        (
            "?s a ?c OPTIONAL { ?s ex:feat ?f FILTER(?f != ex:f1) }",
            true,
        ),
        // One that reads a variable of the left side: the evaluator decides.
        (
            "?s a ?c OPTIONAL { ?s ex:feat ?f FILTER(?f != ?c) }",
            false,
        ),
    ];
    let aggregates = [
        "(COUNT(*) AS ?n)",
        "(COUNT(?f) AS ?n) (COUNT(DISTINCT ?s) AS ?d)",
        "(COUNT(DISTINCT *) AS ?n)",
    ];
    for (p, answered) in patterns {
        for a in aggregates {
            let fast = format!("PREFIX ex: <http://example.org/> SELECT {a} WHERE {{ {p} }}");
            let slow = format!(
                "PREFIX ex: <http://example.org/> SELECT {a} WHERE {{ {{ {p} }} FILTER(true) }}"
            );
            assert_eq!(values(&engine, &fast), values(&engine, &slow), "{fast}");
            let plan = engine.explain(&fast).unwrap();
            assert_eq!(!plan.contains("QuadPattern"), answered, "{fast}: {plan}");
        }
    }
}

/// `FILTER(?a = ?b)` between two variables answered as a join on ids (#165): the filter
/// pushed into each side of a `UNION`, the equality taken as one variable in the triples
/// (`?s a ?o FILTER(?s = ?o)` as `?s a ?s`, whose quads a leapfrog of the predicate's
/// subjects and objects finds), before and after writes, with the rows of the same query
/// evaluated one by one.
#[test]
fn equality_filters_match_the_rows() {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..400 {
        ttl.push_str(&format!(
            "ex:a{i} a ex:C{} ; ex:pub ex:a{} .\n",
            i % 6,
            (i * 7) % 400
        ));
        if i % 9 == 0 {
            // Self loops, also among many other quads of the predicate.
            ttl.push_str(&format!("ex:a{i} a ex:a{i} . ex:a{i} ex:pub ex:a{i} .\n"));
        }
        if i % 4 == 0 {
            ttl.push_str(&format!(
                "ex:a{i} ex:val {} , \"{}.0\"^^<http://www.w3.org/2001/XMLSchema#decimal> .\n",
                i % 5,
                i % 5
            ));
        }
    }
    ttl.push_str("ex:C1 a ex:C1 . GRAPH ex:g { ex:g1 a ex:g1 . }\n");
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::TriG, ttl.as_bytes()).unwrap();
    let patterns = [
        "?s a ?o FILTER(?s = ?o)",
        "?s a ?o FILTER(?o = ?s)",
        "?s a ?o FILTER(sameTerm(?s, ?o))",
        "?s a ?o FILTER(?s != ?o)",
        "{ ?s a ?o } UNION { ?s ex:pub ?o } FILTER(?s = ?o)",
        "{ ?s a ?o } UNION { ?s ex:val ?o } FILTER(?s = ?o)",
        "?s a ?o FILTER(?s = ?o && isIRI(?o))",
        "?s a ?o FILTER(isIRI(?s) && ?o = ?s)",
        "?s a ?o . ?s ex:pub ?p FILTER(?s = ?o)",
        "?s ex:pub ?p . ?x a ?c FILTER(?p = ?x)",
        "?s ex:val ?a . ?s ex:val ?b FILTER(?a = ?b)",
        "?s ?p ?o FILTER(?s = ?o)",
    ];
    let check = |stage: &str| {
        for p in patterns {
            for a in ["(COUNT(*) AS ?n)", "(COUNT(DISTINCT ?o) AS ?n)"] {
                let fast = format!("PREFIX ex: <http://example.org/> SELECT {a} WHERE {{ {p} }}");
                let slow = format!(
                    "PREFIX ex: <http://example.org/> SELECT {a} WHERE {{ {{ {p} }} FILTER(true) }}"
                );
                assert_eq!(
                    values(&engine, &fast),
                    values(&engine, &slow),
                    "{stage}: {fast}"
                );
            }
            // Ordered on every variable: rows that tie on some may come in any order.
            let order = "ORDER BY ?s ?o ?p ?x ?c ?a ?b";
            let rows = format!("PREFIX ex: <http://example.org/> SELECT * WHERE {{ {p} }} {order}");
            let slow = format!(
                "PREFIX ex: <http://example.org/> SELECT * WHERE {{ {{ {p} }} FILTER(true) }} {order}"
            );
            assert_eq!(
                values(&engine, &rows),
                values(&engine, &slow),
                "{stage}: {rows}"
            );
        }
    };
    check("loaded");
    let n = |q: &str| {
        values(
            &engine,
            &format!("PREFIX ex: <http://example.org/> SELECT (COUNT(*) AS ?n) WHERE {{ {q} }}"),
        )
    };
    assert_eq!(
        n("?s a ?o FILTER(?s = ?o)"),
        vec!["?n=\"46\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
    // Writes go to the delta, which the leapfrog reads too.
    engine
        .update("PREFIX ex: <http://example.org/> INSERT DATA { ex:new a ex:new . ex:a1 a ex:a1 } ; DELETE DATA { ex:a9 a ex:a9 . ex:C1 a ex:C1 }")
        .unwrap();
    check("after writes");
    assert_eq!(
        n("?s a ?o FILTER(?s = ?o)"),
        vec!["?n=\"46\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
    // The explained plan of the count has no row-by-row scan.
    let plan = engine
        .explain("PREFIX ex: <http://example.org/> SELECT (COUNT(*) AS ?n) WHERE { { ?s a ?o } UNION { ?s ex:pub ?o } FILTER(?s = ?o) }")
        .unwrap();
    assert!(!plan.contains("QuadPattern"), "{plan}");
}

#[test]
fn counts_are_kept_in_the_checkpoint() {
    let dir = tripletrail_testdir::tempdir("counts");
    {
        let engine = Engine::open_tripletrail(&dir).unwrap();
        engine.load(RdfFormat::TriG, data().as_bytes()).unwrap();
        check(&engine);
    }
    // Reopened: the counts come from stats.bin.
    {
        let engine = Engine::open_tripletrail(&dir).unwrap();
        check(&engine);
    }
    // Without stats.bin (a checkpoint of an earlier version), they are counted on first use.
    for entry in walk(&dir) {
        if entry.file_name().is_some_and(|n| n == "stats.bin") {
            std::fs::remove_file(&entry).unwrap();
        }
    }
    {
        let engine = Engine::open_read_only(&dir).unwrap();
        check(&engine);
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let path = e.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// `GROUP BY` with counts, evaluated on term ids, gives the groups and counts of the same
/// query evaluated row by row.
#[test]
fn grouped_counts_match_the_rows() {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..300 {
        ttl.push_str(&format!(
            "ex:a{i} a ex:C{} ; ex:pub ex:p{} .\n",
            i % 4,
            i % 37
        ));
        if i % 3 == 0 {
            ttl.push_str(&format!(
                "ex:a{i} ex:feat ex:f{} , ex:f{} .\n",
                i % 11,
                i % 5
            ));
        }
        if i % 7 == 0 {
            ttl.push_str(&format!("ex:a{i} ex:val {} , \"v{}\"@en .\n", i % 9, i % 9));
        }
    }
    for i in (0..300).step_by(5) {
        ttl.push_str(&format!(
            "ex:a{i} ex:name \"n{}\" , \"é{}\" .\n",
            i % 13,
            i % 4
        ));
    }
    ttl.push_str("ex:b ex:val [ ex:x 1 ] .\n");
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    // (pattern, keys, aggregates, answered on ids)
    let cases = [
        ("?s a ?c", "?c", "(COUNT(*) AS ?n)", true),
        (
            "?s a ?c . ?s ex:pub ?p",
            "?c",
            "(COUNT(?s) AS ?n) (COUNT(DISTINCT ?p) AS ?d)",
            true,
        ),
        ("?s a ?c . ?s ex:pub ?p", "?c ?p", "(COUNT(*) AS ?n)", true),
        (
            "?s a ?c OPTIONAL { ?s ex:feat ?f }",
            "?f",
            "(COUNT(*) AS ?n) (COUNT(DISTINCT *) AS ?d)",
            true,
        ),
        (
            "?s a ?c OPTIONAL { ?s ex:feat ?f }",
            "?c",
            "(COUNT(?f) AS ?n)",
            true,
        ),
        (
            "?s ex:val ?v FILTER isLiteral(?v)",
            "?v",
            "(COUNT(?s) AS ?n)",
            true,
        ),
        (
            "?s ex:pub ?p MINUS { ?s ex:feat ?f }",
            "?p",
            "(COUNT(*) AS ?n)",
            true,
        ),
        ("?s a ?c", "?nowhere", "(COUNT(*) AS ?n)", true),
        ("?s a ex:None", "?s", "(COUNT(*) AS ?n)", true),
        // A blank node key: the evaluator decides.
        ("ex:b ex:val ?v", "?v", "(COUNT(*) AS ?n)", false),
        // MIN, MAX and SAMPLE of a variable (IRIs, integers, strings).
        ("?s a ?c", "?c", "(SAMPLE(?s) AS ?n)", true),
        (
            "?s a ?c . ?s ex:pub ?p",
            "?c",
            "(COUNT(?s) AS ?n) (MIN(?s) AS ?lo) (MAX(?p) AS ?hi) (SAMPLE(?s) AS ?any)",
            true,
        ),
        (
            "?s a ?c OPTIONAL { ?s ex:feat ?f }",
            "?c",
            "(MIN(?f) AS ?lo) (MAX(DISTINCT ?f) AS ?hi)",
            true,
        ),
        (
            "?s ex:pub ?p . ?s ex:val ?v FILTER isNumeric(?v)",
            "?p",
            "(MIN(?v) AS ?lo) (MAX(?v) AS ?hi)",
            true,
        ),
        (
            "?s ex:pub ?p . ?s ex:val ?v FILTER(isLiteral(?v) && !isNumeric(?v))",
            "?p",
            "(MIN(?v) AS ?lo)",
            // Language strings have string ranks.
            true,
        ),
        (
            "?s a ?c . ?s ex:feat ?f",
            "?c",
            "(MIN(?nowhere) AS ?lo) (MAX(?f) AS ?hi)",
            true,
        ),
        (
            "?s ex:pub ?p . ?s ex:name ?n",
            "?p",
            "(MIN(?n) AS ?lo) (MAX(?n) AS ?hi) (COUNT(*) AS ?c)",
            true,
        ),
        ("?s ex:name ?n", "?n", "(MIN(?s) AS ?lo)", true),
        // Without keys: one group, also of no rows.
        (
            "?s ex:name ?n",
            "",
            "(MIN(?n) AS ?lo) (MAX(?s) AS ?hi) (COUNT(*) AS ?c)",
            true,
        ),
        (
            "?s a ex:None",
            "",
            "(MIN(?s) AS ?lo) (SAMPLE(?s) AS ?any) (COUNT(*) AS ?c)",
            true,
        ),
        // Integers and language strings: the evaluator orders them.
        (
            "?s ex:pub ?p . ?s ex:val ?v",
            "?p",
            "(MIN(?v) AS ?lo)",
            false,
        ),
        // Not a count, MIN, MAX or SAMPLE.
        ("?s a ?c", "?c", "(GROUP_CONCAT(?s) AS ?n)", false),
    ];
    for (p, keys, a, answered) in cases {
        let group = if keys.is_empty() {
            String::new()
        } else {
            format!("GROUP BY {keys}")
        };
        let fast =
            format!("PREFIX ex: <http://example.org/> SELECT {keys} {a} WHERE {{ {p} }} {group}");
        let slow = format!(
            "PREFIX ex: <http://example.org/> SELECT {keys} {a} WHERE {{ {{ {p} }} FILTER(true) }} {group}"
        );
        let (mut f, mut s) = (values(&engine, &fast), values(&engine, &slow));
        f.sort();
        s.sort();
        // SAMPLE may pick another value each time.
        if !a.contains("SAMPLE") {
            assert_eq!(f, s, "{fast}");
        }
        let plan = engine.explain(&fast).unwrap();
        assert_eq!(!plan.contains("QuadPattern"), answered, "{fast}: {plan}");
    }
    // Around the groups: HAVING, ORDER BY and LIMIT still apply.
    let q = "PREFIX ex: <http://example.org/> SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ex:pub ?p } GROUP BY ?p HAVING (COUNT(*) > 8) ORDER BY DESC(?n) ?p LIMIT 3";
    let r = "PREFIX ex: <http://example.org/> SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ex:pub ?p FILTER(true) } GROUP BY ?p HAVING (COUNT(*) > 8) ORDER BY DESC(?n) ?p LIMIT 3";
    assert_eq!(values(&engine, q), values(&engine, r));
    // ORDER BY a count and LIMIT: the first groups only (ties in any order).
    for (order, limit) in [
        ("DESC(?n)", 5),
        ("?n", 3),
        ("DESC(?n) ?d", 4),
        ("DESC(?n)", 0),
    ] {
        let q = format!(
            "PREFIX ex: <http://example.org/> SELECT ?p (COUNT(*) AS ?n) (COUNT(DISTINCT ?s) AS ?d) WHERE {{ ?s ex:pub ?p }} GROUP BY ?p ORDER BY {order} LIMIT {limit}"
        );
        let r = q.replace("?s ex:pub ?p }", "?s ex:pub ?p FILTER(true) }");
        let counts = |rows: Vec<String>| -> Vec<String> {
            rows.iter()
                .map(|r| r.split(' ').skip(1).collect::<Vec<_>>().join(" "))
                .collect()
        };
        assert_eq!(
            counts(values(&engine, &q)),
            counts(values(&engine, &r)),
            "{q}"
        );
        assert!(!engine.explain(&q).unwrap().contains("QuadPattern"), "{q}");
    }
}

/// `?s ?p ?o GROUP BY ?p` with counts (MatGUI's "properties" query, #243): from the
/// per-predicate counts while the store has only the default graph and no pending writes,
/// otherwise evaluated; the same answers either way.
#[test]
fn counts_per_predicate_match_the_rows() {
    let only_default: String = data()
        .lines()
        .filter(|l| !l.starts_with("GRAPH"))
        .map(|l| format!("{l}\n"))
        .collect();
    let queries = [
        "SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ?p ?o } GROUP BY ?p",
        "SELECT ?p (COUNT(?s) AS ?n) (COUNT(DISTINCT ?s) AS ?ds) (COUNT(DISTINCT ?o) AS ?do) WHERE { ?s ?p ?o } GROUP BY ?p",
        "SELECT ?p (COUNT(DISTINCT ?p) AS ?one) WHERE { ?s ?p ?o } GROUP BY ?p",
        "SELECT ?p ?n WHERE { { SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ?p ?o } GROUP BY ?p } } ORDER BY DESC(?n) ?p LIMIT 3",
    ];
    let sorted = |engine: &Engine, q: &str| {
        let mut v = values(engine, q);
        if !q.contains("ORDER BY") {
            v.sort();
        }
        v
    };
    let check = |engine: &Engine| {
        for q in queries {
            let slow = q.replace("{ ?s ?p ?o }", "{ ?s ?p ?o FILTER(true) }");
            assert_eq!(sorted(engine, q), sorted(engine, &slow), "{q}");
            let plan = engine.explain(q).unwrap();
            assert!(!plan.contains("QuadPattern"), "{q}: {plan}");
        }
    };
    let engine = Engine::new_tripletrail();
    engine
        .load(RdfFormat::TriG, only_default.as_bytes())
        .unwrap();
    check(&engine);
    engine
        .update("PREFIX ex: <http://example.org/> INSERT DATA { ex:new ex:p1 \"x\" . ex:new ex:pnew 1 }")
        .unwrap();
    check(&engine);
    // With a named graph, the per-predicate counts (over all graphs) don't answer.
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::TriG, data().as_bytes()).unwrap();
    check(&engine);
}

/// `GROUP BY` of an expression of a few variables, under `BIND` and `FILTER` (MatGUI's
/// "namespaces", "language-tagged-properties", ... #243): counted on ids per distinct value
/// first, then the expressions run once per value. The same answers as row by row, which a
/// `FILTER` reading every variable forces.
#[test]
fn eager_grouping_matches_the_rows() {
    let queries = [
        r#"SELECT ?ns (COUNT(*) AS ?n) WHERE { ?s ?p ?o BIND(REPLACE(STR(?p), "[^#/]+$", "") AS ?ns) } GROUP BY ?ns"#,
        r#"SELECT ?p ?lang (COUNT(*) AS ?n) WHERE { ?s ?p ?o FILTER(isLiteral(?o) && LANG(?o) != "") BIND(LANG(?o) AS ?lang) } GROUP BY ?p ?lang"#,
        r#"SELECT ?p (COUNT(?l) AS ?n) (COUNT(*) AS ?all) WHERE { ?s ?p ?o BIND(IF(isLiteral(?o), LANG(?o), 1/0) AS ?l) } GROUP BY ?p"#,
        r#"SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o FILTER(isIRI(?o) && ?p != <http://example.org/p1>) }"#,
        r#"SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o FILTER(?p = <http://example.org/nothing>) }"#,
        r#"SELECT ?k (COUNT(*) AS ?n) WHERE { ?s <http://example.org/p2> ?o BIND(STRLEN(STR(?o)) AS ?k) } GROUP BY ?k"#,
        r#"SELECT ?p ?dt (COUNT(*) AS ?n) WHERE { ?s ?p ?o FILTER(isLiteral(?o)) BIND(DATATYPE(?o) AS ?dt) } GROUP BY ?p ?dt"#,
        r#"SELECT ?k (COUNT(*) AS ?n) WHERE { ?s ?p ?o BIND(IF(isIRI(?o), "iri", IF(isBlank(?o), "blank", IF(isLiteral(?o), "literal", "other"))) AS ?k) } GROUP BY ?k"#,
        r#"SELECT ?l (COUNT(*) AS ?n) WHERE { ?s ?p ?o FILTER(!isIRI(?o)) BIND(COALESCE(LANG(?o), "none") AS ?l) } GROUP BY ?l"#,
        // MatGUI's temporal properties: MIN and MAX of the values the FILTERs keep.
        r#"SELECT ?p ?dt (COUNT(*) AS ?n) (MIN(?o) AS ?lo) (MAX(?o) AS ?hi) WHERE { ?s ?p ?o FILTER(isLiteral(?o)) BIND(DATATYPE(?o) AS ?dt) FILTER(?dt IN (<http://www.w3.org/2001/XMLSchema#date>, <http://www.w3.org/2001/XMLSchema#dateTime>, <http://www.w3.org/2001/XMLSchema#gYear>)) } GROUP BY ?p ?dt"#,
        r#"SELECT ?p (MIN(?o) AS ?lo) (MAX(?o) AS ?hi) (COUNT(*) AS ?n) WHERE { ?s ?p ?o FILTER(isLiteral(?o) && LANG(?o) = "") } GROUP BY ?p"#,
        r#"SELECT (MIN(?o) AS ?lo) (MAX(?o) AS ?hi) WHERE { ?s ?p ?o FILTER(isIRI(?o) || LANG(?o) != "") }"#,
        r#"SELECT ?p (MAX(?o) AS ?hi) WHERE { ?s ?p ?o FILTER(DATATYPE(?o) = <http://www.w3.org/2001/XMLSchema#integer>) } GROUP BY ?p"#,
    ];
    let sorted = |engine: &Engine, q: &str| {
        let mut v = values(engine, q);
        v.sort();
        v
    };
    let check = |engine: &Engine| {
        for q in queries {
            let slow = q.replacen(
                "WHERE { ?s ",
                "WHERE { FILTER(BOUND(?s) || BOUND(?o) || true) ?s ",
                1,
            );
            assert_ne!(q, slow);
            assert_eq!(sorted(engine, q), sorted(engine, &slow), "{q}");
        }
    };
    let only_default: String = data()
        .lines()
        .filter(|l| !l.starts_with("GRAPH"))
        .map(|l| format!("{l}\n"))
        .collect();
    for ttl in [only_default, data()] {
        let engine = Engine::new_tripletrail();
        engine.load(RdfFormat::TriG, ttl.as_bytes()).unwrap();
        check(&engine);
        engine
            .update("PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> INSERT DATA { ex:new ex:p1 \"x\"@fr . ex:new ex:pnew 1 . ex:new ex:p1 \"300\"^^xsd:byte , \"x\"^^xsd:integer , \"2020-01-01\"^^xsd:date , \"2019-12-31\"^^xsd:date , \"2021-03-04\"^^xsd:date , \"1999\"^^xsd:gYear , \"2005\"^^xsd:gYear , \"2020-01-01T10:00:00Z\"^^xsd:dateTime , \"2020-01-01T09:00:00+02:00\"^^xsd:dateTime , \"y\"^^ex:dt , \"z\"@en--rtl }")
            .unwrap();
        check(&engine);
    }
    // The expression runs once per predicate: the plan has no quad pattern.
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::TriG, data().as_bytes()).unwrap();
    let plan = engine.explain(queries[0]).unwrap();
    assert!(!plan.contains("QuadPattern"), "{plan}");
    // So do LANG, DATATYPE and the kind tests, once per kind and tag of the objects.
    for q in [queries[1], queries[6], queries[7], queries[8], queries[9]] {
        let plan = engine.explain(q).unwrap();
        assert!(!plan.contains("QuadPattern"), "{q}\n{plan}");
    }
}

/// MatGUI's "hubs" (#243): the degree of each node from runs of subjects and of objects,
/// the same as row by row.
#[test]
fn node_degrees_match_the_rows() {
    let queries = [
        "SELECT ?node (COUNT(*) AS ?degree) WHERE { { ?node ?p ?o } UNION { ?s ?p ?node . FILTER(isIRI(?node)) } } GROUP BY ?node",
        "SELECT ?node (COUNT(*) AS ?degree) WHERE { { ?s ?p ?node } UNION { ?node ?q ?o } } GROUP BY ?node",
        "SELECT ?node (COUNT(*) AS ?degree) WHERE { { ?node ?p ?o } UNION { ?s ?p ?node . FILTER(isIRI(?node) || isBlank(?node)) } } GROUP BY ?node",
    ];
    let sorted = |engine: &Engine, q: &str| {
        let mut v = values(engine, q);
        v.sort();
        v
    };
    let check = |engine: &Engine| {
        for q in queries {
            let slow = q.replacen("{ { ?", "{ { FILTER(true || BOUND(?o)) ?", 1);
            assert_ne!(q, slow);
            let fast = sorted(engine, q);
            assert!(fast.len() > 100, "{}", fast.len());
            assert_eq!(fast, sorted(engine, &slow), "{q}");
        }
        // Under ORDER BY and LIMIT: the top degrees.
        let top = |q: &str| {
            let mut d: Vec<String> = values(engine, q)
                .into_iter()
                .filter_map(|r| {
                    r.split(' ')
                        .find(|x| x.starts_with("?degree="))
                        .map(str::to_owned)
                })
                .collect();
            d.sort();
            d
        };
        let q = format!("{} ORDER BY DESC(?degree) LIMIT 7", queries[0]);
        let slow = q.replacen("{ { ?", "{ { FILTER(true || BOUND(?o)) ?", 1);
        assert_eq!(top(&q), top(&slow));
    };
    // Without blank nodes (which a VALUES block cannot hold), no quad is read row by row.
    let no_blanks: String = data()
        .lines()
        .filter(|l| !l.contains("_:"))
        .map(|l| format!("{l}\n"))
        .collect();
    for ttl in [data(), no_blanks] {
        let engine = Engine::new_tripletrail();
        engine.load(RdfFormat::TriG, ttl.as_bytes()).unwrap();
        check(&engine);
        engine
            .update("PREFIX ex: <http://example.org/> INSERT DATA { ex:s1 ex:p1 ex:s2 . ex:hub ex:p1 ex:s1 , ex:s3 }")
            .unwrap();
        check(&engine);
        if !ttl.contains("_:") {
            for q in &queries[..2] {
                let plan = engine.explain(q).unwrap();
                assert!(!plan.contains("QuadPattern"), "{q}\n{plan}");
            }
        }
    }
}

/// `COUNT(*)` over star patterns (one centre variable, multi-valued arms) is counted per
/// centre value without building the rows (#179): the same numbers as row by row, also for a
/// centre in the object position, with constants, after an update, and for a star whose rows
/// would be far too many to hold.
#[test]
fn star_counts_match_the_rows() {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..2000 {
        ttl.push_str(&format!("ex:a{i} a ex:C{} .\n", i % 5));
        for j in 0..(i % 6) {
            ttl.push_str(&format!("ex:a{i} ex:feat ex:f{} .\n", (i + j * 7) % 90));
        }
        for j in 0..(i % 4) {
            ttl.push_str(&format!("ex:a{i} ex:tag \"t{}\" .\n", (i * 3 + j) % 50));
        }
        if i % 3 == 0 {
            ttl.push_str(&format!("ex:o{i} ex:about ex:a{} .\n", i % 700));
        }
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let patterns = [
        "?s a ?c . ?s ex:feat ?f",
        "?s ex:feat ?f . ?s ex:tag ?t",
        "?s a ?c . ?s ex:feat ?f . ?s ex:tag ?t",
        "?s a ex:C2 . ?s ex:feat ?f . ?s ex:tag ?t",
        "?s ex:feat ex:f3 . ?s ex:tag ?t . ?o ex:about ?s",
        "?o ex:about ?s . ?s ex:feat ?f",
        "?s ex:feat ?f . ?t ex:feat ?f",
        "?a ex:feat ?f . ?b ex:feat ?f . ?c ex:feat ?f",
        "?s a ex:Missing . ?s ex:feat ?f",
    ];
    let check = |engine: &Engine| {
        for p in patterns {
            let fast =
                format!("PREFIX ex: <http://example.org/> SELECT (COUNT(*) AS ?n) WHERE {{ {p} }}");
            let slow = format!(
                "PREFIX ex: <http://example.org/> SELECT (COUNT(*) AS ?n) WHERE {{ {p} FILTER(true) }}"
            );
            assert_eq!(values(engine, &fast), values(engine, &slow), "{fast}");
        }
    };
    check(&engine);
    engine
        .update(
            "PREFIX ex: <http://example.org/> INSERT DATA { ex:a1 ex:feat ex:f1, ex:new ; ex:tag \"x\" . ex:z a ex:C2 ; ex:feat ex:f3 ; ex:tag \"y\" } ;
             DELETE DATA { ex:a7 ex:feat ex:f7 }",
        )
        .unwrap();
    check(&engine);

    // 600 × 600 × 600 values of one subject: 216 million rows, counted from 1,800 triples.
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..600 {
        ttl.push_str(&format!(
            "ex:x ex:a ex:v{i} ; ex:b ex:w{i} ; ex:c ex:u{i} .\n"
        ));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    assert_eq!(
        values(
            &engine,
            "PREFIX ex: <http://example.org/> SELECT (COUNT(*) AS ?n) WHERE { ?s ex:a ?a ; ex:b ?b ; ex:c ?c }"
        ),
        vec!["?n=\"216000000\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
}

/// One triple pattern grouped by one of its variables (`?f a ?c GROUP BY ?c`): counted per
/// value from sorted runs and the block index, the same as row by row, with values of a
/// few quads and of thousands, quads in a named graph (not counted), writes not yet
/// compacted, and from disk.
#[test]
fn value_counts_match_the_rows() {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..6000 {
        // Class 0 has thousands of instances, the last ones a few.
        let class = match i % 100 {
            0..=69 => 0,
            70..=89 => 1,
            n => n,
        };
        ttl.push_str(&format!("ex:a{i} a ex:C{class} .\n"));
        ttl.push_str(&format!("ex:a{i} ex:kind \"k{}\" .\n", (i * i) % 13));
        if i % 4 == 0 {
            ttl.push_str(&format!("ex:a{i} ex:kind {} .\n", i % 3));
        }
    }
    ttl.push_str("GRAPH ex:g { ex:a1 a ex:C0 . ex:x a ex:Named . ex:x ex:kind \"k1\" }\n");
    let queries = [
        "SELECT ?c (COUNT(?f) AS ?n) WHERE { ?f a ?c } GROUP BY ?c",
        "SELECT ?k (COUNT(?f) AS ?n) (COUNT(*) AS ?m) (COUNT(DISTINCT ?k) AS ?one) WHERE { ?f ex:kind ?k } GROUP BY ?k",
        "SELECT ?f (COUNT(*) AS ?n) WHERE { ?f ex:kind ?k } GROUP BY ?f",
        "SELECT ?p (COUNT(*) AS ?n) WHERE { ?f ?p ex:C0 } GROUP BY ?p",
        "SELECT ?f (COUNT(*) AS ?n) WHERE { ?f ?p ?o } GROUP BY ?f",
        "SELECT ?c ?n WHERE { { SELECT ?c (COUNT(?f) AS ?n) WHERE { ?f a ?c } GROUP BY ?c } } ORDER BY DESC(?n) ?c LIMIT 2",
        "SELECT ?c (COUNT(?f) AS ?n) WHERE { ?f ex:missing ?c } GROUP BY ?c",
    ];
    let sorted = |engine: &Engine, q: &str| {
        let mut v = values(engine, q);
        if !q.contains("ORDER BY") {
            v.sort();
        }
        v
    };
    let check = |engine: &Engine| {
        for q in queries {
            let q = format!("PREFIX ex: <http://example.org/> {q}");
            let slow = q.replacen(" } GROUP BY", " FILTER(true) } GROUP BY", 1);
            assert_ne!(q, slow);
            assert_eq!(sorted(engine, &q), sorted(engine, &slow), "{q}");
            // `?f ?p ex:C0` comes sorted by subject, not by predicate: left to the rows.
            // Many groups are read back from a table of ids, the only quads read then.
            if !q.contains("?p ex:C0") {
                let plan = engine.explain(&q).unwrap();
                assert_eq!(
                    plan.matches("QuadPattern(").count(),
                    plan.matches("QuadPattern(?__tripletrail_row_").count(),
                    "{q}: {plan}"
                );
            }
        }
    };
    let update = "PREFIX ex: <http://example.org/> INSERT DATA { ex:new a ex:C0, ex:C99, ex:Fresh . ex:new ex:kind \"k1\" } ;
                  DELETE DATA { ex:a0 a ex:C0 . ex:a1 ex:kind \"k1\" . ex:a99 a ex:C99 }";
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::TriG, ttl.as_bytes()).unwrap();
    check(&engine);
    engine.update(update).unwrap();
    check(&engine);

    let dir = tripletrail_testdir::tempdir("value-counts");
    {
        let engine = Engine::open_tripletrail(&dir).unwrap();
        engine.load(RdfFormat::TriG, ttl.as_bytes()).unwrap();
        engine.update(update).unwrap();
        check(&engine);
    }
    {
        let engine = Engine::open_tripletrail(&dir).unwrap();
        check(&engine);
    }
}

/// Text filters on stored strings (`CONTAINS`, `STRENDS`, `STRSTARTS`, `REGEX`) read the
/// values from the dictionary keys, and `COUNT(?v)` of a variable bound in every row counts
/// without building the rows (#165): the same answers as the Oxigraph backend, for simple,
/// language-tagged, directional and inline strings, other literals and IRIs.
#[test]
fn text_filters_and_variable_counts_match_oxigraph() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..300 {
        let label = match i % 6 {
            0 => format!("\"Station com{i} long enough to be stored in the dictionary\""),
            1 => format!("\"stazione {i} della linea\"@it"),
            2 => format!("\"c{i}m\""),
            3 => format!("\"{i}\"^^xsd:integer"),
            4 => format!("ex:iri{i}"),
            _ => format!("\"right to left com{i}\"@ar--rtl"),
        };
        ttl.push_str(&format!("ex:s{i} ex:label {label} ; a ex:C{} .\n", i % 4));
        // Only strings, short (inline) and long, plain and tagged.
        let name = match i % 4 {
            // Upper case makes `ß` two characters, and `İ` lower case two.
            3 => format!("\"Straße İstanbul ÿ, a name in the dictionary {i}\"@de"),
            0 => format!("\"n{i}\""),
            1 => format!("\"a name long enough for the dictionary, number {i}\"@en"),
            _ => format!("\"another name in the dictionary of the store, {i}\""),
        };
        ttl.push_str(&format!("ex:s{i} ex:name {name} .\n"));
        if i % 3 == 0 {
            ttl.push_str(&format!("ex:s{i} ex:other ex:s{} .\n", (i + 1) % 300));
        }
    }
    let tripletrail = Engine::new_tripletrail();
    let oxigraph = Engine::new_oxigraph().unwrap();
    for e in [&tripletrail, &oxigraph] {
        e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    }
    let filters = [
        "CONTAINS(?o, \"com\")",
        "CONTAINS(?o, \"\")",
        "STRENDS(?o, \"m\")",
        "STRSTARTS(?o, \"sta\")",
        "REGEX(?o, \"c.m\")",
        "REGEX(?o, \"COM\", \"i\")",
        "REGEX(?o, \"^c\\\\d+m$\")",
        "!CONTAINS(?o, \"com\")",
        "CONTAINS(?o, \"com\") || STRENDS(?o, \"linea\")",
    ];
    let check = |stage: &str| {
        for f in filters {
            for a in ["(COUNT(*) AS ?n)", "(COUNT(?s) AS ?n)", "(COUNT(?o) AS ?n)"] {
                let q = format!(
                    "PREFIX ex: <http://example.org/> SELECT {a} WHERE {{ ?s ex:label ?o FILTER({f}) }}"
                );
                assert_eq!(
                    values(&tripletrail, &q),
                    values(&oxigraph, &q),
                    "{stage}: {q}"
                );
            }
        }
        for q in [
            "SELECT (COUNT(?s) AS ?n) { ?s a ?o FILTER(?s != ?o) }",
            "SELECT (COUNT(?s) AS ?n) (COUNT(*) AS ?m) { ?s a ?c . ?s ex:label ?o FILTER(?c != ?o) }",
            // ?x is not bound in every row: counted by the rows.
            "SELECT (COUNT(?x) AS ?n) { ?s a ?c OPTIONAL { ?s ex:other ?x } FILTER(?s != ?c) }",
            "SELECT (COUNT(?x) AS ?n) { { ?s a ?c } UNION { ?s ex:other ?x } FILTER(?s != ?c) }",
            "SELECT (COUNT(DISTINCT ?c) AS ?n) { ?s a ?c FILTER(?s != ?c) }",
            // SUM of string lengths and of text tests cast to integers, also with IRIs and
            // numbers among the values (an error for both: the sum is unbound).
            "SELECT (SUM(STRLEN(?o)) AS ?n) { ?s ex:label ?o FILTER(isLiteral(?o) && !isNumeric(?o)) }",
            "SELECT (SUM(STRLEN(?o)) AS ?n) { ?s ex:label ?o }",
            "SELECT (SUM(STRLEN(?o)) AS ?n) { ?s ex:name ?o }",
            "SELECT (SUM(STRLEN(STRBEFORE(?o, \"a\"))) AS ?n) { ?s ex:name ?o }",
            "SELECT (SUM(STRLEN(STRAFTER(?o, \"a\"))) AS ?n) { ?s ex:name ?o }",
            "SELECT (SUM(STRLEN(STRAFTER(?o, \"\"))) AS ?n) (SUM(STRLEN(STRBEFORE(?o, \"\"))) AS ?m) { ?s ex:name ?o }",
            "SELECT (SUM(STRLEN(UCASE(?o))) AS ?n) (SUM(STRLEN(LCASE(?o))) AS ?m) { ?s ex:name ?o }",
            "SELECT (SUM(STRLEN(STRBEFORE(?o, \"com\"))) AS ?n) { ?s ex:label ?o }",
            "SELECT (SUM(<http://www.w3.org/2001/XMLSchema#integer>(STRENDS(?o, \"5\"))) AS ?n) { ?s ex:name ?o }",
            "SELECT (SUM(<http://www.w3.org/2001/XMLSchema#integer>(STRSTARTS(?o, \"sta\"))) AS ?n) { ?s ex:label ?o FILTER(isLiteral(?o) && !isNumeric(?o)) }",
            "SELECT (SUM(<http://www.w3.org/2001/XMLSchema#integer>(CONTAINS(?o, \"com\"))) AS ?n) { ?s ex:label ?o }",
            "SELECT ?c (SUM(STRLEN(?o)) AS ?n) { ?s a ?c ; ex:label ?o FILTER(isLiteral(?o) && !isNumeric(?o)) } GROUP BY ?c ORDER BY ?c",
            "SELECT ?c (SUM(<http://www.w3.org/2001/XMLSchema#integer>(REGEX(?o, \"^c\\\\d+m$\"))) AS ?n) { ?s a ?c ; ex:label ?o FILTER(isLiteral(?o) && !isNumeric(?o)) } GROUP BY ?c ORDER BY ?c",
        ] {
            let q = format!("PREFIX ex: <http://example.org/> {q}");
            assert_eq!(
                values(&tripletrail, &q),
                values(&oxigraph, &q),
                "{stage}: {q}"
            );
        }
    };
    check("loaded");
    let update = "PREFIX ex: <http://example.org/> INSERT DATA { ex:n1 ex:label \"a new com label stored in the delta of the store\" . ex:n2 a ex:n2 } ; DELETE DATA { ex:s0 ex:label \"Station com0 long enough to be stored in the dictionary\" }";
    for e in [&tripletrail, &oxigraph] {
        e.update(update).unwrap();
    }
    check("after writes");
}

/// `COUNT` over `?s <p>+ ?o` and `?s <q>/<p>+ ?o` comes from the edges of `p` read once
/// (#165): the same answers as the Oxigraph backend on chains (`rdf:rest` lists), a cycle,
/// a self loop, a diamond (one pair however many ways it is reached), sequences whose
/// first step ends twice at a node, quads in a named graph (not in the default graph) and
/// writes.
#[test]
fn transitive_path_counts_match_oxigraph() {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for l in 0..40 {
        // Lists of 1 to 7 nodes.
        let len = 1 + l % 7;
        for i in 0..len {
            let next = if i + 1 == len {
                "ex:nil".to_owned()
            } else {
                format!("ex:l{l}_{}", i + 1)
            };
            ttl.push_str(&format!("ex:l{l}_{i} ex:rest {next} .\n"));
        }
        ttl.push_str(&format!("ex:track{} ex:seq ex:l{l}_0 .\n", l % 9));
        if l % 5 == 0 {
            ttl.push_str(&format!("ex:other{l} ex:seq ex:l{l}_0 , ex:l{l}_1 .\n"));
        }
    }
    ttl.push_str(
        "ex:c1 ex:rest ex:c2 . ex:c2 ex:rest ex:c3 . ex:c3 ex:rest ex:c1 . ex:loop ex:rest ex:loop .
         ex:d1 ex:rest ex:d2 , ex:d3 . ex:d2 ex:rest ex:d4 . ex:d3 ex:rest ex:d4 . ex:d4 ex:rest ex:d5 .
         ex:start ex:seq ex:c1 , ex:d1 , ex:loop , ex:nowhere .
         GRAPH ex:g { ex:c3 ex:rest ex:g1 . ex:g1 ex:rest ex:g2 . ex:start ex:seq ex:g1 }\n",
    );
    let tripletrail = Engine::new_tripletrail();
    let oxigraph = Engine::new_oxigraph().unwrap();
    for e in [&tripletrail, &oxigraph] {
        e.load(RdfFormat::TriG, ttl.as_bytes()).unwrap();
    }
    let queries = [
        "SELECT (COUNT(*) AS ?n) { ?s ex:rest+ ?o }",
        "SELECT (COUNT(?o) AS ?n) (COUNT(?s) AS ?m) { ?s ex:rest+ ?o }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:seq/ex:rest+ ?o }",
        "SELECT (COUNT(?s) AS ?n) { ?s ex:seq/ex:rest+ ?o }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:seq ?x . ?x ex:rest+ ?o }",
        "SELECT (COUNT(*) AS ?n) { ex:start ex:rest+ ?o }",
        "SELECT (COUNT(*) AS ?n) { ex:c1 ex:rest+ ?o }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:missing+ ?o }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:missing/ex:rest+ ?o }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:seq/ex:missing+ ?o }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:rest+ ?s }",
        "SELECT (COUNT(DISTINCT ?o) AS ?n) { ?s ex:rest+ ?o }",
        "SELECT (COUNT(*) AS ?n) { GRAPH ex:g { ?s ex:rest+ ?o } }",
    ];
    let check = |stage: &str| {
        for q in queries {
            let q = format!("PREFIX ex: <http://example.org/> {q}");
            assert_eq!(
                values(&tripletrail, &q),
                values(&oxigraph, &q),
                "{stage}: {q}"
            );
        }
    };
    check("loaded");
    let update = "PREFIX ex: <http://example.org/> INSERT DATA { ex:d5 ex:rest ex:d1 . ex:new ex:seq ex:d4 } ; DELETE DATA { ex:c2 ex:rest ex:c3 . ex:l3_1 ex:rest ex:l3_2 }";
    for e in [&tripletrail, &oxigraph] {
        e.update(update).unwrap();
    }
    check("after writes");
}

/// `COUNT` of `MINUS`, `EXISTS`, `NOT EXISTS` and `OPTIONAL` whose right side is one triple
/// with many more matches than the left side has keys, but not so many that each key is
/// looked up: the right side's counts per key come from one sorted scan walked forward
/// (#165). The same answers as the Oxigraph backend, before and after writes.
#[test]
fn keyed_counts_by_a_walk_match_oxigraph() {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..30_000 {
        // Every subject has 1 or 2 types; one in 15 also has a label.
        ttl.push_str(&format!("ex:s{i} a ex:T{}", i % 3));
        if i % 4 == 0 {
            ttl.push_str(&format!(", ex:U{}", i % 5));
        }
        if i % 15 == 0 {
            ttl.push_str(&format!(" ; ex:label \"l{}\"", i % 7));
        }
        ttl.push_str(" .\n");
    }
    // Labels on subjects without a type, and a subject with two labels.
    for i in 0..300 {
        ttl.push_str(&format!("ex:u{i} ex:label \"u\" .\n"));
    }
    ttl.push_str("ex:s0 ex:label \"second\" .\n");
    let tripletrail = Engine::new_tripletrail();
    let oxigraph = Engine::new_oxigraph().unwrap();
    for e in [&tripletrail, &oxigraph] {
        e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    }
    let queries = [
        "SELECT (COUNT(*) AS ?n) { ?s ex:label ?l MINUS { ?s a ?t } }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:label ?l FILTER EXISTS { ?s a ?t } }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:label ?l FILTER NOT EXISTS { ?s a ?t } }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:label ?l OPTIONAL { ?s a ?t } }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:label ?l MINUS { ?s a ex:U1 } }",
        // The other way round: a small right side's keys looked up on the left side.
        "SELECT (COUNT(*) AS ?n) { ?s a ?t MINUS { ?s ex:label ?l } }",
        "SELECT (COUNT(*) AS ?n) { ?s a ?t FILTER EXISTS { ?s ex:label ?l } }",
        "SELECT (COUNT(*) AS ?n) { ?s a ?t FILTER NOT EXISTS { ?s ex:label ?l } }",
        "SELECT (COUNT(*) AS ?n) { ?s a ?t OPTIONAL { ?s ex:label ?l } }",
        "SELECT (COUNT(*) AS ?n) { ?s a ?t FILTER EXISTS { ?s ex:label \"u\" } }",
        "SELECT (COUNT(*) AS ?n) { ?s a ex:T1 MINUS { ?s ex:label \"l3\" } }",
    ];
    let check = |stage: &str| {
        for q in queries {
            let q = format!("PREFIX ex: <http://example.org/> {q}");
            assert_eq!(
                values(&tripletrail, &q),
                values(&oxigraph, &q),
                "{stage}: {q}"
            );
        }
    };
    check("loaded");
    let update = "PREFIX ex: <http://example.org/> INSERT DATA { ex:u1 a ex:T0 . ex:u2 a ex:T1, ex:T2 } ; DELETE DATA { ex:s15 a ex:T0 . ex:s30 a ex:T0 }";
    for e in [&tripletrail, &oxigraph] {
        e.update(update).unwrap();
    }
    check("after writes");
}

#[test]
fn chain_counts_match_oxigraph() {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..6_000 {
        // Each ?a points at a ?b; most ?b are on 0, 1 or 2 elements ?c, which have 0 to 2
        // types, and some ?c are on an element too.
        ttl.push_str(&format!("ex:a{i} ex:at ex:b{} .\n", i % 4_000));
        let b = i % 4_000;
        if i < 4_000 {
            for k in 0..(b % 3) {
                ttl.push_str(&format!("ex:b{b} ex:on ex:c{} .\n", (b + k * 7) % 2_500));
            }
        }
        if i < 2_500 {
            for k in 0..(i % 3) {
                ttl.push_str(&format!("ex:c{i} a ex:T{k} .\n"));
            }
            if i % 11 == 0 {
                ttl.push_str(&format!("ex:c{i} ex:on ex:c{} .\n", (i + 1) % 2_500));
            }
        }
    }
    let tripletrail = Engine::new_tripletrail();
    let oxigraph = Engine::new_oxigraph().unwrap();
    for e in [&tripletrail, &oxigraph] {
        e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    }
    let queries = [
        "SELECT (COUNT(*) AS ?n) { ?a ex:at ?b MINUS { ?b ex:on ?c . ?c a ?d } }",
        "SELECT (COUNT(*) AS ?n) { ?a ex:at ?b OPTIONAL { ?b ex:on ?c . ?c a ?d } }",
        "SELECT (COUNT(*) AS ?n) { ?a ex:at ?b FILTER EXISTS { ?b ex:on ?c . ?c a ?d } }",
        "SELECT (COUNT(*) AS ?n) { ?a ex:at ?b FILTER NOT EXISTS { ?b ex:on ?c . ?c a ?d } }",
        // The triples the other way round, the shared variable as an object, a constant.
        "SELECT (COUNT(*) AS ?n) { ?a ex:at ?b OPTIONAL { ?c a ?d . ?b ex:on ?c } }",
        "SELECT (COUNT(*) AS ?n) { ?a ex:at ?b OPTIONAL { ?b ex:on ?c . ?e ex:on ?c } }",
        "SELECT (COUNT(*) AS ?n) { ?a ex:at ?b MINUS { ?b ex:on ?c . ?c a ex:T1 } }",
        // Stars on the key: counted per triple for the left keys and multiplied.
        "SELECT (COUNT(*) AS ?n) { ?a ex:at ?b OPTIONAL { ?b ex:on ?c . ?b ex:on ?d } }",
        "SELECT (COUNT(*) AS ?n) { ?a ex:at ?b MINUS { ?b ex:on ?c . ?b a ?d } }",
        "SELECT (COUNT(*) AS ?n) { ?c ex:on ?x MINUS { ?c a ?d . ?c ex:on ?e } }",
        "SELECT (COUNT(*) AS ?n) { ?c ex:on ?x FILTER EXISTS { ?c a ?d . ?c ex:on ?e } }",
        "SELECT (COUNT(*) AS ?n) { ?c ex:on ?x OPTIONAL { ?c a ?d . ?c ex:on ?e } }",
        "SELECT (COUNT(*) AS ?n) { ?c ex:on ?x OPTIONAL { ?c a ex:T1 . ?c ex:on ?e . ?c a ?f } }",
        "SELECT (COUNT(*) AS ?n) { ?c ex:on ?x OPTIONAL { ?c a ?d . ?e ex:on ?c } }",
        // Not a chain or a star: a repeated variable, two shared variables.
        "SELECT (COUNT(*) AS ?n) { ?a ex:at ?b OPTIONAL { ?b ex:on ?c . ?c ex:on ?c } }",
        "SELECT (COUNT(*) AS ?n) { ?a ex:at ?b OPTIONAL { ?b ex:on ?c . ?c ?b ?c } }",
    ];
    let check = |stage: &str| {
        for q in queries {
            let q = format!("PREFIX ex: <http://example.org/> {q}");
            assert_eq!(
                values(&tripletrail, &q),
                values(&oxigraph, &q),
                "{stage}: {q}"
            );
        }
    };
    check("loaded");
    let update = "PREFIX ex: <http://example.org/> INSERT DATA { ex:b4 ex:on ex:c2 . ex:c1 a ex:T9 } ; DELETE DATA { ex:c2 a ex:T0 . ex:b1 ex:on ex:c1 }";
    for e in [&tripletrail, &oxigraph] {
        e.update(update).unwrap();
    }
    check("after writes");
}

#[test]
fn union_counts_by_the_other_sides_keys_match_oxigraph() {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..20_000 {
        // Many typed subjects, some with a geometry; few labelled, some of those untyped.
        ttl.push_str(&format!("ex:s{i} a ex:T{}", i % 4));
        if i % 3 == 0 {
            ttl.push_str(&format!(", ex:U{}", i % 5));
        }
        if i % 7 == 0 {
            ttl.push_str(&format!(" ; ex:geom ex:g{i}"));
        }
        ttl.push_str(" .\n");
        if i % 40 == 0 {
            ttl.push_str(&format!("ex:s{i} ex:label \"l{}\" .\n", i % 9));
        }
    }
    for i in 0..200 {
        ttl.push_str(&format!(
            "ex:u{i} ex:label \"u\" ; ex:geom ex:h{} .\n",
            i % 3
        ));
    }
    let tripletrail = Engine::new_tripletrail();
    let oxigraph = Engine::new_oxigraph().unwrap();
    for e in [&tripletrail, &oxigraph] {
        e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    }
    let queries = [
        "SELECT (COUNT(*) AS ?n) { { ?s a ?o } UNION { ?s ex:geom ?o } ?s ex:label ?l }",
        "SELECT (COUNT(*) AS ?n) { { ?s a ?o } UNION { ?s ex:geom ?g } ?s ex:label ?l }",
        "SELECT (COUNT(*) AS ?n) { { ?s a ?o } UNION { { ?s ex:geom ?o } UNION { ?s a ex:U1 } } ?s ex:label ?l }",
        "SELECT (COUNT(*) AS ?n) { { ?s a ?o } UNION { ?s ex:geom ?o } MINUS { ?s ex:label ?l } }",
        "SELECT (COUNT(*) AS ?n) { { ?s a ?o } UNION { ?s ex:geom ?o } OPTIONAL { ?s ex:label ?l } }",
        "SELECT (COUNT(*) AS ?n) { { ?s a ?o } UNION { ?s ex:geom ?o } FILTER EXISTS { ?s ex:label ?l } }",
        "SELECT (COUNT(*) AS ?n) { { ?s a ?o } UNION { ?s ex:geom ?o } FILTER NOT EXISTS { ?s ex:label ?l } }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:label ?l MINUS { { ?s a ?o } UNION { ?s ex:geom ?o } } }",
        "SELECT (COUNT(*) AS ?n) { ?s ex:label ?l OPTIONAL { { ?s a ?o } UNION { ?s ex:geom ?o } } }",
        // A side of the union that is not one triple.
        "SELECT (COUNT(*) AS ?n) { { ?s a ?o } UNION { ?s ex:geom ?o . ?s a ex:T1 } ?s ex:label ?l }",
    ];
    let check = |stage: &str| {
        for q in queries {
            let q = format!("PREFIX ex: <http://example.org/> {q}");
            assert_eq!(
                values(&tripletrail, &q),
                values(&oxigraph, &q),
                "{stage}: {q}"
            );
        }
    };
    check("loaded");
    let update = "PREFIX ex: <http://example.org/> INSERT DATA { ex:u1 a ex:T0 . ex:s40 ex:geom ex:x } ; DELETE DATA { ex:s80 a ex:T0 . ex:u2 ex:geom ex:h2 }";
    for e in [&tripletrail, &oxigraph] {
        e.update(update).unwrap();
    }
    check("after writes");
}
