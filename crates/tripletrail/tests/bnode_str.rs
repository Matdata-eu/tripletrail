//! `BNODE(str)` gives the same blank node for the same string within one solution, and
//! another one in every other solution (W3C bnode01; Jena does the same, #224).

use tripletrail::Engine;
use tripletrail::oxigraph::sparql::QueryResults;

fn rows(e: &Engine, q: &str) -> Vec<Vec<String>> {
    let QueryResults::Solutions(s) = e.query(q).unwrap() else {
        panic!("not a SELECT")
    };
    let vars = s.variables().to_vec();
    s.map(|r| {
        let r = r.unwrap();
        vars.iter().map(|v| r.get(v).unwrap().to_string()).collect()
    })
    .collect()
}

#[test]
fn bnode_of_a_string_is_per_solution() {
    let e = Engine::new_tripletrail();
    let rows = rows(
        &e,
        r#"SELECT ?a ?b (BNODE(?a) AS ?x) (BNODE(?b) AS ?y) (BNODE(?a) AS ?z) WHERE {
            VALUES ?a { "foo" "bar" } VALUES ?b { "foo" "bar" } }"#,
    );
    assert_eq!(rows.len(), 4);
    for r in &rows {
        // The same string in one solution: the same blank node.
        assert_eq!(r[2], r[4], "{rows:?}");
        assert_eq!(r[0] == r[1], r[2] == r[3], "{rows:?}");
    }
    // Another solution: another blank node, also for the same string.
    let mut seen: Vec<&String> = rows.iter().map(|r| &r[2]).collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 4, "{rows:?}");
}

#[test]
fn bnode_of_a_string_in_bind_is_per_solution() {
    let e = Engine::new_tripletrail();
    let rows = rows(
        &e,
        r#"SELECT ?x ?y WHERE { VALUES ?n { 1 2 3 }
            BIND(BNODE("k") AS ?x) BIND(BNODE("k") AS ?y) }"#,
    );
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r[0] == r[1]), "{rows:?}");
    assert_ne!(rows[0][0], rows[1][0]);
    assert_ne!(rows[1][0], rows[2][0]);
}
