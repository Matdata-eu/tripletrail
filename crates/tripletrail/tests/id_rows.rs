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
