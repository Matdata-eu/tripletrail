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
    ttl.push_str("ex:n2999 ex:id \"rare\" . ex:n1 ex:id \"top\" . ex:n3 ex:id \"top\" .\n");
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
        // Joined with other parts: searched from the values they give, either way.
        "?s ex:id \"rare\" . ?s ex:up+ ?o",
        "?s ex:id \"rare\" . ?s ex:up* ?o",
        "VALUES ?s { ex:n2999 ex:n77 } ?s ex:up+ ?o",
        "?s ex:id \"rare\" OPTIONAL { ?s ex:up+ ?o }",
        "?s ex:id \"rare\" . ?s ex:up+ ?m . ?m ex:up+ ?o",
        "{ ?s ex:id \"rare\" } UNION { ?s ex:id \"top\" } ?s ex:up+ ?o",
        "?o ex:id \"top\" . ?s ex:up+ ?o",
        "?o ex:id \"top\" . ?s ^ex:up* ?o",
        "?s a ex:C1 . ?s ex:up+ ?o",
        "?o a ex:C1 . ?s ^ex:up+ ?o",
        "?s ex:id ?x . ?s ex:up+ ?o MINUS { ?o ex:id \"top\" }",
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

/// A path joined with a part that gives few values searches from those values, not over the
/// whole closure: much faster than the closure on a larger tree.
#[test]
fn a_joined_path_searches_from_the_few_values_given() {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 1..60_000 {
        ttl.push_str(&format!("ex:n{i} ex:up ex:n{} .\n", (i - 1) / 4));
    }
    ttl.push_str("ex:n59999 ex:id \"rare\" . ex:n5000 ex:id \"inner\" .\n");
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let time = |body: &str| {
        let q =
            format!("PREFIX ex: <http://example.org/> SELECT (COUNT(*) AS ?n) WHERE {{ {body} }}");
        let start = std::time::Instant::now();
        let n = rows(&e, &q);
        (start.elapsed(), n)
    };
    let (all, _) = time("?s ex:up+ ?o");
    for body in [
        "?s ex:id \"rare\" . ?s ex:up+ ?o",
        "?s ex:id \"rare\" OPTIONAL { ?s ex:up+ ?o }",
        "VALUES ?s { ex:n59999 } ?s ex:up+ ?o",
        "?o ex:id \"inner\" . ?s ex:up+ ?o",
        "?o ex:id \"inner\" . ?o ^ex:up+ ?s",
    ] {
        let (few, n) = time(body);
        assert!(
            few * 10 < all,
            "{body}: {few:?} against {all:?} for the whole closure"
        );
        let off = format!(
            "#pragma tt.id-rewrites off\nPREFIX ex: <http://example.org/> SELECT (COUNT(*) AS ?n) WHERE {{ {body} }}"
        );
        assert_eq!(n, rows(&e, &off), "{body}");
    }
}

/// A level that would make more pairs than the query's memory allows is not built: a hub
/// that every start reaches, with many leaves, is left to the evaluator.
#[test]
fn a_level_past_the_memory_limit_is_left_to_the_evaluator() {
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..50 {
        ttl.push_str(&format!("ex:s{i} a ex:Src ; ex:e ex:hub .\n"));
    }
    for i in 0..4000 {
        ttl.push_str(&format!("ex:hub ex:e ex:leaf{i} .\n"));
    }
    // Enough edges that 50 starts are searched level by level.
    for i in 0..10_000 {
        ttl.push_str(&format!("ex:x{i} ex:e ex:y{i} .\n"));
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let q = "PREFIX ex: <http://example.org/> SELECT (COUNT(*) AS ?n) WHERE { ?s a ex:Src . ?s ex:e+ ?o }";
    let n = rows(&e, q);
    assert_eq!(n, rows(&e, &format!("#pragma tt.id-rewrites off\n{q}")));
    assert!(n[0].contains("200050"), "{n:?}");
    // 200,050 pairs take about a million ids in the search; under a limit of 1 MiB
    // (131,072 ids) it gives up before the level and the evaluator counts them streaming.
    assert_eq!(rows(&e.with_query_memory(1 << 20), q), n);
}
