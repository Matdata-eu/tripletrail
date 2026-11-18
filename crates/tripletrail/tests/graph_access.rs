//! Graph access (#201): a query with `QueryOptions::readable` sees only the readable graphs,
//! however it names them.

use oxigraph::io::RdfFormat;
use oxigraph::model::NamedNode;
use oxigraph::sparql::{QueryDataset, QueryResults};
use tripletrail::{Engine, QueryOptions, ReadableGraphs};

const DATA: &str = "
<urn:a> <urn:p> <urn:b> .
<urn:g1> { <urn:b> <urn:p> <urn:c> . <urn:x> <urn:in> \"g1\" }
<urn:g2> { <urn:c> <urn:p> <urn:d> . <urn:x> <urn:in> \"g2\" }
";

fn engine() -> Engine {
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::TriG, DATA.as_bytes()).unwrap();
    engine
}

fn only(named: &[&str], default_graph: bool) -> QueryOptions {
    QueryOptions {
        readable: Some(ReadableGraphs {
            default_graph,
            named: named.iter().map(|n| NamedNode::new(*n).unwrap()).collect(),
        }),
        ..Default::default()
    }
}

fn rows(engine: &Engine, query: &str, options: QueryOptions) -> Vec<String> {
    let QueryResults::Solutions(s) = engine.query_with_options(query, options).unwrap() else {
        panic!()
    };
    let mut rows: Vec<String> = s
        .map(|row| {
            let row = row.unwrap();
            row.iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn only_readable_graphs_are_seen() {
    let engine = engine();
    let g1 = || only(&["urn:g1"], false);
    // Without FROM, the default graph is the union of the readable graphs.
    assert_eq!(
        rows(&engine, "SELECT ?v { ?x <urn:in> ?v }", g1()),
        ["?v=\"g1\""]
    );
    assert_eq!(
        rows(&engine, "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }", g1()),
        ["?n=\"2\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
    // GRAPH ?g lists and reads only the readable graphs.
    assert_eq!(
        rows(
            &engine,
            "SELECT DISTINCT ?g { GRAPH ?g { ?s ?p ?o } }",
            g1()
        ),
        ["?g=<urn:g1>"]
    );
    assert_eq!(
        rows(
            &engine,
            "SELECT ?v { GRAPH <urn:g2> { ?x <urn:in> ?v } }",
            g1()
        ),
        Vec::<String>::new()
    );
    // FROM and FROM NAMED only narrow.
    assert_eq!(
        rows(&engine, "SELECT ?v FROM <urn:g2> { ?x <urn:in> ?v }", g1()),
        Vec::<String>::new()
    );
    assert_eq!(
        rows(
            &engine,
            "SELECT ?v FROM <urn:g1> FROM <urn:g2> { ?x <urn:in> ?v }",
            g1()
        ),
        ["?v=\"g1\""]
    );
    assert_eq!(
        rows(
            &engine,
            "SELECT ?g FROM NAMED <urn:g1> FROM NAMED <urn:g2> { GRAPH ?g { ?x <urn:in> ?v } }",
            g1()
        ),
        ["?g=<urn:g1>"]
    );
    // A path does not cross into other graphs: a→b is in the default graph, c→d in g2.
    assert_eq!(
        rows(&engine, "SELECT ?o { <urn:b> <urn:p>+ ?o }", g1()),
        ["?o=<urn:c>"]
    );
    // The protocol's dataset is narrowed the same way.
    let mut dataset = QueryDataset::new();
    dataset.set_default_graph(vec![NamedNode::new("urn:g2").unwrap().into()]);
    let options = QueryOptions {
        dataset: Some(dataset),
        ..g1()
    };
    assert_eq!(
        rows(&engine, "SELECT ?v { ?x <urn:in> ?v }", options),
        Vec::<String>::new()
    );
}

#[test]
fn the_default_graph_is_read_when_granted() {
    let engine = engine();
    assert_eq!(
        rows(
            &engine,
            "SELECT ?o { <urn:a> <urn:p>+ ?o }",
            only(&["urn:g1"], true)
        ),
        ["?o=<urn:b>", "?o=<urn:c>"]
    );
    assert_eq!(
        rows(
            &engine,
            "SELECT ?o { <urn:a> <urn:p> ?o }",
            only(&[], false)
        ),
        Vec::<String>::new()
    );
    // Nothing readable: nothing seen, not even graph names.
    assert_eq!(
        rows(&engine, "SELECT ?g { GRAPH ?g { } }", only(&[], false)),
        Vec::<String>::new()
    );
}

/// Path search (#203) follows only the edges of the caller's graphs: those of its default
/// graphs, of a `path:graph` it may read, or of an edge sub-select.
#[test]
fn path_search_follows_the_edges_of_the_readable_graphs() {
    // urn:a -p-> urn:b in the default graph, urn:b -p-> urn:c in g1, urn:c -p-> urn:d in g2.
    let engine = engine();
    let path = |extra: &str| {
        format!(
            "PREFIX path: <http://w3id.org/tripletrail/path#>
             SELECT ?e {{ SERVICE <http://w3id.org/tripletrail/path> {{
               [] path:source <urn:b> ; path:end ?e ; path:minLength 1 {extra} }} }}"
        )
    };
    let predicate = path("; path:predicate <urn:p>");
    let any = path("");
    let sub = path(". { SELECT ?x ?y { ?x <urn:p> ?y } }");
    let named = |g: &str| path(&format!("; path:predicate <urn:p> ; path:graph <{g}>"));
    for q in [&predicate, &any, &sub] {
        assert_eq!(
            rows(&engine, q, only(&["urn:g1", "urn:g2"], false)),
            ["?e=<urn:c>", "?e=<urn:d>"],
            "{q}"
        );
        assert_eq!(
            rows(&engine, q, only(&["urn:g1"], false)),
            ["?e=<urn:c>"],
            "{q}"
        );
        assert!(rows(&engine, q, only(&["urn:g2"], true)).is_empty(), "{q}");
    }
    assert_eq!(
        rows(&engine, &named("urn:g2"), only(&["urn:g2"], false)),
        Vec::<String>::new()
    );
    assert!(rows(&engine, &named("urn:g2"), only(&["urn:g1"], false)).is_empty());
    assert_eq!(
        rows(&engine, &named("urn:g1"), only(&["urn:g1"], false)),
        ["?e=<urn:c>"]
    );
    // FROM narrows it too.
    let from = predicate.replace("SELECT ?e {", "SELECT ?e FROM <urn:g1> {");
    assert_eq!(
        rows(&engine, &from, only(&["urn:g1", "urn:g2"], false)),
        ["?e=<urn:c>"]
    );
}

/// pgRouting (#203) builds its network from the caller's graphs, cached apart for each set
/// of graphs; what reads the store directly (shape filters, tables) is refused.
#[test]
fn routing_builds_its_network_from_the_readable_graphs() {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            r#"@prefix ex: <http://example.com/> .
               <urn:g1> { ex:e1 a ex:Edge ; ex:from ex:a ; ex:to ex:d ; ex:len 10 . }
               <urn:g2> { ex:e2 a ex:Edge ; ex:from ex:a ; ex:to ex:b ; ex:len 1 .
                          ex:e3 a ex:Edge ; ex:from ex:b ; ex:to ex:d ; ex:len 1 . }"#
                .as_bytes(),
        )
        .unwrap();
    let query = r#"PREFIX ex: <http://example.com/> PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
        SELECT (MAX(?c) AS ?cost) { SERVICE <http://w3id.org/tripletrail/pgr> {
          [] pgr:function pgr:pgr_dijkstra ; pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:agg_cost ?c .
          { SELECT ?id ?source ?target ?cost { ?id a ex:Edge ; ex:from ?source ; ex:to ?target ; ex:len ?cost } }
        } }"#;
    let cost = |graphs: &[&str]| rows(&engine, query, only(graphs, false));
    // g1 first: its network must not be the one served to a caller of both, and back.
    assert_eq!(
        cost(&["urn:g1"]),
        ["?cost=\"10\"^^<http://www.w3.org/2001/XMLSchema#double>"]
    );
    assert_eq!(
        cost(&["urn:g1", "urn:g2"]),
        ["?cost=\"2\"^^<http://www.w3.org/2001/XMLSchema#double>"]
    );
    assert_eq!(
        cost(&["urn:g1"]),
        ["?cost=\"10\"^^<http://www.w3.org/2001/XMLSchema#double>"]
    );
    assert_eq!(
        cost(&["urn:g2"]),
        ["?cost=\"2\"^^<http://www.w3.org/2001/XMLSchema#double>"]
    );
    let shapes = query.replace(
        "pgr:agg_cost ?c .",
        "pgr:agg_cost ?c ; pgr:shapes ex:shapes .",
    );
    let err = match engine.query_with_options(&shapes, only(&["urn:g1"], false)) {
        Err(e) => e.to_string(),
        Ok(QueryResults::Solutions(mut s)) => s
            .find_map(Result::err)
            .map(|e| e.to_string())
            .expect("refused"),
        Ok(_) => unreachable!(),
    };
    assert!(err.contains("only some graphs"), "{err}");
}

/// The `st:` and window services read their sub-selects from the caller's graphs (#203);
/// other services, and one nested in another, are refused.
#[test]
fn st_and_window_services_read_the_readable_graphs() {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            r#"@prefix geo: <http://www.opengis.net/ont/geosparql#> .
               <urn:g1> { <urn:f1> geo:asWKT "MULTIPOINT((1 1), (2 2))"^^geo:wktLiteral }
               <urn:g2> { <urn:f2> geo:asWKT "MULTIPOINT((5 5), (6 6), (7 7))"^^geo:wktLiteral }"#
                .as_bytes(),
        )
        .unwrap();
    let st = r#"PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX st: <http://w3id.org/tripletrail/st#>
        SELECT ?id ?part {
          SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:ST_Dump ; st:geom ?part .
            { SELECT ?id ?geom WHERE { ?id geo:asWKT ?geom } }
          }
        }"#;
    let window = r#"PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX win: <http://w3id.org/tripletrail/window#>
        SELECT ?id ?n {
          SERVICE <http://w3id.org/tripletrail/window> {
            [] win:orderBy ?id ; win:rowNumber ?n .
            { SELECT ?id WHERE { ?id geo:asWKT ?w } }
          }
        }"#;
    let ids = |rows: Vec<String>| -> Vec<String> {
        rows.iter()
            .map(|r| {
                r.split(' ')
                    .find(|c| c.starts_with("?id="))
                    .unwrap()
                    .to_owned()
            })
            .collect()
    };
    // A caller of both graphs: five points of two features; a caller of g1: the two of f1.
    let both = || only(&["urn:g1", "urn:g2"], false);
    assert_eq!(rows(&engine, st, both()).len(), 5);
    assert_eq!(
        ids(rows(&engine, st, only(&["urn:g1"], false))),
        ["?id=<urn:f1>", "?id=<urn:f1>"]
    );
    assert_eq!(rows(&engine, window, both()).len(), 2);
    assert_eq!(
        rows(&engine, window, only(&["urn:g2"], false)),
        ["?id=<urn:f2> ?n=\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
    // FROM narrows the services' graphs too.
    let from = window.replace("SELECT ?id ?n {", "SELECT ?id ?n FROM <urn:g2> {");
    assert_eq!(rows(&engine, &from, both()).len(), 1);
    // Other services, and a service in a service or in an EXISTS, are refused.
    for query in [
        "SELECT * { SERVICE <https://example.org/sparql> { ?s ?p ?o } }",
        "SELECT * { ?s ?p ?o FILTER EXISTS { SERVICE <http://w3id.org/tripletrail/path> { ?s ?p ?o } } }",
        "SELECT * { SERVICE <http://w3id.org/tripletrail/window> { [] <http://w3id.org/tripletrail/window#rowNumber> ?n . \
           { SELECT ?s { SERVICE <http://w3id.org/tripletrail/path> { ?s ?p ?o } } } } }",
    ] {
        let err = engine
            .query_with_options(query, only(&["urn:g1"], false))
            .err()
            .unwrap_or_else(|| panic!("{query} is refused"));
        assert!(err.to_string().contains("SERVICE"), "{err}");
    }
}

#[test]
fn spatial_filters_keep_to_the_readable_graphs() {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            r#"@prefix geo: <http://www.opengis.net/ont/geosparql#> .
               <urn:g1> { <urn:f1> geo:asWKT "POINT(1 1)"^^geo:wktLiteral }
               <urn:g2> { <urn:f2> geo:asWKT "POINT(2 2)"^^geo:wktLiteral }"#
                .as_bytes(),
        )
        .unwrap();
    let query = r#"PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
        SELECT ?f { ?f geo:asWKT ?w
          FILTER(geof:sfIntersects(?w, "POLYGON((0 0, 3 0, 3 3, 0 3, 0 0))"^^geo:wktLiteral)) }"#;
    assert_eq!(
        rows(&engine, query, only(&["urn:g1"], false)),
        ["?f=<urn:f1>"]
    );
    assert_eq!(
        rows(&engine, query, only(&["urn:g1", "urn:g2"], false)),
        ["?f=<urn:f1>", "?f=<urn:f2>"]
    );
}

/// The spatial index answers for such a caller (#203), from the geometries of the readable
/// graphs only: a geometry only in another graph is never a candidate, a pair or a join side.
#[test]
fn the_spatial_index_keeps_to_the_readable_graphs() {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            r#"@prefix geo: <http://www.opengis.net/ont/geosparql#> .
               <urn:a> geo:asWKT "POINT(5 5)"^^geo:wktLiteral .
               <urn:g1> { <urn:f1> geo:asWKT "POINT(1 1)"^^geo:wktLiteral .
                          <urn:z1> geo:asWKT "POLYGON((0 0, 3 0, 3 3, 0 3, 0 0))"^^geo:wktLiteral .
                          <urn:s1> geo:asWKT "POINT(2 2)"^^geo:wktLiteral }
               <urn:g2> { <urn:f2> geo:asWKT "POINT(2 2)"^^geo:wktLiteral .
                          <urn:h2> geo:asWKT "POINT(1.5 1.5)"^^geo:wktLiteral .
                          <urn:z2> geo:asWKT "POLYGON((1 1, 9 1, 9 9, 1 9, 1 1))"^^geo:wktLiteral }"#
                .as_bytes(),
        )
        .unwrap();
    let prefixes = "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX geof: <http://www.opengis.net/def/function/geosparql/> ";
    let run = |body: &str, options: QueryOptions| {
        let found = rows(&engine, &format!("{prefixes} {body}"), options);
        (found, engine.last_spatial_stats())
    };
    // A constant test: the index gives candidates, from g1 only.
    let constant = r#"SELECT ?f ?w { ?f geo:asWKT ?w
        FILTER(geof:sfIntersects(?w, "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))"^^geo:wktLiteral)) }"#;
    let (found, stats) = run(constant, only(&["urn:g1"], false));
    assert_eq!(
        found,
        [
            "?f=<urn:f1> ?w=\"POINT(1 1)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>",
            "?f=<urn:s1> ?w=\"POINT(2 2)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>",
            "?f=<urn:z1> ?w=\"POLYGON((0 0, 3 0, 3 3, 0 3, 0 0))\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>",
        ]
    );
    assert_eq!(stats.filters, 1, "{stats:?}");
    // A literal that both graphs hold is seen through the readable one only.
    let (found, _) = run(constant, only(&["urn:g2"], false));
    assert_eq!(found.len(), 3);
    assert!(
        found
            .iter()
            .all(|r| r.contains("urn:f2") || r.contains("urn:h2") || r.contains("urn:z2"))
    );
    // A join between two variables, answered from the index.
    let join = "SELECT ?a ?b { ?a geo:asWKT ?x . ?b geo:asWKT ?y
        FILTER(?a != ?b && geof:sfWithin(?x, ?y)) }";
    let (found, stats) = run(join, only(&["urn:g1"], false));
    assert_eq!(
        found,
        ["?a=<urn:f1> ?b=<urn:z1>", "?a=<urn:s1> ?b=<urn:z1>"]
    );
    assert_eq!(stats.joins, 1, "{stats:?}");
    let (found, _) = run(join, only(&["urn:g2"], false));
    assert_eq!(
        found,
        ["?a=<urn:f2> ?b=<urn:z2>", "?a=<urn:h2> ?b=<urn:z2>"]
    );
    // With the default graph readable too.
    let (found, _) = run(join, only(&["urn:g2"], true));
    assert_eq!(
        found,
        [
            "?a=<urn:a> ?b=<urn:z2>",
            "?a=<urn:f2> ?b=<urn:z2>",
            "?a=<urn:h2> ?b=<urn:z2>"
        ]
    );
    // Every caller gets the same answer as a query without the index would give.
    engine.set_spatial_rewrite(false);
    for graphs in [&["urn:g1"][..], &["urn:g2"], &["urn:g1", "urn:g2"]] {
        for body in [constant, join] {
            let plain = rows(&engine, &format!("{prefixes} {body}"), only(graphs, false));
            engine.set_spatial_rewrite(true);
            let indexed = rows(&engine, &format!("{prefixes} {body}"), only(graphs, false));
            engine.set_spatial_rewrite(false);
            assert_eq!(plain, indexed, "{graphs:?} {body}");
        }
    }
}

#[test]
fn a_zero_length_path_does_not_tell_whether_a_term_is_elsewhere() {
    let engine = engine();
    // "g2" is a node of g2 only, "nowhere" of no graph: to a caller who reads g1 they are
    // alike, and a zero-length path matches each of them once.
    for term in ["\"g2\"", "\"nowhere\"", "<urn:d>", "<urn:nowhere>"] {
        let query = format!("SELECT ?s {{ ?s <urn:zz>* {term} }}");
        assert_eq!(
            rows(&engine, &query, only(&["urn:g1"], false)),
            [format!("?s={term}")],
            "{term}"
        );
    }
    // The same for a query's own FROM, without access limits.
    for term in ["\"g2\"", "\"nowhere\""] {
        let query = format!("SELECT ?s FROM <urn:g1> {{ ?s <urn:zz>* {term} }}");
        assert_eq!(
            rows(&engine, &query, QueryOptions::default()),
            [format!("?s={term}")],
            "{term}"
        );
    }
}

fn graphs(named: &[&str], default_graph: bool) -> ReadableGraphs {
    ReadableGraphs {
        default_graph,
        named: named.iter().map(|n| NamedNode::new(*n).unwrap()).collect(),
    }
}

#[test]
fn updates_write_only_writable_graphs() {
    let engine = engine();
    let options = tripletrail::UpdateOptions {
        readable: Some(graphs(&["urn:g1", "urn:g2"], false)),
        writable: Some(graphs(&["urn:g1"], false)),
        ..Default::default()
    };
    let update = |u: &str| engine.update_with_options(u, &options);
    for refused in [
        "INSERT DATA { GRAPH <urn:g2> { <urn:s> <urn:p> 1 } }",
        "INSERT DATA { <urn:s> <urn:p> 1 }",
        "DELETE DATA { GRAPH <urn:g2> { <urn:c> <urn:p> <urn:d> } }",
        "INSERT { GRAPH ?g { <urn:s> <urn:p> 1 } } WHERE { GRAPH ?g { ?s ?p ?o } }",
        "WITH <urn:g2> DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }",
        "CLEAR ALL",
        "DROP NAMED",
        "CLEAR DEFAULT",
        "CREATE GRAPH <urn:g3>",
        "INSERT DATA { GRAPH <urn:g1> { <urn:s> <urn:p> 1 } } ; DROP GRAPH <urn:g2>",
    ] {
        let err = update(refused).err().unwrap_or_else(|| panic!("{refused}"));
        assert!(
            err.to_string().contains("may not write"),
            "{refused}: {err}"
        );
    }
    // Nothing of a refused update was applied.
    let count = |g: &str| {
        let QueryResults::Solutions(mut s) = engine
            .query(&format!(
                "SELECT (COUNT(*) AS ?n) {{ GRAPH <{g}> {{ ?s ?p ?o }} }}"
            ))
            .unwrap()
        else {
            panic!()
        };
        s.next().unwrap().unwrap().get("n").unwrap().to_string()
    };
    assert!(count("urn:g1").starts_with("\"2\""));

    // Copying from a readable graph into a writable one works; the WHERE reads only the
    // readable graphs, so the default graph's triple is not copied.
    update("INSERT { GRAPH <urn:g1> { ?s <urn:copied> ?o } } WHERE { ?s <urn:p> ?o }").unwrap();
    assert!(count("urn:g1").starts_with("\"4\""), "{}", count("urn:g1"));
    update("CLEAR GRAPH <urn:g1>").unwrap();
    assert!(count("urn:g1").starts_with("\"0\""));
    assert!(count("urn:g2").starts_with("\"2\""));
}

#[test]
fn an_update_reads_only_readable_graphs() {
    let engine = engine();
    let options = tripletrail::UpdateOptions {
        readable: Some(graphs(&["urn:g1"], false)),
        writable: Some(graphs(&["urn:g1"], false)),
        ..Default::default()
    };
    // USING <urn:g2> is narrowed away: nothing to copy.
    engine
        .update_with_options(
            "INSERT { GRAPH <urn:g1> { ?x <urn:leak> ?v } } USING <urn:g2> WHERE { ?x <urn:in> ?v }",
            &options,
        )
        .unwrap();
    let QueryResults::Boolean(leaked) = engine.query("ASK { ?x <urn:leak> ?v }").unwrap() else {
        panic!()
    };
    assert!(!leaked);
}

/// Explain for such a caller (#203) evaluates on the readable graphs, and gives the row count
/// alone: the plan would tell what other graphs hold.
#[test]
fn explain_keeps_to_the_readable_graphs() {
    let engine = engine();
    let query = "SELECT * { GRAPH ?g { ?s ?p ?o } }";
    let json = |text: String| -> serde_json::Value { serde_json::from_str(&text).unwrap() };
    let all = json(engine.explain(query).unwrap());
    assert_eq!(all["rows"], 4);
    let readable = graphs(&["urn:g1"], false);
    let some = json(engine.explain_readable(query, &readable).unwrap());
    assert_eq!(some["rows"], 2);
    // Only the row count: the plan is chosen from statistics of the whole store.
    assert_eq!(some, serde_json::json!({"rows": 2, "restricted": true}));
    // FROM only narrows.
    let from = "SELECT * FROM <urn:g2> { ?s ?p ?o }";
    assert_eq!(
        json(engine.explain_readable(from, &readable).unwrap())["rows"],
        0
    );
}

#[test]
fn the_profile_keeps_to_the_readable_graphs() {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            &br#"@prefix ex: <http://example.com/> .
               <urn:a> { ex:s1 a ex:Station ; ex:name "one" . ex:Station ex:note "public" }
               <urn:b> { ex:s1 ex:secret "x" . ex:s2 a ex:Station , ex:Hidden ; ex:secret "y" .
                         ex:Station <http://www.w3.org/2000/01/rdf-schema#subClassOf> ex:Asset }"#
                [..],
        )
        .unwrap();
    let options = tripletrail::ProfileOptions::default();
    let all = engine.dataset_profile(options).unwrap();
    let view = graphs(&["urn:a"], false);
    let some = engine.dataset_profile_in(options, Some(&view)).unwrap();
    let names = |p: &tripletrail::DatasetProfile| -> Vec<String> {
        p.classes
            .iter()
            .map(|c| c.class.as_str().to_owned())
            .collect()
    };
    assert!(names(&all).iter().any(|c| c.ends_with("Hidden")));
    assert_eq!(names(&some), ["http://example.com/Station"]);
    let station = &some.classes[0];
    assert_eq!(station.instances, 1);
    assert!(station.superclasses.is_empty());
    let properties: Vec<&str> = station
        .properties
        .iter()
        .map(|p| p.property.as_str())
        .collect();
    assert!(
        !properties.iter().any(|p| p.ends_with("secret")),
        "{properties:?}"
    );
    assert!(properties.iter().any(|p| p.ends_with("name")));
    // Kept per set of graphs: asked again, the same; the whole dataset's is unchanged.
    assert!(std::sync::Arc::ptr_eq(
        &some,
        &engine.dataset_profile_in(options, Some(&view)).unwrap()
    ));
    assert!(std::sync::Arc::ptr_eq(
        &all,
        &engine.dataset_profile(options).unwrap()
    ));
}

#[test]
fn a_grant_of_every_graph_reads_like_no_grant() {
    // Granting every graph that holds data reads the union in one scan (#204); the answers
    // stay those of the graphs granted.
    let engine = engine();
    let every = || only(&["urn:g1", "urn:g2"], true);
    assert_eq!(
        rows(&engine, "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }", every()),
        ["?n=\"5\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
    assert_eq!(
        rows(&engine, "SELECT ?s ?o { ?s <urn:p> ?o }", every()),
        [
            "?s=<urn:a> ?o=<urn:b>",
            "?s=<urn:b> ?o=<urn:c>",
            "?s=<urn:c> ?o=<urn:d>"
        ]
    );
    // GRAPH ?g still lists only named graphs.
    assert_eq!(
        rows(
            &engine,
            "SELECT DISTINCT ?g { GRAPH ?g { ?s ?p ?o } }",
            every()
        ),
        ["?g=<urn:g1>", "?g=<urn:g2>"]
    );
    // Leaving out the default graph, which holds a triple, still hides it.
    assert_eq!(
        rows(
            &engine,
            "SELECT ?o { <urn:a> <urn:p> ?o }",
            only(&["urn:g1", "urn:g2"], false)
        ),
        Vec::<String>::new()
    );
    // And a grant naming a graph that doesn't exist is no wider than the graphs it names.
    assert_eq!(
        rows(
            &engine,
            "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }",
            only(&["urn:g1", "urn:none"], false)
        ),
        ["?n=\"2\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
}

#[test]
fn lookups_by_subject_or_object_keep_to_the_readable_graphs() {
    // A lookup by subject or object reads every graph in one seek and drops the copies of
    // graphs not granted (#204); the answers must be those of the granted graphs, each once.
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            "<urn:g1> { <urn:s> <urn:p> <urn:o> . <urn:s> <urn:q> \"1\" }
             <urn:g2> { <urn:s> <urn:p> <urn:o> . <urn:s> <urn:q> \"2\" }
             <urn:g3> { <urn:s> <urn:p> <urn:o> . <urn:s> <urn:q> \"3\" . <urn:t> <urn:p> <urn:o> }"
                .as_bytes(),
        )
        .unwrap();
    let two = || only(&["urn:g1", "urn:g2"], false);
    assert_eq!(
        rows(&engine, "SELECT ?v { <urn:s> <urn:q> ?v }", two()),
        ["?v=\"1\"", "?v=\"2\""]
    );
    assert_eq!(
        rows(&engine, "SELECT ?s { ?s <urn:p> <urn:o> }", two()),
        ["?s=<urn:s>"]
    );
    assert_eq!(
        rows(
            &engine,
            "SELECT ?v { ?s <urn:p> <urn:o> . ?s <urn:q> ?v }",
            two()
        ),
        ["?v=\"1\"", "?v=\"2\""]
    );
}

#[test]
fn a_granted_graph_without_data_is_empty() {
    // A grant is the caller's dataset, as FROM NAMED gives one: a granted graph that holds
    // nothing is an empty graph of it.
    let engine = engine();
    let query = "SELECT ?g { GRAPH ?g { } }";
    let listed = rows(&engine, query, only(&["urn:g1", "urn:none"], false));
    assert_eq!(listed, ["?g=<urn:g1>", "?g=<urn:none>"]);
    assert_eq!(
        rows(
            &engine,
            "SELECT ?g FROM NAMED <urn:g1> FROM NAMED <urn:none> { GRAPH ?g { } }",
            QueryOptions::default()
        ),
        listed
    );
}

#[test]
fn a_named_virtual_graph_is_its_service_unless_only_some_graphs_are_readable() {
    let engine = engine()
        .with_fx(tripletrail::fx::FxConfig::default())
        .with_fx_graphs([(
            NamedNode::new("urn:g1").unwrap(),
            NamedNode::new("x-sparql-anything:content=from-the-service,media-type=text/plain")
                .unwrap(),
        )]);
    let query = "SELECT ?o WHERE { GRAPH <urn:g1> { ?s ?p ?o FILTER(isLiteral(?o)) } }";
    assert_eq!(
        rows(&engine, query, QueryOptions::default()),
        ["?o=\"from-the-service\""]
    );
    // A caller who may not call a SERVICE reads the store's graph.
    assert_eq!(
        rows(&engine, query, only(&["urn:g1"], false)),
        ["?o=\"g1\""]
    );
}

/// Graph management under a write grant (#208): MOVE, COPY and ADD into a writable graph read
/// their source as the caller does, so an unreadable source copies nothing, and an update that
/// would drop or change a graph the caller may not write is refused whole.
#[test]
fn graph_management_keeps_to_the_grants() {
    let options = tripletrail::UpdateOptions {
        readable: Some(graphs(&["urn:g1"], false)),
        writable: Some(graphs(&["urn:g1"], false)),
        ..Default::default()
    };
    let quads = |e: &Engine| {
        let QueryResults::Solutions(s) = e
            .query("SELECT ?g ?s ?p ?o { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }")
            .unwrap()
        else {
            panic!()
        };
        let mut rows: Vec<String> = s
            .map(|r| {
                let r = r.unwrap();
                let t = |v: &str| r.get(v).map_or("-".to_owned(), |t| t.to_string());
                format!("{} {} {} {}", t("g"), t("s"), t("p"), t("o"))
            })
            .collect();
        rows.sort();
        rows
    };
    let before = quads(&engine());
    let without_g1: Vec<String> = before
        .iter()
        .filter(|q| !q.starts_with("<urn:g1>"))
        .cloned()
        .collect();
    let with_g1 = |extra: &[&str]| {
        let mut rows = before.clone();
        rows.extend(extra.iter().map(|q| q.to_string()));
        rows.sort();
        rows
    };
    for (update, after) in [
        // The source is unreadable: as if empty. COPY clears the target, ADD adds nothing.
        ("COPY <urn:g2> TO <urn:g1>", Some(without_g1.clone())),
        ("ADD <urn:g2> TO <urn:g1>", Some(before.clone())),
        // The caller's default graph is the union of its readable graphs: g1 itself, read as
        // it was before the request (#782), so a copy changes nothing.
        ("ADD DEFAULT TO <urn:g1>", Some(before.clone())),
        ("COPY DEFAULT TO <urn:g1>", Some(before.clone())),
        ("DROP GRAPH <urn:g1>", Some(without_g1.clone())),
        (
            "INSERT DATA { GRAPH <urn:g1> { <urn:n> <urn:p> <urn:m> } }",
            Some(with_g1(&["<urn:g1> <urn:n> <urn:p> <urn:m>"])),
        ),
        // Changing an unwritable graph, even SILENT or as the source of a MOVE: refused.
        ("MOVE <urn:g2> TO <urn:g1>", None),
        ("MOVE <urn:g1> TO <urn:g2>", None),
        ("COPY <urn:g1> TO <urn:g2>", None),
        ("ADD <urn:g1> TO DEFAULT", None),
        ("DROP SILENT GRAPH <urn:g2>", None),
        ("CLEAR SILENT GRAPH <urn:g2>", None),
        ("CREATE SILENT GRAPH <urn:g2>", None),
        (
            "LOAD SILENT <http://127.0.0.1:1/x> INTO GRAPH <urn:g2>",
            None,
        ),
        ("LOAD SILENT <http://127.0.0.1:1/x>", None),
        // A refused part refuses the whole request.
        ("DROP GRAPH <urn:g1> ; DROP SILENT GRAPH <urn:g2>", None),
    ] {
        let e = engine();
        let result = e.update_with_options(update, &options);
        match after {
            Some(after) => {
                assert!(result.is_ok(), "{update}: {result:?}");
                assert_eq!(quads(&e), after, "{update}");
            }
            None => {
                let err = result.err().unwrap_or_else(|| panic!("{update}"));
                assert!(err.to_string().contains("may not write"), "{update}: {err}");
                assert_eq!(quads(&e), before, "{update}");
            }
        }
    }
}

/// A GeoSPARQL relation in a service's sub-select (#857) is rewritten from the caller's
/// graphs: a zone in a hidden graph contains nothing.
#[test]
fn relations_in_service_sub_selects_read_the_readable_graphs() {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            r#"@prefix geo: <http://www.opengis.net/ont/geosparql#> .
               <urn:g1> { <urn:p> geo:hasGeometry <urn:pg> . <urn:pg> geo:asWKT "POINT(1 1)"^^geo:wktLiteral . }
               <urn:g2> { <urn:z> geo:hasGeometry <urn:zg> . <urn:zg> geo:asWKT "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"^^geo:wktLiteral . }"#
                .as_bytes(),
        )
        .unwrap();
    let query = r#"PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        SELECT ?a { SERVICE <http://w3id.org/tripletrail/window> {
          [] <http://w3id.org/tripletrail/window#orderBy> ?a ;
             <http://w3id.org/tripletrail/window#rowNumber> ?n .
          { SELECT ?a WHERE { ?a geo:sfWithin <urn:z> ; geo:hasGeometry ?g } } } }"#;
    assert_eq!(
        rows(&engine, query, only(&["urn:g1", "urn:g2"], false)),
        ["?a=<urn:p>", "?a=<urn:z>"]
    );
    assert!(rows(&engine, query, only(&["urn:g1"], false)).is_empty());
    assert!(rows(&engine, query, only(&["urn:g2"], false)).len() == 1);
}

/// An update of a caller limited to some graphs may call the scoped services (#203): their
/// sub-selects read the graphs of its `WHERE`, so hidden rows are not counted. Other services,
/// and operations that call one over different graphs, are refused.
#[test]
fn updates_call_the_scoped_services_on_the_readable_graphs() {
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            r#"<urn:g1> { <urn:b> <urn:n> 2 . }
               <urn:g2> { <urn:a> <urn:n> 1 . }"#
                .as_bytes(),
        )
        .unwrap();
    let options = |read: &[&str]| tripletrail::UpdateOptions {
        readable: Some(graphs(read, false)),
        writable: Some(graphs(&["urn:g1"], false)),
        ..Default::default()
    };
    let rank = r#"INSERT { GRAPH <urn:g1> { ?s <urn:rank> ?r } } WHERE {
        SERVICE <http://w3id.org/tripletrail/window> {
          [] <http://w3id.org/tripletrail/window#orderBy> ?n ; <http://w3id.org/tripletrail/window#rowNumber> ?r .
          { SELECT ?s ?n { ?s <urn:n> ?n } } } }"#;
    // Reading g1 only: <urn:a> in g2 is not ranked before <urn:b>.
    engine
        .update_with_options(rank, &options(&["urn:g1"]))
        .unwrap();
    let ranks = || {
        rows(
            &engine,
            "SELECT ?s ?r { GRAPH <urn:g1> { ?s <urn:rank> ?r } }",
            QueryOptions::default(),
        )
    };
    assert_eq!(
        ranks(),
        ["?s=<urn:b> ?r=\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
    // Reading both, the same update ranks <urn:a> first.
    engine
        .update("DELETE WHERE { GRAPH <urn:g1> { ?s <urn:rank> ?r } }")
        .unwrap();
    engine
        .update_with_options(rank, &options(&["urn:g1", "urn:g2"]))
        .unwrap();
    assert_eq!(
        ranks(),
        [
            "?s=<urn:a> ?r=\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>",
            "?s=<urn:b> ?r=\"2\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        ]
    );
    for refused in [
        "INSERT { GRAPH <urn:g1> { ?s ?p ?o } } WHERE { SERVICE <https://example.org/sparql> { ?s ?p ?o } }",
        "INSERT { GRAPH <urn:g1> { ?s <urn:x> 1 } } WHERE { ?s ?p ?o FILTER EXISTS { SERVICE <http://w3id.org/tripletrail/path> { ?s ?p ?o } } }",
        // Two operations whose services would read different graphs.
        &format!(
            "{rank} ; {}",
            rank.replace("WHERE {", "USING <urn:g2> WHERE {")
        ),
    ] {
        let err = engine
            .update_with_options(refused, &options(&["urn:g1", "urn:g2"]))
            .err()
            .unwrap_or_else(|| panic!("{refused} is refused"));
        assert!(err.to_string().contains("SERVICE"), "{err}");
    }
}
