//! A triple pattern on an IRI made by `BIND` is a lookup per row of the part that makes it,
//! whichever side is estimated smaller (issue #374): it was joined to that part on no key,
//! every row with every row, when the looked-up class was the smaller side.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

const QUERY: &str = r#"PREFIX src: <http://example.org/source/>
PREFIX d: <http://example.org/data/>
PREFIX v: <http://example.org/vocab#>
SELECT (COUNT(*) AS ?n) WHERE {
  ?this src:km ?km .
  BIND(IRI(CONCAT(STR(d:), "lc_", STR(?km))) AS ?candidate)
  ?candidate a v:LinearCoordinate .
}"#;

/// `rows` sources with a km out of `kms`, and a linear coordinate for each of `coordinates`
/// kms (the first ones).
fn engine(rows: usize, kms: usize, coordinates: usize) -> Engine {
    let mut data = String::new();
    for i in 0..rows {
        data.push_str(&format!(
            "<http://example.org/source/{i}> <http://example.org/source/km> \"{}\" .\n",
            i % kms
        ));
    }
    for k in 0..coordinates {
        data.push_str(&format!(
            "<http://example.org/data/lc_{k}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/vocab#LinearCoordinate> .\n"
        ));
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::NTriples, data.as_bytes()).unwrap();
    e
}

fn count(e: &Engine) -> String {
    let QueryResults::Solutions(mut s) = e.query(QUERY).unwrap() else {
        panic!("not a SELECT")
    };
    let row = s.next().unwrap().unwrap();
    row.get("n").unwrap().to_string()
}

/// The evaluator's plan looks the class up per row. (Under `--executor ids` the whole query
/// runs on term ids, with the same count: no evaluator plan to check.)
fn assert_lookup(e: &Engine) {
    let plan = e
        .explain(&format!("#pragma tt.executor spareval\n{QUERY}"))
        .unwrap();
    assert!(plan.contains("Lateral"), "{plan}");
    assert!(!plan.contains("keys = )"), "{plan}");
}

#[test]
fn the_looked_up_class_is_smaller() {
    // 2,000 rows, 1,000 kms, 400 coordinates: rows with km < 400 match.
    let e = engine(2000, 1000, 400);
    assert_eq!(
        count(&e),
        "\"800\"^^<http://www.w3.org/2001/XMLSchema#integer>"
    );
    assert_lookup(&e);
}

#[test]
fn the_looked_up_class_is_larger() {
    let e = engine(500, 100, 5000);
    assert_eq!(
        count(&e),
        "\"500\"^^<http://www.w3.org/2001/XMLSchema#integer>"
    );
    assert_lookup(&e);
}
