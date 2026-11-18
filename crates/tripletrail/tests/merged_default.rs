//! A default graph that merges several graphs (the union default graph, `FROM` with several
//! graphs) gives each triple once, as an RDF merge (issue #376): a triple in two graphs was
//! returned twice, which made counts and joins wrong.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

const DATA: &str = "@prefix : <http://example.org/> .
GRAPH :g1 { :a :p :b . :a :p :c . }
GRAPH :g2 { :a :p :b . }
GRAPH :g3 { :x :q :y . }
";

fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let query = format!("PREFIX : <http://example.org/>\n{query}");
    let QueryResults::Solutions(solutions) = engine.query(&query).unwrap() else {
        panic!("not a SELECT")
    };
    let mut rows: Vec<String> = solutions
        .map(|s| {
            s.unwrap()
                .iter()
                .map(|(v, t)| format!("{}={t}", v.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
                .replace("<http://example.org/", ":")
                .replace('>', "")
                .replace("^^<http://www.w3.org/2001/XMLSchema#integer", "")
        })
        .collect();
    rows.sort();
    rows
}

fn count(engine: &Engine, query: &str) -> String {
    let r = rows(engine, query);
    assert_eq!(r.len(), 1, "{r:?}");
    r[0].clone()
}

fn check(e: &Engine) {
    assert_eq!(count(e, "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }"), "n=\"3\"");
    assert_eq!(
        rows(e, "SELECT ?s ?o { ?s :p ?o }"),
        ["s=:a o=:b", "s=:a o=:c"]
    );
    assert_eq!(count(e, "SELECT (COUNT(*) AS ?n) { :a :p ?o }"), "n=\"2\"");
    assert_eq!(
        count(e, "SELECT (COUNT(*) AS ?n) { ?s :p ?o . ?s :p ?o2 }"),
        "n=\"4\""
    );
    assert_eq!(
        rows(e, "SELECT ?s (COUNT(*) AS ?n) { ?s :p ?o } GROUP BY ?s"),
        ["s=:a n=\"2\""]
    );
    assert_eq!(count(e, "SELECT (COUNT(*) AS ?n) { :a :p :b }"), "n=\"1\"");
    // Each order a scan can read (SPO, POS, OSP).
    assert_eq!(rows(e, "SELECT ?s { ?s :p :b }"), ["s=:a"]);
    assert_eq!(rows(e, "SELECT ?p { :a ?p :b }"), ["p=:p"]);
    assert_eq!(rows(e, "SELECT ?s ?p { ?s ?p :b }"), ["s=:a p=:p"]);
    assert_eq!(
        rows(e, "SELECT ?p ?o { :a ?p ?o }"),
        ["p=:p o=:b", "p=:p o=:c"]
    );
    // GRAPH ?g still sees each graph's own copy.
    assert_eq!(
        count(e, "SELECT (COUNT(*) AS ?n) { GRAPH ?g { :a :p :b } }"),
        "n=\"2\""
    );
}

/// The union default graph of Oxigraph's store (`--store oxigraph`) gives each triple once too
/// (#778): there the evaluator merges the graphs, not the store.
#[test]
fn the_union_default_graph_of_the_oxigraph_store_gives_each_triple_once() {
    let e = Engine::new_oxigraph().unwrap();
    e.load(RdfFormat::TriG, DATA.as_bytes()).unwrap();
    e.set_union_default_graph(true);
    check(&e);
    assert_eq!(
        count(&e, "SELECT (COUNT(*) AS ?n) { ?s :p+ ?o }"),
        "n=\"2\""
    );
    assert_eq!(
        count(&e, "SELECT (COUNT(*) AS ?n) FROM :g1 FROM :g2 { ?s ?p ?o }"),
        "n=\"2\""
    );
    // Whichever graph is listed first (#799: each triple is kept from one graph by a lookup).
    assert_eq!(
        rows(&e, "SELECT ?o FROM :g2 FROM :g1 { :a :p ?o }"),
        ["o=:b", "o=:c"]
    );
    assert_eq!(
        count(
            &e,
            "SELECT (COUNT(*) AS ?n) FROM :g2 FROM :g3 FROM :g1 { ?s ?p ?o }"
        ),
        "n=\"3\""
    );
    // A triple in the stored default graph and in named graphs, once in the union.
    e.update("PREFIX : <http://example.org/> INSERT DATA { :a :p :b . :z :p :b }")
        .unwrap();
    assert_eq!(rows(&e, "SELECT ?s { ?s :p :b }"), ["s=:a", "s=:z"]);
    assert_eq!(count(&e, "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }"), "n=\"4\"");
    e.update("PREFIX : <http://example.org/> DELETE DATA { :a :p :b . :z :p :b }")
        .unwrap();
    // A path whose edges are in several graphs counts each pair of ends once.
    e.update(
        "PREFIX : <http://example.org/> INSERT DATA { \
         GRAPH :g1 { :b :p :c . :c :p :d } GRAPH :g2 { :b :p :c . :c :p :d } }",
    )
    .unwrap();
    for e in [&e, &{
        let m = Engine::new_tripletrail();
        m.load(RdfFormat::TriG, DATA.as_bytes()).unwrap();
        m.update(
            "PREFIX : <http://example.org/> INSERT DATA { \
             GRAPH :g1 { :b :p :c . :c :p :d } GRAPH :g2 { :b :p :c . :c :p :d } }",
        )
        .unwrap();
        m.set_union_default_graph(true);
        m
    }] {
        assert_eq!(count(e, "SELECT (COUNT(*) AS ?n) { ?s :p+ ?o }"), "n=\"6\"");
        assert_eq!(
            count(e, "SELECT (COUNT(*) AS ?n) { ?s :p/:p ?o }"),
            "n=\"3\""
        );
        assert_eq!(count(e, "SELECT (COUNT(*) AS ?n) { ?s :p ?o }"), "n=\"4\"");
    }
}

#[test]
fn the_union_default_graph_gives_each_triple_once() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::TriG, DATA.as_bytes()).unwrap();
    e.set_union_default_graph(true);
    check(&e);
    // After writes too (in the store's delta, not its segment).
    e.update("PREFIX : <http://example.org/> INSERT DATA { GRAPH :g3 { :a :p :b . :a :p :d } }")
        .unwrap();
    assert_eq!(count(&e, "SELECT (COUNT(*) AS ?n) { ?s :p ?o }"), "n=\"3\"");
    assert_eq!(count(&e, "SELECT (COUNT(*) AS ?n) { :a :p :b }"), "n=\"1\"");
    // An update's WHERE reads the merge too.
    e.update(
        "PREFIX : <http://example.org/> INSERT { GRAPH :out { :r :n ?n } } \
         WHERE { SELECT (COUNT(*) AS ?n) { ?s :p ?o } }",
    )
    .unwrap();
    assert_eq!(
        rows(&e, "SELECT ?n { GRAPH :out { :r :n ?n } }"),
        ["n=\"3\""]
    );
}

#[test]
fn from_with_several_graphs_gives_each_triple_once() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::TriG, DATA.as_bytes()).unwrap();
    assert_eq!(
        count(&e, "SELECT (COUNT(*) AS ?n) FROM :g1 FROM :g2 { ?s ?p ?o }"),
        "n=\"2\""
    );
    assert_eq!(
        count(&e, "SELECT (COUNT(*) AS ?n) FROM :g2 FROM :g3 { ?s ?p ?o }"),
        "n=\"2\""
    );
    assert_eq!(
        count(&e, "SELECT (COUNT(*) AS ?n) FROM :g1 { ?s ?p ?o }"),
        "n=\"2\""
    );
    // Each order a scan can read, graph by graph.
    for (q, expected) in [
        (
            "SELECT ?s ?o FROM :g1 FROM :g2 { ?s :p ?o }",
            &["s=:a o=:b", "s=:a o=:c"][..],
        ),
        ("SELECT ?s FROM :g1 FROM :g2 { ?s :p :b }", &["s=:a"]),
        ("SELECT ?p FROM :g1 FROM :g2 { :a ?p :b }", &["p=:p"]),
        (
            "SELECT ?s ?p FROM :g1 FROM :g2 { ?s ?p :b }",
            &["s=:a p=:p"],
        ),
        (
            "SELECT ?p ?o FROM :g1 FROM :g2 { :a ?p ?o }",
            &["p=:p o=:b", "p=:p o=:c"],
        ),
        (
            "SELECT ?o FROM :g2 FROM :g1 { :a :p ?o . :a :p ?o }",
            &["o=:b", "o=:c"],
        ),
    ] {
        assert_eq!(rows(&e, q), expected, "{q}");
    }
    // The plain default graph is empty here.
    assert_eq!(count(&e, "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }"), "n=\"0\"");
}

/// `COPY`, `MOVE` and `ADD DEFAULT` on a store that reads the union as its default graph take
/// the stored default graph, as Fuseki does under `tdb2:unionDefaultGraph` (#782). A `USING`
/// that merges the target is read before the target changes: the target ends up holding the
/// whole merge, its own triples included.
#[test]
fn copy_of_a_merged_default_graph_into_one_of_its_graphs_keeps_that_graph() {
    let all = "SELECT ?g ?s ?p ?o { GRAPH ?g { ?s ?p ?o } }";
    let stored_default = "SELECT ?s { ?s ?p ?o }";
    for store in ["tripletrail", "oxigraph"] {
        let engine = || {
            let e = if store == "tripletrail" {
                Engine::new_tripletrail()
            } else {
                Engine::new_oxigraph().unwrap()
            };
            e.load(RdfFormat::TriG, DATA.as_bytes()).unwrap();
            e.load(
                RdfFormat::Turtle,
                &b"<http://example.org/d> <http://example.org/p> <http://example.org/e> ."[..],
            )
            .unwrap();
            e
        };
        let in_g1 = |e: &Engine| -> Vec<String> {
            rows(e, all)
                .into_iter()
                .filter(|r| r.starts_with("g=:g1 "))
                .collect()
        };
        let d = "g=:g1 s=:d p=:p o=:e";
        let g1 = ["g=:g1 s=:a p=:p o=:b", "g=:g1 s=:a p=:p o=:c"];
        for (update, after, default_left) in [
            ("COPY DEFAULT TO :g1", vec![d], true),
            ("MOVE DEFAULT TO :g1", vec![d], false),
            ("ADD DEFAULT TO :g1", vec![g1[0], g1[1], d], true),
            // A copy to a graph of its own, in a request with other operations.
            (
                "INSERT DATA { GRAPH :g1 { :n :p :m } } ; COPY DEFAULT TO :g4 ; ADD :g4 TO :g1",
                vec![g1[0], g1[1], d, "g=:g1 s=:n p=:p o=:m"],
                true,
            ),
        ] {
            let e = engine();
            e.set_union_default_graph(true);
            e.update(&format!("PREFIX : <http://example.org/> {update}"))
                .unwrap();
            // Queries still read the union.
            assert!(rows(&e, stored_default).len() >= 3, "{store}: {update}");
            e.set_union_default_graph(false);
            assert_eq!(in_g1(&e), after, "{store}: {update}");
            let left = rows(&e, stored_default);
            if default_left {
                assert_eq!(left, ["s=:d"], "{store}: {update}");
            } else {
                assert!(left.is_empty(), "{store}: {left:?}");
            }
        }
        // An update written like the ones COPY and ADD become still reads the union.
        let ab = "g=:g1 s=:a p=:p o=:b";
        let ac = "g=:g1 s=:a p=:p o=:c";
        let xy = "g=:g1 s=:x p=:q o=:y";
        for (update, after) in [
            (
                "INSERT { GRAPH :g1 { ?s ?p ?o } } WHERE { ?s ?p ?o }",
                vec![ab, ac, d, xy],
            ),
            // The drop runs first: the union no longer holds g1's own :a :p :c.
            (
                "DROP SILENT GRAPH :g1 ; INSERT { GRAPH :g1 { ?s ?p ?o } } WHERE { ?s ?p ?o }",
                vec![ab, d, xy],
            ),
        ] {
            let e = engine();
            e.set_union_default_graph(true);
            e.update(&format!("PREFIX : <http://example.org/> {update}"))
                .unwrap();
            e.set_union_default_graph(false);
            assert_eq!(in_g1(&e), after, "{store}: {update}");
        }
        // USING-graph-uri that names the target is such a merge too.
        let e = engine();
        let ex =
            |l: &str| oxigraph::model::NamedNode::new(format!("http://example.org/{l}")).unwrap();
        let options = tripletrail::UpdateOptions {
            using: Some((vec![ex("g1"), ex("g3")], vec![])),
            ..Default::default()
        };
        e.update_with_options(
            "PREFIX : <http://example.org/> COPY DEFAULT TO :g1",
            &options,
        )
        .unwrap();
        assert_eq!(
            in_g1(&e),
            [
                "g=:g1 s=:a p=:p o=:b",
                "g=:g1 s=:a p=:p o=:c",
                "g=:g1 s=:x p=:q o=:y"
            ],
            "{store}: USING"
        );
        // Without a merge, the copy replaces the graph.
        let e = engine();
        e.update("PREFIX : <http://example.org/> COPY DEFAULT TO :g1")
            .unwrap();
        assert_eq!(in_g1(&e), ["g=:g1 s=:d p=:p o=:e"], "{store}");
    }
}

/// Without named graphs the union default graph is the default graph: its counts are answered
/// on ids as the default graph's (#808), and from the first named graph on as the merge.
#[test]
fn the_union_default_graph_of_a_store_without_named_graphs_is_its_default_graph() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        "@prefix : <http://example.org/> . :a :p :b , :c . :b :p :c . :c a :T . :b a :T , :U ."
            .as_bytes(),
    )
    .unwrap();
    e.set_union_default_graph(true);
    let queries = [
        ("SELECT (COUNT(*) AS ?n) { ?s ?p ?o }", "n=\"6\""),
        (
            "SELECT (COUNT(*) AS ?n) { ?s :p ?o MINUS { ?o a ?t } }",
            "n=\"0\"",
        ),
        (
            "SELECT (COUNT(*) AS ?n) { ?s :p ?o OPTIONAL { ?o a ?t } }",
            "n=\"4\"",
        ),
        ("SELECT (COUNT(*) AS ?n) { ?s :p ?j . ?j :p ?o }", "n=\"1\""),
    ];
    for (q, n) in queries {
        assert_eq!(count(&e, q), n, "{q}");
    }
    // A named graph that repeats a triple and adds one: the merge has each triple once.
    e.update("PREFIX : <http://example.org/> INSERT DATA { GRAPH :g { :a :p :b . :d :p :a } }")
        .unwrap();
    assert_eq!(count(&e, "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }"), "n=\"7\"");
    assert_eq!(
        count(&e, "SELECT (COUNT(*) AS ?n) { ?s :p ?o MINUS { ?o a ?t } }"),
        "n=\"1\""
    );
}

/// With named graphs, the union default graph is read on ids from every graph without the
/// copies of a triple held by several (#808): counts, groups and joins equal those of the
/// same triples, each once, in a store's default graph, before and after writes.
#[test]
fn the_union_default_graph_with_named_graphs_counts_each_triple_once_on_ids() {
    let mut trig = String::from("@prefix : <http://example.org/> .\n");
    let mut merged = std::collections::BTreeSet::new();
    for i in 0..3_000u32 {
        // Most triples in one graph, some in two or three (the default graph among them).
        let triple = match i % 5 {
            0 => format!(":s{} a :T{}", i % 700, i % 4),
            1 => format!(":s{} :on :s{}", i % 700, (i * 7) % 700),
            2 => format!(":s{} :label \"l{}\"", i % 700, i % 13),
            3 => format!(":s{} :at :x{}", i % 700, i % 50),
            _ => format!(":x{} a :Place", i % 50),
        };
        let graphs: &[&str] = match i % 7 {
            0 => &["", ":g1"],
            1 => &[":g1", ":g2"],
            2 => &["", ":g1", ":g2"],
            3 => &[":g2"],
            4 => &[":g3"],
            _ => &[""],
        };
        for g in graphs {
            if g.is_empty() {
                trig.push_str(&format!("{triple} .\n"));
            } else {
                trig.push_str(&format!("{g} {{ {triple} . }}\n"));
            }
        }
        merged.insert(triple);
    }
    let union = Engine::new_tripletrail();
    union.load(RdfFormat::TriG, trig.as_bytes()).unwrap();
    union.set_union_default_graph(true);
    let flat = Engine::new_tripletrail();
    let ttl: String = std::iter::once("@prefix : <http://example.org/> .\n".to_owned())
        .chain(merged.iter().map(|t| format!("{t} .\n")))
        .collect();
    flat.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let queries = [
        "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }",
        "SELECT (COUNT(DISTINCT ?s) AS ?n) { ?s ?p ?o }",
        "SELECT ?p (COUNT(*) AS ?n) { ?s ?p ?o } GROUP BY ?p",
        "SELECT ?t (COUNT(?s) AS ?n) { ?s a ?t } GROUP BY ?t",
        "SELECT (COUNT(*) AS ?n) { ?s :on ?o . ?o a ?t }",
        "SELECT (COUNT(*) AS ?n) { ?s :label ?l MINUS { ?s a :T1 } }",
        "SELECT (COUNT(*) AS ?n) { ?s :on ?o OPTIONAL { ?o :label ?l } }",
        "SELECT (COUNT(*) AS ?n) { ?s :at ?x FILTER EXISTS { ?x a :Place } }",
        "SELECT (COUNT(*) AS ?n) { ?s :on ?o MINUS { ?o :on ?j . ?j a ?t } }",
        "SELECT (COUNT(*) AS ?n) { { ?s a ?t } UNION { ?s :at ?t } ?s :label ?l }",
        "SELECT ?t (COUNT(*) AS ?n) (SAMPLE(?s) AS ?x) { ?s a ?t . ?s :at ?p } GROUP BY ?t HAVING (COUNT(*) > 0)",
        "SELECT ?s ?l { ?s :label ?l . ?s a :T2 } ORDER BY ?s ?l LIMIT 20",
    ];
    let check = |stage: &str| {
        for q in queries {
            let mut a = rows(&union, q);
            let mut b = rows(&flat, q);
            // SAMPLE may pick another member of the group.
            if q.contains("SAMPLE") {
                for r in a.iter_mut().chain(b.iter_mut()) {
                    *r = r.split(" x=").next().unwrap().to_owned();
                }
            }
            assert_eq!(a, b, "{stage}: {q}");
        }
        // A graph is still read whole.
        assert_eq!(
            count(&union, "SELECT (COUNT(*) AS ?n) { GRAPH :g2 { ?s ?p ?o } }"),
            count(
                &union,
                "SELECT (COUNT(*) AS ?n) { GRAPH :g2 { ?s ?p ?o FILTER(true) } }"
            ),
        );
    };
    check("loaded");
    let update = "PREFIX : <http://example.org/> INSERT DATA { GRAPH :g3 { :s1 a :T1 . :s9 :label \"new\" } :s9 :label \"new\" } ; DELETE DATA { GRAPH :g1 { :s0 a :T0 } }";
    union.update(update).unwrap();
    flat.update("PREFIX : <http://example.org/> INSERT DATA { :s1 a :T1 . :s9 :label \"new\" }")
        .unwrap();
    // :s0 a :T0 is also in the default graph (i = 0), so the merge keeps it.
    check("after writes");
}

/// With the union default graph, the built-in services read the union too (#861): a named
/// network stored in a named graph, its edges in another, and the st: and window services'
/// sub-selects.
#[test]
fn the_built_in_services_read_the_union_default_graph() {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            r#"@prefix ex: <http://example.com/> .
               @prefix pgr: <http://w3id.org/tripletrail/pgr#> .
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .
               <urn:g1> { ex:net a pgr:Network ; pgr:edgeQuery "SELECT ?id ?source ?target ?cost { ?id <http://example.com/from> ?source ; <http://example.com/to> ?target ; <http://example.com/len> ?cost }" . }
               <urn:g2> { ex:e1 ex:from ex:a ; ex:to ex:b ; ex:len 3 .
                          ex:f geo:asWKT "MULTIPOINT((1 1), (2 2))"^^geo:wktLiteral . }"#
                .as_bytes(),
        )
        .unwrap();
    engine.set_union_default_graph(true);
    let route = r#"PREFIX ex: <http://example.com/> PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
        SELECT (MAX(?c) AS ?n) { SERVICE <http://w3id.org/tripletrail/pgr> {
          [] pgr:function pgr:pgr_dijkstra ; pgr:network ex:net ; pgr:start_vid ex:a ;
             pgr:end_vid ex:b ; pgr:agg_cost ?c } }"#;
    assert_eq!(
        rows(&engine, route),
        ["n=\"3\"^^<http://www.w3.org/2001/XMLSchema#double"]
    );
    let st = r#"PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX st: <http://w3id.org/tripletrail/st#>
        SELECT (COUNT(*) AS ?n) { SERVICE <http://w3id.org/tripletrail/st> {
          [] st:function st:ST_Dump ; st:geom ?part .
          { SELECT ?id ?geom WHERE { ?id geo:asWKT ?geom } } } }"#;
    assert_eq!(count(&engine, st), "n=\"2\"");
    let window = r#"PREFIX win: <http://w3id.org/tripletrail/window#>
        SELECT (COUNT(*) AS ?n) { SERVICE <http://w3id.org/tripletrail/window> {
          [] win:orderBy ?id ; win:rowNumber ?r .
          { SELECT ?id WHERE { ?id <http://example.com/from> ?x } } } }"#;
    assert_eq!(count(&engine, window), "n=\"1\"");
}
