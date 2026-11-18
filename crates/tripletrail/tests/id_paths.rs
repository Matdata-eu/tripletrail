//! Transitive paths evaluated on term ids (#11): the same rows as the evaluator's.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

fn data() -> String {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    // A tree, large enough for the rewrite, with a cycle and a self-loop at its edge.
    for i in 1..3000 {
        ttl.push_str(&format!("ex:n{i} ex:up ex:n{} .\n", (i - 1) / 4));
        ttl.push_str(&format!("ex:n{i} a ex:C{} .\n", i % 5));
    }
    ttl.push_str("ex:n0 ex:up ex:loop1 . ex:loop1 ex:up ex:loop2 . ex:loop2 ex:up ex:loop1 .\n");
    ttl.push_str("ex:loop2 ex:up ex:loop2 .\n");
    ttl.push_str("ex:n5 ex:up \"a literal\" .\n");
    ttl.push_str("GRAPH ex:g { ex:n7 ex:up ex:elsewhere . }\n");
    ttl
}

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
fn closures_on_ids_give_the_evaluators_rows() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::TriG, data().as_bytes()).unwrap();
    for body in [
        "?s ex:up+ ?o",
        "?s ^ex:up+ ?o",
        "?s (^ex:up)+ ?o",
        "?s ex:up+ ?o . ?o a ex:C2",
        "?s a ex:C1 . ?s ex:up+ ?o FILTER NOT EXISTS { ?o a ex:C3 }",
        "?s ex:up+ ?o MINUS { ?s a ex:C4 }",
        "?s ex:up+ ex:n3 . ?s a ex:C1 FILTER(?s != ex:n13)",
        "ex:n2999 ex:up+ ?o FILTER(isIRI(?o))",
        "ex:n2999 ex:up* ?o FILTER(isIRI(?o))",
        "?s ex:up* ex:n12 FILTER(isIRI(?s))",
        "ex:nowhere ex:up* ?o FILTER(isIRI(?o))",
        "?s ex:up+ \"a literal\" FILTER(isIRI(?s))",
        "?s ex:up+ ?s FILTER(isIRI(?s))",
        "?s ex:nothing+ ?o FILTER(isIRI(?s))",
    ] {
        let q = format!("PREFIX ex: <http://example.org/> SELECT * WHERE {{ {body} }}");
        let off = format!("#pragma tt.id-rewrites off\n{q}");
        let (got, want) = (rows(&e, &q), rows(&e, &off));
        assert!(
            !want.is_empty() || body.contains("nothing") || body.contains("nowhere"),
            "{body}"
        );
        assert_eq!(got, want, "{body}");
        // Counted and grouped the same too.
        let counted = format!(
            "PREFIX ex: <http://example.org/> SELECT ?o (COUNT(*) AS ?n) WHERE {{ {body} }} GROUP BY ?o"
        );
        if body.contains("?o") {
            let off = format!("#pragma tt.id-rewrites off\n{counted}");
            assert_eq!(rows(&e, &counted), rows(&e, &off), "{body}");
        }
    }
}
