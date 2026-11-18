//! End-to-end SPARQL tests: geo functions and pgr routing through the evaluator.

use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::Term;
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail::{Engine, QueryOptions};

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
# A generic edge table expressed as triples. a -> b -> c -> d, plus a slow a -> d.
ex:e1 a ex:Edge ; ex:from ex:a ; ex:to ex:b ; ex:len 1 ; ex:back 1 ;
  geo:hasGeometry [ geo:asWKT "LINESTRING(0 0, 1 0)"^^geo:wktLiteral ] .
ex:e2 a ex:Edge ; ex:from ex:b ; ex:to ex:c ; ex:len 1 ;
  geo:hasGeometry [ geo:asWKT "LINESTRING(1 0, 2 0)"^^geo:wktLiteral ] .
ex:e3 a ex:Edge ; ex:from ex:c ; ex:to ex:d ; ex:len 1 ;
  geo:hasGeometry [ geo:asWKT "LINESTRING(2 0, 3 0)"^^geo:wktLiteral ] .
ex:e4 a ex:Edge ; ex:from ex:a ; ex:to ex:d ; ex:len 10 ;
  geo:hasGeometry [ geo:asWKT "LINESTRING(0 0, 3 0)"^^geo:wktLiteral ] .
ex:station geo:hasGeometry [ geo:asWKT "POLYGON((0.5 -1, 1.5 -1, 1.5 1, 0.5 1, 0.5 -1))"^^geo:wktLiteral ] .
"#;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX st: <http://w3id.org/tripletrail/st#>
PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
"#;

const EDGES: &str = r#"
{ SELECT ?id ?source ?target ?cost ?reverse_cost WHERE {
    ?id a ex:Edge ; ex:from ?source ; ex:to ?target ; ex:len ?cost .
    OPTIONAL { ?id ex:back ?reverse_cost }
} }"#;

fn engine() -> Engine {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e
}

fn rows(e: &Engine, q: &str) -> Vec<Vec<Option<String>>> {
    let QueryResults::Solutions(s) = e.query(&format!("{PREFIXES}{q}")).unwrap() else {
        panic!()
    };
    let vars = s.variables().to_vec();
    s.map(|r| {
        let r = r.unwrap();
        vars.iter()
            .map(|v| {
                r.get(v).map(|t| match t {
                    Term::NamedNode(n) => n
                        .as_str()
                        .trim_start_matches("http://example.com/")
                        .to_owned(),
                    Term::Literal(l) => l.value().to_owned(),
                    other => other.to_string(),
                })
            })
            .collect()
    })
    .collect()
}

fn s(v: &str) -> Option<String> {
    Some(v.to_owned())
}

#[test]
fn dijkstra_over_runtime_edge_query() {
    let e = engine();
    let r = rows(
        &e,
        &format!(
            r#"
        SELECT ?seq ?node ?edge ?cost ?agg_cost WHERE {{
          SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_dijkstra ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
               pgr:seq ?seq ; pgr:node ?node ; pgr:edge ?edge ; pgr:cost ?cost ; pgr:agg_cost ?agg_cost .
            {EDGES}
          }}
        }} ORDER BY ?seq"#
        ),
    );
    assert_eq!(
        r,
        vec![
            vec![s("1"), s("a"), s("e1"), s("1"), s("0")],
            vec![s("2"), s("b"), s("e2"), s("1"), s("1")],
            vec![s("3"), s("c"), s("e3"), s("1"), s("2")],
            vec![s("4"), s("d"), None, s("0"), s("3")],
        ]
    );
    // Second call hits the network cache.
    rows(
        &e,
        &format!(
            r#"SELECT * WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function pgr:pgr_dijkstra ; pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:agg_cost ?c . {EDGES} }} }}"#
        ),
    );
    assert!(e.last_routing_stats().cache_hit);
}

#[test]
fn directed_and_reverse_cost() {
    let e = engine();
    let cost = |directed: bool, from: &str, to: &str| {
        rows(
            &e,
            &format!(
                r#"SELECT ?agg WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_dijkstraCost ; pgr:start_vid ex:{from} ; pgr:end_vid ex:{to} ;
               pgr:directed {directed} ; pgr:agg_cost ?agg . {EDGES} }} }}"#
            ),
        )
    };
    // d -> a is impossible when directed (only e1 has a reverse cost)...
    assert!(cost(true, "d", "a").is_empty());
    assert_eq!(cost(true, "b", "a"), vec![vec![s("1")]]);
    // ...but undirected every arc works both ways.
    assert_eq!(cost(false, "d", "a"), vec![vec![s("3")]]);
}

#[test]
fn results_join_back_to_the_graph() {
    // Route, then fetch the geometry of each edge and measure it: routing results are ordinary bindings.
    let e = engine();
    let r = rows(
        &e,
        &format!(
            r#"
        SELECT (SUM(st:ST_Length(?wkt)) AS ?len) WHERE {{
          SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_bdDijkstra ; pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:edge ?edge .
            {EDGES}
          }}
          ?edge geo:hasGeometry/geo:asWKT ?wkt .
        }}"#
        ),
    );
    assert_eq!(r, vec![vec![s("3")]]);
}

#[test]
fn named_network_is_cached_and_invalidated() {
    let e = engine();
    e.update(&format!(r#"{PREFIXES}
        INSERT DATA {{ ex:net a pgr:Network ; pgr:edgeQuery """
          PREFIX ex: <http://example.com/>
          SELECT ?id ?source ?target ?cost WHERE {{ ?id a ex:Edge ; ex:from ?source ; ex:to ?target ; ex:len ?cost }}""" }}"#))
        .unwrap();
    let q = r#"SELECT ?agg WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
        [] pgr:function pgr:pgr_dijkstraCost ; pgr:network ex:net ; pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:agg_cost ?agg } }"#;
    assert_eq!(rows(&e, q), vec![vec![s("3")]]);
    assert!(!e.last_routing_stats().cache_hit);
    assert_eq!(rows(&e, q), vec![vec![s("3")]]);
    assert!(e.last_routing_stats().cache_hit);
    // Restating data (deleted, then inserted again) changes nothing: the graph is kept.
    e.update(&format!(
        "{PREFIXES} DELETE WHERE {{ ex:e1 ex:len ?l }} ; INSERT DATA {{ ex:e1 ex:len 1 }}"
    ))
    .unwrap();
    assert_eq!(rows(&e, q), vec![vec![s("3")]]);
    // (Oxigraph's store does not report what an update changed: any write drops the networks.)
    assert_eq!(
        e.last_routing_stats().cache_hit,
        e.store().kind() == "tripletrail"
    );
    // Removing e2 changes the answer: the update invalidated the cached graph.
    e.update(&format!("{PREFIXES} DELETE WHERE {{ ex:e2 ex:len ?l }}"))
        .unwrap();
    assert_eq!(rows(&e, q), vec![vec![s("10")]]);
}

#[test]
fn driving_distance() {
    let e = engine();
    let r = rows(
        &e,
        &format!(
            r#"
        SELECT ?node ?agg WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_drivingDistance ; pgr:start_vid ex:a ; pgr:distance 2 ;
               pgr:node ?node ; pgr:agg_cost ?agg . {EDGES} }} }} ORDER BY ?agg"#
        ),
    );
    assert_eq!(
        r,
        vec![
            vec![s("a"), s("0")],
            vec![s("b"), s("1")],
            vec![s("c"), s("2")]
        ]
    );
}

#[test]
fn astar_with_coordinates_from_geometry() {
    let e = engine();
    let r = rows(
        &e,
        r#"
        SELECT ?agg WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
            [] pgr:function pgr:pgr_aStar ; pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:heuristic 4 ; pgr:agg_cost ?agg .
            { SELECT ?id ?source ?target ?cost ?x1 ?y1 ?x2 ?y2 WHERE {
                ?id a ex:Edge ; ex:from ?source ; ex:to ?target ; ex:len ?cost ; geo:hasGeometry/geo:asWKT ?w .
                BIND(st:ST_X(st:ST_StartPoint(?w)) AS ?x1) BIND(st:ST_Y(st:ST_StartPoint(?w)) AS ?y1)
                BIND(st:ST_X(st:ST_EndPoint(?w)) AS ?x2) BIND(st:ST_Y(st:ST_EndPoint(?w)) AS ?y2)
            } }
        } } ORDER BY DESC(?agg) LIMIT 1"#,
    );
    assert_eq!(r, vec![vec![s("3")]]);
}

#[test]
fn geosparql_filter() {
    let e = engine();
    let r = rows(
        &e,
        r#"
        SELECT ?edge WHERE {
          ex:station geo:hasGeometry/geo:asWKT ?area .
          ?edge a ex:Edge ; geo:hasGeometry/geo:asWKT ?w .
          FILTER(geof:sfIntersects(?w, ?area))
        } ORDER BY ?edge"#,
    );
    assert_eq!(r, vec![vec![s("e1")], vec![s("e2")], vec![s("e4")]]);
}

#[test]
fn bad_requests_are_errors() {
    let e = engine();
    let q = format!(
        r#"{PREFIXES} SELECT * WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function pgr:pgr_dijkstra ; pgr:start_vid ex:a ; pgr:agg_cost ?c . {EDGES} }} }}"#
    );
    let err = match e.query(&q) {
        Ok(QueryResults::Solutions(mut s)) => s.next().unwrap().err().map(|e| e.to_string()),
        Err(e) => Some(e.to_string()),
        _ => None,
    };
    assert!(err.unwrap().contains("end_vid"));
}

#[test]
fn ksp_lists_alternatives_cheapest_first() {
    let e = engine();
    let r = rows(
        &e,
        &format!(
            r#"
        SELECT ?path_id ?path_seq ?node ?agg_cost WHERE {{
          SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_KSP ; pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:K 3 ;
               pgr:path_id ?path_id ; pgr:path_seq ?path_seq ; pgr:node ?node ; pgr:agg_cost ?agg_cost .
            {EDGES}
          }}
        }}"#
        ),
    );
    let summary: Vec<_> = r
        .iter()
        .map(|row| {
            (
                row[0].clone().unwrap(),
                row[2].clone().unwrap(),
                row[3].clone().unwrap(),
            )
        })
        .collect();
    let expect = |p: &str, n: &str, c: &str| (p.to_owned(), n.to_owned(), c.to_owned());
    assert_eq!(
        summary,
        vec![
            expect("1", "a", "0"),
            expect("1", "b", "1"),
            expect("1", "c", "2"),
            expect("1", "d", "3"),
            expect("2", "a", "0"),
            expect("2", "d", "10"),
        ]
    );
}

#[test]
fn trsp_avoids_restricted_sequences() {
    let e = engine();
    let query = |restrictions: &str| {
        rows(
            &e,
            &format!(
                r#"
            SELECT ?edge ?agg_cost WHERE {{
              SERVICE <http://w3id.org/tripletrail/pgr> {{
                [] pgr:function pgr:pgr_trsp ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
                   pgr:edge ?edge ; pgr:agg_cost ?agg_cost .
                {EDGES}
                {restrictions}
              }}
            }}"#
            ),
        )
    };
    let last = |r: &[Vec<Option<String>>]| r.last().unwrap()[1].clone().unwrap();
    // No restriction rows: the plain shortest path.
    let none = query(r#"{ SELECT ?path WHERE { FILTER(false) BIND("" AS ?path) } }"#);
    assert_eq!(last(&none), "3");
    // Forbid e1 then e2: the slow direct edge.
    let forbidden = query(
        r#"{ SELECT ?path WHERE { BIND("http://example.com/e1 http://example.com/e2" AS ?path) } }"#,
    );
    assert_eq!(
        forbidden.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
        vec![s("e4"), None]
    );
    // A penalty smaller than the detour is paid instead.
    let penalty = query(
        r#"{ SELECT ?path ?cost WHERE { BIND("<http://example.com/e2>,<http://example.com/e3>" AS ?path) BIND(2 AS ?cost) } }"#,
    );
    assert_eq!(last(&penalty), "5");
    // Restrictions stored as data, one row per position.
    e.update(&format!(
        "{PREFIXES} INSERT DATA {{ ex:r1 ex:step [ ex:seq 1 ; ex:edge ex:e2 ], [ ex:seq 2 ; ex:edge ex:e3 ] }}"
    ))
    .unwrap();
    let stored =
        query("{ SELECT ?id ?seq ?edge WHERE { ?id ex:step [ ex:seq ?seq ; ex:edge ?edge ] } }");
    assert_eq!(last(&stored), "10");
}

#[test]
fn dijkstra_via_chains_legs() {
    let e = engine();
    let r = rows(
        &e,
        &format!(
            r#"
        SELECT ?path_id ?node ?agg_cost ?route_agg_cost WHERE {{
          SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_dijkstraVia ; pgr:via ex:a ; pgr:via ex:c ; pgr:via ex:d ;
               pgr:path_id ?path_id ; pgr:node ?node ; pgr:agg_cost ?agg_cost ;
               pgr:route_agg_cost ?route_agg_cost .
            {EDGES}
          }}
        }}"#
        ),
    );
    let got: Vec<Vec<&str>> = r
        .iter()
        .map(|row| row.iter().map(|c| c.as_deref().unwrap()).collect())
        .collect();
    assert_eq!(
        got,
        vec![
            vec!["1", "a", "0", "0"],
            vec!["1", "b", "1", "1"],
            vec!["1", "c", "2", "2"],
            vec!["2", "c", "0", "2"],
            vec!["2", "d", "1", "3"],
        ]
    );
    // An unreachable leg (d has no way back to a) is skipped, or empties a strict result.
    let via = |strict: &str| {
        rows(
            &e,
            &format!(
                r#"
            SELECT ?path_id ?node WHERE {{
              SERVICE <http://w3id.org/tripletrail/pgr> {{
                [] pgr:function pgr:pgr_dijkstraVia ; pgr:via ex:a ; pgr:via ex:b ; pgr:via ex:d ; pgr:via ex:a ;
                   pgr:strict {strict} ; pgr:path_id ?path_id ; pgr:node ?node .
                {EDGES}
              }}
            }}"#
            ),
        )
        .len()
    };
    assert_eq!(via("false"), 2 + 3);
    assert_eq!(via("true"), 0);
}

#[test]
fn components_cut_vertices_and_bridges() {
    let e = engine();
    // An a-b-c triangle, a c-d-e tail hanging off it, and a separate f-g.
    let edges = r#"
        { SELECT ?id ?source ?target ?cost ?reverse_cost WHERE {
            VALUES (?id ?source ?target ?cost ?reverse_cost) {
              (ex:x1 ex:a ex:b 1 1) (ex:x2 ex:b ex:c 1 1) (ex:x3 ex:c ex:a 1 1)
              (ex:x4 ex:c ex:d 1 1) (ex:x5 ex:d ex:e 1 1) (ex:x6 ex:f ex:g 1 1)
            }
        } }"#;
    let run = |function: &str, out: &str| -> Vec<Vec<String>> {
        rows(
            &e,
            &format!(
                r#"
            SELECT ?component ?node ?edge WHERE {{
              SERVICE <http://w3id.org/tripletrail/pgr> {{
                [] pgr:function pgr:pgr_{function} ; {out} .
                {edges}
              }}
            }}"#
            ),
        )
        .into_iter()
        .map(|r| r.into_iter().flatten().collect())
        .collect()
    };
    let comps = run(
        "connectedComponents",
        "pgr:component ?component ; pgr:node ?node",
    );
    let pairs: Vec<(String, String)> = comps.iter().map(|r| (r[0].clone(), r[1].clone())).collect();
    let expect: Vec<(String, String)> = [
        ("a", "a"),
        ("a", "b"),
        ("a", "c"),
        ("a", "d"),
        ("a", "e"),
        ("f", "f"),
        ("f", "g"),
    ]
    .iter()
    .map(|(c, n)| (c.to_string(), n.to_string()))
    .collect();
    assert_eq!(pairs, expect);
    // pgr:size without pgr:node: one row per component.
    let sizes = rows(
        &e,
        &format!(
            r#"SELECT ?component ?size WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
                [] pgr:function pgr:pgr_connectedComponents ; pgr:component ?component ;
                   pgr:size ?size . {edges} }} }}"#
        ),
    );
    let expect = vec![
        vec![Some("a".to_owned()), Some("5".to_owned())],
        vec![Some("f".to_owned()), Some("2".to_owned())],
    ];
    assert_eq!(sizes, expect);
    // Counting the vertices per component is answered the same way, by the SERVICE.
    let grouped = format!(
        r#"SELECT ?component (COUNT(?node) AS ?size) WHERE {{
             SERVICE <http://w3id.org/tripletrail/pgr> {{
               [] pgr:function pgr:pgr_connectedComponents ; pgr:component ?component ;
                  pgr:node ?node . {edges} }} }} GROUP BY ?component ORDER BY ?component"#
    );
    assert_eq!(rows(&e, &grouped), expect);
    assert!(
        !e.explain(&format!("{PREFIXES}{grouped}"))
            .unwrap()
            .contains("Aggregate")
    );
    let cuts: Vec<String> = run("articulationPoints", "pgr:node ?node")
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(cuts, ["c", "d"]);
    let bridges: Vec<String> = run("bridges", "pgr:edge ?edge")
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(bridges, ["x4", "x5", "x6"]);
    // On the shared graph a <-> b is the only cycle of directed arcs.
    let strong = rows(
        &e,
        &format!(
            r#"
        SELECT ?component ?node WHERE {{
          SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_strongComponents ; pgr:component ?component ; pgr:node ?node .
            {EDGES}
          }}
        }}"#
        ),
    );
    let strong: Vec<(String, String)> = strong
        .into_iter()
        .map(|r| (r[0].clone().unwrap(), r[1].clone().unwrap()))
        .collect();
    let expect: Vec<(String, String)> = [("a", "a"), ("a", "b"), ("c", "c"), ("d", "d")]
        .iter()
        .map(|(c, n)| (c.to_string(), n.to_string()))
        .collect();
    assert_eq!(strong, expect);
}

fn pgr(e: &Engine, function: &str, args: &str, select: &str) -> Vec<Vec<String>> {
    rows(
        e,
        &format!(
            r#"
        SELECT {select} WHERE {{
          SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_{function} ; {args} .
            {EDGES}
          }}
        }}"#
        ),
    )
    .into_iter()
    .map(|r| r.into_iter().map(|v| v.unwrap_or_default()).collect())
    .collect()
}

#[test]
fn dijkstra_near_and_cost_variants() {
    let e = engine();
    // c (cost 2) is nearer to a than d (cost 3).
    assert_eq!(
        pgr(
            &e,
            "dijkstraNear",
            "pgr:start_vid ex:a ; pgr:end_vid ex:d, ex:c ; pgr:node ?node ; pgr:agg_cost ?agg",
            "?node ?agg"
        ),
        [["a", "0"], ["b", "1"], ["c", "2"]]
    );
    assert_eq!(
        pgr(
            &e,
            "dijkstraNearCost",
            "pgr:start_vid ex:a ; pgr:end_vid ex:d, ex:c ; pgr:cap 2 ; pgr:end_vid ?t ; pgr:agg_cost ?agg",
            "?t ?agg"
        ),
        [["c", "2"], ["d", "3"]]
    );
    assert_eq!(
        pgr(
            &e,
            "bdDijkstraCost",
            "pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:start_vid ?s ; pgr:end_vid ?t ; pgr:agg_cost ?agg",
            "?s ?t ?agg"
        ),
        [["a", "d", "3"]]
    );
}

#[test]
fn all_pairs_and_spanning_tree() {
    let e = engine();
    let pairs = pgr(
        &e,
        "floydWarshall",
        "pgr:start_vid ?s ; pgr:end_vid ?t ; pgr:agg_cost ?agg",
        "?s ?t ?agg",
    );
    let expected = [
        ("a", "b", "1"),
        ("a", "c", "2"),
        ("a", "d", "3"),
        ("b", "a", "1"),
        ("b", "c", "1"),
        ("b", "d", "2"),
        ("c", "d", "1"),
    ]
    .map(|(s, t, c)| vec![s.to_owned(), t.to_owned(), c.to_owned()]);
    assert_eq!(pairs, expected);
    assert_eq!(
        pgr(
            &e,
            "johnson",
            "pgr:start_vid ?s ; pgr:end_vid ?t ; pgr:agg_cost ?agg",
            "?s ?t ?agg"
        ),
        expected
    );
    // The slow a -> d edge is left out of the tree.
    let tree = pgr(
        &e,
        "kruskal",
        "pgr:edge ?edge ; pgr:cost ?cost ; pgr:agg_cost ?agg",
        "?edge ?cost ?agg",
    );
    assert_eq!(tree, [["e1", "1", "1"], ["e2", "1", "2"], ["e3", "1", "3"]]);
    assert_eq!(
        pgr(&e, "prim", "pgr:edge ?edge", "?edge"),
        [["e1"], ["e2"], ["e3"]]
    );
}

const POINTS: &str = r#"
{ SELECT ?pid ?edge ?fraction WHERE {
    VALUES (?pid ?edge ?fraction) { (ex:p1 ex:e2 0.5) (ex:p2 ex:e4 0.1) }
} }"#;

#[test]
fn with_points_splits_edges() {
    let e = engine();
    let run = |function: &str, args: &str, select: &str| {
        pgr(
            &e,
            function,
            &format!("{args} . {POINTS} [] pgr:function pgr:pgr_{function}"),
            select,
        )
    };
    // a -> b -> p1, half way along e2; the parts keep e2's id.
    assert_eq!(
        run(
            "withPoints",
            "pgr:start_vid ex:a ; pgr:end_vid ex:p1 ; pgr:node ?node ; pgr:edge ?edge ; pgr:agg_cost ?agg",
            "?node ?edge ?agg"
        ),
        [["a", "e1", "0"], ["b", "e2", "1"], ["p1", "", "1.5"]]
    );
    assert_eq!(
        run(
            "withPointsCost",
            "pgr:start_vid ex:p1, ex:p2 ; pgr:end_vid ex:d, ex:a ; pgr:start_vid ?s ; pgr:end_vid ?t ; pgr:agg_cost ?agg",
            "?s ?t ?agg"
        ),
        // e4 is one-way a -> d, so p2 cannot reach a.
        [["p1", "d", "1.5"], ["p2", "d", "9"]]
    );
    assert_eq!(
        run(
            "withPointsDD",
            "pgr:start_vid ex:p1 ; pgr:distance 1 ; pgr:node ?node ; pgr:agg_cost ?agg",
            "?node ?agg"
        ),
        [["p1", "0"], ["c", "0.5"]]
    );
}

#[test]
fn contraction_hierarchy_engine_gives_the_same_routes() {
    let e = engine();
    for function in ["dijkstra", "bdDijkstra", "dijkstraCost", "dijkstraNear"] {
        let args = "pgr:start_vid ex:a, ex:b ; pgr:end_vid ex:d, ex:c ; pgr:node ?node ; pgr:agg_cost ?agg";
        let args = if function == "dijkstraCost" {
            "pgr:start_vid ex:a, ex:b ; pgr:end_vid ex:d, ex:c ; pgr:end_vid ?node ; pgr:agg_cost ?agg"
        } else {
            args
        };
        let plain = pgr(&e, function, args, "?node ?agg");
        let ch = pgr(
            &e,
            function,
            &format!("{args} ; pgr:engine \"ch\""),
            "?node ?agg",
        );
        assert!(!plain.is_empty());
        assert_eq!(plain, ch, "{function}");
    }
}

#[test]
fn networks_are_kept_on_disk_until_the_data_changes() {
    let dir = tripletrail_testdir::tempdir("networks");
    let store = tripletrail::oxigraph::store::Store::new().unwrap();
    let query = format!(
        r#"{PREFIXES} SELECT ?agg WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_dijkstraCost ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
               pgr:engine "ch" ; pgr:agg_cost ?agg .
            {EDGES} }} }}"#
    );
    let cost = |e: &Engine| -> Vec<Option<String>> {
        rows(e, query.trim_start_matches(PREFIXES))
            .into_iter()
            .map(|r| r[0].clone())
            .collect()
    };
    {
        let e = Engine::with_network_dir(store.clone(), &dir).unwrap();
        e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        assert_eq!(cost(&e), [s("3")]);
        assert!(!e.last_routing_stats().cache_hit);
    }
    let saved: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|f| f.path().extension().is_some_and(|x| x == "net"))
        .collect();
    assert_eq!(saved.len(), 1);
    // A new engine on the same data reuses the saved network and hierarchy.
    let e = Engine::with_network_dir(store.clone(), &dir).unwrap();
    assert_eq!(cost(&e), [s("3")]);
    assert!(e.last_routing_stats().cache_hit);
    // A write through the engine makes the saved network stale.
    e.update("PREFIX ex: <http://example.com/> DELETE DATA { ex:e3 ex:len 1 } ; INSERT DATA { ex:e3 ex:len 5 }")
        .unwrap();
    let e = Engine::with_network_dir(store, &dir).unwrap();
    assert_eq!(cost(&e), [s("7")]);
    assert!(!e.last_routing_stats().cache_hit);
}

#[test]
fn writes_with_no_network_on_disk_leave_the_directory_alone() {
    // #709: with no network saved, a write neither rewrites the generation file nor lists the
    // directory; the file is brought up to date when a network is saved.
    let dir = tripletrail_testdir::tempdir("networks-w");
    let store = tripletrail::store::Store::new();
    let query = format!(
        r#"SELECT ?agg WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_dijkstraCost ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
               pgr:agg_cost ?agg . {EDGES} }} }}"#
    );
    let generation = || std::fs::read_to_string(dir.join("generation")).ok();
    {
        let e = Engine::with_network_dir(store.clone(), &dir).unwrap();
        e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        let after_load = generation();
        for i in 0..5 {
            e.update(&format!(
                "PREFIX ex: <http://example.com/> INSERT DATA {{ ex:x{i} ex:p {i} }}"
            ))
            .unwrap();
        }
        assert_eq!(generation(), after_load);
        assert_eq!(rows(&e, &query), vec![vec![s("3")]]);
        assert!(!e.last_routing_stats().cache_hit);
        assert_ne!(generation(), after_load);
    }
    // The saved network carries the generation that was written before it.
    let e = Engine::with_network_dir(store.clone(), &dir).unwrap();
    assert_eq!(rows(&e, &query), vec![vec![s("3")]]);
    assert!(e.last_routing_stats().cache_hit);
    // A write to its edges still makes it stale, also after a restart.
    e.update("PREFIX ex: <http://example.com/> DELETE DATA { ex:e3 ex:len 1 } ; INSERT DATA { ex:e3 ex:len 5 }")
        .unwrap();
    let e = Engine::with_network_dir(store, &dir).unwrap();
    assert_eq!(rows(&e, &query), vec![vec![s("7")]]);
    assert!(!e.last_routing_stats().cache_hit);
}

#[test]
fn networks_with_constraint_columns_are_kept_on_disk() {
    let dir = tripletrail_testdir::tempdir("networks-c");
    let store = tripletrail::store::Store::new();
    let cost = |e: &Engine, constraint: &str| -> Vec<Option<String>> {
        rows(
            e,
            &format!(
                r#"SELECT ?agg WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
                [] pgr:function pgr:pgr_dijkstraCost ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
                   pgr:constraint "{constraint}" ; pgr:agg_cost ?agg .
                {{ SELECT ?id ?source ?target ?cost ?c_len WHERE {{
                    ?id a ex:Edge ; ex:from ?source ; ex:to ?target ; ex:len ?cost .
                    BIND(?cost AS ?c_len) }} }} }} }}"#
            ),
        )
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
    };
    {
        let e = Engine::with_network_dir(store.clone(), &dir).unwrap();
        e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        assert_eq!(cost(&e, "?c_len < 5"), [s("3")]);
        assert!(!e.last_routing_stats().cache_hit);
    }
    // A new engine reads the network, with its ?c_* columns, from disk.
    let e = Engine::with_network_dir(store, &dir).unwrap();
    assert_eq!(cost(&e, "?c_len > 5"), [s("10")]);
    assert!(e.last_routing_stats().cache_hit);
}

#[test]
fn the_network_cache_reports_its_size() {
    // Counted in a dataset's memory quota (#225).
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    assert_eq!(e.networks().bytes(), 0);
    let q = format!(
        r#"SELECT ?agg WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function pgr:pgr_dijkstraCost ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
           pgr:agg_cost ?agg . {EDGES} }} }}"#
    );
    assert_eq!(rows(&e, &q), vec![vec![s("3")]]);
    assert!(e.networks().bytes() > 0);
    assert!(e.networks().bytes() >= e.networks().unpinned_bytes());
}

#[test]
fn memory_use_counts_geometries_and_the_spatial_and_text_indexes() {
    // What a dataset's memory quota counts (#225).
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let before = e.memory_use().unwrap();
    assert!(before.dictionary > 0 && before.quads > 0);
    assert_eq!(
        (before.geometries, before.spatial_index, before.text_index),
        (0, 0, 0)
    );
    let r = rows(
        &e,
        r#"
        SELECT ?edge WHERE {
          ex:station geo:hasGeometry/geo:asWKT ?area .
          ?edge a ex:Edge ; geo:hasGeometry/geo:asWKT ?w .
          FILTER(geof:sfIntersects(?w, ?area))
        } ORDER BY ?edge"#,
    );
    assert_eq!(r.len(), 3);
    e.update(r#"INSERT DATA { <http://example.com/station> <http://www.w3.org/2000/01/rdf-schema#label> "Brussel-Zuid" }"#)
        .unwrap();
    e.spatial_index().unwrap();
    e.text_index().unwrap();
    let after = e.memory_use().unwrap();
    assert!(after.geometries > 0, "{after:?}");
    assert!(after.spatial_index > 0, "{after:?}");
    assert!(after.text_index > 0, "{after:?}");
    assert_eq!(
        after.total(),
        after.parts().iter().map(|p| p.1).sum::<usize>()
    );
    // Oxigraph's store has no count.
    assert_eq!(Engine::new_oxigraph().unwrap().memory_use(), None);
}

#[test]
fn writes_keep_networks_that_do_not_read_their_predicates() {
    // The tripletrail store reports what an update changed; Oxigraph's store does not, so there
    // every update drops the networks.
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let q = format!(
        r#"SELECT ?agg WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function pgr:pgr_dijkstraCost ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
           pgr:agg_cost ?agg . {EDGES} }} }}"#
    );
    assert_eq!(rows(&e, &q), vec![vec![s("3")]]);
    assert!(!e.last_routing_stats().cache_hit);
    // Neither ex:label nor geo:asWKT is read by the edge query.
    e.update(&format!(
        r#"{PREFIXES} INSERT DATA {{ ex:e1 ex:label "first" .
            ex:x geo:asWKT "POINT(9 9)"^^geo:wktLiteral }}"#
    ))
    .unwrap();
    e.insert(&tripletrail::oxigraph::model::Quad::new(
        tripletrail::oxigraph::model::NamedNode::new_unchecked("http://example.com/e2"),
        tripletrail::oxigraph::model::NamedNode::new_unchecked("http://example.com/label"),
        tripletrail::oxigraph::model::Literal::new_simple_literal("second"),
        tripletrail::oxigraph::model::GraphName::DefaultGraph,
    ))
    .unwrap();
    assert_eq!(rows(&e, &q), vec![vec![s("3")]]);
    assert!(e.last_routing_stats().cache_hit);
    // ex:len is.
    e.update(&format!(
        "{PREFIXES} DELETE DATA {{ ex:e2 ex:len 1 }} ; INSERT DATA {{ ex:e2 ex:len 4 }}"
    ))
    .unwrap();
    assert_eq!(rows(&e, &q), vec![vec![s("6")]]);
    assert!(!e.last_routing_stats().cache_hit);
    // An update that changes nothing keeps the network.
    e.update(&format!("{PREFIXES} DELETE DATA {{ ex:e2 ex:len 1 }}"))
        .unwrap();
    assert_eq!(rows(&e, &q), vec![vec![s("6")]]);
    assert!(e.last_routing_stats().cache_hit);
}

#[test]
fn saved_networks_survive_unrelated_writes() {
    let dir = tripletrail_testdir::tempdir("networks-kept");
    let store = tripletrail::store::Store::new();
    let q = format!(
        r#"SELECT ?agg WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function pgr:pgr_dijkstraCost ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
           pgr:engine "ch" ; pgr:agg_cost ?agg . {EDGES} }} }}"#
    );
    {
        let e = Engine::with_network_dir(store.clone(), &dir).unwrap();
        e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
        assert_eq!(rows(&e, &q), vec![vec![s("3")]]);
        e.update(&format!(
            r#"{PREFIXES} INSERT DATA {{ ex:e1 ex:label "first" }}"#
        ))
        .unwrap();
    }
    let e = Engine::with_network_dir(store, &dir).unwrap();
    assert_eq!(rows(&e, &q), vec![vec![s("3")]]);
    assert!(e.last_routing_stats().cache_hit);
}

#[test]
fn cost_changes_customize_the_hierarchy_again() {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    let q = format!(
        r#"SELECT ?agg WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function pgr:pgr_dijkstraCost ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
           pgr:engine "ch" ; pgr:agg_cost ?agg . {EDGES} }} }}"#
    );
    assert_eq!(rows(&e, &q), vec![vec![s("3")]]);
    assert!(e.last_routing_stats().build.customize.is_none());
    // Only a cost changes: the new network keeps the hierarchy and customizes it.
    e.update(&format!(
        "{PREFIXES} DELETE DATA {{ ex:e2 ex:len 1 }} ; INSERT DATA {{ ex:e2 ex:len 4 }}"
    ))
    .unwrap();
    assert_eq!(rows(&e, &q), vec![vec![s("6")]]);
    let stats = e.last_routing_stats();
    assert!(!stats.cache_hit);
    assert!(stats.build.customize.is_some());
}

/// Runs a pgr function on `edges` (a sub-select, or any other sub-selects) and returns the
/// rows of `select`, unbound cells as "".
fn call(e: &Engine, function: &str, args: &str, edges: &str, select: &str) -> Vec<Vec<String>> {
    rows(
        e,
        &format!(
            r#"
        SELECT {select} WHERE {{
          SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_{function} ; {args} .
            {edges}
          }}
        }}"#
        ),
    )
    .into_iter()
    .map(|r| r.into_iter().map(|v| v.unwrap_or_default()).collect())
    .collect()
}

/// An edge sub-select from `(id source target cost)` tuples, written as VALUES rows.
fn values(rows: &str) -> String {
    format!(
        "{{ SELECT ?id ?source ?target ?cost WHERE {{ VALUES (?id ?source ?target ?cost) {{ {rows} }} }} }}"
    )
}

/// The error message of a query that must fail.
fn error(e: &Engine, q: &str) -> String {
    match e.query(&format!("{PREFIXES}{q}")) {
        Ok(QueryResults::Solutions(mut s)) => s.next().unwrap().unwrap_err().to_string(),
        Err(e) => e.to_string(),
        _ => panic!("not a SELECT"),
    }
}

#[test]
fn shortest_paths_without_dijkstra() {
    let e = engine();
    let ends = "pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:node ?node ; pgr:agg_cost ?agg";
    for function in ["bellmanFord", "edwardMoore"] {
        assert_eq!(
            call(&e, function, ends, EDGES, "?node ?agg"),
            [["a", "0"], ["b", "1"], ["c", "2"], ["d", "3"]]
        );
    }
    // A negative arc is used, not dropped.
    let negative = values("(1 1 2 4) (2 1 3 5) (3 3 2 -3) (4 2 4 1)");
    let args = "pgr:start_vid 1 ; pgr:end_vid 4 ; pgr:node ?node ; pgr:agg_cost ?agg";
    for function in ["bellmanFord", "edwardMoore"] {
        assert_eq!(
            call(&e, function, args, &negative, "?node ?agg"),
            [["1", "0"], ["3", "5"], ["2", "2"], ["4", "3"]]
        );
    }
    // A negative cycle gives no path.
    let cycle = values("(1 1 2 1) (2 2 3 -2) (3 3 2 1) (4 3 4 1)");
    assert!(call(&e, "bellmanFord", args, &cycle, "?node").is_empty());
    // The DAG search needs an acyclic graph: e1 runs both ways.
    assert!(call(&e, "dagShortestPath", ends, EDGES, "?node").is_empty());
    let dag = values("(1 1 2 1) (2 2 3 1) (3 3 4 1) (4 1 4 10)");
    assert_eq!(
        call(&e, "dagShortestPath", args, &dag, "?node ?agg")
            .last()
            .unwrap(),
        &["4", "3"]
    );
    // 0-1 BFS: costs 0 and one other value only.
    let binary = values("(1 1 2 1) (2 2 3 0) (3 3 4 1) (4 1 4 1)");
    assert_eq!(
        call(&e, "binaryBreadthFirstSearch", args, &binary, "?agg")
            .last()
            .unwrap(),
        &["1"]
    );
    let q = format!(
        r#"SELECT * WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function pgr:pgr_binaryBreadthFirstSearch ; {ends} . {EDGES} }} }}"#
    );
    assert!(error(&e, &q).contains("needs arc costs of 0 and one other value"));
}

#[test]
fn traversals_and_spanning_tree_traversals() {
    let e = engine();
    let tree = "pgr:start_vid ex:a ; pgr:depth ?depth ; pgr:pred ?pred ; pgr:node ?node ; pgr:edge ?edge ; pgr:agg_cost ?agg";
    let select = "?depth ?pred ?node ?edge ?agg";
    // Neighbours in edge order: e1 to b, then e4 to d.
    assert_eq!(
        call(&e, "breadthFirstSearch", tree, EDGES, select),
        [
            ["0", "a", "a", "", "0"],
            ["1", "a", "b", "e1", "1"],
            ["1", "a", "d", "e4", "10"],
            ["2", "b", "c", "e2", "2"],
        ]
    );
    assert_eq!(
        call(&e, "depthFirstSearch", tree, EDGES, "?node ?depth"),
        [["a", "0"], ["b", "1"], ["c", "2"], ["d", "3"]]
    );
    assert_eq!(
        call(
            &e,
            "breadthFirstSearch",
            &format!("{tree} ; pgr:max_depth 1"),
            EDGES,
            "?node"
        ),
        [["a"], ["b"], ["d"]]
    );
    // The spanning tree leaves the slow e4 out.
    for function in ["kruskalBFS", "kruskalDFS", "primBFS", "primDFS"] {
        assert_eq!(
            call(&e, function, tree, EDGES, "?node ?edge ?agg"),
            [
                ["a", "", "0"],
                ["b", "e1", "1"],
                ["c", "e2", "2"],
                ["d", "e3", "3"]
            ],
            "{function}"
        );
    }
    for function in ["kruskalDD", "primDD"] {
        assert_eq!(
            call(
                &e,
                function,
                &format!("{tree} ; pgr:distance 2"),
                EDGES,
                "?node ?depth"
            ),
            [["a", "0"], ["b", "1"], ["c", "2"]]
        );
    }
}

#[test]
fn trsp_via_and_with_points() {
    let e = engine();
    let via = |extra: &str| {
        call(
            &e,
            "trspVia",
            &format!(
                "pgr:via ex:a, ex:b, ex:a ; pgr:directed false ; {extra} pgr:path_id ?path ; pgr:edge ?edge ; pgr:route_agg_cost ?route"
            ),
            &format!("{EDGES} {{ SELECT ?path WHERE {{ FILTER(false) BIND(\"\" AS ?path) }} }}"),
            "?path ?edge ?route",
        )
    };
    // Back along e1 ...
    assert_eq!(
        via(""),
        [
            ["1", "e1", "0"],
            ["1", "", "1"],
            ["2", "e1", "1"],
            ["2", "", "2"]
        ]
    );
    // ... unless U-turns on an edge are not allowed: round by c and d.
    let no_u_turn = via("pgr:U_turn_on_edge false ;");
    assert_eq!(no_u_turn.last().unwrap(), &["2", "", "13"]);
    assert_eq!(no_u_turn[2], ["2", "e2", "1"]);
    // trsp_withPoints: a point half way along e2, behind a forbidden e1 -> e2 turn.
    let points = r#"{ SELECT ?pid ?edge ?fraction WHERE { VALUES (?pid ?edge ?fraction) { (ex:p1 ex:e2 0.5) } } }"#;
    let to_point = |restriction: &str| {
        call(
            &e,
            "trsp_withPoints",
            "pgr:start_vid ex:a ; pgr:end_vid ex:p1 ; pgr:agg_cost ?agg",
            &format!(
                "{EDGES} {points} {{ SELECT ?path WHERE {{ BIND(\"{restriction}\" AS ?path) }} }}"
            ),
            "?agg",
        )
    };
    assert_eq!(
        to_point("http://example.com/e3 http://example.com/e4")
            .last()
            .unwrap(),
        &["1.5"]
    );
    assert!(to_point("http://example.com/e1 http://example.com/e2").is_empty());
}

/// s -> a (3), s -> b (2), a -> b (1), a -> t (2), b -> t (3), with costs 1, 1, 1, 3, 1.
const FLOW: &str = r#"{ SELECT ?id ?source ?target ?capacity ?cost WHERE {
    VALUES (?id ?source ?target ?capacity ?cost) {
      (ex:sa ex:s ex:a 3 1) (ex:sb ex:s ex:b 2 1) (ex:ab ex:a ex:b 1 1) (ex:at ex:a ex:t 2 3) (ex:bt ex:b ex:t 3 1)
    } } }"#;

#[test]
fn flows() {
    let e = engine();
    let ends = "pgr:start_vid ex:s ; pgr:end_vid ex:t";
    assert_eq!(
        call(&e, "maxFlow", &format!("{ends} ; pgr:flow ?f"), FLOW, "?f"),
        [["5"]]
    );
    // Every unit leaving s arrives at t, whatever the algorithm.
    for function in ["pushRelabel", "edmondsKarp", "boykovKolmogorov"] {
        let r = call(
            &e,
            function,
            &format!(
                "{ends} ; pgr:edge ?edge ; pgr:start_vid ?from ; pgr:end_vid ?to ; pgr:flow ?flow ; pgr:residual_capacity ?rest"
            ),
            FLOW,
            "?edge ?from ?to ?flow ?rest",
        );
        let into_t: i64 = r
            .iter()
            .filter(|r| r[2] == "t")
            .map(|r| r[3].parse::<i64>().unwrap())
            .sum();
        assert_eq!(into_t, 5, "{function}");
    }
    // With a maximum flow of 5 every arc is forced; its cost is 1 + 6 + 3 + 3 + 2.
    assert_eq!(
        call(
            &e,
            "maxFlowMinCost",
            &format!(
                "{ends} ; pgr:edge ?edge ; pgr:flow ?flow ; pgr:cost ?cost ; pgr:agg_cost ?agg"
            ),
            FLOW,
            "?edge ?flow ?cost ?agg"
        ),
        [
            ["ab", "1", "1", "1"],
            ["at", "2", "6", "7"],
            ["bt", "3", "3", "10"],
            ["sa", "3", "3", "13"],
            ["sb", "2", "2", "15"]
        ]
    );
    assert_eq!(
        call(
            &e,
            "maxFlowMinCost_Cost",
            &format!("{ends} ; pgr:cost ?c"),
            FLOW,
            "?c"
        ),
        [["15"]]
    );
    // Flow functions need capacities.
    let q = format!(
        r#"SELECT * WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function pgr:pgr_maxFlow ; pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:flow ?f . {EDGES} }} }}"#
    );
    assert!(error(&e, &q).contains("?capacity"));
    // Two ways from a to d share no edge.
    assert_eq!(
        call(
            &e,
            "edgeDisjointPaths",
            "pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:path_id ?p ; pgr:edge ?edge ; pgr:agg_cost ?agg",
            EDGES,
            "?p ?edge ?agg"
        ),
        [
            ["1", "e1", "0"],
            ["1", "e2", "1"],
            ["1", "e3", "2"],
            ["1", "", "3"],
            ["2", "e4", "0"],
            ["2", "", "10"]
        ]
    );
    // The 4-cycle a-b-c-d matches two opposite edges, and is cut by two cheap edges.
    assert_eq!(
        call(
            &e,
            "maxCardinalityMatch",
            "pgr:edge ?edge ; pgr:source ?s ; pgr:target ?t",
            EDGES,
            "?edge ?s ?t"
        ),
        [["e1", "a", "b"], ["e3", "c", "d"]]
    );
    let cut = call(
        &e,
        "stoerWagner",
        "pgr:edge ?edge ; pgr:mincut ?cut",
        EDGES,
        "?edge ?cut",
    );
    assert_eq!(cut.len(), 2);
    assert_eq!(cut[1][1], "2");
}

#[test]
fn travelling_salesperson() {
    let e = engine();
    // Over the network's own cost matrix: pgr_TSP's sub-select calls the pgr SERVICE.
    let matrix = format!(
        r#"{{ SELECT ?start_vid ?end_vid ?agg_cost WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_dijkstraCostMatrix ; pgr:directed false ;
               pgr:start_vid ex:a, ex:b, ex:c, ex:d ; pgr:end_vid ex:a, ex:b, ex:c, ex:d ;
               pgr:start_vid ?start_vid ; pgr:end_vid ?end_vid ; pgr:agg_cost ?agg_cost .
            {EDGES} }} }} }}"#
    );
    let tour = call(
        &e,
        "TSP",
        "pgr:start_id ex:a ; pgr:seq ?seq ; pgr:node ?node ; pgr:agg_cost ?agg",
        &matrix,
        "?node ?agg",
    );
    assert_eq!(tour.len(), 5);
    assert_eq!(tour[0], ["a", "0"]);
    assert_eq!(tour[4], ["a", "6"]);
    // The corners of a unit square, listed crossed.
    let corners = r#"{ SELECT ?id ?x ?y WHERE {
        VALUES (?id ?x ?y) { (1 0 0) (2 1 1) (3 1 0) (4 0 1) } } }"#;
    let tour = call(
        &e,
        "TSPeuclidean",
        "pgr:start_id 1 ; pgr:end_id 4 ; pgr:node ?node ; pgr:agg_cost ?agg",
        corners,
        "?node ?agg",
    );
    assert_eq!(
        tour.iter().map(|r| r[0].as_str()).collect::<Vec<_>>(),
        ["1", "3", "2", "4", "1"]
    );
    assert_eq!(tour[4][1], "4");
}

#[test]
fn line_graphs_and_contraction() {
    let e = engine();
    assert_eq!(
        call(
            &e,
            "lineGraph",
            "pgr:source ?s ; pgr:target ?t ; pgr:cost ?c ; pgr:reverse_cost ?r",
            EDGES,
            "?s ?t ?c ?r"
        ),
        [
            ["e1", "e2", "1", "-1"],
            ["e1", "e4", "1", "-1"],
            ["e2", "e3", "1", "-1"]
        ]
    );
    let full = call(
        &e,
        "lineGraphFull",
        "pgr:source ?s ; pgr:target ?t ; pgr:edge ?edge",
        EDGES,
        "?s ?t ?edge",
    );
    // 5 arcs along edges and 5 turns.
    assert_eq!(full.len(), 10);
    assert_eq!(full.iter().filter(|r| r[2].is_empty()).count(), 5);
    // Dead ends fold into vertex 3.
    let tree = values("(1 1 2 1) (2 2 3 1) (3 3 4 1) (4 3 5 1)");
    let contraction = "pgr:type ?type ; pgr:id ?id ; pgr:contracted_vertices ?cv ; pgr:source ?s ; pgr:target ?t ; pgr:cost ?c";
    assert_eq!(
        call(
            &e,
            "contraction",
            &format!("pgr:directed false ; pgr:methods 1 ; {contraction}"),
            &tree,
            "?type ?id ?cv"
        ),
        [["v", "3", "1 2 4 5"]]
    );
    // Linear contraction of a square 1-2-3-4-1 with a chord 1-3: removing 2 leaves 3 with
    // two neighbours, so it goes too.
    let diamond = values("(1 1 2 1) (2 2 3 1) (3 3 4 1) (4 4 1 1) (5 1 3 5)");
    let linear = |forbidden: &str| {
        call(
            &e,
            "contraction",
            &format!("pgr:directed false ; pgr:methods 2 ; {forbidden} {contraction}"),
            &diamond,
            "?type ?id ?cv ?s ?t ?c",
        )
    };
    assert_eq!(linear(""), [["e", "-1", "2 3", "1", "4", "3"]]);
    // Keeping 3, the square's other corners go.
    assert_eq!(
        linear("pgr:forbidden 3 ;"),
        [
            ["e", "-1", "2", "1", "3", "2"],
            ["e", "-2", "4", "1", "3", "2"]
        ]
    );
}

#[test]
fn graph_analysis_functions() {
    let e = engine();
    let dag = values("(1 1 2 1) (2 1 3 1) (3 3 2 1) (4 2 4 1)");
    let two = values("(1 1 2 1) (2 3 4 1)");
    let triangle = values("(1 1 2 1) (2 2 3 1) (3 3 1 1)");
    // Topology-only functions take edges without costs.
    let pendant = "{ SELECT ?id ?source ?target WHERE { VALUES (?id ?source ?target) { (1 1 2) (2 2 3) (3 3 1) (4 3 4) } } }";
    assert_eq!(
        call(
            &e,
            "biconnectedComponents",
            "pgr:edge ?e ; pgr:component ?c",
            pendant,
            "?e ?c"
        ),
        [["1", "1"], ["2", "1"], ["3", "1"], ["4", "4"]]
    );
    assert!(
        error(
            &e,
            &format!(
                r#"SELECT * WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
                [] pgr:function pgr:pgr_dijkstra ; pgr:start_vid 1 ; pgr:end_vid 4 ; pgr:node ?n . {pendant} }} }}"#
            )
        )
        .contains("?cost")
    );
    assert_eq!(
        call(&e, "degree", "pgr:node ?n ; pgr:degree ?d", &dag, "?n ?d"),
        [["1", "2"], ["2", "3"], ["3", "2"], ["4", "1"]]
    );
    let vertices = call(
        &e,
        "extractVertices",
        "pgr:id ?v ; pgr:in_edges ?in ; pgr:out_edges ?out",
        EDGES,
        "?v ?in ?out",
    );
    assert_eq!(
        vertices[0],
        ["a", "", "http://example.com/e1 http://example.com/e4"]
    );
    assert_eq!(
        call(&e, "topologicalSort", "pgr:sorted_v ?v", &dag, "?v"),
        [["1"], ["3"], ["2"], ["4"]]
    );
    assert_eq!(
        call(
            &e,
            "transitiveClosure",
            "pgr:vid ?v ; pgr:target_array ?t",
            &dag,
            "?v ?t"
        ),
        [["1", "2 3 4"], ["2", "4"], ["3", "2 4"], ["4", ""]]
    );
    assert_eq!(
        call(
            &e,
            "makeConnected",
            "pgr:start_vid ?s ; pgr:end_vid ?t",
            &two,
            "?s ?t"
        ),
        [["1", "3"]]
    );
    assert_eq!(
        call(&e, "isPlanar", "pgr:planar ?p", EDGES, "?p"),
        [["true"]]
    );
    let k5: String = (1..=5)
        .flat_map(|a| (a + 1..=5).map(move |b| format!("({a}{b} {a} {b} 1)")))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        call(&e, "isPlanar", "pgr:planar ?p", &values(&k5), "?p"),
        [["false"]]
    );
    let diamond = values("(1 1 2 1) (2 1 3 1) (3 2 4 1) (4 3 4 1) (5 4 5 1)");
    assert_eq!(
        call(
            &e,
            "lengauerTarjanDominatorTree",
            "pgr:root_vertex 1 ; pgr:vid ?v ; pgr:idom ?d",
            &diamond,
            "?v ?d"
        ),
        [["1", ""], ["2", "1"], ["3", "1"], ["4", "1"], ["5", "4"]]
    );
    // The only circuit: a -> b -> a along e1 (it has a reverse cost).
    assert_eq!(
        call(
            &e,
            "hawickCircuits",
            "pgr:path_id ?p ; pgr:node ?n ; pgr:edge ?edge ; pgr:agg_cost ?agg",
            EDGES,
            "?p ?n ?edge ?agg"
        ),
        [
            ["1", "a", "e1", "0"],
            ["1", "b", "e1", "1"],
            ["1", "a", "", "2"]
        ]
    );
    // The 4-cycle a-b-c-d: two colours, either way.
    assert_eq!(
        call(
            &e,
            "sequentialVertexColoring",
            "pgr:vertex_id ?v ; pgr:color_id ?c",
            EDGES,
            "?v ?c"
        ),
        [["a", "1"], ["b", "2"], ["c", "1"], ["d", "2"]]
    );
    assert_eq!(
        call(
            &e,
            "bipartite",
            "pgr:vertex_id ?v ; pgr:color_id ?c",
            EDGES,
            "?v ?c"
        ),
        [["a", "0"], ["b", "1"], ["c", "0"], ["d", "1"]]
    );
    assert!(call(&e, "bipartite", "pgr:vertex_id ?v", &triangle, "?v").is_empty());
    assert_eq!(
        call(
            &e,
            "edgeColoring",
            "pgr:edge_id ?e ; pgr:color_id ?c",
            EDGES,
            "?e ?c"
        ),
        [["e1", "1"], ["e2", "2"], ["e3", "1"], ["e4", "2"]]
    );
    // a and d are 3 apart in vertex order.
    assert_eq!(
        call(&e, "bandwidth", "pgr:bandwidth ?b", EDGES, "?b"),
        [["3"]]
    );
    for function in ["kingOrdering", "sloanOrdering"] {
        let mut order: Vec<String> = call(&e, function, "pgr:node ?n", EDGES, "?n")
            .into_iter()
            .map(|r| r[0].clone())
            .collect();
        order.sort();
        assert_eq!(order, ["a", "b", "c", "d"]);
    }
    // A directed triangle with a chord: the way back from the chord is walked twice.
    let postman = values("(1 1 2 1) (2 2 3 1) (3 3 1 1) (4 1 3 1)");
    assert_eq!(
        call(&e, "chinesePostmanCost", "pgr:cost ?c", &postman, "?c"),
        [["5"]]
    );
    let walk = call(
        &e,
        "chinesePostman",
        "pgr:node ?n ; pgr:edge ?edge ; pgr:agg_cost ?agg",
        &postman,
        "?n ?edge ?agg",
    );
    assert_eq!(walk.len(), 6);
    assert_eq!(walk[5], ["1", "", "5"]);
    // The functions without an edge set.
    assert_eq!(
        rows(
            &e,
            r#"SELECT ?v ?b WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
                [] pgr:function pgr:pgr_full_version ; pgr:version ?v ; pgr:build_type ?b } }"#
        )
        .len(),
        1
    );
    assert_eq!(
        rows(
            &e,
            r#"SELECT ?v WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
                [] pgr:function pgr:pgr_version ; pgr:version ?v } }"#
        )[0][0]
            .as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn turn_restricted_path_skips_paths_through_restrictions() {
    let e = engine();
    let query = |options: &str| {
        rows(
            &e,
            &format!(
                r#"
            SELECT ?path_id ?edge ?agg_cost WHERE {{
              SERVICE <http://w3id.org/tripletrail/pgr> {{
                [] pgr:function pgr:pgr_turnRestrictedPath ; pgr:start_vid ex:a ;
                   pgr:end_vid ex:d ; {options}
                   pgr:path_id ?path_id ; pgr:edge ?edge ; pgr:agg_cost ?agg_cost .
                {EDGES}
                {{ SELECT ?path WHERE {{ BIND("http://example.com/e1 http://example.com/e2" AS ?path) }} }}
              }}
            }} ORDER BY ?path_id ?agg_cost"#
            ),
        )
    };
    let edges = |r: &[Vec<Option<String>>]| {
        r.iter()
            .map(|x| (x[0].clone().unwrap(), x[1].clone()))
            .collect::<Vec<_>>()
    };
    let p = |id: &str, e: Option<&str>| (id.to_owned(), e.map(str::to_owned));
    // The cheapest path a-b-c-d uses e1 then e2; the first compliant one is the direct edge.
    assert_eq!(
        edges(&query("pgr:K 2 ;")),
        vec![p("1", Some("e4")), p("1", None)]
    );
    // Without stop_on_first, both paths, the restricted one first.
    let both = query("pgr:K 2 ; pgr:stop_on_first false ;");
    assert_eq!(both.iter().filter(|r| r[1].is_none()).count(), 2);
    assert_eq!(both[0][1], s("e1"));
    // strict drops the restricted path; with K 1 nothing complies.
    let strict = query("pgr:K 2 ; pgr:stop_on_first false ; pgr:strict true ;");
    assert_eq!(edges(&strict), vec![p("1", Some("e4")), p("1", None)]);
    assert!(query("pgr:K 1 ; pgr:strict true ;").is_empty());
    // Not strict: with no compliant path among K, the paths found are returned.
    assert_eq!(query("pgr:K 1 ;").last().unwrap()[2], s("3"));
}

#[test]
fn query_dataset_options_select_the_graphs() {
    use oxigraph::model::{GraphName, NamedNode};
    use oxigraph::sparql::QueryDataset;
    for e in [Engine::new_tripletrail(), Engine::new().unwrap()] {
        e.update(
            "INSERT DATA { <http://e/a> <http://e/p> 1 .
               GRAPH <http://e/g1> { <http://e/b> <http://e/p> 2 }
               GRAPH <http://e/g2> { <http://e/c> <http://e/p> 3 } }",
        )
        .unwrap();
        let sum = |dataset: Option<QueryDataset>| -> String {
            let QueryResults::Solutions(mut s) = e
                .query_with_options(
                    "SELECT (SUM(?o) AS ?s) WHERE { ?x <http://e/p> ?o }",
                    QueryOptions {
                        dataset,
                        ..Default::default()
                    },
                )
                .unwrap()
            else {
                panic!()
            };
            s.next().unwrap().unwrap().get("s").unwrap().to_string()
        };
        assert!(sum(None).starts_with("\"1\""));
        let mut union = QueryDataset::new();
        union.set_default_graph_as_union();
        let u = sum(Some(union.clone()));
        // The union of all graphs, the default one included, as in Oxigraph.
        assert!(u.starts_with("\"6\""), "{} {u}", e.store().kind());
        let mut g1 = QueryDataset::new();
        g1.set_default_graph(vec![GraphName::NamedNode(
            NamedNode::new("http://e/g1").unwrap(),
        )]);
        assert!(sum(Some(g1)).starts_with("\"2\""));
        let mut named = QueryDataset::new();
        named.set_available_named_graphs(vec![NamedNode::new("http://e/g2").unwrap().into()]);
        let QueryResults::Solutions(s) = e
            .query_with_options(
                "SELECT ?g WHERE { GRAPH ?g { ?x ?p ?o } }",
                QueryOptions {
                    dataset: Some(named),
                    ..Default::default()
                },
            )
            .unwrap()
        else {
            panic!()
        };
        let graphs: Vec<_> = s
            .map(|s| s.unwrap().get("g").unwrap().to_string())
            .collect();
        assert_eq!(graphs, ["<http://e/g2>"]);
        let QueryResults::Solutions(s) = e
            .query_with_options(
                "SELECT ?x ?o WHERE { ?x <http://e/p> ?o }",
                QueryOptions {
                    dataset: Some(union.clone()),
                    substitutions: vec![(
                        tripletrail::oxigraph::sparql::Variable::new("o").unwrap(),
                        tripletrail::oxigraph::model::Literal::from(3).into(),
                    )],
                    ..Default::default()
                },
            )
            .unwrap()
        else {
            panic!()
        };
        let xs: Vec<_> = s
            .map(|s| s.unwrap().get("x").unwrap().to_string())
            .collect();
        assert_eq!(xs, ["<http://e/c>"]);
    }
}

#[test]
fn rdf_12_terms_round_trip() {
    for e in [Engine::new_tripletrail(), Engine::new().unwrap()] {
        e.update(
            "INSERT DATA { <http://e/a> <http://e/says> <<( <http://e/s> <http://e/p> \"o\"@en--rtl )>> }",
        )
        .unwrap();
        let QueryResults::Solutions(mut s) = e
            .query(
                "SELECT ?t ?o (isTRIPLE(?t) AS ?is) WHERE { <http://e/a> <http://e/says> ?t
                   BIND(OBJECT(?t) AS ?o) }",
            )
            .unwrap()
        else {
            panic!()
        };
        let row = s.next().unwrap().unwrap();
        assert_eq!(
            row.get("t").unwrap().to_string(),
            "<<( <http://e/s> <http://e/p> \"o\"@en--rtl )>>",
            "{}",
            e.store().kind()
        );
        assert_eq!(row.get("o").unwrap().to_string(), "\"o\"@en--rtl");
        assert_eq!(
            row.get("is"),
            Some(&tripletrail::oxigraph::model::Literal::from(true).into())
        );
    }
}

#[test]
fn the_network_cache_stays_within_its_budget() {
    let e = engine();
    // Room for a few of these small networks, not for 100.
    let one = {
        pgr(
            &e,
            "dijkstra",
            "pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:node ?n",
            "?n",
        );
        e.networks().unpinned_bytes()
    };
    assert!(one > 0);
    e.networks().set_budget(3 * one);
    for i in 0..100 {
        // A distinct sub-select each time (a different literal), so a different network.
        let q = format!(
            r#"SELECT ?n WHERE {{
              SERVICE <http://w3id.org/tripletrail/pgr> {{
                [] pgr:function pgr:pgr_dijkstra ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
                   pgr:node ?n .
                {{ SELECT ?id ?source ?target ?cost WHERE {{
                    ?id a ex:Edge ; ex:from ?source ; ex:to ?target ; ex:len ?cost .
                    FILTER(?cost < {})
                }} }}
              }}
            }}"#,
            1000 + i
        );
        assert!(!rows(&e, &q).is_empty());
        assert!(e.networks().unpinned_bytes() <= 3 * one + one / 2, "{i}");
    }
    assert!(e.networks().len() <= 4, "{}", e.networks().len());
}

/// SERVICE-first planning (`plan.rs`) drops route rows whose linking variable is unbound,
/// such as the last row of a path, which has no `?edge`. In SPARQL an unbound variable joins
/// with anything, so the plain join would pair that row with every `ex:len` triple. Oxigraph's
/// own join gives the rewrite's answer because its optimizer types SERVICE variables as always
/// bound. This test fails if either side changes.
#[test]
fn route_rows_without_an_edge_do_not_join_with_every_triple() {
    let q = format!(
        r#"
        SELECT ?node ?edge ?len WHERE {{
          SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_dijkstra ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
               pgr:node ?node ; pgr:edge ?edge .
            {EDGES}
          }}
          ?edge ex:len ?len .
        }} ORDER BY ?node"#
    );
    let expected = [["a", "e1", "1"], ["b", "e2", "1"], ["c", "e3", "1"]]
        .map(|r| r.map(|v| Some(v.to_owned())).to_vec())
        .to_vec();
    let oxigraph = Engine::from_store(tripletrail::oxigraph::store::Store::new().unwrap());
    oxigraph.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    for e in [engine(), oxigraph] {
        let kind = e.store().kind();
        // Planned (SERVICE first, then LATERAL triple lookups).
        assert_eq!(rows(&e, &q), expected, "{kind}");
        // As written, through Oxigraph's join.
        let QueryResults::Solutions(s) = e
            .store()
            .execute(
                e.evaluator()
                    .parse_query(&format!("{PREFIXES}{q}"))
                    .unwrap(),
            )
            .unwrap()
        else {
            panic!()
        };
        let mut plain: Vec<Vec<Option<String>>> = s
            .map(|r| {
                let r = r.unwrap();
                ["node", "edge", "len"]
                    .iter()
                    .map(|v| {
                        r.get(*v).map(|t| match t {
                            Term::NamedNode(n) => n
                                .as_str()
                                .trim_start_matches("http://example.com/")
                                .to_owned(),
                            Term::Literal(l) => l.value().to_owned(),
                            other => other.to_string(),
                        })
                    })
                    .collect()
            })
            .collect();
        plain.sort();
        assert_eq!(plain, expected, "{kind}, unplanned");
    }
}

#[test]
fn union_default_graph_reads_every_graph_for_queries_and_updates() {
    let count = |e: &Engine, q: &str| -> String {
        let QueryResults::Solutions(mut s) = e.query(q).unwrap() else {
            panic!()
        };
        s.next().unwrap().unwrap().get("n").unwrap().to_string()
    };
    for e in [Engine::new_tripletrail(), Engine::new().unwrap()] {
        e.update(
            "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
             INSERT DATA { <http://e/a> <http://e/p> 1 .
               GRAPH <http://e/g1> { <http://e/b> <http://e/p> 2 .
                 <http://e/b> geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral }
               GRAPH <http://e/g2> { <http://e/c> <http://e/p> 3 .
                 <http://e/c> geo:asWKT \"POINT(2 2)\"^^geo:wktLiteral .
                 <http://e/d> geo:asWKT \"POINT(9 9)\"^^geo:wktLiteral } }",
        )
        .unwrap();
        let sum = "SELECT (SUM(?o) AS ?n) WHERE { ?x <http://e/p> ?o }";
        assert!(count(&e, sum).starts_with("\"1\""));
        e.set_union_default_graph(true);
        assert!(count(&e, sum).starts_with("\"6\""), "{}", e.store().kind());
        // A query's own FROM keeps its dataset.
        let from = "SELECT (SUM(?o) AS ?n) FROM <http://e/g1> WHERE { ?x <http://e/p> ?o }";
        assert!(count(&e, from).starts_with("\"2\""));
        // The spatial rewrite answers a FILTER over geometries of several named graphs.
        let within = "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
            PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
            SELECT (COUNT(*) AS ?n) WHERE { ?x geo:asWKT ?w
              FILTER(geof:sfWithin(?w, \"POLYGON((0 0, 5 0, 5 5, 0 5, 0 0))\"^^geo:wktLiteral)) }";
        assert!(
            count(&e, within).starts_with("\"2\""),
            "{}",
            e.store().kind()
        );
        assert_eq!(e.last_spatial_stats().filters, 1);
        // An update's WHERE reads the union; its inserts go to the default graph.
        e.update("INSERT { ?x <http://e/seen> true } WHERE { ?x <http://e/p> ?o }")
            .unwrap();
        e.set_union_default_graph(false);
        assert!(
            count(
                &e,
                "SELECT (COUNT(*) AS ?n) WHERE { ?x <http://e/seen> true }"
            )
            .starts_with("\"3\""),
            "{}",
            e.store().kind()
        );
        // With USING, an update keeps its own dataset.
        e.set_union_default_graph(true);
        e.update(
            "INSERT { ?x <http://e/g1only> true } USING <http://e/g1> WHERE { ?x <http://e/p> ?o }",
        )
        .unwrap();
        assert!(
            count(
                &e,
                "SELECT (COUNT(*) AS ?n) WHERE { ?x <http://e/g1only> true }"
            )
            .starts_with("\"1\"")
        );
    }
}

/// `STR` of a constant is folded into the string it is (so `REPLACE` and `REGEX` compile their
/// pattern once), with the same values and datatypes as evaluating it per row.
#[test]
fn str_of_a_constant_is_the_same_string() {
    let e = engine();
    let r = rows(
        &e,
        r#"SELECT ?id ?l ?t ?dl ?dt ?k WHERE {
            ?e a ex:Edge .
            BIND(REPLACE(STR(?e), STR(ex:), "") AS ?id)
            BIND(STR("x"@en) AS ?l)
            BIND(STR(12) AS ?t)
            BIND(DATATYPE(?l) AS ?dl)
            BIND(DATATYPE(STR(ex:)) AS ?dt)
            FILTER(REGEX(STR(?e), CONCAT("^", STR(ex:), "e1$")))
            BIND(STRSTARTS(STR(?e), STR(ex:)) AS ?k)
        }"#,
    );
    let string = "http://www.w3.org/2001/XMLSchema#string";
    assert_eq!(
        r,
        vec![vec![
            s("e1"),
            s("x"),
            s("12"),
            s(string),
            s(string),
            s("true")
        ]]
    );
}
