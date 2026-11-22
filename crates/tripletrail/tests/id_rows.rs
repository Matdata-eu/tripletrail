//! `MINUS`, `FILTER (NOT) EXISTS` and filters on ids evaluated on whole id tables, with the
//! rows read back by the evaluator (issue #167), give the same solutions as the evaluator
//! alone, also with `OPTIONAL` (unbound cells), `UNION`, blank nodes and after writes.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = engine.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    let mut out: Vec<String> = solutions
        .map(|s| {
            let s = s.unwrap();
            let mut cells: Vec<String> = s.iter().map(|(v, t)| format!("{v}={t}")).collect();
            cells.sort();
            cells.join(" ")
        })
        .collect();
    out.sort();
    out
}

fn data() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..3000 {
        ttl.push_str(&format!("ex:s{i} a ex:C{} .\n", i % 3));
        ttl.push_str(&format!("ex:s{i} ex:p ex:o{} .\n", i % 700));
        if i % 4 == 0 {
            ttl.push_str(&format!("ex:o{} ex:q \"v{}\" .\n", i % 700, i % 11));
        }
        if i % 7 == 0 {
            ttl.push_str(&format!("ex:s{i} ex:r _:b{} .\n", i % 13));
        }
        if i % 5 == 0 {
            let lang = ["en", "en-GB", "EN-us", "nl", "fr", "en--ltr", "eng"][i / 5 % 7];
            ttl.push_str(&format!("ex:s{i} ex:label \"l{i}\"@{lang} .\n"));
        }
        if i % 6 == 0 {
            ttl.push_str(&format!("ex:s{i} ex:label \"plain{}\" .\n", i % 17));
        }
        if i % 8 == 0 {
            let value = [
                "\"3\"^^xsd:integer",
                "\"3.0\"^^xsd:decimal",
                "\"3e0\"^^xsd:double",
                "\"x\"^^ex:dt",
                "true",
                "ex:o1",
                "_:v",
                "\"3\"",
                "\"abc\"^^xsd:integer",
                "\"300\"^^xsd:byte",
                "\"-7\"^^xsd:byte",
                "\"12\"^^xsd:unsignedShort",
            ][i / 8 % 12];
            ttl.push_str(&format!("ex:s{i} ex:val {value} .\n"));
        }
        if i % 9 == 0 {
            ttl.push_str(&format!(
                "ex:s{i} ex:same ex:s{} .\n",
                if i % 2 == 0 { i } else { i + 1 }
            ));
        }
    }
    ttl
}

const PATTERNS: &[&str] = &[
    "?s ex:p ?o . MINUS { ?o ex:q ?v }",
    "?s a ?c . ?s ex:p ?o . MINUS { ?o ex:q ?v }",
    "?s ex:p ?o . FILTER EXISTS { ?o ex:q ?v }",
    "?s ex:p ?o . FILTER NOT EXISTS { ?o ex:q ?v }",
    "?s a ?c . FILTER NOT EXISTS { ?s ex:r ?b }",
    "?s ex:same ?t . FILTER(?s = ?t)",
    "?s ex:same ?t . FILTER(sameTerm(?s, ?t))",
    "?s ex:p ?o . ?s ex:r ?b . FILTER(isBlank(?b))",
    // Filters on ids (issue #170).
    "?s ex:label ?l . FILTER(LANG(?l) = \"en\")",
    "?s ex:label ?l . FILTER(LANG(?l) = \"\")",
    // The tag read from the dictionary key: a prefix of a longer tag, a directional string.
    "?s ex:label ?l . FILTER(LANG(?l) = \"e\")",
    "?s ex:label ?l . FILTER(LANG(?l) = \"en-GB\")",
    "?s ex:label ?l . FILTER(LANG(?l) = \"en-gb\")",
    "?s ex:label ?l . FILTER(LANG(?l) = \"en--ltr\")",
    "?s ex:label ?l . FILTER(langMatches(LANG(?l), \"EN\"))",
    "?s ex:label ?l . FILTER(langMatches(LANG(?l), \"*\"))",
    "?s ex:val ?x . FILTER(DATATYPE(?x) = xsd:integer)",
    "?s ex:val ?x . FILTER(DATATYPE(?x) = ex:dt)",
    "?s ex:val ?x . FILTER(DATATYPE(?x) = xsd:byte)",
    "?s ex:val ?x . FILTER(?x = ex:o1 || DATATYPE(?x) = ex:dt)",
    "?s ex:val ?x . FILTER(isNumeric(?x))",
    "?s ex:val ?x . FILTER(!isNumeric(?x) || isBlank(?x))",
    "?s ex:val ?x . FILTER(?x != ex:o1)",
    "?s ex:val ?x . FILTER(?x = ex:o1 || ?x = ex:missing)",
    "?s ex:val ?x . FILTER(?x = \"3\")",
    "?s ex:val ?x . FILTER(?x = 3)",
    "?s ex:val ?x . FILTER(?x != true)",
    // STR(?x) = "text" and IN (#657): strings and IRIs by id, other literals by lexical form,
    // blank nodes an error.
    "?s ex:val ?x . FILTER(STR(?x) = \"3\")",
    "?s ex:val ?x . FILTER(STR(?x) IN (\"3\", \"true\", \"x\", \"http://example.org/o1\", \"abc\", \"300\"))",
    "?s ex:val ?x . FILTER(!(STR(?x) IN (\"3\", \"-7\")))",
    "?s ex:val ?x . FILTER(\"12\" = STR(?x) || isBlank(?x))",
    "?s ex:label ?l . FILTER(STR(?l) IN (\"l5\", \"plain3\", \"l70\"))",
    "?s ex:label ?l . FILTER(STR(?l) = \"l5\"@en)",
    "?s ex:val ?x . FILTER(?x IN (3, ex:o1, \"x\"^^ex:dt))",
    "?s ex:val ?x . FILTER(?x IN ())",
    "?s ex:val ?x . FILTER(!(?x IN (true)))",
    // IN over a hash set of ids (#63): numbers equal across datatypes, ill-typed and derived
    // integers, language tags, IRIs not in the store, NOT IN and unbound values.
    "?s ex:val ?x . FILTER(?x IN (3.0, ex:missing, \"3\", true))",
    "?s ex:val ?x . FILTER(?x NOT IN (3e0, ex:o1, -7))",
    "?s ex:val ?x . FILTER(?x IN (\"12\"^^xsd:integer, \"abc\"^^xsd:integer, false, 300))",
    "?s ex:val ?x . FILTER(?x IN (\"x\"^^ex:dt, \"y\"^^ex:dt, \"3\"^^xsd:string))",
    "?s ex:label ?l . FILTER(?l IN (\"l5\"@en, \"plain3\", \"l70\"@EN, \"plain4\"@en, \"l10\"))",
    "?s ex:label ?l . FILTER(?l NOT IN (\"plain3\", \"l5\"@en-gb))",
    "?s ex:p ?o . FILTER(?o IN (ex:o1, ex:o2, ex:o3, ex:missing, ex:o650, ex:o699, ex:o700))",
    "?s ex:p ?o . OPTIONAL { ?s ex:r ?b } FILTER(?b IN (ex:o1, ex:o2) || !BOUND(?b))",
    "?s ex:p ?o . OPTIONAL { ?o ex:q ?v } FILTER(?v NOT IN (\"v1\", \"v2\", ex:o1))",
    "?s ex:p ?o . OPTIONAL { ?s ex:r ?b } FILTER(!BOUND(?b))",
    "?s ex:p ?o . OPTIONAL { ?s ex:r ?b } FILTER(BOUND(?b) || ?o = ex:o3)",
    "?s ex:p ?o . OPTIONAL { ?s ex:label ?l } FILTER(!(LANG(?l) = \"nl\"))",
    "?s ex:p ?o . FILTER(BOUND(?nope))",
    // OPTIONAL under the filter: the rows read back have unbound cells.
    "?s ex:p ?o . OPTIONAL { ?s ex:r ?b } FILTER NOT EXISTS { ?o ex:q ?v }",
    "?s ex:p ?o . OPTIONAL { ?o ex:q ?v } MINUS { ?s a ex:C1 }",
    "{ ?s a ex:C0 } UNION { ?s ex:r ?b } FILTER NOT EXISTS { ?s ex:same ?t }",
    // Blank nodes of the pattern are not columns.
    "?s ex:p ?o . ?s ex:r [] . MINUS { ?o ex:q ?v }",
    // Nothing left.
    "?s ex:p ?o . MINUS { ?s ex:p ?o }",
];

fn check(engine: &Engine) {
    for p in PATTERNS {
        let prefix =
            "PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>";
        // Under a LIMIT without an order the evaluator does it all.
        let fast = rows(engine, &format!("{prefix} SELECT * WHERE {{ {p} }}"));
        let slow = rows(
            engine,
            &format!("{prefix} SELECT * WHERE {{ {p} }} LIMIT 100000000"),
        );
        assert_eq!(fast, slow, "{p}");
        let distinct = rows(
            engine,
            &format!("{prefix} SELECT DISTINCT ?s WHERE {{ {p} }} ORDER BY ?s"),
        );
        let slow_distinct = rows(
            engine,
            &format!("{prefix} SELECT DISTINCT ?s WHERE {{ {p} }} LIMIT 100000000"),
        );
        assert_eq!(distinct, slow_distinct, "{p}");
    }
}

/// The solutions of `query` in the order given.
fn ordered(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = engine.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    solutions
        .map(|s| {
            let s = s.unwrap();
            s.iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// Whole queries answered by the id executor (#11) are the evaluator's answers.
fn check_executor(engine: &Engine) {
    let prefix = "PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>";
    for p in PATTERNS {
        let run = |executor: &str, q: &str, keep_order: bool| {
            let q = format!("#pragma tt.executor {executor}\n{prefix} {q}");
            if keep_order {
                ordered(engine, &q)
            } else {
                rows(engine, &q)
            }
        };
        for (q, keep_order) in [
            (format!("SELECT * WHERE {{ {p} }}"), false),
            (format!("SELECT ?s ?o WHERE {{ {p} }}"), false),
            (format!("SELECT REDUCED ?s WHERE {{ {p} }}"), false),
            (
                format!("SELECT DISTINCT ?s WHERE {{ {p} }} ORDER BY DESC(?s)"),
                true,
            ),
            (
                format!("SELECT DISTINCT ?s WHERE {{ {p} }} ORDER BY ?s LIMIT 7 OFFSET 3"),
                true,
            ),
            (format!("SELECT * WHERE {{ {p} }} OFFSET 5"), false),
            (
                format!("SELECT ?s ?missing WHERE {{ {p} }} ORDER BY ?s"),
                false,
            ),
        ] {
            assert_eq!(
                run("ids", &q, keep_order),
                run("spareval", &q, keep_order),
                "{q}"
            );
        }
    }
}

/// `BIND` and `SELECT (expr AS ?v)` (#11): copies, constants and numbers computed on ids, as
/// the terms the evaluator gives; errors unbound. `ids` says whether the id executor must
/// answer it (else it may leave it to the evaluator).
const BINDS: &[(&str, bool)] = &[
    ("SELECT ?s ?y WHERE { ?s ex:val ?x BIND(?x AS ?y) }", true),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o OPTIONAL { ?s ex:val ?x } BIND(?x AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?k ?l WHERE { ?s a ex:C1 BIND(ex:k AS ?k) BIND(\"text\"@en AS ?l) }",
        true,
    ),
    ("SELECT ?s ?y WHERE { ?s ex:n ?x BIND(?x + 1 AS ?y) }", true),
    // A top-level offset without an order: the ids' rows, in the evaluator's order.
    ("SELECT ?s ?o WHERE { ?s ex:p ?o } OFFSET 2", true),
    (
        "SELECT ?s ?y WHERE { ?s ex:d ?x BIND(?x * 1.5 AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:f ?x BIND(?x * 2.5e0 AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x ; ex:d ?d BIND(-?x - ?d AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:d ?x BIND(ABS(?x) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x ; ex:f ?f BIND(?x * ?f AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(?nope + 1 AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x } BIND(?x * 2 AS ?y) }",
        true,
    ),
    ("SELECT ?s (?x * 3 AS ?y) WHERE { ?s ex:n ?x }", true),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(?x + 1 AS ?y) FILTER(?y > 3) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:f ?x ; ex:n ?n BIND(?x * ?n AS ?y) FILTER(?y > 50000 && ?y < 900000) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:d ?x BIND(?x * 1.21 AS ?y) FILTER(?y != 2.42 || BOUND(?x)) }",
        true,
    ),
    (
        "SELECT DISTINCT ?y WHERE { ?s ex:d ?x BIND(?x * 0 AS ?y) }",
        true,
    ),
    (
        "SELECT DISTINCT ?y WHERE { ?s ex:f ?x BIND(?x * 0 AS ?y) }",
        true,
    ),
    // `VALUES`, also of terms not in the store, and the counts it carries after a `GROUP BY`.
    (
        "SELECT * WHERE { VALUES (?s ?k) { (ex:s1 1) (ex:s2 1.0) (ex:nowhere \"x\"@en) } }",
        true,
    ),
    (
        "SELECT ?s ?o ?k WHERE { VALUES ?s { ex:s1 ex:s7 ex:nowhere } ?s ex:p ?o BIND(1 AS ?k) }",
        true,
    ),
    (
        "SELECT ?s ?x WHERE { VALUES ?x { 3 \"3\" 3.0 ex:o1 } ?s ex:val ?x }",
        true,
    ),
    (
        "SELECT ?s ?t WHERE { ?s a ex:C2 VALUES ?t { ex:a ex:b } }",
        true,
    ),
    (
        "SELECT ?s ?x WHERE { VALUES (?s ?x) { (ex:s0 UNDEF) (UNDEF 3) } }",
        true,
    ),
    (
        "SELECT ?o (COUNT(*) AS ?n) WHERE { ?s ex:p ?o } GROUP BY ?o",
        false,
    ),
    (
        "SELECT ?o (COUNT(*) AS ?n) WHERE { ?s ex:p ?o } GROUP BY ?o ORDER BY DESC(?n) ?o LIMIT 5",
        false,
    ),
    (
        "SELECT ?s ?x WHERE { VALUES (?s ?x) { (ex:s0 UNDEF) (UNDEF 3) } ?s ex:val ?x }",
        false,
    ),
    (
        "SELECT ?s ?v WHERE { VALUES ?v { 2.5e0 \"2.5000\"^^xsd:decimal } ?s ex:b ?b FILTER(?v = ?b) }",
        false,
    ),
    (
        "SELECT ?s ?v WHERE { ?s ex:b ?b VALUES ?v { 2.5e0 3 } FILTER(?b IN (?v)) }",
        false,
    ),
    (
        "SELECT ?s ?v WHERE { VALUES ?v { 2.5e0 \"2.50\"^^xsd:decimal } ?s ex:b ?v }",
        false,
    ),
    // An order on an expression the kernels compute: a column of its own, ordered on ids.
    (
        "SELECT ?s ?x WHERE { ?s ex:n ?x } ORDER BY DESC(?x * 2) ?s LIMIT 9",
        true,
    ),
    (
        "SELECT ?s ?x WHERE { ?s ex:n ?x } ORDER BY ABS(?x) DESC(?s) LIMIT 12 OFFSET 3",
        true,
    ),
    (
        "SELECT ?s ?d WHERE { ?s ex:d ?d } ORDER BY (?d - 1) ?s",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x } } ORDER BY (?x + 1) ?s",
        true,
    ),
    (
        "SELECT DISTINCT ?x WHERE { ?s ex:n ?x } ORDER BY DESC(-?x) LIMIT 5",
        true,
    ),
    (
        "SELECT ?s ?f WHERE { ?s ex:f ?f } ORDER BY DESC(?f * 1.1) ?s LIMIT 4",
        false,
    ),
    (
        "SELECT ?s WHERE { ?s ex:label ?l } ORDER BY UCASE(?l) ?s",
        false,
    ),
    // Strings and IRIs (`STR`, `CONCAT`, `IRI`), as terms of their own; blank nodes and other
    // literals errors where the evaluator's are.
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(STR(?o) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(STR(?x) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:r ?b BIND(STR(?b) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(CONCAT(?l, \"-\", STR(?s)) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(CONCAT(?l, ?l) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(CONCAT(?l, \"x\"@en) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(CONCAT(\"v:\", ?x) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?i WHERE { ?s ex:p ?o BIND(IRI(CONCAT(STR(?o), \"/part\")) AS ?i) }",
        true,
    ),
    (
        "SELECT ?s ?i WHERE { ?s ex:n ?x BIND(IRI(CONCAT(\"http://example.org/n/\", STR(?x * 2))) AS ?i) }",
        true,
    ),
    (
        "SELECT ?s ?i WHERE { ?s ex:p ?o BIND(IRI(?o) AS ?i) ?i ex:q ?v }",
        true,
    ),
    (
        "SELECT ?s ?i WHERE { ?s ex:label ?l BIND(IRI(?l) AS ?i) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o OPTIONAL { ?s ex:label ?l } BIND(CONCAT(STR(?o), ?l) AS ?y) }",
        false,
    ),
    // (The labels hold a direction-tagged string, whose concatenation is the evaluator's;
    // without it, on ids.)
    (
        "SELECT ?s ?y ?z WHERE { ?s ex:label ?l BIND(CONCAT(STR(?l), \"x\"@en) AS ?y) BIND(CONCAT(\"a\"@en, \"b\"@en, ?y) AS ?z) BIND(CONCAT(\"a\"@en, \"b\"@en) AS ?w) }",
        true,
    ),
    (
        "SELECT DISTINCT ?y WHERE { ?s ex:label ?l BIND(STR(?l) AS ?y) }",
        true,
    ),
    // Division: of integers and decimals a decimal (exact, or the evaluator's 24 digits, which
    // give up), of doubles a double, by zero an error; in filters and orders too.
    ("SELECT ?s ?y WHERE { ?s ex:n ?x BIND(?x / 2 AS ?y) }", true),
    (
        "SELECT ?s ?y WHERE { ?s ex:d ?x BIND(ROUND(?x * 1000) / 1000 AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(?x / 3 AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(?x / (?x - ?x) AS ?y) }",
        true,
    ),
    ("SELECT ?s ?y WHERE { ?s ex:f ?x BIND(?x / 4 AS ?y) }", true),
    (
        "SELECT ?s ?y WHERE { ?s ex:f ?x BIND(?x / 0 AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:d ?x ; ex:n ?n BIND(?x / ?n AS ?y) }",
        false,
    ),
    ("SELECT ?s WHERE { ?s ex:n ?x FILTER(?x / 4 > 100) }", true),
    (
        "SELECT ?s ?x WHERE { ?s ex:n ?x } ORDER BY DESC(?x / 8) ?s LIMIT 7",
        true,
    ),
    // `OPTIONAL { … FILTER(e) }`: on ids when the filter reads only what the right side
    // always binds or the left side never does.
    (
        "SELECT ?s ?x WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x FILTER(?x > 50) } }",
        true,
    ),
    (
        "SELECT ?s ?x WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x FILTER(?s != ex:s3 && ?x < 0) } }",
        true,
    ),
    (
        "SELECT ?s ?d WHERE { ?s ex:n ?n OPTIONAL { ?s ex:d ?d OPTIONAL { ?s ex:f ?x FILTER(?x > 0) } FILTER(!BOUND(?x)) } }",
        true,
    ),
    (
        "SELECT ?s ?x WHERE { ?s ex:d ?y OPTIONAL { ?s ex:n ?x FILTER(?x > ?y) } }",
        false,
    ),
    (
        "SELECT ?s ?o WHERE { ?s ex:n ?x OPTIONAL { ?s ex:p ?o OPTIONAL { ?s ex:d ?x } FILTER(BOUND(?x)) } }",
        false,
    ),
    (
        "SELECT ?s ?x WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x FILTER(?x * 2 > 100) } } ORDER BY ?s LIMIT 20",
        true,
    ),
    // `IF` and `COALESCE`: numbers to inline ids, other terms through the term kernels; only
    // the branch taken is computed, and an error in the condition is unbound.
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(IF(?x > 50, 1, 0) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x ; ex:d ?d BIND(IF(?x < 0 && ?d < -100, ?x * 2, ?d + 1) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(IF(?x > 50, \"big\", \"small\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(IF(!(?x = 0) || ?x > 9, ex:a, STR(?x)) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x } BIND(IF(BOUND(?x), ?x, -1) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x } BIND(IF(?x > 0, 1, 2) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x } BIND(IF(?x > 0 || ?o = ?o, 1, 2) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(IF(?x > 0, ?x / 3, 7) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y ?z WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x } BIND(COALESCE(?x, 0) AS ?y) BIND(COALESCE(?x * 2, 1.5) AS ?z) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o OPTIONAL { ?s ex:label ?l } BIND(COALESCE(?l, STR(?o)) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(COALESCE(?x / 0, ?x) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:n ?x FILTER(IF(?x > 0, ?x, -?x) > 3000) }",
        true,
    ),
    (
        "SELECT ?s (SUM(IF(?x > 0, 1, 0)) AS ?n) WHERE { ?s ex:n ?x } GROUP BY ?s",
        false,
    ),
    // A branch is the term it gives: a cast keeps its text (`7`, `1500.0`), a constant its
    // form (`2.50`), and a cast nested in arithmetic is a computed decimal (`7.0`).
    (
        "SELECT ?s ?y ?z WHERE { ?s ex:n ?x ; ex:f ?f BIND(IF(?x > 0, xsd:decimal(?x), xsd:decimal(?f * 2)) AS ?y) BIND(COALESCE(xsd:decimal(?x / 0), xsd:decimal(?x)) AS ?z) }",
        true,
    ),
    (
        "SELECT ?s ?y ?z WHERE { ?s ex:n ?x BIND(IF(?x > 0, \"2.50\"^^xsd:decimal, 1) AS ?y) BIND(COALESCE(?x / 0, \"007\"^^xsd:integer) AS ?z) }",
        true,
    ),
    (
        "SELECT ?s ?y ?z ?w WHERE { ?s ex:n ?x BIND(ABS(xsd:decimal(?x)) AS ?y) BIND(xsd:decimal(?x) * 1 AS ?z) BIND(STR(IF(?x > 0, xsd:decimal(?x), 0)) AS ?w) }",
        true,
    ),
    // Doubles rounded (halves up, -0.5 to -0), and `xsd:integer` and `xsd:decimal` of numbers.
    (
        "SELECT ?s ?a ?b ?c ?d WHERE { ?s ex:f ?x BIND(ROUND(?x) AS ?a) BIND(CEIL(?x) AS ?b) BIND(FLOOR(?x) AS ?c) BIND(ABS(?x) AS ?d) }",
        true,
    ),
    (
        "SELECT ?s ?y ?z WHERE { ?s ex:f ?x BIND(ROUND(?x - ?x - 0.5) AS ?y) BIND(CEIL(?x - ?x - 0.5) AS ?z) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:f ?x BIND(ROUND(?x * 1000) / 1000 AS ?y) FILTER(?y > 7.0) }",
        true,
    ),
    (
        "SELECT ?s ?a ?b ?c WHERE { ?s ex:f ?x ; ex:d ?d ; ex:n ?n BIND(xsd:integer(?x) AS ?a) BIND(xsd:integer(?d) AS ?b) BIND(xsd:integer(?n / 8) AS ?c) }",
        true,
    ),
    (
        "SELECT ?s ?a ?b ?c WHERE { ?s ex:f ?x ; ex:d ?d ; ex:n ?n BIND(xsd:decimal(?x) AS ?a) BIND(xsd:decimal(?n) AS ?b) BIND(xsd:decimal(?d * 3) AS ?c) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:f ?x BIND(xsd:decimal(?x / 3) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?t ?u WHERE { ?s ex:n ?n BIND(STR(xsd:decimal(?n)) AS ?t) BIND(xsd:decimal(?n) + 0 AS ?u) }",
        true,
    ),
    (
        "SELECT DISTINCT ?y WHERE { ?s ex:n ?n BIND(xsd:decimal(?n * 0) AS ?y) }",
        true,
    ),
    // The cast keeps the text: `7` of an integer, `1500.0` of a double; a whole decimal is
    // the evaluator's.
    (
        "SELECT DISTINCT ?y WHERE { ?s ex:f ?x BIND(xsd:decimal(?x * 2) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y ?z WHERE { ?s ex:n ?n BIND(xsd:decimal(?n * 2.5e0) AS ?y) BIND(STR(xsd:decimal(?n * 2e0)) AS ?z) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:d ?d BIND(xsd:decimal(ROUND(?d)) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:d ?d BIND(ROUND(xsd:decimal(?d * 2e0)) AS ?y) }",
        true,
    ),
    (
        "SELECT ?y WHERE { ?s ex:huge ?x BIND(xsd:integer(?x * 1.0e3) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(xsd:double(?x) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(xsd:integer(?l) AS ?y) }",
        false,
    ),
    // `REPLACE` with a constant pattern (and flags), `STRLANG`, `STRDT`.
    (
        "SELECT ?s ?i WHERE { ?s ex:p ?o BIND(IRI(REPLACE(STR(?o), \"example\", \"other\")) AS ?i) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(REPLACE(STR(?o), \"^(.*)/([a-z]+)$\", \"$2-$1\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(REPLACE(?x, \"A\", \"_\", \"i\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(REPLACE(?x, \".\", \"*\", \"q\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(REPLACE(?x, \"a\", ?x) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(REPLACE(?x, \"a\", \"b\", \"z\") AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(REPLACE(?x, STR(?x), \"b\") AS ?y) }",
        false,
    ),
    // A pattern or flags without variables, computed once.
    (
        "SELECT ?s ?i WHERE { ?s ex:p ?o BIND(IRI(CONCAT(STR(ex:), \"_x\", REPLACE(STR(?o), STR(ex:), \"\"))) AS ?i) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(REPLACE(?x, CONCAT(\"A\", \"|b\"), \"_\", CONCAT(\"i\")) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(REPLACE(?x, STR(ex:o1), \"_\", STR(ex:o1)) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(REPLACE(?x, UCASE(\"a\"), \"_\") AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(STRLANG(STR(?x), \"en\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(STRLANG(STR(?x), \"not a tag\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(STRDT(CONCAT(\"POINT(\", STR(?o), \")\"), <http://www.opengis.net/ont/geosparql#wktLiteral>) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(STRDT(STR(?x), ex:type) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(STRDT(CONCAT(\"0\", STR(?x)), xsd:integer) AS ?y) }",
        false,
    ),
    // Filters on such values (left to the evaluator: the filter kernels don't compare
    // strings they compute), a count of distinct ones and a join of a made geometry to a
    // stored one (on ids).
    (
        "SELECT ?o ?y WHERE { ?o ex:q ?v BIND(CONCAT(STR(?v), \"\") AS ?y) FILTER(?y = \"v3\") }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(CONCAT(STR(?o), \"/x\") AS ?y) FILTER(?y = \"http://example.org/o1/x\") }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(STR(?o) AS ?y) FILTER(STRSTARTS(?y, \"http://example.org/o1\")) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(STRLANG(STR(?l), \"nl\") AS ?y) FILTER(LANG(?y) = \"nl\") }",
        false,
    ),
    (
        "SELECT (COUNT(DISTINCT ?y) AS ?n) WHERE { ?s ex:p ?o BIND(REPLACE(STR(?o), \"[0-9]$\", \"\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?g WHERE { ?s ex:n ?x BIND(STRDT(CONCAT(\"POINT(\", STR(?x), \" 1)\"), <http://www.opengis.net/ont/geosparql#wktLiteral>) AS ?w) ?g ex:wkt ?w }",
        true,
    ),
    // Left to the evaluator: an order on a computed value, values the ids don't decide, a
    // string function the kernels don't have, an overflow.
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(?x * 2 AS ?y) } ORDER BY DESC(?y) ?s LIMIT 9",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(?x * 2 AS ?y) FILTER(BOUND(?y)) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(?x + 1 AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(UCASE(?l) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(?x + 9223372036854775807 AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:huge ?x BIND(?x + 1 AS ?y) }",
        false,
    ),
    // A constant whose lexical form is not the stored one (`2.5e0`, the store holds `2.5`):
    // compared by value, in a list, and joined; and copies of such a column.
    (
        "SELECT ?s ?v WHERE { ?s ex:b ?b BIND(2.5e0 AS ?v) FILTER(?v = ?b) }",
        false,
    ),
    (
        "SELECT ?s ?v WHERE { ?s ex:b ?b BIND(2.5e0 AS ?v) FILTER(?b IN (?v)) }",
        false,
    ),
    (
        "SELECT ?s ?v WHERE { ?s ex:b ?b BIND(2.5e0 AS ?v) FILTER(?b IN (?v, 7)) }",
        false,
    ),
    (
        "SELECT ?s ?t WHERE { ?s ex:b ?b BIND(2.5e0 AS ?v) ?t ex:b ?v }",
        false,
    ),
    (
        "SELECT ?s ?v WHERE { ?s ex:b ?b BIND(2.5e0 AS ?v) FILTER(?v = 2.5 && isNumeric(?v)) }",
        false,
    ),
    (
        "SELECT ?s ?v WHERE { ?s ex:b ?b BIND(2.5e0 AS ?v) FILTER(isLiteral(?v) && DATATYPE(?v) = xsd:double) }",
        false,
    ),
    (
        "SELECT ?s ?v WHERE { ?s ex:b ?b BIND(2.5 AS ?v) FILTER(?v = ?b) }",
        false,
    ),
    (
        "SELECT ?s ?c WHERE { ?s ex:b ?b BIND(?b AS ?c) FILTER(?c = 2.5e0) }",
        false,
    ),
    (
        "SELECT ?s ?c WHERE { ?s ex:b ?b BIND(?b AS ?c) FILTER(?c IN (2.5e0, 2.5)) }",
        false,
    ),
    (
        "SELECT ?s ?t WHERE { ?s ex:b ?b BIND(?b AS ?c) ?t ex:b ?x FILTER(?c = ?x) }",
        false,
    ),
    (
        "SELECT ?s ?t WHERE { ?s ex:b ?b BIND(?b AS ?c) ?t ex:b ?c }",
        false,
    ),
];

#[test]
fn binds_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    // Numbers of each kind, negative, zero and large ones among them.
    let mut numbers = String::from(
        "@prefix ex: <http://example.org/> . @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..400_i64 {
        let n = (i - 200) * 37;
        // Decimals and doubles in the forms the store inlines (no trailing zero, no exponent).
        numbers.push_str(&format!(
            "ex:s{i} ex:n {n} ; ex:d {}.{} ; ex:f \"{n}.5\"^^<http://www.w3.org/2001/XMLSchema#double> .\n",
            n / 7,
            i % 9 + 1,
        ));
    }
    for i in (0..400_i64).step_by(4) {
        let n = (i - 200) * 37;
        numbers.push_str(&format!(
            "ex:g{i} ex:wkt \"POINT({n} 1)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral> .\n"
        ));
    }
    numbers.push_str("ex:big ex:huge 9223372036854775000 .\n");
    // 2.5 as a double, a decimal and a non-canonical decimal, and another value.
    for (i, b) in ["\"2.5\"^^xsd:double", "2.5", "\"2.50\"^^xsd:decimal", "3"]
        .iter()
        .enumerate()
    {
        for j in 0..20 {
            numbers.push_str(&format!("ex:b{i}_{j} ex:b {b} .\n"));
        }
    }
    engine.load(RdfFormat::Turtle, numbers.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>";
    for &(q, on_ids) in BINDS {
        let ordered_query = q.contains("ORDER BY");
        let run = |executor: &str| {
            let q = format!("#pragma tt.executor {executor}\n{prefix} {q}");
            if ordered_query {
                ordered(&engine, &q)
            } else {
                rows(&engine, &q)
            }
        };
        assert_eq!(run("ids"), run("spareval"), "{q}");
        let plan = engine
            .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
            .unwrap();
        if on_ids {
            assert!(plan.contains(r#""executor":"ids""#), "not on ids: {q}");
        }
    }
}

/// Groups a count pass reads back from a table of ids (1,024 or more): the id executor answers
/// the query from that table (#11), with the evaluator's answers.
const GROUPS: &[(&str, bool)] = &[
    (
        "SELECT ?c (COUNT(*) AS ?n) WHERE { ?o ex:by ?c } GROUP BY ?c",
        true,
    ),
    (
        "SELECT ?c ?name (COUNT(*) AS ?n) WHERE { ?o ex:by ?c . ?c ex:name ?name } GROUP BY ?c ?name",
        true,
    ),
    (
        "SELECT ?c (SUM(?q) AS ?t) WHERE { ?o ex:by ?c ; ex:q ?q } GROUP BY ?c ORDER BY DESC(?t) ?c LIMIT 7",
        true,
    ),
    (
        "SELECT ?c (MAX(?q) AS ?m) (MIN(?q) AS ?l) WHERE { ?o ex:by ?c ; ex:q ?q } GROUP BY ?c",
        true,
    ),
    (
        "SELECT ?c ?n WHERE { { SELECT ?c (COUNT(*) AS ?n) WHERE { ?o ex:by ?c } GROUP BY ?c } FILTER(?n > 2) }",
        false,
    ),
    (
        "SELECT ?c ?k WHERE { { SELECT ?c (COUNT(*) AS ?n) WHERE { ?o ex:by ?c } GROUP BY ?c } BIND(?n + 1 AS ?k) } ORDER BY ?c LIMIT 9",
        false,
    ),
    (
        "SELECT ?c ?n ?name WHERE { { SELECT ?c (COUNT(*) AS ?n) WHERE { ?o ex:by ?c } GROUP BY ?c } OPTIONAL { ?c ex:name ?name } }",
        false,
    ),
    (
        "SELECT ?c ?x WHERE { { SELECT ?c WHERE { ?o ex:by ?c } } BIND(?o AS ?x) }",
        false,
    ),
    // Sub-selects with `DISTINCT`, an order and a slice inside.
    (
        "SELECT ?c ?q WHERE { { SELECT DISTINCT ?c WHERE { ?o ex:by ?c } } ?o2 ex:by ?c ; ex:q ?q }",
        true,
    ),
    (
        "SELECT ?c ?n WHERE { { SELECT ?c WHERE { ?o ex:by ?c } ORDER BY DESC(?c) LIMIT 5 } ?c ex:name ?n }",
        true,
    ),
    (
        "SELECT * WHERE { { SELECT ?o ?q WHERE { ?o ex:q ?q } ORDER BY ?q ?o LIMIT 3 OFFSET 2 } }",
        true,
    ),
    (
        "SELECT * WHERE { { SELECT ?o ?q WHERE { ?o ex:q ?q } ORDER BY DESC(?q) ?o } } ORDER BY ?o LIMIT 4",
        true,
    ),
    (
        "SELECT ?q (COUNT(*) AS ?n) WHERE { { SELECT DISTINCT ?c ?q WHERE { ?o ex:by ?c ; ex:q ?q } } } GROUP BY ?q",
        false,
    ),
    (
        "SELECT * WHERE { { SELECT ?o WHERE { ?o ex:q ?q } LIMIT 3 } }",
        false,
    ),
    (
        "SELECT * WHERE { { SELECT REDUCED ?q WHERE { ?o ex:q ?q } } }",
        true,
    ),
    // A zero-length path from a `VALUES` term the store doesn't hold: no node, no match.
    (
        "SELECT * WHERE { VALUES ?v { ex:nope ex:o1 } ?v ex:by* ?c }",
        false,
    ),
    (
        "SELECT * WHERE { VALUES ?v { ex:nope ex:o1 } ?v ex:by? ?c }",
        false,
    ),
];

#[test]
fn groups_read_back_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    let mut data = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..9000_u32 {
        let c = i % 2500;
        data.push_str(&format!("ex:o{i} ex:by ex:c{c} ; ex:q {} .\n", i % 17));
        if i < 2500 && i % 3 != 0 {
            data.push_str(&format!("ex:c{i} ex:name \"c{}\" .\n", i % 400));
        }
    }
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    for &(q, on_ids) in GROUPS {
        let run = |executor: &str| {
            let q = format!("#pragma tt.executor {executor}\n{prefix} {q}");
            if q.contains("ORDER BY") {
                ordered(&engine, &q)
            } else {
                rows(&engine, &q)
            }
        };
        assert_eq!(run("ids"), run("spareval"), "{q}");
        let plan = engine
            .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
            .unwrap();
        if on_ids {
            assert!(plan.contains(r#""executor":"ids""#), "not on ids: {q}");
        }
    }
}

#[test]
fn the_id_executor_answers_as_the_evaluator() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    check_executor(&engine);
    engine
        .update(
            "PREFIX ex: <http://example.org/> INSERT DATA { ex:o1 ex:q \"new\" . ex:s5 ex:r _:x . }
             ; DELETE DATA { ex:o4 ex:q \"v4\" }",
        )
        .unwrap();
    check_executor(&engine);
}

#[test]
fn rows_evaluated_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    check(&engine);
    // The rewrite is used: the plan reads a result table.
    let plan = engine
        .explain(
            "PREFIX ex: <http://example.org/> SELECT * WHERE { ?s ex:p ?o . MINUS { ?o ex:q ?v } }",
        )
        .unwrap();
    assert!(plan.contains("urn:tripletrail:table:"), "{plan}");
    // After writes, before they are folded into the indexes.
    engine
        .update(
            "PREFIX ex: <http://example.org/> INSERT DATA { ex:o1 ex:q \"new\" . ex:s5 ex:r _:x . }
             ; DELETE DATA { ex:o4 ex:q \"v4\" }",
        )
        .unwrap();
    check(&engine);
}

/// A long `IN` list of IRIs is one hash lookup per row (#63), not one comparison per item: it
/// keeps the same rows as a `VALUES` join and stays within a few times its time.
#[test]
fn long_in_lists_are_hash_sets() {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..60_000 {
        ttl.push_str(&format!("ex:s{i} ex:by ex:c{} .\n", i * 7 % 5000));
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    // 1,000 customers, some not in the store.
    let list: Vec<String> = (0..1000)
        .map(|i| format!("<http://example.org/c{}>", i * 6))
        .collect();
    let count = |q: &str| {
        let QueryResults::Solutions(mut s) = engine.query(q).unwrap() else {
            panic!()
        };
        s.next().unwrap().unwrap().get("n").unwrap().to_string()
    };
    let values = format!(
        "SELECT (COUNT(*) AS ?n) WHERE {{ VALUES ?c {{ {} }} ?s <http://example.org/by> ?c }}",
        list.join(" ")
    );
    let within = format!(
        "SELECT (COUNT(*) AS ?n) WHERE {{ ?s <http://example.org/by> ?c FILTER(?c IN ({})) }}",
        list.join(", ")
    );
    let outside = within.replace(" IN (", " NOT IN (");
    let all = count("SELECT (COUNT(*) AS ?n) WHERE { ?s <http://example.org/by> ?c }");
    let (expected, got) = (count(&values), count(&within));
    assert_eq!(expected, got);
    let number = |n: &str| n.split('"').nth(1).unwrap().parse::<u64>().unwrap();
    assert_eq!(number(&all) - number(&expected), number(&count(&outside)));
    let time = |q: &str| {
        let start = std::time::Instant::now();
        for _ in 0..3 {
            count(q);
        }
        start.elapsed()
    };
    let (join, filter) = (time(&values), time(&within));
    // One comparison per item took about 85 times the join.
    assert!(
        filter < join * 10 + std::time::Duration::from_millis(50),
        "IN {filter:?}, VALUES {join:?}"
    );
}
