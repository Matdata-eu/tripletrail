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
    // `OPTIONAL { … FILTER(e) }` on ids: before the join when the filter reads only what the
    // right side always binds or the left side never does, on the joined rows otherwise.
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
        true,
    ),
    (
        "SELECT ?s ?o WHERE { ?s ex:n ?x OPTIONAL { ?s ex:p ?o OPTIONAL { ?s ex:d ?x } FILTER(BOUND(?x)) } }",
        true,
    ),
    // A left value that may be unbound, an error (a comparison with an unbound value), a filter
    // with `EXISTS`, rows of which only some pass, and no shared variable.
    (
        "SELECT ?s ?x ?y WHERE { ?s ex:p ?o OPTIONAL { ?s ex:d ?y } OPTIONAL { ?s ex:n ?x FILTER(!BOUND(?y) || ?x > ?y) } }",
        true,
    ),
    (
        "SELECT ?s ?x ?y WHERE { ?s ex:p ?o OPTIONAL { ?s ex:d ?y } OPTIONAL { ?s ex:n ?x FILTER(?x > ?y) } }",
        true,
    ),
    (
        "SELECT ?s ?x WHERE { ?s ex:d ?y OPTIONAL { ?s ex:n ?x FILTER(?x > ?y && EXISTS { ?s ex:p ?o }) } }",
        true,
    ),
    (
        "SELECT ?s ?x WHERE { ?s ex:d ?y OPTIONAL { ?t ex:n ?x FILTER(?x = ?y + 3) } }",
        true,
    ),
    (
        "SELECT (COUNT(*) AS ?n) (COUNT(?x) AS ?m) WHERE { ?s ex:d ?y OPTIONAL { ?s ex:n ?x FILTER(?x < ?y) } }",
        true,
    ),
    (
        "SELECT ?s ?x WHERE { ?s ex:d ?y OPTIONAL { ?s ex:n ?x FILTER(?x > ?y) } } ORDER BY DESC(?x) ?s LIMIT 9",
        true,
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
        true,
    ),
    // `LANG`, `DATATYPE`, `isIRI`, `isBlank`, `isLiteral`.
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(LANG(?l) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(LANG(?o) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(LANG(?x) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(DATATYPE(?l) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(DATATYPE(?x * 2) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(DATATYPE(?x) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x FILTER(!isBlank(?x) && !isIRI(?x) && DATATYPE(?x) != xsd:byte) BIND(DATATYPE(?x) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(DATATYPE(STR(?o)) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?a ?b ?c WHERE { ?s ex:val ?x BIND(isIRI(?x) AS ?a) BIND(isBlank(?x) AS ?b) BIND(isLiteral(?x) AS ?c) }",
        true,
    ),
    (
        "SELECT ?s ?a WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x } BIND(isLiteral(?x) AS ?a) }",
        true,
    ),
    (
        "SELECT ?s ?a WHERE { ?s ex:p ?o BIND(IF(isIRI(?o), STR(?o), \"-\") AS ?a) }",
        false,
    ),
    // Parts of date-times (#11): `SECONDS` an `xsd:decimal` with its milliseconds. On a date
    // or a number the time parts are left to the evaluator.
    (
        "SELECT ?s ?y WHERE { ?s ex:t ?x BIND(SECONDS(?x) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:day ?x BIND(SECONDS(?x) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?h ?m ?y WHERE { ?s ex:t ?x BIND(HOURS(?x) AS ?h) BIND(MINUTES(?x) AS ?m) BIND(YEAR(?x) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:t ?x BIND(SECONDS(?x) * 1000 AS ?y) FILTER(SECONDS(?x) > 30) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(SECONDS(?x) AS ?y) }",
        false,
    ),
    // Tests of strings as `xsd:boolean`s: of a label, of a computed string, of terms that are
    // no strings (an error, so unbound), with constant patterns.
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(STRSTARTS(?l, \"pl\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(CONTAINS(LCASE(?l), \"1\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(STRENDS(STR(?l), \"3\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(REGEX(?l, \"^P.*[0-4]$\", \"i\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(STRSTARTS(?o, \"http\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(STRSTARTS(STR(?o), \"http://example.org/o1\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(CONTAINS(?x, \"2\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o OPTIONAL { ?s ex:label ?l } BIND(IF(CONTAINS(?l, \"x\"), 1, 0) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(STRSTARTS(?l, \"l\"@en) AS ?y) }",
        false,
    ),
    // Conditions on numbers as `xsd:boolean`s.
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(?x > 50 AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x ; ex:d ?d BIND(?x < 0 && ?d < -100 AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(!(?x = 3) || ?x < 10 AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x } BIND(?x > 5 AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x } BIND(BOUND(?x) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(?x > 2 AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(STR(?x > 50) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(CONCAT(STR(?x >= 7), \"!\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?y (COUNT(*) AS ?c) WHERE { ?s ex:n ?x BIND(?x > 50 AS ?y) } GROUP BY ?y",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:n ?x BIND(?x > 50 AS ?y) FILTER(?y) }",
        true,
    ),
    // The effective boolean value of a column.
    ("SELECT ?s WHERE { ?s ex:val ?x FILTER(?x) }", false),
    ("SELECT ?s WHERE { ?s ex:val ?x FILTER(!?x) }", false),
    ("SELECT ?s WHERE { ?s ex:n ?x FILTER(?x) }", true),
    (
        "SELECT ?s WHERE { ?s ex:label ?l FILTER(?l || BOUND(?l)) }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:p ?o BIND(STRAFTER(STR(?o), \"o1\") AS ?y) FILTER(?y) }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:n ?x ; ex:d ?d BIND(?x * ?d AS ?y) FILTER(?y) }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:p ?o OPTIONAL { ?s ex:n ?x } FILTER(!?x) }",
        true,
    ),
    // Hashes and `ENCODE_FOR_URI`.
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(MD5(STR(?o)) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(SHA1(?l) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(SHA256(?x) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(SHA384(STR(?o)) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(IRI(CONCAT(STR(ex:), \"h/\", SHA512(STR(?o)))) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(ENCODE_FOR_URI(STR(?o)) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(ENCODE_FOR_URI(CONCAT(STR(?o), \" é/?\")) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(ENCODE_FOR_URI(?x) AS ?y) }",
        true,
    ),
    // Filters on the strings the term kernels compute.
    (
        "SELECT ?s WHERE { ?s ex:p ?o FILTER(STRAFTER(STR(?o), STR(ex:)) = \"o17\") }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(STRAFTER(STR(?o), \"http://example.org/\") AS ?y) FILTER(?y = \"o3\") }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:label ?l FILTER(LCASE(?l) = \"l10\") }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:label ?l FILTER(UCASE(STR(?l)) = \"L10\") }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:label ?l FILTER(\"plain3\" = SUBSTR(?l, 1)) }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:label ?l FILTER(LCASE(?l) != \"plain3\") }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:val ?x FILTER(!(UCASE(?x) = \"X\")) }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:val ?x FILTER(STRDT(STR(?x), ex:dt) = \"x\") }",
        false,
    ),
    (
        "SELECT ?s WHERE { ?s ex:p ?o FILTER(IRI(STR(?o)) = \"x\") }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:p ?o FILTER(CONTAINS(STR(?o), \"o1\")) }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:p ?o FILTER(STRENDS(STR(?o), \"9\")) }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:label ?l FILTER(STRSTARTS(UCASE(?l), \"PLAIN1\")) }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:label ?l FILTER(REGEX(LCASE(?l), \"^l[0-9]0$\")) }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:val ?x FILTER(!CONTAINS(STR(?x), \"3\")) }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:p ?o FILTER(!CONTAINS(IRI(STR(?o)), \"3\")) }",
        true,
    ),
    (
        "SELECT ?s WHERE { ?s ex:val ?x FILTER(CONTAINS(COALESCE(?x, \"a\"), \"3\") || STRLEN(STR(?x)) > 2) }",
        false,
    ),
    // `UCASE`, `LCASE`, `STRLEN`, `SUBSTR` (integer constants), `STRBEFORE`, `STRAFTER`.
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(UCASE(?l) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(LCASE(UCASE(STR(?o))) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(UCASE(?x) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(STRLEN(?l) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(STRLEN(STR(?o)) + 1 AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(SUBSTR(?l, 2) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(SUBSTR(?l, 0, 3) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(SUBSTR(STR(?o), 20, 100) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(SUBSTR(?l, 1, -1) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(SUBSTR(?l, 1.5) AS ?y) }",
        false,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(STRAFTER(?l, \"l\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(STRBEFORE(?l, \"1\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(STRBEFORE(?l, \"\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(STRAFTER(?l, \"l\"@en) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:p ?o BIND(STRAFTER(STR(?o), STR(ex:)) AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:val ?x BIND(STRBEFORE(?x, \"0\") AS ?y) }",
        true,
    ),
    (
        "SELECT ?s ?y WHERE { ?s ex:label ?l BIND(STRAFTER(?l, ?l) AS ?y) }",
        true,
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
    // Date-times with and without milliseconds and zones, and dates.
    for i in 0..120_i64 {
        let tz = ["", "Z", "+02:00", "-05:30"][i as usize % 4];
        let ms = ["", ".5", ".125", ".007"][i as usize / 4 % 4];
        numbers.push_str(&format!(
            "ex:s{i} ex:t \"2020-{:02}-{:02}T{:02}:{:02}:{:02}{ms}{tz}\"^^xsd:dateTime ; ex:day \"2020-01-{:02}\"^^xsd:date .\n",
            1 + i % 12,
            1 + i % 28,
            i % 24,
            (i * 7) % 60,
            (i * 13) % 60,
            1 + i % 28,
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
    // Over a `BIND` with few distinct inputs: counted per input first, the expression then
    // computed per input (#11).
    (
        "SELECT ?k (COUNT(*) AS ?n) WHERE { ?o ex:q ?q BIND(?q * 2 AS ?k) } GROUP BY ?k",
        false,
    ),
    (
        "SELECT ?k (SUM(?q) AS ?t) (MAX(?q) AS ?m) WHERE { ?o ex:q ?q BIND(?q - 3 AS ?k) FILTER(?k > 1) } GROUP BY ?k",
        false,
    ),
    (
        "SELECT ?c (COUNT(DISTINCT ?k) AS ?n) WHERE { ?o ex:by ?c ; ex:q ?q BIND(?q / 2 AS ?k) } GROUP BY ?c",
        false,
    ),
    // `GROUP_CONCAT` of strings, in order of their text on ids (SPARQL leaves the order open;
    // the evaluator's follows its plan); anything else is the evaluator's.
    (
        "SELECT ?c (GROUP_CONCAT(?l) AS ?g) WHERE { ?o ex:by ?c ; ex:label ?l } GROUP BY ?c",
        true,
    ),
    (
        "SELECT ?c (GROUP_CONCAT(DISTINCT ?l ; SEPARATOR = \", \") AS ?g) (COUNT(*) AS ?n) WHERE { ?o ex:by ?c ; ex:label ?l } GROUP BY ?c",
        true,
    ),
    (
        "SELECT ?c (GROUP_CONCAT(?name ; SEPARATOR = \"\") AS ?g) WHERE { ?o ex:by ?c . ?c ex:name ?name } GROUP BY ?c",
        true,
    ),
    (
        "SELECT (GROUP_CONCAT(DISTINCT ?l ; SEPARATOR = \"|\") AS ?g) WHERE { ?o ex:label ?l }",
        true,
    ),
    (
        "SELECT ?l (GROUP_CONCAT(?l) AS ?g) WHERE { ?o ex:label ?l } GROUP BY ?l",
        true,
    ),
    (
        "SELECT ?c (GROUP_CONCAT(?l) AS ?g) WHERE { ?c ex:name ?name . ?o ex:by ?c ; ex:label ?l ; ex:q ?q FILTER(?q > 3) } GROUP BY ?c",
        true,
    ),
    (
        "SELECT ?c (GROUP_CONCAT(?l ; SEPARATOR = \"/\") AS ?g) WHERE { { ?o ex:by ?c ; ex:label ?l } UNION { ?c ex:name ?l } } GROUP BY ?c",
        true,
    ),
    (
        "SELECT ?c (GROUP_CONCAT(?l) AS ?g) WHERE { ?o ex:by ?c OPTIONAL { ?o ex:other ?x } ?o ex:label ?l } GROUP BY ?c",
        true,
    ),
    (
        "SELECT ?c (GROUP_CONCAT(?q) AS ?g) WHERE { ?o ex:by ?c ; ex:q ?q } GROUP BY ?c",
        false,
    ),
    (
        "SELECT ?c (GROUP_CONCAT(?o) AS ?g) WHERE { ?o ex:by ?c } GROUP BY ?c",
        false,
    ),
    (
        "SELECT ?c (GROUP_CONCAT(?x) AS ?g) WHERE { ?o ex:by ?c ; ex:label ?l OPTIONAL { ?o ex:other ?x } } GROUP BY ?c",
        false,
    ),
    // With many distinct inputs (3 rows each) and an expression the kernels compute: per row
    // on ids, the groups counted there.
    (
        "SELECT ?k (COUNT(*) AS ?n) WHERE { ?o ex:v ?v BIND(?v / 10 AS ?k) } GROUP BY ?k",
        true,
    ),
    (
        "SELECT ?k (COUNT(*) AS ?n) (SUM(?v) AS ?t) WHERE { ?o ex:v ?v } GROUP BY (?v * 2 AS ?k)",
        true,
    ),
    // Whole decimals the kernels make are written either way (`3` or `3.0`): the evaluator's.
    (
        "SELECT ?k (COUNT(*) AS ?n) (SUM(?v) AS ?t) WHERE { ?o ex:v ?v } GROUP BY (FLOOR(?v / 7) AS ?k)",
        false,
    ),
    (
        "SELECT ?k (COUNT(*) AS ?n) WHERE { ?o ex:v ?v BIND(?v - 3 AS ?k) FILTER(?k > 100) } GROUP BY ?k",
        true,
    ),
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
        let label = match i % 4 {
            0 => format!("\"l{}\"", i % 5),
            1 => format!("\"l{}\"@en", i % 3),
            2 => format!("\"m{}\"", i % 7),
            _ => format!("\"n{}\"@en--ltr", i % 2),
        };
        data.push_str(&format!("ex:o{i} ex:label {label} .\n"));
        if i % 11 == 0 {
            data.push_str(&format!("ex:o{i} ex:other {} .\n", i % 13));
        }
        data.push_str(&format!("ex:o{i} ex:v {} .\n", i % 3000));
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
        // The strings of a `GROUP_CONCAT` may come in another order than the evaluator's:
        // compared as the characters of each row.
        let same = |mut rows: Vec<String>| {
            if q.contains("GROUP_CONCAT") {
                for r in &mut rows {
                    let mut chars: Vec<char> = r.chars().collect();
                    chars.sort_unstable();
                    *r = chars.into_iter().collect();
                }
                rows.sort();
            }
            rows
        };
        assert_eq!(same(run("ids")), same(run("spareval")), "{q}");
        // And the evaluator alone, without the count passes both executors run.
        assert_eq!(
            same(run("ids")),
            same(run("spareval\n#pragma tt.id-rewrites off")),
            "{q}"
        );
        let plan = engine
            .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
            .unwrap();
        if on_ids {
            assert!(plan.contains(r#""executor":"ids""#), "not on ids: {q}");
        }
    }
    // On ids, the strings of a group in order of their text, whatever the plan.
    for q in [
        "SELECT (GROUP_CONCAT(DISTINCT ?l ; SEPARATOR = \"|\") AS ?g) WHERE { ?o ex:label ?l }",
        "SELECT (GROUP_CONCAT(DISTINCT ?l ; SEPARATOR = \"|\") AS ?g) WHERE { ?o ex:label ?l ; ex:by ?c }",
        "SELECT (GROUP_CONCAT(DISTINCT ?l ; SEPARATOR = \"|\") AS ?g) WHERE { ?o ex:by ?c ; ex:label ?l }",
    ] {
        let [row] = ordered(&engine, &format!("#pragma tt.executor ids\n{prefix} {q}"))
            .try_into()
            .unwrap();
        let text = row.split('"').nth(1).unwrap().to_owned();
        let parts: Vec<&str> = text.split('|').collect();
        assert!(parts.len() > 3 && parts.is_sorted(), "{q}: {text}");
    }
}

/// The casts to `xsd:string`, `xsd:boolean`, `xsd:double`, `xsd:float`, `xsd:integer` and
/// `xsd:dateTime` on ids give the evaluator's terms, for every kind of argument.
#[test]
fn casts_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    let forms = [
        "\"-10.2E3\"",
        "\"+33.3300\"",
        "\"0.0\"",
        "\"0\"",
        "\"0E1\"",
        "\"1.5\"",
        "\"1\"",
        "\"1E0\"",
        "\"13\"",
        "\" 13 \"",
        "\"2002-10-10T17:00:00Z\"",
        "\"false\"",
        "\"true\"",
        "\"TRUE\"",
        "\"INF\"",
        "\"NaN\"",
        "\"abc\"",
        "\"\"",
        "\"3\"^^xsd:integer",
        "\"-0\"^^xsd:integer",
        "\"0012\"^^xsd:integer",
        "\"1e3\"^^xsd:integer",
        "\"99999999999999999999999\"^^xsd:integer",
        "\"+33.3300\"^^xsd:decimal",
        "\"3.0\"^^xsd:decimal",
        "\"-0.5\"^^xsd:decimal",
        "\"1e3\"^^xsd:decimal",
        "\"1E0\"^^xsd:double",
        "\"-0.0E0\"^^xsd:double",
        "\"INF\"^^xsd:double",
        "\"NaN\"^^xsd:double",
        "\"1.5e300\"^^xsd:double",
        "\"100.0\"^^xsd:double",
        "\"1.5\"^^xsd:float",
        "\"57.90\"^^xsd:decimal",
        "\"1\"^^xsd:double",
        "\"1e0\"^^xsd:double",
        "\"1.0\"^^xsd:double",
        "\"1\"^^xsd:boolean",
        "\"false\"^^xsd:boolean",
        "true",
        "\"2002-10-10T17:00:00\"^^xsd:dateTime",
        "\"2002-10-10T17:00:00+02:00\"^^xsd:dateTime",
        "\"2002-10-10\"^^xsd:date",
        "\"300\"^^xsd:byte",
        "\"-7\"^^xsd:byte",
        "\"12\"^^xsd:unsignedShort",
        "\"x\"^^<http://example.org/dt>",
        "\"chat\"@fr",
        "\"1\"@en",
        "<http://example.org/iri>",
        "_:b",
    ];
    let mut data = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for (i, f) in forms.iter().enumerate() {
        for j in 0..3 {
            data.push_str(&format!("ex:s{i}_{j} ex:v {f} .\n"));
        }
    }
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>";
    for cast in [
        "string", "boolean", "double", "float", "integer", "dateTime",
    ] {
        for (q, on_ids) in [
            (
                format!("SELECT ?s ?y WHERE {{ ?s ex:v ?v BIND(xsd:{cast}(?v) AS ?y) }}"),
                true,
            ),
            (
                format!("SELECT ?s ?y WHERE {{ ?s ex:v ?v BIND(xsd:{cast}(STR(?v)) AS ?y) }}"),
                true,
            ),
            (
                format!(
                    "SELECT ?s ?y WHERE {{ ?s ex:v ?v BIND(xsd:{cast}(UCASE(STR(?v))) AS ?y) }}"
                ),
                true,
            ),
            (
                format!(
                    "SELECT ?s ?y WHERE {{ ?s ex:v ?v BIND(xsd:{cast}(COALESCE(?o, ?v)) AS ?y) }}"
                ),
                false,
            ),
            (
                format!(
                    "SELECT ?s ?y WHERE {{ ?s ex:v ?v BIND(xsd:{cast}(IF(isLiteral(?v), ?v, \"x\")) AS ?y) }}"
                ),
                false,
            ),
            (
                format!(
                    "SELECT (COUNT(DISTINCT ?y) AS ?n) WHERE {{ ?s ex:v ?v BIND(xsd:{cast}(COALESCE(?o, ?v)) AS ?y) }}"
                ),
                false,
            ),
            (
                format!(
                    "SELECT ?y (COUNT(*) AS ?n) WHERE {{ ?s ex:v ?v BIND(xsd:{cast}(COALESCE(?o, ?v)) AS ?y) }} GROUP BY ?y"
                ),
                false,
            ),
            (
                format!("SELECT ?s WHERE {{ ?s ex:v ?v FILTER(STR(xsd:{cast}(?v)) = \"1\") }}"),
                false,
            ),
            (
                format!(
                    "SELECT (COUNT(DISTINCT ?y) AS ?n) WHERE {{ ?s ex:v ?v BIND(xsd:{cast}(?v) AS ?y) }}"
                ),
                false,
            ),
            (
                format!(
                    "SELECT ?y (COUNT(*) AS ?n) WHERE {{ ?s ex:v ?v BIND(xsd:{cast}(?v) AS ?y) }} GROUP BY ?y"
                ),
                false,
            ),
        ] {
            let run = |executor: &str| {
                rows(
                    &engine,
                    &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
                )
            };
            assert_eq!(run("ids"), run("spareval"), "{q}");
            if on_ids {
                let plan = engine
                    .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
                    .unwrap();
                assert!(plan.contains(r#""executor":"ids""#), "not on ids: {q}");
            }
        }
    }
}

/// The triples of a `CONSTRUCT`, blank nodes written `_:`, sorted, and how many distinct blank
/// nodes they hold.
fn triples(engine: &Engine, query: &str) -> (Vec<String>, usize) {
    let QueryResults::Graph(triples) = engine.query(query).unwrap() else {
        panic!("not a CONSTRUCT")
    };
    let mut blanks = std::collections::HashSet::new();
    let mut out: Vec<String> = triples
        .map(|t| {
            let t = t.unwrap();
            let mut part = |x: String, b: Option<String>| match b {
                Some(b) => {
                    blanks.insert(b);
                    "_:".to_owned()
                }
                None => x,
            };
            let s = part(
                t.subject.to_string(),
                match &t.subject {
                    oxigraph::model::NamedOrBlankNode::BlankNode(b) => Some(b.to_string()),
                    _ => None,
                },
            );
            let o = part(
                t.object.to_string(),
                match &t.object {
                    oxigraph::model::Term::BlankNode(b) => Some(b.to_string()),
                    _ => None,
                },
            );
            format!("{s} {} {o}", t.predicate)
        })
        .collect();
    out.sort();
    (out, blanks.len())
}

/// `CONSTRUCT` on ids gives the evaluator's triples: unbound parts and literal subjects or
/// non-IRI predicates left out, new blank nodes per solution, each triple without a blank node
/// once.
#[test]
fn constructs_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    for (q, on_ids) in [
        ("CONSTRUCT { ?s ex:copy ?o } WHERE { ?s ex:p ?o }", true),
        (
            "CONSTRUCT { ?s ex:copy ?o } WHERE { ?s ex:p ?o } ORDER BY ?s ?o LIMIT 5",
            false,
        ),
        (
            "CONSTRUCT { ?o ex:from ?s . ?o a ex:Target } WHERE { ?s ex:p ?o }",
            true,
        ),
        ("CONSTRUCT { ?s a ?c . ?s a ex:C1 } WHERE { ?s a ?c }", true),
        (
            "CONSTRUCT { _:n ex:of ?s ; ex:v ?x } WHERE { ?s ex:val ?x }",
            true,
        ),
        ("CONSTRUCT { ?x ex:back ?s } WHERE { ?s ex:val ?x }", true),
        ("CONSTRUCT { ?s ?x 1 } WHERE { ?s ex:val ?x }", true),
        (
            "CONSTRUCT { ?s ex:v2 ?x } WHERE { ?s ex:p ?o OPTIONAL { ?s ex:val ?x } }",
            true,
        ),
        (
            "CONSTRUCT { ?s ex:up ?n } WHERE { ?s ex:p ?o BIND(UCASE(STR(?o)) AS ?n) }",
            true,
        ),
        (
            "CONSTRUCT { ?s ex:tag \"UNIT\" } WHERE { ?s ex:p ?o BIND(\"UNIT\" AS ?n) }",
            true,
        ),
        (
            "CONSTRUCT { ?s ex:r ?b . ?b ex:inv ?s } WHERE { ?s ex:r ?b }",
            true,
        ),
        ("CONSTRUCT WHERE { ?s ex:label ?l }", true),
        (
            "CONSTRUCT { ?s ex:q ?v } WHERE { ?s ex:p ?o . ?o ex:q ?v FILTER(?v != \"v3\") }",
            true,
        ),
    ] {
        let run = |executor: &str| {
            triples(
                &engine,
                &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
            )
        };
        assert_eq!(run("ids"), run("spareval"), "{q}");
        if on_ids {
            let plan = engine
                .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
                .unwrap();
            assert!(plan.contains(r#""executor":"ids""#), "not on ids: {q}");
        }
    }
}

/// `MIN` and `MAX` of decimals and doubles, alone and mixed with integers and other terms,
/// on ids are the evaluator's, ties of equal values written differently included.
#[test]
fn extremes_of_numbers_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    let numbers = [
        "\"3\"^^xsd:integer",
        "\"0012\"^^xsd:integer",
        "\"-4\"^^xsd:integer",
        "\"3.0\"^^xsd:decimal",
        "\"3\"^^xsd:decimal",
        "\"3.00\"^^xsd:decimal",
        "\"-0.5\"^^xsd:decimal",
        "\"12.25\"^^xsd:decimal",
        "\"+7.10\"^^xsd:decimal",
        "\"123456789012345678.123\"^^xsd:decimal",
        "\"3e0\"^^xsd:double",
        "\"3.0E0\"^^xsd:double",
        "\"-0.0e0\"^^xsd:double",
        "\"0.0e0\"^^xsd:double",
        "\"1.5e300\"^^xsd:double",
        "\"INF\"^^xsd:double",
        "\"-INF\"^^xsd:double",
        "\"NaN\"^^xsd:double",
        "\"12.25e0\"^^xsd:double",
        "\"1.5\"^^xsd:float",
    ];
    let others = [
        "\"abc\"",
        "<http://example.org/x>",
        "true",
        "\"1\"@en",
        "\"2002-10-10T17:00:00Z\"^^xsd:dateTime",
    ];
    let mut data = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    // Groups of two to four numbers (each value with each), and groups with another term.
    let mut g = 0;
    for (i, a) in numbers.iter().enumerate() {
        for b in &numbers[i..] {
            data.push_str(&format!("ex:g{g} ex:v {a}, {b} .\n"));
            data.push_str(&format!(
                "ex:h{g} ex:v {a}, {b}, \"7.5\"^^xsd:decimal, \"2\"^^xsd:integer .\n"
            ));
            g += 1;
        }
        for o in others {
            data.push_str(&format!("ex:o{i}_{} ex:v {a}, {o} .\n", o.len()));
        }
    }
    for i in 0..300 {
        data.push_str(&format!(
            "ex:d{} ex:amount \"{}.{}\"^^xsd:decimal .\n",
            i % 40,
            i * 7 % 1000,
            i % 100
        ));
        data.push_str(&format!(
            "ex:d{} ex:weight \"{}.5e{}\"^^xsd:double .\n",
            i % 40,
            i % 90,
            i % 5
        ));
    }
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>";
    for (q, on_ids) in [
        (
            "SELECT ?s (MIN(?v) AS ?lo) (MAX(?v) AS ?hi) WHERE { ?s ex:v ?v } GROUP BY ?s",
            false,
        ),
        (
            "SELECT ?d (MIN(?a) AS ?lo) (MAX(?a) AS ?hi) WHERE { ?d ex:amount ?a } GROUP BY ?d",
            true,
        ),
        (
            "SELECT ?d (MAX(?w) AS ?hi) (COUNT(*) AS ?n) WHERE { ?d ex:weight ?w } GROUP BY ?d",
            true,
        ),
        (
            "SELECT (MAX(?a) AS ?hi) (MIN(?a) AS ?lo) WHERE { ?d ex:amount ?a }",
            true,
        ),
        ("SELECT (MAX(?w) AS ?hi) WHERE { ?d ex:weight ?w }", true),
        (
            "SELECT (MAX(?v) AS ?hi) (MIN(?v) AS ?lo) WHERE { ?s ex:v ?v FILTER(isNumeric(?v)) }",
            false,
        ),
    ] {
        let run = |executor: &str| {
            rows(
                &engine,
                &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
            )
        };
        assert_eq!(run("ids"), run("spareval"), "{q}");
        if on_ids {
            let plan = engine
                .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
                .unwrap();
            assert!(plan.contains(r#""executor":"ids""#), "not on ids: {q}");
        }
    }
}

/// Joins, `OPTIONAL` and `MINUS` on variables a side may leave unbound (`VALUES` with
/// `UNDEF`, a value an `OPTIONAL` may not give): compatible rows pair up, an unbound value
/// takes the other side's.
#[test]
fn joins_on_maybe_unbound_variables_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    let mut data = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..200 {
        data.push_str(&format!("ex:s{i} ex:name \"A{}\" .\n", i % 50));
        if i % 7 != 0 {
            data.push_str(&format!("ex:s{i} ex:age {} .\n", 20 + i % 20));
        }
        if i % 4 == 0 {
            data.push_str(&format!("ex:s{i} ex:knows ex:s{} .\n", i * 3 % 230));
        }
    }
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    for q in [
        "SELECT * WHERE { ?s ex:name ?n . ?s ex:age ?a } \
         VALUES (?n ?a) { (\"A1\" UNDEF) (UNDEF 30) (\"A2\" 22) (UNDEF UNDEF) }",
        "SELECT * WHERE { ?s ex:name ?n OPTIONAL { ?s ex:knows ?k } } VALUES ?k { ex:s3 ex:s9 }",
        "SELECT * WHERE { ?s ex:name ?n OPTIONAL { ?s ex:knows ?k } OPTIONAL { ?k ex:age ?a } }",
        "SELECT * WHERE { ?s ex:name ?n OPTIONAL { ?s ex:age ?a } \
         OPTIONAL { ?s ex:knows ?k . ?k ex:age ?a } }",
        "SELECT ?s ?t WHERE { { ?s ex:name \"A3\" OPTIONAL { ?s ex:age ?a } } ?t ex:age ?a }",
        "SELECT * WHERE { ?s ex:name ?n OPTIONAL { ?s ex:age ?a } \
         MINUS { ?x ex:age ?a FILTER(?a > 35) } }",
        "SELECT * WHERE { ?s ex:name ?n \
         MINUS { VALUES (?s ?n) { (ex:s1 UNDEF) (UNDEF \"A3\") (UNDEF UNDEF) } } }",
        "SELECT * WHERE { ?s ex:name ?n OPTIONAL { ?s ex:age ?a } \
         MINUS { ?s ex:knows ?k OPTIONAL { ?k ex:age ?a } } }",
        "SELECT * WHERE { VALUES (?x ?y) { (1 UNDEF) (UNDEF 2) (3 4) } \
         VALUES (?x ?y) { (1 2) (UNDEF 3) (3 UNDEF) (5 6) } }",
    ] {
        let run = |executor: &str| {
            rows(
                &engine,
                &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
            )
        };
        let expected = run("spareval");
        assert!(!expected.is_empty(), "{q}");
        assert_eq!(run("ids"), expected, "{q}");
        let plan = engine
            .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
            .unwrap();
        assert!(plan.contains(r#""executor":"ids""#), "not on ids: {q}");
    }
}

/// `AVG` of integers and decimals on ids is the evaluator's decimal quotient (exact, else 24
/// fraction digits, as Jena), of doubles a double.
#[test]
fn averages_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    let mut data = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..400_i64 {
        let g = i % 37;
        data.push_str(&format!(
            "ex:i{i} ex:g ex:g{g} ; ex:n {} .\n",
            i * 7 % 101 - 30
        ));
        data.push_str(&format!(
            "ex:i{i} ex:d \"{}.{:02}\"^^xsd:decimal .\n",
            i % 53 - 10,
            i * 13 % 100
        ));
        if i % 3 == 0 {
            data.push_str(&format!("ex:i{i} ex:m {} .\n", i % 11));
        } else {
            data.push_str(&format!("ex:i{i} ex:m \"{}.5\"^^xsd:decimal .\n", i % 9));
        }
        data.push_str(&format!("ex:i{i} ex:w \"{}.25\"^^xsd:double .\n", i % 17));
    }
    // Large values: the quotient's whole part is long.
    data.push_str("ex:big1 ex:g ex:big ; ex:n 100000000000000 .\n");
    data.push_str("ex:big2 ex:g ex:big ; ex:n 7 .\n");
    data.push_str("ex:big3 ex:g ex:big ; ex:n 2 .\n");
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    for q in [
        "SELECT ?g (AVG(?n) AS ?a) WHERE { ?i ex:g ?g ; ex:n ?n } GROUP BY ?g",
        "SELECT ?g (AVG(?d) AS ?a) (COUNT(*) AS ?c) WHERE { ?i ex:g ?g ; ex:d ?d } GROUP BY ?g",
        "SELECT ?g (AVG(?m) AS ?a) WHERE { ?i ex:g ?g ; ex:m ?m } GROUP BY ?g",
        "SELECT ?g (AVG(?w) AS ?a) WHERE { ?i ex:g ?g ; ex:w ?w } GROUP BY ?g",
        "SELECT (AVG(?n) AS ?a) WHERE { ?i ex:n ?n }",
        "SELECT (AVG(?d) AS ?a) (SUM(?d) AS ?s) WHERE { ?i ex:d ?d }",
    ] {
        let run = |executor: &str| {
            rows(
                &engine,
                &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
            )
        };
        let expected = run("spareval");
        assert!(!expected.is_empty(), "{q}");
        assert_eq!(run("ids"), expected, "{q}");
        let plan = engine
            .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
            .unwrap();
        assert!(plan.contains(r#""executor":"ids""#), "not on ids: {q}");
    }
}

/// `SUM(DISTINCT …)` and `AVG(DISTINCT …)` on ids add each value once, telling values apart as
/// the evaluator does: `1`, `1.0` and `"1.0"^^xsd:double` are three values, `"01"` and `1` one.
#[test]
fn distinct_sums_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    let mut data = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..400_i64 {
        let g = i % 23;
        data.push_str(&format!("ex:i{i} ex:g ex:g{g} ; ex:n {} .\n", i % 7 - 2));
        data.push_str(&format!(
            "ex:i{i} ex:d \"{}.{}\"^^xsd:decimal .\n",
            i % 5,
            i % 3 * 5
        ));
        data.push_str(&format!("ex:i{i} ex:w \"{}.5\"^^xsd:double .\n", i % 4));
        // Equal numbers of several datatypes and forms.
        data.push_str(&match i % 6 {
            0 => format!("ex:i{i} ex:m 1 .\n"),
            1 => format!("ex:i{i} ex:m \"01\"^^xsd:integer .\n"),
            2 => format!("ex:i{i} ex:m 1.0 .\n"),
            3 => format!("ex:i{i} ex:m \"1.00\"^^xsd:decimal .\n"),
            4 => format!("ex:i{i} ex:m 1.0e0 .\n"),
            _ => format!("ex:i{i} ex:m {} .\n", i % 4),
        });
        data.push_str(&match i % 4 {
            0 => format!("ex:i{i} ex:k 1 .\n"),
            1 => format!("ex:i{i} ex:k 1.0 .\n"),
            2 => format!("ex:i{i} ex:k \"1.0\"^^xsd:double .\n"),
            _ => format!("ex:i{i} ex:k {} .\n", i % 3),
        });
    }
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    // A non-canonical `"01"` or `1.0e0` is not an inline number: the evaluator sums those.
    for (q, on_ids) in [
        (
            "SELECT ?g (SUM(DISTINCT ?n) AS ?s) WHERE { ?i ex:g ?g ; ex:n ?n } GROUP BY ?g",
            true,
        ),
        (
            "SELECT ?g (AVG(DISTINCT ?n) AS ?a) (AVG(?n) AS ?b) WHERE { ?i ex:g ?g ; ex:n ?n } GROUP BY ?g",
            true,
        ),
        (
            "SELECT ?g (SUM(DISTINCT ?d) AS ?s) (AVG(DISTINCT ?d) AS ?a) WHERE { ?i ex:g ?g ; ex:d ?d } GROUP BY ?g",
            true,
        ),
        (
            "SELECT ?g (SUM(DISTINCT ?w) AS ?s) (AVG(DISTINCT ?w) AS ?a) WHERE { ?i ex:g ?g ; ex:w ?w } GROUP BY ?g",
            true,
        ),
        (
            "SELECT ?g (SUM(DISTINCT ?m) AS ?s) (AVG(DISTINCT ?m) AS ?a) WHERE { ?i ex:g ?g ; ex:m ?m } GROUP BY ?g",
            false,
        ),
        (
            "SELECT (SUM(DISTINCT ?m) AS ?s) (AVG(DISTINCT ?m) AS ?a) (COUNT(DISTINCT ?m) AS ?c) WHERE { ?i ex:m ?m }",
            false,
        ),
        (
            "SELECT ?g (SUM(DISTINCT ?k) AS ?s) (AVG(DISTINCT ?k) AS ?a) WHERE { ?i ex:g ?g ; ex:k ?k } GROUP BY ?g",
            true,
        ),
        (
            "SELECT (SUM(DISTINCT ?k) AS ?s) (AVG(DISTINCT ?k) AS ?a) WHERE { ?i ex:k ?k }",
            true,
        ),
        (
            "SELECT (SUM(DISTINCT ?n * 2) AS ?s) (AVG(DISTINCT ?n + ?d) AS ?a) WHERE { ?i ex:n ?n ; ex:d ?d }",
            true,
        ),
    ] {
        let run = |executor: &str| {
            rows(
                &engine,
                &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
            )
        };
        let expected = run("spareval");
        assert!(!expected.is_empty(), "{q}");
        assert_eq!(run("ids"), expected, "{q}");
        assert_eq!(rows(&engine, &format!("{prefix} {q}")), expected, "{q}");
        let plan = engine
            .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
            .unwrap();
        assert_eq!(plan.contains(r#""executor":"ids""#), on_ids, "{q}");
    }
}

/// An alternative of predicates and their inverses (`ex:a|^ex:b`) on ids is the evaluator's
/// union of their pairs without duplicates.
#[test]
fn alternative_paths_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    let mut data = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..300 {
        data.push_str(&format!("ex:n{i} ex:a ex:n{} .\n", i * 7 % 300));
        if i % 3 == 0 {
            // Some pairs by both predicates.
            data.push_str(&format!("ex:n{i} ex:b ex:n{} .\n", i * 7 % 300));
        } else {
            data.push_str(&format!("ex:n{i} ex:b ex:n{} .\n", (i + 1) % 300));
        }
        if i % 10 == 0 {
            data.push_str(&format!("ex:n{i} ex:c \"v{i}\" ; ex:a ex:n{i} .\n"));
        }
        data.push_str(&format!("ex:n{i} ex:name \"N{}\" .\n", i % 40));
    }
    // A pair by a predicate and the same pair by an inverse, the ends in id ranges that
    // don't overlap.
    data.push_str("ex:x ex:d ex:y . ex:y ex:e ex:x .\n");
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    for (q, on_ids) in [
        ("SELECT * WHERE { ?s ex:d|^ex:e ?o }", true),
        ("SELECT * WHERE { ?s ex:a|ex:b ?o }", true),
        ("SELECT * WHERE { ?s ex:a|^ex:b ?o }", true),
        ("SELECT * WHERE { ?s ex:a|ex:b|ex:c ?o }", true),
        ("SELECT * WHERE { ex:n3 ex:a|ex:b ?o }", true),
        ("SELECT * WHERE { ?s ex:a|ex:b ex:n21 }", true),
        ("SELECT * WHERE { ex:n3 ex:a|ex:b ex:n21 }", true),
        ("SELECT * WHERE { ex:n3 ex:a|ex:b ex:n5 }", true),
        ("SELECT * WHERE { ?x ex:a|ex:b ?x }", true),
        ("SELECT * WHERE { ?s ex:a|ex:b ?o . ?o ex:name ?n }", true),
        (
            "SELECT * WHERE { ?s ex:name ?n OPTIONAL { ?s ex:c|^ex:b ?o } }",
            true,
        ),
        ("SELECT (COUNT(*) AS ?c) WHERE { ?s ex:a|^ex:a ?o }", true),
        (
            "SELECT ?n (COUNT(*) AS ?c) WHERE { ?s ex:a|ex:b ?o . ?o ex:name ?n } GROUP BY ?n",
            true,
        ),
        (
            "SELECT * WHERE { ?s ex:a|ex:b ?o MINUS { ?s ex:c ?v } }",
            true,
        ),
        ("SELECT * WHERE { ?s ex:a|(ex:b/ex:a) ?o }", false),
    ] {
        let run = |executor: &str| {
            rows(
                &engine,
                &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
            )
        };
        let expected = run("spareval");
        assert_eq!(run("ids"), expected, "{q}");
        assert_eq!(rows(&engine, &format!("{prefix} {q}")), expected, "{q}");
        if on_ids {
            let plan = engine
                .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
                .unwrap();
            assert!(plan.contains(r#""executor":"ids""#), "not on ids: {q}");
        }
    }
}

/// `GRAPH` on ids: the pattern in one named graph, or in each with its name bound, is the
/// evaluator's, also when the pattern binds the graph's variable itself.
#[test]
fn graph_patterns_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    let mut data = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..200 {
        data.push_str(&format!("ex:s{i} ex:p ex:o{} .\n", i % 30));
    }
    for g in 0..4 {
        data.push_str(&format!("ex:g{g} {{\n"));
        for i in 0..150 {
            if (i + g) % 3 == 0 {
                continue;
            }
            data.push_str(&format!("ex:s{i} ex:p ex:o{} .\n", (i + g) % 30));
            data.push_str(&format!("ex:s{i} ex:q {} .\n", i % 7));
            if i % 10 == g {
                data.push_str(&format!("ex:s{i} ex:p ex:g{} .\n", (g + i) % 4));
                data.push_str(&format!("ex:o{} ex:r ex:g{g} .\n", i % 30));
            }
            if i % 5 == 0 {
                data.push_str(&format!("ex:o{} ex:p ex:o{} .\n", i % 30, (i + 1) % 30));
            }
        }
        data.push_str("}\n");
    }
    engine.load(RdfFormat::TriG, data.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    for (q, on_ids) in [
        ("SELECT * WHERE { GRAPH ?g { ?s ex:p ?o } }", true),
        (
            "SELECT * WHERE { GRAPH ex:g2 { ?s ex:p ?o ; ex:q ?v } }",
            true,
        ),
        ("SELECT * WHERE { GRAPH ex:nothing { ?s ex:p ?o } }", false),
        (
            "SELECT * WHERE { ?s ex:p ?o . GRAPH ?g { ?s ex:q ?v } }",
            true,
        ),
        ("SELECT * WHERE { GRAPH ?g { ?s ex:p ?g } }", true),
        (
            "SELECT * WHERE { GRAPH ?g { ?s ex:p ?o OPTIONAL { ?o ex:r ?g } } }",
            true,
        ),
        (
            "SELECT ?g (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g",
            true,
        ),
        ("SELECT DISTINCT ?g WHERE { GRAPH ?g { ?s ex:q 3 } }", true),
        ("SELECT * WHERE { GRAPH ?g { ex:o0 ex:p+ ?o } }", true),
        ("SELECT * WHERE { GRAPH ?g { ex:o0 ex:p* ?o } }", false),
        (
            "SELECT ?x WHERE { { SELECT * WHERE { GRAPH ?g { ?x ex:q ?y } } } }",
            true,
        ),
        (
            "SELECT * WHERE { GRAPH ?g { ?s ex:q ?v FILTER(?v > 4) } } ORDER BY ?g ?s LIMIT 5",
            true,
        ),
        (
            "SELECT ?s ?g WHERE { ?s ex:p ?o MINUS { GRAPH ?g { ?s ex:q 1 } } }",
            true,
        ),
    ] {
        let run = |executor: &str| {
            rows(
                &engine,
                &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
            )
        };
        let expected = run("spareval");
        assert_eq!(run("ids"), expected, "{q}");
        assert_eq!(rows(&engine, &format!("{prefix} {q}")), expected, "{q}");
        let plan = engine
            .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
            .unwrap();
        assert_eq!(plan.contains(r#""executor":"ids""#), on_ids, "{q}");
    }
}

/// Groups on a computed string, ordered on it: the strings the store does not hold get ids
/// made up for the view the rewrites read, and the evaluator reads them back on that view
/// (more than ~1,300 groups used to fail with "unknown term Ephemeral#0").
#[test]
fn groups_on_computed_strings_ordered_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    let mut data = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..1650_u32 {
        data.push_str(&format!("ex:s{i} ex:link ex:o{} .\n", i % 1400));
    }
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    for e in ["STR(?v)", "UCASE(STR(?v))", "SHA1(STR(?v))"] {
        let q = format!(
            "SELECT ?k (COUNT(*) AS ?n) WHERE {{ ?s ex:link ?v BIND({e} AS ?k) }} GROUP BY ?k ORDER BY ?k"
        );
        let run = |executor: &str| {
            ordered(
                &engine,
                &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
            )
        };
        let expected = run("spareval");
        assert_eq!(expected.len(), 1400, "{q}");
        assert_eq!(run("ids"), expected, "{q}");
        engine
            .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
            .unwrap();
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

/// A negated property set (`!(ex:a|ex:b)`, `^!ex:a`) on ids is the evaluator's: a row per
/// triple whose predicate is not in the set.
#[test]
fn negated_property_sets_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    let mut data = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..300 {
        data.push_str(&format!("ex:n{i} ex:a ex:n{} .\n", i * 7 % 300));
        if i % 3 == 0 {
            // The same pair by two predicates: two rows.
            data.push_str(&format!("ex:n{i} ex:b ex:n{} .\n", i * 7 % 300));
        }
        if i % 10 == 0 {
            data.push_str(&format!("ex:n{i} ex:c ex:n{i} ; a ex:C .\n"));
        }
        data.push_str(&format!("ex:n{i} ex:name \"N{}\" .\n", i % 40));
    }
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    for (q, on_ids) in [
        ("SELECT * WHERE { ?s !ex:a ?o }", true),
        ("SELECT * WHERE { ?s !(ex:a|ex:name) ?o }", true),
        ("SELECT * WHERE { ?s !(ex:a|ex:nowhere) ?o }", true),
        ("SELECT * WHERE { ?s ^!ex:a ?o }", true),
        ("SELECT * WHERE { ?s !(^ex:a|^ex:name) ?o }", true),
        ("SELECT * WHERE { ex:n3 !ex:name ?o }", true),
        ("SELECT * WHERE { ?s !ex:name ex:n21 }", true),
        ("SELECT * WHERE { ?x !ex:name ?x }", true),
        ("SELECT * WHERE { ?s a ex:C . ?s !(ex:name|ex:c) ?o }", true),
        (
            "SELECT ?o (COUNT(*) AS ?c) WHERE { ?s !(ex:name|a) ?o } GROUP BY ?o",
            true,
        ),
        ("SELECT * WHERE { ex:n0 !ex:name ex:n0 }", false),
        ("SELECT * WHERE { ?s !(ex:a|^ex:b) ?o }", false),
        // Both ends bound by the rows before: once per triple between them, as Jena gives it.
        ("SELECT * WHERE { ?s ex:a ?o . ?s !ex:name ?o }", true),
        ("SELECT * WHERE { ?s ex:c ?o . ?s !ex:name ?o }", true),
        ("SELECT * WHERE { ?s !ex:name ?o . ?s ex:a ?o }", true),
        (
            "SELECT * WHERE { ?s ex:a ?o OPTIONAL { ?s !ex:name ?o } }",
            true,
        ),
        // The query's own variables with the names of the made-up ones.
        ("SELECT * WHERE { ?s ?__negated_p ?o . ?s !ex:a ?o }", true),
        (
            "SELECT * WHERE { ?s a ex:C ; ?negated_p ?x . ?s !ex:name ?o }",
            true,
        ),
    ] {
        let run = |executor: &str| {
            rows(
                &engine,
                &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
            )
        };
        let expected = run("spareval");
        assert!(!expected.is_empty(), "{q}");
        assert_eq!(run("ids"), expected, "{q}");
        assert_eq!(rows(&engine, &format!("{prefix} {q}")), expected, "{q}");
        let plan = engine
            .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
            .unwrap();
        assert_eq!(plan.contains(r#""executor":"ids""#), on_ids, "{q}");
    }
}

/// `<p>?` on ids from a constant end, or from the values a joined pattern gives an end: each
/// start itself and the nodes one edge away, once each, as the evaluator gives them.
#[test]
fn optional_paths_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    let mut data = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..300 {
        data.push_str(&format!("ex:n{i} ex:a ex:n{} .\n", i * 7 % 300));
        if i % 4 == 0 {
            data.push_str(&format!("ex:n{i} ex:a ex:n{} .\n", (i + 1) % 300));
        }
        if i % 10 == 0 {
            // A loop: the start one edge away from itself.
            data.push_str(&format!("ex:n{i} ex:a ex:n{i} ; a ex:C .\n"));
        }
        if i % 25 == 0 {
            data.push_str(&format!("ex:n{i} a ex:D .\n"));
        }
        data.push_str(&format!("ex:n{i} ex:name \"N{}\" .\n", i % 40));
    }
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    for (q, on_ids) in [
        ("SELECT * WHERE { ex:n3 ex:a? ?o }", true),
        ("SELECT * WHERE { ex:n10 ex:a? ?o }", true),
        ("SELECT * WHERE { ?s ex:a? ex:n21 }", true),
        ("SELECT * WHERE { ex:n21 ^ex:a? ?s }", true),
        ("SELECT * WHERE { ex:n21 (^ex:a)? ?s }", true),
        ("SELECT * WHERE { ?s a ex:C . ?s ex:a? ?o }", true),
        ("SELECT * WHERE { ?o a ex:D . ?s ex:a? ?o }", true),
        (
            "SELECT * WHERE { ?s a ex:C . ?s ex:a? ?o . ?o ex:name ?n }",
            true,
        ),
        (
            "SELECT ?n (COUNT(*) AS ?c) WHERE { ?s a ex:C . ?s ex:a? ?o . ?o ex:name ?n } GROUP BY ?n",
            true,
        ),
        (
            "SELECT * WHERE { ?s a ex:C OPTIONAL { ?s ex:a? ?o } }",
            true,
        ),
        ("SELECT * WHERE { ex:n3 ex:a? ex:n21 }", false),
        ("SELECT * WHERE { ex:n3 ex:a? ex:n3 }", false),
        ("SELECT * WHERE { ex:nowhere ex:a? ?o }", false),
        ("SELECT * WHERE { ?s ex:a? ?o }", false),
    ] {
        let run = |executor: &str| {
            rows(
                &engine,
                &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
            )
        };
        let expected = run("spareval");
        assert!(!expected.is_empty(), "{q}");
        assert_eq!(run("ids"), expected, "{q}");
        assert_eq!(rows(&engine, &format!("{prefix} {q}")), expected, "{q}");
        let plan = engine
            .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
            .unwrap();
        assert_eq!(plan.contains(r#""executor":"ids""#), on_ids, "{q}");
    }
    // On a few triples the edges are read by a scan, not looked up from the start: only
    // the start's count.
    let small = Engine::new_tripletrail();
    small
        .load(
            RdfFormat::Turtle,
            "@prefix ex: <http://example.org/> . ex:a ex:next ex:b , ex:c . ex:b ex:next ex:c . ex:x ex:next ex:y ."
                .as_bytes(),
        )
        .unwrap();
    for q in [
        "SELECT ?o WHERE { ex:a ex:next? ?o }",
        "SELECT ?s WHERE { ?s ex:next? ex:c }",
    ] {
        let run = |executor: &str| {
            rows(
                &small,
                &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
            )
        };
        assert_eq!(run("ids"), run("spareval"), "{q}");
    }
    // A value another pattern gives an end matches itself only if it is a node of the
    // graph read: `ex:only` is in a named graph alone, `ex:a` a predicate, `ex:g1` a graph.
    let graphs = Engine::new_tripletrail();
    graphs
        .load(
            RdfFormat::TriG,
            "@prefix ex: <http://example.org/> .
            ex:n1 ex:a ex:n2 . ex:n2 ex:a ex:n3 . ex:n3 ex:a ex:n1 .
            ex:g1 { ex:only ex:a ex:n3 . ex:n1 ex:a ex:n3 . }
            ex:g2 { ex:other ex:a ex:only . }"
                .as_bytes(),
        )
        .unwrap();
    for q in [
        "SELECT * WHERE { GRAPH ex:g1 { ?s ex:a ex:n3 } ?s ex:a? ?o }",
        "SELECT * WHERE { GRAPH ex:g1 { ?s ex:a ex:n3 } ?s ex:a* ?o }",
        "SELECT * WHERE { GRAPH ex:g1 { ?s ex:a ex:n3 } ?o ex:a? ?s }",
        "SELECT * WHERE { ex:only ex:a? ?o . ?o (^ex:a)? ?x }",
        "SELECT * WHERE { VALUES ?s { ex:only } ?s ex:a? ?o }",
        "SELECT * WHERE { VALUES ?s { ex:only ex:n1 ex:a ex:g1 } ?s ex:a? ?o }",
        "SELECT * WHERE { VALUES ?s { ex:only ex:n1 ex:a ex:g1 } ?s ex:a* ?o }",
        "SELECT * WHERE { VALUES ?s { ex:a } ?s ex:a* ?o }",
        "SELECT * WHERE { ex:only ex:a? ?o }",
    ] {
        let run = |executor: &str| {
            rows(
                &graphs,
                &format!("#pragma tt.executor {executor}\n{prefix} {q}"),
            )
        };
        let expected = run("spareval");
        assert_eq!(run("ids"), expected, "{q}");
        assert_eq!(rows(&graphs, &format!("{prefix} {q}")), expected, "{q}");
    }
}

/// `FILTER (NOT) EXISTS` with `FILTER`s and nested `EXISTS` inside (#11): on ids when every
/// value they read that the row gives is bound by the pattern itself, with the answers of the
/// evaluator alone (which substitutes the row's values into the pattern).
const EXISTS_FILTERS: &[(&str, bool)] = &[
    (
        "?s ex:p ?o FILTER EXISTS { ?o ex:q ?v FILTER(?v = \"v3\") }",
        true,
    ),
    (
        "?s ex:p ?o FILTER NOT EXISTS { ?o ex:q ?v FILTER(?v = \"v3\" || ?v = \"v4\") }",
        true,
    ),
    (
        "?s ex:p ?o FILTER EXISTS { ?o ex:q ?v FILTER(STRSTARTS(?v, \"v1\")) }",
        true,
    ),
    (
        "?s a ?c FILTER NOT EXISTS { ?s ex:label ?l FILTER(LANG(?l) = \"en\") }",
        true,
    ),
    (
        "?s ex:p ?o FILTER EXISTS { ?s ex:p ?o2 . ?o2 ex:q ?v FILTER EXISTS { ?o2 ex:q \"v2\" } }",
        true,
    ),
    (
        "?s ex:p ?o FILTER NOT EXISTS { ?s ex:label ?l FILTER NOT EXISTS { ?s ex:r ?b } }",
        true,
    ),
    // The nested pattern reads `?o`, which only the row binds: substituted, not joined.
    (
        "?s ex:p ?o FILTER EXISTS { ?s ex:label ?l FILTER EXISTS { ?o ex:q ?v } }",
        false,
    ),
    (
        "?s ex:p ?o FILTER EXISTS { ?o ex:q ?v FILTER NOT EXISTS { ?s ex:r ?b } }",
        false,
    ),
    // A filter on a value of the row alone.
    (
        "?s ex:p ?o . ?o ex:q ?w FILTER EXISTS { ?s ex:label ?l FILTER(?w = \"v3\") }",
        false,
    ),
    (
        "?s ex:p ?o FILTER EXISTS { ?s ex:label ?l FILTER(BOUND(?o)) }",
        false,
    ),
    // A filter of an inner group sees that group's rows only: on ids it would not see `?o`.
    (
        "?s ex:p ?o FILTER EXISTS { { ?s ex:label ?l FILTER(?o != ex:nothing) } ?s ex:p ?o }",
        false,
    ),
    (
        "?s ex:p ?o FILTER EXISTS { { ?s ex:label ?l FILTER EXISTS { ?o ex:q ?v } } ?s ex:p ?o }",
        false,
    ),
    // Its own group binds `?o`: the same rows joined or substituted.
    (
        "?s ex:p ?o FILTER EXISTS { { ?s ex:p ?o FILTER(?o != ex:nothing) } ?s ex:label ?l }",
        true,
    ),
    // Not an `EXISTS` shape on ids.
    (
        "?s ex:p ?o FILTER EXISTS { ?o ex:q ?v OPTIONAL { ?s ex:r ?b } FILTER(!BOUND(?b)) }",
        false,
    ),
];

#[test]
fn exists_with_filters_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    for &(p, on_ids) in EXISTS_FILTERS {
        for q in [
            format!("SELECT * WHERE {{ {p} }}"),
            format!("SELECT (COUNT(*) AS ?n) WHERE {{ {p} }}"),
        ] {
            let ids = rows(&engine, &format!("#pragma tt.executor ids\n{prefix} {q}"));
            for hints in [
                "#pragma tt.executor spareval\n#pragma tt.id-rewrites off\n",
                "#pragma tt.executor spareval\n",
                "",
            ] {
                assert_eq!(
                    ids,
                    rows(&engine, &format!("{hints}{prefix} {q}")),
                    "{hints}{q}"
                );
            }
        }
        let plan = engine
            .explain(&format!(
                "#pragma tt.executor ids\n{prefix} SELECT * WHERE {{ {p} }}"
            ))
            .unwrap();
        assert_eq!(plan.contains(r#""executor":"ids""#), on_ids, "on ids: {p}");
    }
}

/// `MIN` and `MAX` of date-times on term ids (#11), by instant, as the evaluator gives them;
/// values it doesn't order by instant alone (with and without a time zone, dates with
/// date-times, other terms) and ties at one instant written differently are left to it.
#[test]
fn date_time_extremes_on_ids_are_the_evaluators() {
    let engine = Engine::new_tripletrail();
    let mut data = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..3_000 {
        let (month, day, hour) = (1 + i % 12, 1 + i % 28, i % 24);
        data.push_str(&format!(
            "ex:s{i} ex:k ex:k{} ; ex:at \"2024-{month:02}-{day:02}T{hour:02}:{:02}:{:02}.{:03}Z\"^^xsd:dateTime ; \
             ex:local \"2023-{month:02}-{day:02}T{hour:02}:00:00\"^^xsd:dateTime ; \
             ex:zoned \"2024-{month:02}-{day:02}T{hour:02}:00:00{}\"^^xsd:dateTime ; \
             ex:day \"2022-{month:02}-{day:02}\"^^xsd:date .\n",
            i % 30,
            i % 60,
            i % 59,
            i % 1000,
            ["+02:00", "-05:30", "Z"][i % 3],
        ));
    }
    // One group mixes in a value without a time zone, one a date, one a string, and one two
    // writings of the same instant.
    data.push_str("ex:x1 ex:k ex:k1 ; ex:at \"2024-01-01T00:00:00\"^^xsd:dateTime .\n");
    data.push_str("ex:x2 ex:k ex:k2 ; ex:at \"2024-01-01\"^^xsd:date .\n");
    data.push_str("ex:x3 ex:k ex:k3 ; ex:at \"soon\" .\n");
    data.push_str("ex:x4 ex:k ex:k4 ; ex:zoned \"2030-01-01T00:00:00Z\"^^xsd:dateTime .\n");
    data.push_str("ex:x5 ex:k ex:k4 ; ex:zoned \"2030-01-01T01:00:00+01:00\"^^xsd:dateTime .\n");
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let prefix = "PREFIX ex: <http://example.org/>";
    for (q, on_ids) in [
        (
            "SELECT (MIN(?d) AS ?a) (MAX(?d) AS ?b) WHERE { ?s ex:k ex:k5 ; ex:at ?d }",
            true,
        ),
        (
            "SELECT (MIN(?d) AS ?a) (MAX(?d) AS ?b) WHERE { ?s ex:local ?d }",
            true,
        ),
        (
            "SELECT (MIN(?d) AS ?a) (MAX(?d) AS ?b) WHERE { ?s ex:day ?d }",
            true,
        ),
        (
            "SELECT (MIN(?d) AS ?a) (MAX(?d) AS ?b) WHERE { ?s ex:k ex:k7 ; ex:zoned ?d }",
            true,
        ),
        (
            "SELECT ?k (MIN(?d) AS ?a) (MAX(?d) AS ?b) WHERE { ?s ex:k ?k ; ex:local ?d } GROUP BY ?k",
            true,
        ),
        (
            "SELECT ?k (MAX(?d) AS ?b) (COUNT(*) AS ?n) WHERE { ?s ex:k ?k ; ex:zoned ?d FILTER(?k != ex:k4) } GROUP BY ?k",
            true,
        ),
        (
            "SELECT ?k (MIN(?d) AS ?a) WHERE { ?s ex:k ?k ; ex:at ?d } GROUP BY ?k",
            false,
        ),
        (
            "SELECT ?k (MAX(?d) AS ?b) WHERE { ?s ex:k ?k ; ex:zoned ?d } GROUP BY ?k",
            false,
        ),
        (
            "SELECT (MAX(?d) AS ?b) WHERE { { ?s ex:local ?d } UNION { ?s ex:zoned ?d } }",
            false,
        ),
        (
            "SELECT (MIN(?d) AS ?a) WHERE { { ?s ex:day ?d } UNION { ?s ex:local ?d } }",
            false,
        ),
    ] {
        let ids = rows(&engine, &format!("#pragma tt.executor ids\n{prefix} {q}"));
        assert!(!ids.is_empty(), "{q}");
        for hints in [
            "#pragma tt.executor spareval\n#pragma tt.id-rewrites off\n",
            "#pragma tt.executor spareval\n",
            "",
        ] {
            assert_eq!(
                ids,
                rows(&engine, &format!("{hints}{prefix} {q}")),
                "{hints}{q}"
            );
        }
        let plan = engine
            .explain(&format!("#pragma tt.executor ids\n{prefix} {q}"))
            .unwrap();
        assert_eq!(plan.contains(r#""executor":"ids""#), on_ids, "on ids: {q}");
    }
}
