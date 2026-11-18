//! `GROUP_CONCAT` gives a simple literal, also when every value has the same language tag
//! (W3C agg-groupconcat-04 and 06; Jena does the same).

use tripletrail::Engine;
use tripletrail::oxigraph::sparql::QueryResults;

fn concat(e: &Engine, values: &str) -> String {
    let q = format!(
        "SELECT (GROUP_CONCAT(?o; SEPARATOR=\",\") AS ?g) WHERE {{ VALUES ?o {{ {values} }} }}"
    );
    let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
        panic!("not a SELECT")
    };
    s.next().unwrap().unwrap().get("g").unwrap().to_string()
}

#[test]
fn group_concat_gives_a_simple_literal() {
    let e = Engine::new_tripletrail();
    assert_eq!(concat(&e, r#""1"@en"#), r#""1""#);
    assert_eq!(concat(&e, r#""1"@en "2"@en"#), r#""1,2""#);
    assert_eq!(concat(&e, r#""1"@en "2"@fr"#), r#""1,2""#);
    assert_eq!(concat(&e, r#""1" "2"@en"#), r#""1,2""#);
    assert_eq!(concat(&e, r#""1" "2""#), r#""1,2""#);
}
