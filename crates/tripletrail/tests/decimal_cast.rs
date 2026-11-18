//! `xsd:decimal` of a double is the decimal the double prints as (issue #380), as on Jena:
//! the binary expansion (`0.518134714999999936`) made a `ROUND` after it fall the other way.

use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

fn values(query: &str) -> Vec<String> {
    let e = Engine::new_tripletrail();
    let query = format!("PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>\n{query}");
    let QueryResults::Solutions(solutions) = e.query(&query).unwrap() else {
        panic!("not a SELECT")
    };
    solutions
        .map(|s| {
            s.unwrap()
                .iter()
                .map(|(v, t)| format!("{}={t}", v.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
                .replace("^^<http://www.w3.org/2001/XMLSchema#decimal>", "")
        })
        .collect()
}

#[test]
fn doubles_become_the_decimal_they_print_as() {
    assert_eq!(
        values(
            "SELECT ?dec ?r { VALUES ?m { \"0.518134715\" \"0.035\" \"1.005\" } \
             BIND(xsd:decimal(xsd:double(?m)) AS ?dec) \
             BIND(ROUND(?dec * 100000000) / 100000000 AS ?r) } ORDER BY ?m"
        ),
        [
            "dec=\"0.035\" r=\"0.035\"",
            "dec=\"0.518134715\" r=\"0.51813472\"",
            "dec=\"1.005\" r=\"1.005\"",
        ]
    );
    assert_eq!(
        values("SELECT (xsd:decimal(\"0.1\"^^xsd:float) AS ?d) {}"),
        ["d=\"0.1\""]
    );
    assert_eq!(
        values("SELECT (xsd:decimal(1.5e3) AS ?d) {}"),
        ["d=\"1500.0\""]
    );
    // Too small for 18 decimals: as the cast always did.
    assert_eq!(
        values("SELECT (xsd:decimal(1e-30) AS ?d) {}"),
        ["d=\"0.0\""]
    );
}

#[test]
fn other_casts_to_decimal_are_written_as_in_jena() {
    assert_eq!(
        values(
            "SELECT (xsd:decimal(\"2.50\") AS ?a) (xsd:decimal(7) AS ?b) \
             (xsd:decimal(true) AS ?c) (xsd:decimal(1.25) AS ?d) {}"
        ),
        // As Jena: a string, an integer or a decimal keeps its text; a boolean gets the form of
        // a computed decimal (#513).
        ["a=\"2.50\" b=\"7\" c=\"1.0\" d=\"1.25\""]
    );
    assert_eq!(
        values("SELECT ?x { OPTIONAL { BIND(xsd:decimal(\"abc\") AS ?x) } }"),
        [""]
    );
    assert_eq!(
        values("SELECT ?x { BIND(xsd:decimal(\"INF\"^^xsd:double) AS ?x) }"),
        [""]
    );
}

#[test]
fn updates_use_the_same_cast() {
    let e = Engine::new_tripletrail();
    e.update(
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> \
         INSERT { <urn:s> <urn:p> ?r } \
         WHERE { BIND(ROUND(xsd:decimal(xsd:double(\"0.518134715\")) * 100000000) / 100000000 AS ?r) }",
    )
    .unwrap();
    let QueryResults::Solutions(mut s) = e.query("SELECT ?r { <urn:s> <urn:p> ?r }").unwrap()
    else {
        panic!("not a SELECT")
    };
    assert_eq!(
        s.next().unwrap().unwrap().get("r").unwrap().to_string(),
        "\"0.51813472\"^^<http://www.w3.org/2001/XMLSchema#decimal>"
    );
}
