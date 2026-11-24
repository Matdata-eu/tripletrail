//! `ORDER BY` over values of different kinds (#148): numbers compare by value, but a number
//! and a string only as written, so `2 < 10`, `10 < "1z"` and `"1z" < 2` made a cycle and the
//! sort panicked ("user-provided comparison function does not correctly implement a total
//! order"). Literals are ordered by kind first (IRIs, then plain strings, language strings,
//! numbers, booleans…), then by value within a kind.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

const VALUES: &[&str] = &[
    "1",
    "2",
    "10",
    "-3",
    "2.5",
    "\"1\"^^xsd:long",
    "\"1.0\"^^xsd:decimal",
    "\"1e0\"^^xsd:double",
    "\"NaN\"^^xsd:double",
    "\"INF\"^^xsd:double",
    "\"-INF\"^^xsd:double",
    "\"-0.0\"^^xsd:double",
    "\"1z\"",
    "\"9\"",
    "\"abc\"",
    "\"abc\"@en",
    "\"abc\"@fr",
    "\"b\"@en",
    "true",
    "ex:iri",
    "\"2020-01-01\"^^xsd:date",
    "\"x\"^^ex:other",
];

fn engine() -> Engine {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    // Every pair of values, so that a sort meets each mix many times.
    let mut i = 0;
    for a in VALUES {
        for b in VALUES {
            ttl.push_str(&format!("ex:s{i} ex:a {a} ; ex:b {b} .\n"));
            i += 1;
        }
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    e
}

fn column(e: &Engine, query: &str) -> Vec<String> {
    let query = format!("PREFIX ex: <http://example.org/>\n{query}");
    let QueryResults::Solutions(s) = e.query(&query).unwrap() else {
        unreachable!("SELECT")
    };
    s.map(|row| {
        row.unwrap()
            .get("a")
            .map_or(String::new(), ToString::to_string)
    })
    .collect()
}

/// The kind a value of [`VALUES`] sorts in, as written in the results.
fn kind(text: &str) -> u8 {
    if text.starts_with('<') {
        0
    } else if text.contains("^^<http://www.w3.org/2001/XMLSchema#date>") {
        6
    } else if text.contains("^^<http://example.org/other>") {
        7
    } else if text.contains("\"@") {
        2
    } else if text == "true" || text.contains("#boolean>") {
        5
    } else if text.starts_with('"') && !text.contains("^^") {
        1
    } else {
        4
    }
}

#[test]
fn values_of_every_kind_sort_without_panicking() {
    let e = engine();
    for order in [
        "?a",
        "DESC(?a)",
        "?a ?b",
        "(?a * 2) ?a ?b ?s",
        "DESC(?a * ?b) ?s",
        "(?a + ?b)",
        "STR(?a) ?a",
    ] {
        let rows = column(
            &e,
            &format!("SELECT ?a WHERE {{ ?s ex:a ?a ; ex:b ?b }} ORDER BY {order}"),
        );
        assert_eq!(rows.len(), VALUES.len() * VALUES.len(), "ORDER BY {order}");
    }
    // Kinds in order, and numbers by value within theirs (NaN last).
    let rows = column(&e, "SELECT DISTINCT ?a WHERE { ?s ex:a ?a } ORDER BY ?a");
    let kinds: Vec<u8> = rows.iter().map(|r| kind(r)).collect();
    assert!(kinds.is_sorted(), "kinds out of order: {rows:?}");
    let numbers: Vec<&String> = rows.iter().filter(|r| kind(r) == 4).collect();
    assert!(numbers.first().unwrap().contains("-INF"), "{numbers:?}");
    assert!(numbers.last().unwrap().contains("NaN"), "{numbers:?}");
    let strings: Vec<&String> = rows.iter().filter(|r| kind(r) == 1).collect();
    assert_eq!(
        strings.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        ["\"1z\"", "\"9\"", "\"abc\""]
    );
}

/// The ids executor orders a column of mixed kinds, blank nodes and literals of other
/// datatypes as the evaluator does (#11), with and without a limit, `DISTINCT` and an
/// offset, instead of leaving the query to the evaluator.
#[test]
fn mixed_kinds_are_ordered_on_ids_as_the_evaluator_orders_them() {
    let e = engine();
    e.load(
        RdfFormat::Turtle,
        b"@prefix ex: <http://example.org/> .\nex:b1 ex:a _:x1 ; ex:b 1 .\nex:b2 ex:a _:x2 ; ex:b \"z\" .\n"
            .as_slice(),
    )
    .unwrap();
    let rows = |pragma: &str, q: &str| {
        let q = format!("{pragma}PREFIX ex: <http://example.org/>\n{q}");
        let QueryResults::Solutions(s) = e.query(&q).unwrap() else {
            unreachable!("SELECT")
        };
        s.map(|row| {
            let row = row.unwrap();
            row.iter()
                .map(|(v, t)| {
                    // Blank nodes as a kind only: labels are the writer's.
                    let t = match t {
                        oxigraph::model::Term::BlankNode(_) => "_:".to_owned(),
                        t => t.to_string(),
                    };
                    format!("{v}={t}")
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
    };
    for q in [
        "SELECT ?a ?s WHERE { ?s ex:a ?a } ORDER BY ?a ?s",
        "SELECT ?a ?s WHERE { ?s ex:a ?a } ORDER BY DESC(?a) ?s LIMIT 40",
        "SELECT ?a ?b ?s WHERE { ?s ex:a ?a ; ex:b ?b } ORDER BY ?b DESC(?a) ?s LIMIT 100 OFFSET 7",
        "SELECT DISTINCT ?a WHERE { ?s ex:a ?a } ORDER BY ?a",
        "SELECT DISTINCT ?b WHERE { ?s ex:b ?b } ORDER BY DESC(?b) LIMIT 9",
        "SELECT ?a ?s WHERE { ?s ex:a ?a OPTIONAL { ?s ex:c ?c } } ORDER BY ?c ?a ?s LIMIT 30",
        "SELECT ?x ?s WHERE { { ?s ex:a ?x } UNION { ?s ex:b ?x } } ORDER BY ?x ?s LIMIT 50",
    ] {
        let ids = rows("#pragma tt.executor ids\n", q);
        assert_eq!(ids, rows("#pragma tt.executor spareval\n", q), "{q}");
        let plan = e
            .explain(&format!(
                "#pragma tt.executor ids\nPREFIX ex: <http://example.org/>\n{q}"
            ))
            .unwrap();
        assert!(plan.contains(r#""executor":"ids""#), "not on ids: {q}");
    }
}
