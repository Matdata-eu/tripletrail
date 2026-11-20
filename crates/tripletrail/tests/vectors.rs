//! Embedding vectors and the similarity functions through the engine (#236).

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const DATA: &str = r#"@prefix ex: <http://example.com/> .
@prefix tt: <http://w3id.org/tripletrail/fn#> .
ex:crossing ex:embedding "[0.9, 0.1, 0.0]"^^tt:vector .
ex:signal ex:embedding "[0.1, 0.9, 0.1]"^^tt:vector .
ex:bridge ex:embedding "[0.7, 0.0, 0.7]"^^tt:vector .
ex:odd ex:embedding "[1, 2]"^^tt:vector .
ex:broken ex:embedding "[1, x, 2]"^^tt:vector .
"#;

fn rows(e: &Engine, q: &str) -> Vec<Vec<String>> {
    let QueryResults::Solutions(s) = e
        .query(&format!(
            "PREFIX ex: <http://example.com/> PREFIX tt: <http://w3id.org/tripletrail/fn#> {q}"
        ))
        .unwrap()
    else {
        panic!()
    };
    s.map(|r| {
        let r = r.unwrap();
        r.iter().map(|(_, t)| t.to_string()).collect()
    })
    .collect()
}

fn engines() -> Vec<Engine> {
    let all = vec![Engine::new().unwrap(), Engine::new_tripletrail()];
    for e in &all {
        e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    }
    all
}

#[test]
fn the_nearest_vectors_come_first() {
    for e in engines() {
        let r = rows(
            &e,
            r#"SELECT ?s WHERE { ?s ex:embedding ?v
                 BIND(tt:cosine(?v, "[1, 0, 0.1]") AS ?score) FILTER(BOUND(?score)) }
               ORDER BY DESC(?score) LIMIT 2"#,
        );
        assert_eq!(
            r,
            [
                ["<http://example.com/crossing>"],
                ["<http://example.com/bridge>"]
            ]
        );
    }
}

#[test]
fn similarity_functions_give_doubles() {
    for e in engines() {
        let r = rows(
            &e,
            r#"SELECT (tt:dot(?a, ?b) AS ?dot) (tt:l2(?a, ?b) AS ?l2) (tt:cosine(?a, ?a) AS ?same)
               WHERE { BIND("[3, 0]"^^tt:vector AS ?a) BIND("[0, 4]" AS ?b) }"#,
        );
        let d = |x: &str| format!("\"{x}\"^^<http://www.w3.org/2001/XMLSchema#double>");
        assert_eq!(r, [[d("0"), d("5"), d("1")]]);
    }
}

#[test]
fn vectors_of_other_lengths_or_not_vectors_give_no_score() {
    for e in engines() {
        let r = rows(
            &e,
            r#"SELECT ?s WHERE { ?s ex:embedding ?v
                 BIND(COALESCE(tt:cosine(?v, "[1, 0, 0]"), "none") AS ?score)
                 FILTER(?score = "none") } ORDER BY ?s"#,
        );
        assert_eq!(
            r,
            [
                ["<http://example.com/broken>"],
                ["<http://example.com/odd>"]
            ]
        );
    }
}

#[test]
fn nearest_finds_the_k_most_similar_resources() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let q = r#"SELECT ?s ?score WHERE {
                 ?s tt:nearest (ex:embedding "[1, 0, 0.1]" 2 ?score) } ORDER BY DESC(?score)"#;
    let r = rows(&e, q);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], "<http://example.com/crossing>");
    assert_eq!(r[1][0], "<http://example.com/bridge>");
    // The same as ranking with tt:cosine.
    let scan = rows(
        &e,
        r#"SELECT ?s WHERE { ?s ex:embedding ?v
             BIND(tt:cosine(?v, "[1, 0, 0.1]") AS ?c) FILTER(BOUND(?c)) }
           ORDER BY DESC(?c) LIMIT 2"#,
    );
    assert_eq!(
        scan,
        r.iter().map(|row| vec![row[0].clone()]).collect::<Vec<_>>()
    );
    // Joined with the rest of the query, and with the vector literal.
    let r = rows(
        &e,
        r#"SELECT ?s ?v WHERE { ?s tt:nearest (ex:embedding "[0, 1, 0]" 1 ?score ?v) }"#,
    );
    assert_eq!(
        r,
        [[
            "<http://example.com/signal>".to_owned(),
            "\"[0.1, 0.9, 0.1]\"^^<http://w3id.org/tripletrail/fn#vector>".to_owned()
        ]]
    );
}

#[test]
fn nearest_to_a_resource_reads_its_vector_from_the_query() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let r = rows(
        &e,
        r#"SELECT ?s WHERE { ex:crossing ex:embedding ?q .
             ?s tt:nearest (ex:embedding ?q 3) FILTER(?s != ex:crossing) }"#,
    );
    let mut got: Vec<String> = r.into_iter().map(|mut row| row.remove(0)).collect();
    got.sort();
    assert_eq!(
        got,
        ["<http://example.com/bridge>", "<http://example.com/signal>"]
    );
}

#[test]
fn nearest_sees_writes() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let q = r#"SELECT ?s WHERE { ?s tt:nearest (ex:embedding "[0, 0, 1]" 1) }"#;
    assert_eq!(rows(&e, q), [["<http://example.com/bridge>"]]);
    // The copy of the vectors counts in the dataset's memory.
    assert!(e.memory_use().unwrap().vector_indexes > 0);
    e.update(
        r#"PREFIX ex: <http://example.com/> PREFIX tt: <http://w3id.org/tripletrail/fn#>
           INSERT DATA { ex:tunnel ex:embedding "[0, 0, 2]"^^tt:vector }"#,
    )
    .unwrap();
    assert_eq!(rows(&e, q), [["<http://example.com/tunnel>"]]);
    e.update(r#"PREFIX ex: <http://example.com/> DELETE WHERE { ex:tunnel ex:embedding ?v }"#)
        .unwrap();
    assert_eq!(rows(&e, q), [["<http://example.com/bridge>"]]);
}

#[test]
fn a_constant_subject_asks_whether_it_is_among_the_best_k() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let ask = |s: &str, k: usize| {
        rows(
            &e,
            &format!(r#"SELECT * WHERE {{ ex:{s} tt:nearest (ex:embedding "[1, 0, 0.1]" {k}) }}"#),
        )
        .len()
    };
    // crossing is nearest, then bridge, then signal.
    assert_eq!(ask("crossing", 1), 1);
    assert_eq!(ask("bridge", 1), 0);
    assert_eq!(ask("bridge", 2), 1);
    assert_eq!(ask("signal", 2), 0);
    assert_eq!(ask("signal", 3), 1);
}

#[test]
fn nearest_refuses_what_it_cannot_read() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    for bad in [
        r#"SELECT ?s WHERE { ?s tt:nearest (ex:embedding "not a vector") }"#,
        r#"SELECT ?s WHERE { ?s tt:nearest ("[1]") }"#,
        r#"SELECT ?s WHERE { ?s tt:nearest (ex:embedding "[1]" "ten") }"#,
    ] {
        let q = format!(
            "PREFIX ex: <http://example.com/> PREFIX tt: <http://w3id.org/tripletrail/fn#> {bad}"
        );
        assert!(e.query(&q).is_err(), "{bad}");
    }
}

#[test]
fn nearest_agrees_with_cosine_on_very_large_and_very_small_numbers() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix ex: <http://example.com/> . @prefix tt: <http://w3id.org/tripletrail/fn#> .
        ex:big ex:e "[1e30, 1e30, 0]"^^tt:vector .
        ex:tiny ex:e "[1e-30, 0, 0]"^^tt:vector .
        ex:huge ex:e "[1e200, 0, 1e200]"^^tt:vector ."#
            .as_bytes(),
    )
    .unwrap();
    for query in ["[1, 0, 0]", "[1e39, 0, 0]", "[1e-40, 0, 0]"] {
        let nearest = rows(
            &e,
            &format!(
                r#"SELECT ?s ?score WHERE {{ ?s tt:nearest (ex:e "{query}" ?score) }} ORDER BY ?s"#
            ),
        );
        let scan = rows(
            &e,
            &format!(
                r#"SELECT ?s ?score WHERE {{ ?s ex:e ?v
                     BIND(xsd:double(STR(ROUND(tt:cosine(?v, "{query}") * 1000000) / 1000000)) AS ?score) }}
                   ORDER BY ?s"#
            )
            .replace("SELECT", "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> SELECT"),
        );
        assert_eq!(nearest.len(), 3, "{query}: {nearest:?}");
        let number = |t: &str| -> f64 { t.split('"').nth(1).unwrap().parse().unwrap() };
        for (n, c) in nearest.iter().zip(&scan) {
            assert_eq!(n[0], c[0]);
            assert!(
                (number(&n[1]) - number(&c[1])).abs() < 1e-5,
                "{query}: {n:?} {c:?}"
            );
        }
    }
}

/// `n` vectors of `dim` numbers around 20 centres, as N-Triples of `ex:r{i} ex:embedding`.
fn clustered(from: usize, n: usize, dim: usize) -> String {
    let mut x: u64 = 3 + from as u64;
    let mut next = move || {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (x >> 40) as f64 / (1u64 << 24) as f64 - 0.5
    };
    let centres: Vec<Vec<f64>> = (0..20)
        .map(|_| (0..dim).map(|_| next()).collect())
        .collect();
    let mut out = String::new();
    for i in from..from + n {
        let v: Vec<String> = centres[i % 20]
            .iter()
            .map(|c| format!("{:.5}", c + 0.4 * next()))
            .collect();
        out.push_str(&format!(
            "<http://example.com/r{i}> <http://example.com/embedding> \"[{}]\"^^<http://w3id.org/tripletrail/fn#vector> .\n",
            v.join(",")
        ));
    }
    out
}

#[test]
fn nearest_follows_a_graph_past_the_threshold_and_still_sees_writes() {
    use tripletrail::oxigraph::model::NamedNode;
    let dim = 16;
    let e = Engine::new_tripletrail();
    e.set_vector_graph_min(500);
    e.load(RdfFormat::NTriples, clustered(0, 3000, dim).as_bytes())
        .unwrap();
    let query: Vec<String> = (0..dim)
        .map(|i| format!("{}", (i % 5) as f64 / 5.0))
        .collect();
    let query = format!("[{}]", query.join(","));
    let nearest =
        format!(r#"SELECT ?s ?score WHERE {{ ?s tt:nearest (ex:embedding "{query}" 10 ?score) }}"#);
    let scan = format!(
        r#"SELECT ?s WHERE {{ ?s ex:embedding ?v BIND(tt:cosine(?v, "{query}") AS ?score) }}
           ORDER BY DESC(?score) LIMIT 10"#
    );
    let subjects = |q: &str| {
        let mut s: Vec<String> = rows(&e, q).into_iter().map(|r| r[0].clone()).collect();
        s.sort();
        s
    };
    // The first search compares every vector, and starts the graph.
    let exact = subjects(&scan);
    assert_eq!(subjects(&nearest), exact);
    let property = NamedNode::new("http://example.com/embedding").unwrap();
    let index = e.vector_index(None, &property).unwrap();
    let start = std::time::Instant::now();
    while index.graph_status(dim) != (3000, false) {
        assert!(
            start.elapsed().as_secs() < 60,
            "{:?}",
            index.graph_status(dim)
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Now the graph answers: what it finds has the scan's scores.
    let found = subjects(&nearest);
    assert_eq!(found.len(), 10);
    let same = found.iter().filter(|s| exact.contains(s)).count();
    assert!(same >= 9, "{found:?} against {exact:?}");
    // A vector written after the graph was made is compared one by one: found at once.
    e.update(&format!(
        r#"PREFIX ex: <http://example.com/> PREFIX tt: <http://w3id.org/tripletrail/fn#>
           INSERT DATA {{ ex:new ex:embedding "{query}"^^tt:vector }}"#
    ))
    .unwrap();
    let found = rows(&e, &nearest);
    assert_eq!(
        found[0],
        [
            "<http://example.com/new>",
            "\"1.000000\"^^<http://www.w3.org/2001/XMLSchema#double>"
        ]
    );
    // A removed vector is not found, though the graph still holds it.
    let first = rows(&e, &scan)[1][0].clone();
    e.update(&format!(
        "PREFIX ex: <http://example.com/> DELETE WHERE {{ {first} ex:embedding ?v }}"
    ))
    .unwrap();
    let found = rows(&e, &nearest);
    assert!(found.iter().all(|r| r[0] != first), "{found:?}");
    assert_eq!(found.len(), 10);
    // Exact again when told so.
    e.set_vector_graph_min(0);
    assert_eq!(subjects(&nearest), subjects(&scan));
}

#[test]
fn a_saved_graph_is_read_again_by_a_new_engine() {
    use tripletrail::oxigraph::model::NamedNode;
    // Removed when the test ends, after the last engine: its background save of the graph
    // built again may still be writing into the folder (removing it then raced that save).
    let dir = tripletrail_testdir::tempdir("vector-graph");
    let store = tripletrail::store::Store::new();
    let property = NamedNode::new("http://example.com/embedding").unwrap();
    let dim = 16;
    let query: Vec<String> = (0..dim).map(|i| format!("{}", (i % 3) as f64)).collect();
    let query = format!("[{}]", query.join(","));
    let nearest = format!(r#"SELECT ?s WHERE {{ ?s tt:nearest (ex:embedding "{query}" 5) }}"#);
    let wait = |e: &Engine, nodes: usize| {
        let index = e.vector_index(None, &property).unwrap();
        let start = std::time::Instant::now();
        while index.graph_status(dim) != (nodes, false) {
            assert!(
                start.elapsed().as_secs() < 60,
                "{:?}",
                index.graph_status(dim)
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    let saved = || -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir.join("vectors"))
            .map(|d| {
                d.flatten()
                    .map(|f| f.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "hnsw"))
                    .collect()
            })
            .unwrap_or_default()
    };
    {
        let e = Engine::with_network_dir(store.clone(), &dir).unwrap();
        e.set_vector_graph_min(500);
        e.load(RdfFormat::NTriples, clustered(0, 3000, dim).as_bytes())
            .unwrap();
        rows(&e, &nearest);
        wait(&e, 3000);
        let start = std::time::Instant::now();
        while saved().is_empty() {
            assert!(start.elapsed().as_secs() < 30, "not saved");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let file = saved().remove(0);
    let written = std::fs::metadata(&file).unwrap().modified().unwrap();
    // A new engine on the same store (a restart), with one more vector written since.
    let e = Engine::with_network_dir(store, &dir).unwrap();
    e.set_vector_graph_min(500);
    e.update(&format!(
        r#"PREFIX ex: <http://example.com/> PREFIX tt: <http://w3id.org/tripletrail/fn#>
           INSERT DATA {{ ex:new ex:embedding "{query}"^^tt:vector }}"#
    ))
    .unwrap();
    assert_eq!(rows(&e, &nearest)[0], ["<http://example.com/new>"]);
    // The graph read from the file holds the 3,000 saved; the new vector is compared on its
    // own, and still comes first.
    wait(&e, 3000);
    assert_eq!(rows(&e, &nearest)[0], ["<http://example.com/new>"]);
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        written
    );
    // A file that is not a graph is ignored, and the graph built again.
    std::fs::write(&file, b"not a graph").unwrap();
    let e = Engine::with_network_dir(e.store().clone(), &dir).unwrap();
    e.set_vector_graph_min(500);
    assert_eq!(rows(&e, &nearest)[0], ["<http://example.com/new>"]);
    wait(&e, 3001);
    drop(e);
}

/// `ORDER BY DESC(tt:cosine(…)) LIMIT k` gives on the tripletrail store, which answers it
/// from the vector index, what the in-memory store gives by computing every cosine: with a
/// selective pattern beside it, an offset, vectors of another length and objects that are
/// not vectors, a limit past what there is, and plain strings in the form of vectors.
#[test]
fn cosine_order_through_the_index_gives_what_computing_every_cosine_gives() {
    let mut data = clustered(0, 3000, 8);
    for i in (0..3000).step_by(7) {
        data.push_str(&format!(
            "<http://example.com/r{i}> <http://example.com/kind> <http://example.com/A> .\n"
        ));
    }
    data.push_str(
        "<http://example.com/short> <http://example.com/embedding> \"[1, 0]\"^^<http://w3id.org/tripletrail/fn#vector> .\n\
         <http://example.com/zero> <http://example.com/embedding> \"[0, 0, 0, 0, 0, 0, 0, 0]\"^^<http://w3id.org/tripletrail/fn#vector> .\n\
         <http://example.com/word> <http://example.com/embedding> \"word\"@en .\n",
    );
    let q: Vec<String> = (0..8)
        .map(|i| format!("{}", (i % 3) as f64 - 0.7))
        .collect();
    let q = q.join(", ");
    let queries = [
        format!("SELECT ?s WHERE {{ ?s ex:embedding ?v }} ORDER BY DESC(tt:cosine(?v, \"[{q}]\"^^tt:vector)) ?s LIMIT 10"),
        format!("SELECT ?s ?c WHERE {{ ?s ex:embedding ?v BIND(tt:cosine(\"[{q}]\", ?v) AS ?c) }} ORDER BY DESC(?c) ?s LIMIT 5 OFFSET 20"),
        // The first round keeps enough vectors for the offset too.
        format!("SELECT ?s WHERE {{ ?s ex:embedding ?v }} ORDER BY DESC(tt:cosine(?v, \"[{q}]\")) ?s LIMIT 10 OFFSET 70"),
        // One in seven has the kind: the index takes in more vectors until enough are left.
        format!("SELECT ?s WHERE {{ ?s ex:embedding ?v ; ex:kind ex:A }} ORDER BY DESC(tt:cosine(?v, \"[{q}]\")) ?s LIMIT 50"),
        format!("SELECT DISTINCT ?s WHERE {{ ?s ex:embedding ?v FILTER(?s != ex:r0) }} ORDER BY DESC(tt:cosine(?v, \"[{q}]\")) ?s LIMIT 3"),
        // One row in a hundred: two rounds, then the query as written.
        format!("SELECT ?s WHERE {{ ?s ex:embedding ?v FILTER(STRENDS(STR(?s), \"77\")) }} ORDER BY DESC(tt:cosine(?v, \"[{q}]\")) ?s LIMIT 10"),
        // More than there are vectors of the length: then the rows without a cosine.
        format!("SELECT ?s WHERE {{ ?s ex:embedding ?v }} ORDER BY DESC(tt:cosine(?v, \"[{q}]\")) ?s LIMIT 3010"),
        "SELECT ?s WHERE { ?s ex:embedding ?v } ORDER BY DESC(tt:cosine(?v, \"[1, 1]\")) ?s LIMIT 2".to_owned(),
    ];
    let memory = Engine::new().unwrap();
    let indexed = Engine::new_tripletrail();
    for e in [&memory, &indexed] {
        e.load(RdfFormat::NTriples, data.as_bytes()).unwrap();
    }
    let check = |label: &str| {
        for query in &queries {
            assert_eq!(
                rows(&indexed, query),
                rows(&memory, query),
                "{label}: {query}"
            );
        }
    };
    check("vectors only");
    // The first five from the index; the selective one and the long limit as written.
    assert_eq!(indexed.cosine_orders_answered(), 5);
    // A plain string in the form of a vector has a cosine too: the query runs as written.
    let string = format!(
        "INSERT DATA {{ <http://example.com/plain> <http://example.com/embedding> \"[{q}]\" }}"
    );
    for e in [&memory, &indexed] {
        e.update(&string).unwrap();
    }
    check("with a plain string");
    assert_eq!(indexed.cosine_orders_answered(), 5);
    assert_eq!(
        rows(&indexed, &queries[0])[0],
        ["<http://example.com/plain>"]
    );
}

/// On the union of the named graphs as the default graph, and for a caller who may read only
/// some graphs, the index answers from the vectors of those graphs, as `tt:nearest` does.
#[test]
fn cosine_order_through_the_index_on_the_union_and_readable_graphs() {
    use tripletrail::oxigraph::model::NamedNode;
    use tripletrail::{QueryOptions, ReadableGraphs};
    let mut data = String::new();
    for (k, line) in clustered(0, 2000, 8).lines().enumerate() {
        let graph = if k % 2 == 0 { "urn:g1" } else { "urn:g2" };
        data.push_str(&format!("{} <{graph}> .\n", line.trim_end_matches(" .")));
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::NQuads, data.as_bytes()).unwrap();
    let q = "[0.3, -0.2, 0.1, 0.5, -0.4, 0.2, 0.0, 0.1]";
    let query = |key: &str| {
        format!(
            "PREFIX ex: <http://example.com/> PREFIX tt: <http://w3id.org/tripletrail/fn#> \
             SELECT ?s WHERE {{ ?s ex:embedding ?v }} ORDER BY DESC({key}) ?s LIMIT 10"
        )
    };
    let indexed = query(&format!("tt:cosine(?v, \"{q}\")"));
    let computed = query(&format!("tt:cosine(?v, \"{q}\") + 0"));
    let run = |text: &str, options: QueryOptions| -> Vec<String> {
        let QueryResults::Solutions(s) = e.query_with_options(text, options).unwrap() else {
            panic!()
        };
        s.map(|r| r.unwrap().iter().map(|(_, t)| t.to_string()).collect())
            .collect()
    };
    // The default graph is empty: no rows, from the index or not.
    assert!(run(&indexed, QueryOptions::default()).is_empty());
    e.set_union_default_graph(true);
    let before = e.cosine_orders_answered();
    let union = run(&indexed, QueryOptions::default());
    assert_eq!(union.len(), 10);
    assert_eq!(union, run(&computed, QueryOptions::default()));
    assert_eq!(e.cosine_orders_answered(), before + 1);
    let only_g1 = || QueryOptions {
        readable: Some(ReadableGraphs {
            default_graph: false,
            named: vec![NamedNode::new("urn:g1").unwrap()],
        }),
        ..Default::default()
    };
    let readable = run(&indexed, only_g1());
    assert_eq!(readable, run(&computed, only_g1()));
    assert_ne!(readable, union);
    assert_eq!(e.cosine_orders_answered(), before + 2);
}

/// Hybrid search (#219): similarity, an area and full text in one SPARQL query, ranked by
/// the cosine, from the vector index: the same as computing every cosine.
#[test]
fn similarity_area_and_full_text_in_one_query() {
    let mut data = String::new();
    for i in 0..2000 {
        let (x, y) = ((i % 20) as f64 * 0.5, (i / 20) as f64 * 0.5);
        let kind = if i % 3 == 0 {
            "level crossing"
        } else {
            "signal"
        };
        let v = [
            (i % 7) as f64 - 3.0,
            (i % 5) as f64 - 2.0,
            (i % 11) as f64 * 0.3,
            1.0,
        ];
        let v: Vec<String> = v.iter().map(|x| x.to_string()).collect();
        data.push_str(&format!(
            "<http://example.com/a{i}> <http://www.w3.org/2000/01/rdf-schema#label> \"{kind} {i}\" ;\n\
             <http://example.com/embedding> \"[{}]\"^^<http://w3id.org/tripletrail/fn#vector> ;\n\
             <http://www.opengis.net/ont/geosparql#hasGeometry> <http://example.com/g{i}> .\n\
             <http://example.com/g{i}> <http://www.opengis.net/ont/geosparql#asWKT> \"POINT({x} {y})\"^^<http://www.opengis.net/ont/geosparql#wktLiteral> .\n",
            v.join(", ")
        ));
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let query = |key: &str| {
        format!(
            "PREFIX geo: <http://www.opengis.net/ont/geosparql#> \
             PREFIX geof: <http://www.opengis.net/def/function/geosparql/> \
             SELECT ?s WHERE {{ ?s tt:search \"crossing\" ; ex:embedding ?v ; \
               geo:hasGeometry/geo:asWKT ?w \
               FILTER(geof:sfWithin(?w, \"POLYGON((0 0, 6 0, 6 30, 0 30, 0 0))\"^^geo:wktLiteral)) }} \
             ORDER BY DESC({key}) ?s LIMIT 5"
        )
    };
    let before = e.cosine_orders_answered();
    let found = rows(&e, &query("tt:cosine(?v, \"[1, 1, 0.5, 1]\")"));
    assert_eq!(found.len(), 5);
    assert_eq!(
        found,
        rows(&e, &query("tt:cosine(?v, \"[1, 1, 0.5, 1]\") + 0"))
    );
    assert_eq!(e.cosine_orders_answered(), before + 1);
}

/// Timings for the docs: `cargo test --release -p tripletrail --test vectors -- --ignored --nocapture`.
#[test]
#[ignore]
fn timing() {
    let (n, dim) = (100_000usize, 768usize);
    let mut data = String::new();
    let mut x: u64 = 7;
    for i in 0..n {
        let v: Vec<String> = (0..dim)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                format!("{:.4}", (x >> 40) as f64 / (1u64 << 24) as f64 - 0.5)
            })
            .collect();
        data.push_str(&format!(
            "<http://example.com/r{i}> <http://example.com/embedding> \"[{}]\"^^<http://w3id.org/tripletrail/fn#vector> .\n",
            v.join(",")
        ));
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::NTriples, data.as_bytes()).unwrap();
    let q: Vec<String> = (0..dim)
        .map(|i| format!("{}", (i % 7) as f64 / 7.0))
        .collect();
    let query = format!(
        "SELECT ?s ?score WHERE {{ ?s tt:nearest (ex:embedding \"[{}]\" 20 ?score) }}",
        q.join(",")
    );
    for run in 0..3 {
        let t = std::time::Instant::now();
        let r = rows(&e, &query);
        println!("run {run}: {} rows in {:?}", r.len(), t.elapsed());
    }
    // `ORDER BY` through the index, and (`+ 0` hides the cosine) computing every cosine.
    for (label, key) in [
        ("ORDER BY through the index", "tt:cosine(?v, ?q)"),
        ("ORDER BY computing every cosine", "tt:cosine(?v, ?q) + 0"),
    ] {
        let key = key.replace("?q", &format!("\"[{}]\"", q.join(",")));
        // All the rows, then one in a hundred: the selective one is evaluated as written
        // after two rounds.
        for filter in ["", "FILTER(STRENDS(STR(?s), \"77\"))"] {
            let query = format!(
                "SELECT ?s WHERE {{ ?s ex:embedding ?v {filter} }} ORDER BY DESC({key}) LIMIT 20"
            );
            for run in 0..3 {
                let t = std::time::Instant::now();
                let r = rows(&e, &query);
                println!(
                    "{label} {filter}, run {run}: {} rows in {:?}",
                    r.len(),
                    t.elapsed()
                );
            }
        }
    }
}

/// Recall and latency of the graph (HNSW) search against the exact one, on clustered
/// vectors, for the docs: `VECTORS=1000000 cargo test --release -p tripletrail --test vectors
/// -- --ignored --nocapture graph_recall`. `VECTORS` and `DIMENSIONS` set the size.
#[test]
#[ignore]
fn graph_recall_and_latency() {
    use tripletrail::oxigraph::model::NamedNode;
    use tripletrail::store::TermId;
    use tripletrail::vector::VectorIndex;
    let env = |name: &str, default: usize| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let (n, dim) = (env("VECTORS", 100_000), env("DIMENSIONS", 768));
    let mut x: u64 = 11;
    let mut next = move || {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (x >> 40) as f64 / (1u64 << 24) as f64 - 0.5
    };
    let (count, spread) = (env("CENTRES", 1000), env("SPREAD", 10) as f64 / 10.0);
    let centres: Vec<Vec<f64>> = (0..count)
        .map(|_| (0..dim).map(|_| next()).collect())
        .collect();
    // With LATENT set, the vectors are mixes of that many directions (plus a little noise)
    // instead of points around centres: like text embeddings, whose numbers depend on
    // far fewer than `dim` things.
    let latent = env("LATENT", 0);
    let mut point = |i: usize, spread: f64| -> Vec<f64> {
        if latent > 0 {
            let mix: Vec<f64> = (0..latent).map(|_| next()).collect();
            return (0..dim)
                .map(|d| {
                    mix.iter()
                        .enumerate()
                        .map(|(j, m)| m * centres[j % centres.len()][d])
                        .sum::<f64>()
                        + 0.05 * spread * next()
                })
                .collect();
        }
        centres[(i * 7919) % centres.len()]
            .iter()
            .map(|c| c + spread * next())
            .collect()
    };
    let t = std::time::Instant::now();
    let property = NamedNode::new("http://example.com/embedding").unwrap();
    let index = VectorIndex::from_vectors(
        &property,
        (0..n).map(|i| (TermId(i as u64), TermId((n + i) as u64), point(i, spread))),
    )
    .unwrap();
    println!(
        "{n} x {dim} ({count} centres, spread {spread}): indexed in {:?}",
        t.elapsed()
    );
    let queries: Vec<Vec<f64>> = (0..200).map(|i| point(i * 13 + 5, spread)).collect();
    let k = 20;
    let t = std::time::Instant::now();
    index.set_graph_min(usize::MAX);
    let truth: Vec<Vec<TermId>> = queries
        .iter()
        .map(|q| {
            index
                .nearest(q, Some(k))
                .iter()
                .map(|h| h.subject)
                .collect()
        })
        .collect();
    println!("exact: {:?} per search", t.elapsed() / queries.len() as u32);
    index.set_graph_min(1);
    let t = std::time::Instant::now();
    index.nearest(&queries[0], Some(k));
    while index.graph_status(dim) != (n, false) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    println!(
        "graph made in {:?}; index {} MB",
        t.elapsed(),
        index.heap_size() >> 20
    );
    for width in [256, 512, 768, 1024] {
        index.set_search_width(width);
        let mut took = Vec::new();
        let mut hit = 0;
        for (q, truth) in queries.iter().zip(&truth) {
            let t = std::time::Instant::now();
            let got = index.nearest(q, Some(k));
            took.push(t.elapsed());
            hit += got.iter().filter(|h| truth.contains(&h.subject)).count();
        }
        took.sort();
        println!(
            "width {width}: recall@{k} {:.3}, median {:?}, p99 {:?}",
            hit as f64 / (k * queries.len()) as f64,
            took[took.len() / 2],
            took[took.len() * 99 / 100]
        );
    }
}
