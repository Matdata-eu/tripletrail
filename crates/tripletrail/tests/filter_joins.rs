//! Joins written as filters (#185): `FILTER(?a = ?b)` between two parts of a pattern is
//! evaluated as a join when one side is surely an IRI or a blank node, with the same answers.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
ex:line ex:on ex:n1 ; ex:begin 0 ; ex:end 10 .
ex:s1 ex:on ex:n1 ; ex:begin 2 ; ex:end 5 .
ex:s2 ex:on ex:n2 ; ex:begin 3 ; ex:end 4 .
ex:s3 ex:on ex:n1 ; ex:begin 8 ; ex:end 12 .
ex:a ex:val 1 .
ex:b ex:val2 1.0 .
ex:c ex:val2 "1" .
"#;

const PARENT: &str = "PREFIX ex: <http://example.com/>
    SELECT ?l ?s ?sn WHERE {
      ?l ex:on ?ln ; ex:begin ?lb ; ex:end ?le .
      ?s ex:on ?sn ; ex:begin ?sb ; ex:end ?se .
      FILTER(?ln = ?sn && ?lb <= ?sb && ?le >= ?se && ?l != ?s)
    } ORDER BY ?l ?s";

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e
}

fn rows(e: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = e.query(query).unwrap() else {
        panic!("not a SELECT");
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

#[test]
fn an_equality_of_iris_is_answered_as_a_join() {
    let e = engine();
    assert_eq!(
        rows(&e, PARENT),
        [
            "?l=<http://example.com/line> ?s=<http://example.com/s1> ?sn=<http://example.com/n1>",
            // s3 ends past the line, s2 is on another net element.
        ]
    );
    let plan = e.explain(PARENT).unwrap();
    assert!(!plan.contains("?ln = ?sn"), "{plan}");
}

#[test]
fn literals_of_equal_value_still_match() {
    // ex:val and ex:val2 hold literals: `1 = 1.0` is true though they are different terms,
    // and "1" is no number.
    let e = engine();
    let q = "PREFIX ex: <http://example.com/>
        SELECT ?x ?y WHERE { ?x ex:val ?a . ?y ex:val2 ?b FILTER(?a = ?b) }";
    assert_eq!(
        rows(&e, q),
        ["?x=<http://example.com/a> ?y=<http://example.com/b>"]
    );
}

#[test]
fn an_update_joins_too_and_sees_what_its_earlier_operations_wrote() {
    let e = engine();
    e.update(
        &PARENT
            .replace("SELECT ?l ?s ?sn WHERE", "INSERT { ?s ex:parent ?l } WHERE")
            .replace("} ORDER BY ?l ?s", "}"),
    )
    .unwrap();
    let q = "PREFIX ex: <http://example.com/> SELECT ?s ?l WHERE { ?s ex:parent ?l }";
    assert_eq!(
        rows(&e, q),
        ["?s=<http://example.com/s1> ?l=<http://example.com/line>"]
    );
    // ex:on holds IRIs only before the update, and its first operation writes a number to it:
    // the second must still match it with the 1.0 of ex:on2 by value.
    e.update(
        "PREFIX ex: <http://example.com/>
         INSERT DATA { ex:t ex:on 1 . ex:u ex:on2 1.0 } ;
         INSERT { ?x ex:same ?y } WHERE { ?x ex:on ?a . ?y ex:on2 ?b FILTER(?a = ?b) }",
    )
    .unwrap();
    let q = "PREFIX ex: <http://example.com/> SELECT ?x ?y WHERE { ?x ex:same ?y }";
    assert_eq!(
        rows(&e, q),
        ["?x=<http://example.com/t> ?y=<http://example.com/u>"]
    );
}

#[test]
fn a_filter_inside_exists_or_lateral_keeps_the_outer_binding() {
    // Inside EXISTS and on the right of LATERAL, ?sn is bound from outside: the pattern there
    // must be matched with that value, and the filter must not make ?sn a variable of its own,
    // whichever side of `=` it is on.
    let e = engine();
    for filter in ["?ln = ?sn", "?sn = ?ln"] {
        // ex:begin has 4 subjects; ex:n2 has one ?l and one ?t on it, ex:n9 none.
        for (outer, matched) in [("ex:n2", 4), ("ex:n9", 0)] {
            let inner = format!("{{ ?l ex:on ?ln . ?t ex:on ?sn FILTER({filter}) }}");
            let head = format!(
                "PREFIX ex: <http://example.com/> SELECT ?s ?l WHERE {{ ?s ex:begin ?b . BIND({outer} AS ?sn)"
            );
            let exists = format!("{head} FILTER EXISTS {inner} }}");
            assert_eq!(rows(&e, &exists).len(), matched, "{exists}");
            let not_exists = format!("{head} FILTER NOT EXISTS {inner} }}");
            assert_eq!(rows(&e, &not_exists).len(), 4 - matched, "{not_exists}");
            let lateral = format!("{head} LATERAL {inner} }}");
            assert_eq!(rows(&e, &lateral).len(), matched, "{lateral}");
        }
    }
}
