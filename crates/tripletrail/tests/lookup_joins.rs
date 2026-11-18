//! Lookup joins (#494): a join without keys, or with weak keys, on a basic graph pattern that
//! shares a variable made by BIND looks the pattern up per row, with the same answers as the
//! hash join. A test file of its own: the setting is for the whole process.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

fn engine() -> Engine {
    let mut d = String::new();
    for i in 0..400 {
        d += &format!(
            "<urn:l{i}> <urn:seq> <urn:seq{}> .\n<urn:l{i}> <urn:m> \"{}\" .\n<urn:ne{i}> <urn:from> <urn:l{i}> .\n<urn:l{i}> <urn:valid> \"2020-01-{:02}\" .\n",
            i / 4,
            if i % 4 == 0 { "1.5" } else { "0.0" },
            1 + i % 28
        );
    }
    for i in 0..60 {
        d += &format!(
            "<urn:t{i}> a <urn:Turn> ; <urn:to> \"seq{}\" ; <urn:at> \"0.0\" .\n",
            (i * 7) % 100
        );
        // Half of them name a net element; the others leave ?ne unbound.
        if i % 2 == 0 {
            d += &format!("<urn:t{i}> <urn:ne> \"ne{}\" .\n", (i * 13) % 400);
        }
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    engine
}

fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = engine.query(query).unwrap() else {
        panic!()
    };
    let mut rows: Vec<String> = s
        .map(|r| {
            let r = r.unwrap();
            r.iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn lookups_give_the_hash_joins_answers() {
    let engine = engine();
    let queries = [
        // OPTIONAL on an IRI made by BIND (#484), bound in half of the rows.
        "SELECT ?t ?v { ?t a <urn:Turn> OPTIONAL { ?t <urn:ne> ?id } BIND(IRI(CONCAT(\"urn:\", ?id)) AS ?ne) OPTIONAL { ?ne <urn:from>/<urn:valid> ?v } }",
        // A join on an IRI made by BIND and a weak literal key (#486).
        "SELECT ?t ?l { ?t a <urn:Turn> ; <urn:to> ?to ; <urn:at> ?m BIND(IRI(CONCAT(\"urn:\", ?to)) AS ?s) ?l <urn:seq> ?s ; <urn:m> ?m }",
        // No shared key at all but a BIND-made IRI.
        "SELECT ?t ?x { ?t <urn:to> ?to BIND(IRI(CONCAT(\"urn:\", ?to)) AS ?s) ?x <urn:seq> ?s }",
        // A genuine cross product stays one.
        "SELECT (COUNT(*) AS ?n) { ?t a <urn:Turn> . ?x <urn:from> ?y }",
    ];
    let expected: Vec<Vec<String>> = queries.iter().map(|q| rows(&engine, q)).collect();
    assert!(expected.iter().all(|r| !r.is_empty()));
    tripletrail::set_lookup_joins(true);
    for (q, e) in queries.iter().zip(&expected) {
        assert_eq!(&rows(&engine, q), e, "{q}");
    }
}
