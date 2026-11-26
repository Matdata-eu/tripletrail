//! An `OPTIONAL` whose left side has no rows reads nothing of its right side (#256: the
//! labels of a map facet without values under a filter were every label of the store).

use std::time::{Duration, Instant};
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

fn rows(e: &Engine, q: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = e.query(q).unwrap() else {
        panic!()
    };
    let mut out: Vec<String> = s
        .map(|r| {
            r.unwrap()
                .iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out.sort();
    out
}

#[test]
fn labels_of_no_values_are_not_read() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         @prefix skos: <http://www.w3.org/2004/02/skos/core#> .\n",
    );
    for i in 0..150_000 {
        ttl.push_str(&format!(
            "ex:c{i} rdfs:label \"c{i}\" ; skos:prefLabel \"C{i}\" .\n"
        ));
    }
    for i in 0..300 {
        ttl.push_str(&format!("ex:t{i} a ex:Track ; ex:kind ex:c{} .\n", i % 7));
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    // The map's facet query, as the server writes it.
    let facet = |p: &str| {
        format!(
            "PREFIX ex: <http://example.org/> \
             PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> \
             PREFIX skos: <http://www.w3.org/2004/02/skos/core#> \
             SELECT ?v ?n (SAMPLE(?l) AS ?label) WHERE {{ \
               {{ SELECT ?v (COUNT(DISTINCT ?f) AS ?n) WHERE {{ ?f a ex:Track . ?f {p} ?v }} \
                 GROUP BY ?v ORDER BY DESC(?n) LIMIT 51 }} \
               OPTIONAL {{ ?v rdfs:label|skos:prefLabel ?l }} }} \
             GROUP BY ?v ?n ORDER BY DESC(?n) STR(?v)"
        )
    };
    let time = |q: &str| {
        let start = Instant::now();
        let n = rows(&e, q);
        (start.elapsed(), n)
    };
    // Every label once, to time the others against.
    let (all, _) = time(
        "PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> \
         PREFIX skos: <http://www.w3.org/2004/02/skos/core#> \
         SELECT (COUNT(*) AS ?n) WHERE { ?v rdfs:label|skos:prefLabel ?l }",
    );
    let (none, n) = time(&facet("ex:nothing"));
    assert!(n.is_empty());
    assert!(
        none * 5 < all + Duration::from_millis(5),
        "{none:?} against {all:?} for every label"
    );
    // With values, the same labels as without the id rewrites.
    let (_, n) = time(&facet("ex:kind"));
    assert_eq!(n.len(), 7);
    let off = format!("#pragma tt.id-rewrites off\n{}", facet("ex:kind"));
    assert_eq!(n, rows(&e, &off));
    // An empty left side under a hash join keyed on a variable too.
    let q = "PREFIX ex: <http://example.org/> \
             PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> \
             SELECT * WHERE { ?f ex:nothing ?v OPTIONAL { ?v rdfs:label ?l } }";
    assert!(rows(&e, q).is_empty());
}
