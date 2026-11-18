//! Numeric range FILTERs answered from the value-ordered index (issue #71) give the same
//! rows as the FILTER alone, whatever the literal types, bounds and writes in between.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = engine.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    let mut v: Vec<String> = solutions
        .map(|s| {
            s.unwrap()
                .iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    v.sort();
    v
}

/// The same query on an engine without the rewrite.
fn reference(data: &str, query: &str) -> Vec<String> {
    let engine = Engine::new_tripletrail();
    engine.set_spatial_rewrite(false);
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    rows(&engine, query)
}

fn data() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..3000 {
        let km = (i * 37 % 2000) as f64 / 10.0;
        // Every numeric spelling, plus values that are not numbers at all.
        let literal = match i % 7 {
            0 => format!("\"{km}\"^^xsd:decimal"),
            1 => format!("\"{km:e}\"^^xsd:double"),
            2 => format!("{}", km as i64),
            3 => format!("\"+{}\"^^xsd:int", km as i64),
            4 => format!("\"{km}\"^^xsd:float"),
            5 => format!("\"{km}\""),
            _ => format!("\"0{}\"^^xsd:integer", km as i64),
        };
        ttl.push_str(&format!(
            "ex:s{i} a ex:Signal ; ex:ref ex:lr{i} . ex:lr{i} ex:line ex:line{} ; ex:km {literal} .\n",
            i % 40
        ));
    }
    ttl.push_str("ex:x ex:km \"NaN\"^^xsd:double . ex:y ex:km \"INF\"^^xsd:double . ex:z ex:km \"-INF\"^^xsd:double .\n");
    ttl.push_str("ex:inside ex:km 100.5 .\n");
    ttl
}

const P: &str =
    "PREFIX ex: <http://example.org/>\nPREFIX xsd: <http://www.w3.org/2001/XMLSchema#>\n";

#[test]
fn range_filters_give_the_same_rows() {
    let data = data();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let queries = [
        "SELECT ?s ?km WHERE { ?s ex:ref ?lr . ?lr ex:km ?km . FILTER(?km >= 100 && ?km <= 102.5) }",
        "SELECT ?s WHERE { ?s ex:ref ?lr . ?lr ex:km ?km . FILTER(?km > 100.0 && ?km < 102.5) }",
        "SELECT ?s WHERE { ?s ex:ref ?lr . ?lr ex:km ?km . FILTER(100 < ?km && 101 >= ?km) }",
        "SELECT ?lr WHERE { ?lr ex:km ?km . FILTER(?km >= \"1.995e2\"^^xsd:double) }",
        "SELECT ?lr WHERE { ?lr ex:km ?km . FILTER(?km < -1) }",
        "SELECT ?lr WHERE { ?lr ex:km ?km . FILTER(?km >= 5 && ?km <= 4) }",
        "SELECT ?s WHERE { ?s a ex:Signal ; ex:ref ?lr . ?lr ex:line ex:line7 ; ex:km ?km . FILTER(?km >= 50 && ?km <= 60) }",
        "SELECT ?lr ?km WHERE { ?lr ex:km ?km . FILTER(?km >= 199 && ?km <= \"INF\"^^xsd:double && isLiteral(?km)) }",
        "SELECT (COUNT(*) AS ?n) WHERE { ?lr ex:km ?km . FILTER(?km >= 10 && ?km <= 11) }",
        "SELECT ?lr WHERE { ?lr ex:km ?km . OPTIONAL { ?lr ex:line ?l } FILTER(?km >= 10 && ?km <= 11) }",
    ];
    for q in queries {
        let q = format!("{P}{q}");
        assert_eq!(rows(&engine, &q), reference(&data, &q), "{q}");
    }
    // Writes patch the index of the predicate: the next query sees them.
    engine
        .update(&format!("{P}INSERT DATA {{ ex:new ex:km 100.75 . ex:new2 ex:km \"100.8\"^^xsd:decimal }} ; DELETE WHERE {{ ex:lr0 ex:km ?k }}"))
        .unwrap();
    let q =
        format!("{P}SELECT ?lr WHERE {{ ?lr ex:km ?km . FILTER(?km >= 100.7 && ?km <= 100.9) }}");
    let got = rows(&engine, &q);
    assert!(
        got.iter()
            .any(|r| r.contains("ex:new>") || r.ends_with("/new>")),
        "{got:?}"
    );
    assert!(got.iter().any(|r| r.ends_with("/new2>")), "{got:?}");
}

#[test]
fn explain_says_what_was_decided() {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let narrow = engine
        .explain(&format!("{P}SELECT ?s WHERE {{ ?s ex:ref ?lr . ?lr ex:km ?km . FILTER(?km >= 100 && ?km <= 100.5) }}"))
        .unwrap();
    assert!(
        narrow.contains("range ?km in [100, 100.5]") && narrow.contains(": inject"),
        "{narrow}"
    );
    let wide = engine
        .explain(&format!("{P}SELECT ?s WHERE {{ ?s ex:ref ?lr . ?lr ex:km ?km . FILTER(?km >= 0 && ?km <= 150) }}"))
        .unwrap();
    assert!(wide.contains(": filter"), "{wide}");
    // Inside GRAPH, and with FROM, the default-graph index does not apply.
    let graph = engine
        .explain(&format!("{P}SELECT ?s WHERE {{ GRAPH ?g {{ ?s ex:km ?km }} FILTER(?km >= 100 && ?km <= 100.5) }}"))
        .unwrap();
    assert!(!graph.contains("range ?km"), "{graph}");
}
