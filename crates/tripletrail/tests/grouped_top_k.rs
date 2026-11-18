//! `GROUP BY` under `ORDER BY` and `LIMIT`: when the order is made of counts and group keys,
//! only the first groups are kept, on ids. The rows are those the evaluator gives.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use std::fmt::Write;
use tripletrail::Engine;

fn values(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = engine.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    solutions
        .map(|s| {
            s.unwrap()
                .iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

#[test]
fn top_groups_by_counts_and_keys_match_the_evaluator() {
    let mut d = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..3000 {
        // Counts of 1 to 3 per subject, many ties; names and numbers as keys too.
        writeln!(d, "ex:s{i} a ex:T{} .", i % 5).unwrap();
        if i % 3 == 0 {
            writeln!(d, "ex:s{i} a ex:U .").unwrap();
        }
        if i % 7 == 0 {
            writeln!(d, "ex:s{i} a ex:V .").unwrap();
        }
        writeln!(
            d,
            "ex:s{i} ex:name \"n{}\" ; ex:num {} .",
            (i * 31) % 400,
            i % 250
        )
        .unwrap();
        if i % 4 == 0 {
            writeln!(d, "ex:s{i} ex:tag ex:g{} .", i % 9).unwrap();
        }
        // Keys of mixed kinds: numbers, strings and IRIs.
        match i % 3 {
            0 => writeln!(d, "ex:s{i} ex:mix {} .", i % 40).unwrap(),
            1 => writeln!(d, "ex:s{i} ex:mix \"m{}\" .", i % 50).unwrap(),
            _ => writeln!(d, "ex:s{i} ex:mix ex:k{} .", i % 60).unwrap(),
        }
    }
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let queries = [
        "SELECT ?s (COUNT(*) AS ?c) WHERE { ?s a ?t } GROUP BY ?s ORDER BY DESC(?c) ?s LIMIT 20",
        "SELECT ?s (COUNT(*) AS ?c) WHERE { ?s a ?t } GROUP BY ?s ORDER BY ?c DESC(?s) LIMIT 15",
        "SELECT ?s (COUNT(*) AS ?c) WHERE { ?s a ?t } GROUP BY ?s ORDER BY DESC(?s) LIMIT 10 OFFSET 5",
        "SELECT ?m (COUNT(*) AS ?c) WHERE { ?s ex:name ?m ; a ?t } GROUP BY ?m ORDER BY DESC(?c) ?m LIMIT 12",
        "SELECT ?m (COUNT(*) AS ?c) WHERE { ?s ex:name ?m ; a ?t } GROUP BY ?m ORDER BY DESC(?m) LIMIT 12",
        "SELECT ?v (COUNT(?s) AS ?c) WHERE { ?s ex:num ?v ; a ?t } GROUP BY ?v ORDER BY DESC(?c) DESC(?v) LIMIT 9",
        // A key unbound in some groups (unbound comes first).
        "SELECT ?g (COUNT(*) AS ?c) WHERE { ?s ex:num ?v OPTIONAL { ?s ex:tag ?g } } GROUP BY ?g ORDER BY ?g LIMIT 4",
        "SELECT ?g ?t (COUNT(*) AS ?c) WHERE { ?s a ?t OPTIONAL { ?s ex:tag ?g } } GROUP BY ?g ?t ORDER BY DESC(?c) ?g DESC(?t) LIMIT 11",
        // Renamed by the projection.
        "SELECT ?x ?c WHERE { { SELECT ?s (COUNT(*) AS ?c) WHERE { ?s a ?t } GROUP BY ?s } BIND(?s AS ?x) } ORDER BY DESC(?c) ?x LIMIT 5",
        // A later key that is not a count or group key: the cut keeps the ties of the first keys.
        "SELECT ?m (COUNT(*) AS ?c) WHERE { ?s ex:name ?m ; a ?t } GROUP BY ?m ORDER BY DESC(?c) STR(?m) LIMIT 13",
        "SELECT ?m (COUNT(DISTINCT ?s) AS ?c) WHERE { ?s ex:name ?m ; a ?t } GROUP BY ?m ORDER BY ?c DESC(STR(?m)) LIMIT 7 OFFSET 3",
        "SELECT ?m (COUNT(*) AS ?c) (MIN(?v) AS ?low) WHERE { ?s ex:name ?m ; ex:num ?v } GROUP BY ?m ORDER BY DESC(?c) ?low ?m LIMIT 9",
        // Group keys of mixed kinds.
        "SELECT ?k (COUNT(*) AS ?c) WHERE { ?s ex:mix ?k ; a ?t } GROUP BY ?k ORDER BY ?k LIMIT 8",
        "SELECT ?k (COUNT(*) AS ?c) WHERE { ?s ex:mix ?k ; a ?t } GROUP BY ?k ORDER BY DESC(?k) LIMIT 8",
        "SELECT ?k (COUNT(*) AS ?c) WHERE { ?s ex:mix ?k ; a ?t } GROUP BY ?k ORDER BY DESC(?c) ?k LIMIT 21",
        "SELECT ?k (COUNT(*) AS ?c) WHERE { ?s ex:mix ?k ; a ?t } GROUP BY ?k ORDER BY ?c DESC(?k) LIMIT 6 OFFSET 2",
    ];
    for q in queries {
        let q = format!("PREFIX ex: <http://example.org/> {q}");
        let slow = q.replacen(" } GROUP BY", " FILTER(true) } GROUP BY", 1);
        assert_ne!(q, slow);
        let fast = values(&engine, &q);
        assert!(!fast.is_empty(), "{q}");
        assert_eq!(fast, values(&engine, &slow), "{q}");
    }
}
