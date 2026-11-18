//! End-to-end tests of the functions beyond pgRouting: constrained, time-dependent and
//! hierarchical routing, route geometries and the topology functions, on small graphs whose
//! answers are worked out by hand in the comments.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::Term;
use tripletrail::oxigraph::sparql::QueryResults;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

# Tracks, usable both ways. a(0 0) b(1 0) c(2 0) d(3 0) e(2 1).
#   a -t1- b -t2- c -t3- d      (1 each; t3 not electrified)
#         b -t4- e -t5- d       (2 each)
#   a -t6- d                    (10, broad gauge)
#         d -t7- f              (1, its geometry does not start at d)
ex:t1 a ex:Track ; ex:from ex:a ; ex:to ex:b ; ex:len 1 ; ex:gauge 1435 ; ex:electrified true ;
  ex:wkt "LINESTRING(0 0, 1 0)"^^geo:wktLiteral .
ex:t2 a ex:Track ; ex:from ex:b ; ex:to ex:c ; ex:len 1 ; ex:gauge 1435 ; ex:electrified true ;
  ex:wkt "LINESTRING(1 0, 2 0)"^^geo:wktLiteral .
ex:t3 a ex:Track ; ex:from ex:c ; ex:to ex:d ; ex:len 1 ; ex:gauge 1435 ;
  ex:wkt "LINESTRING(2 0, 3 0)"^^geo:wktLiteral .
ex:t4 a ex:Track ; ex:from ex:b ; ex:to ex:e ; ex:len 2 ; ex:gauge 1435 ; ex:electrified true ;
  ex:wkt "LINESTRING(1 0, 1 1, 2 1)"^^geo:wktLiteral .
ex:t5 a ex:Track ; ex:from ex:e ; ex:to ex:d ; ex:len 2 ; ex:gauge 1435 ; ex:electrified true ;
  ex:wkt "LINESTRING(2 1, 3 0)"^^geo:wktLiteral .
ex:t6 a ex:Track ; ex:from ex:a ; ex:to ex:d ; ex:len 10 ; ex:gauge 1668 ; ex:electrified true ;
  ex:wkt "LINESTRING(0 0, 3 0)"^^geo:wktLiteral .
ex:t7 a ex:Track ; ex:from ex:d ; ex:to ex:f ; ex:len 1 ; ex:gauge 1435 ; ex:electrified true ;
  ex:wkt "LINESTRING(5 5, 6 6)"^^geo:wktLiteral .

# A one-way line x -r1- y -r2- z (10 each) and a slower x -r3- z (45), with a possession
# closing r2 from 5 to 30 (seconds), and the same in date-times.
ex:r1 a ex:Run ; ex:from ex:x ; ex:to ex:y ; ex:len 10 .
ex:r2 a ex:Run ; ex:from ex:y ; ex:to ex:z ; ex:len 10 .
ex:r3 a ex:Run ; ex:from ex:x ; ex:to ex:z ; ex:len 45 .
ex:p1 a ex:Possession ; ex:edge ex:r2 ; ex:start 5 ; ex:end 30 .
ex:p2 a ex:DatedPossession ; ex:edge ex:r2 ;
  ex:start "2024-01-01T08:00:05Z"^^xsd:dateTime ; ex:end "2024-01-01T08:00:30Z"^^xsd:dateTime .

# Two levels. Coarse: A -k1- B -k2- C (10 each), A -k3- C (30).
# Fine: a1 -f1- m1 -f2- b1 -f3- m2 -f4- c1 (5 each) and a shortcut a1 -f5- c1 (12).
ex:k1 a ex:Section ; ex:from ex:A ; ex:to ex:B ; ex:len 10 .
ex:k2 a ex:Section ; ex:from ex:B ; ex:to ex:C ; ex:len 10 .
ex:k3 a ex:Section ; ex:from ex:A ; ex:to ex:C ; ex:len 30 .
ex:f1 a ex:Micro ; ex:from ex:a1 ; ex:to ex:m1 ; ex:len 5 ; ex:partOf ex:k1 .
ex:f2 a ex:Micro ; ex:from ex:m1 ; ex:to ex:b1 ; ex:len 5 ; ex:partOf ex:k1 .
ex:f3 a ex:Micro ; ex:from ex:b1 ; ex:to ex:m2 ; ex:len 5 ; ex:partOf ex:k2 .
ex:f4 a ex:Micro ; ex:from ex:m2 ; ex:to ex:c1 ; ex:len 5 ; ex:partOf ex:k2 .
ex:f5 a ex:Micro ; ex:from ex:a1 ; ex:to ex:c1 ; ex:len 12 ; ex:partOf ex:k3 .

# An edge set with problems: q's ends are 0.5 apart, b3 is a loop, b4 has no cost or
# geometry, p and s are dangling.
ex:b1 a ex:Bad ; ex:from ex:p ; ex:to ex:q ; ex:len 1 ; ex:wkt "LINESTRING(0 0, 1 0)"^^geo:wktLiteral .
ex:b2 a ex:Bad ; ex:from ex:q ; ex:to ex:r ; ex:len 1 ; ex:wkt "LINESTRING(1.5 0, 2 0)"^^geo:wktLiteral .
ex:b3 a ex:Bad ; ex:from ex:r ; ex:to ex:r ; ex:len 1 ; ex:wkt "LINESTRING(2 0, 2 1, 2 0)"^^geo:wktLiteral .
ex:b4 a ex:Bad ; ex:from ex:r ; ex:to ex:s .

# Lines without topology: a cross, and one ending on the first.
ex:l1 a ex:Line ; ex:wkt "LINESTRING(0 0, 4 0)"^^geo:wktLiteral .
ex:l2 a ex:Line ; ex:wkt "LINESTRING(2 -2, 2 2)"^^geo:wktLiteral .
ex:l3 a ex:Line ; ex:wkt "LINESTRING(3 3, 3 0.001)"^^geo:wktLiteral .
"#;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
PREFIX rail: <http://w3id.org/tripletrail/rail#>
"#;

const TRACKS: &str = r#"
{ SELECT ?id ?source ?target ?cost ?reverse_cost ?geom ?c_gauge ?c_electrified WHERE {
    ?id a ex:Track ; ex:from ?source ; ex:to ?target ; ex:len ?cost ; ex:gauge ?c_gauge ;
        ex:wkt ?geom .
    BIND(?cost AS ?reverse_cost)
    OPTIONAL { ?id ex:electrified ?c_electrified }
} }"#;

const RUNS: &str = r#"
{ SELECT ?id ?source ?target ?cost WHERE { ?id a ex:Run ; ex:from ?source ; ex:to ?target ; ex:len ?cost } }"#;

fn engine() -> Engine {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e
}

fn rows(e: &Engine, q: &str) -> Vec<Vec<Option<String>>> {
    let QueryResults::Solutions(s) = e
        .query(&format!("{PREFIXES}{q}"))
        .unwrap_or_else(|err| panic!("{err}\n{q}"))
    else {
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

fn strs(rows: &[Vec<Option<String>>]) -> Vec<Vec<&str>> {
    rows.iter()
        .map(|r| r.iter().map(|c| c.as_deref().unwrap_or("-")).collect())
        .collect()
}

/// The edges of a route from `from` to `to` on the tracks, under `extra` parameters.
fn route(e: &Engine, from: &str, to: &str, extra: &str) -> Vec<Vec<Option<String>>> {
    rows(
        e,
        &format!(
            r#"SELECT ?edge ?agg_cost WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function rail:route ; pgr:start_vid ex:{from} ; pgr:end_vid ex:{to} ; {extra}
           pgr:seq ?seq ; pgr:edge ?edge ; pgr:agg_cost ?agg_cost .
        {TRACKS} }} }} ORDER BY ?seq"#
        ),
    )
}

#[test]
fn constrained_routes() {
    let e = engine();
    let edges = |r: Vec<Vec<Option<String>>>| -> Vec<String> {
        r.into_iter().filter_map(|row| row[0].clone()).collect()
    };
    // Unconstrained: along the bottom, 3.
    let r = route(&e, "a", "d", "");
    assert_eq!(edges(r.clone()), ["t1", "t2", "t3"]);
    assert_eq!(r.last().unwrap()[1], s("3"));
    // Electrified only: t3 is out, so over e, 5.
    for engine in ["dijkstra", "ch"] {
        let r = route(
            &e,
            "a",
            "d",
            &format!(
                r#"pgr:constraint "?c_gauge = 1435 && ?c_electrified" ; pgr:engine "{engine}" ;"#
            ),
        );
        assert_eq!(edges(r.clone()), ["t1", "t4", "t5"], "{engine}");
        assert_eq!(r.last().unwrap()[1], s("5"));
    }
    // The constrained graph is cached with the network.
    route(
        &e,
        "a",
        "d",
        r#"pgr:constraint "?c_gauge = 1435 && ?c_electrified" ;"#,
    );
    let stats = e.last_routing_stats();
    assert!(stats.cache_hit);
    assert_eq!(stats.constraint, std::time::Duration::ZERO);
    // Broad gauge only: the long way.
    assert_eq!(
        edges(route(&e, "a", "d", r#"pgr:constraint "?c_gauge = 1668" ;"#)),
        ["t6"]
    );
    // Nothing fits: no route.
    assert!(route(&e, "a", "d", r#"pgr:constraint "?c_gauge = 1000" ;"#).is_empty());
    // An unknown column is an error.
    assert!(
        e.query(&format!(
            r#"{PREFIXES} SELECT * WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function rail:route ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
               pgr:constraint "?c_speed > 100" ; pgr:edge ?e . {TRACKS} }} }}"#
        ))
        .map_or(true, |r| match r {
            QueryResults::Solutions(mut s) => s.any(|r| r.is_err()),
            _ => false,
        })
    );

    // Reachable within 3, electrified: a 0, b 1, c 2, e 3 (d needs t3, or 5 over e).
    let reach = |constraint: &str| {
        rows(
            &e,
            &format!(
                r#"SELECT ?node ?agg_cost WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function rail:reachable ; pgr:start_vid ex:a ; pgr:distance 3 ; {constraint}
               pgr:node ?node ; pgr:agg_cost ?agg_cost . {TRACKS} }} }} ORDER BY ?agg_cost ?node"#
            ),
        )
    };
    assert_eq!(
        strs(&reach(r#"pgr:constraint "?c_electrified" ;"#)),
        [["a", "0"], ["b", "1"], ["c", "2"], ["e", "3"]]
    );
    assert_eq!(reach("").len(), 5);
}

#[test]
fn route_geometries() {
    let e = engine();
    // d to a runs every edge backwards: the geometries are reversed.
    let r = rows(
        &e,
        &format!(
            r#"SELECT ?edge ?g WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function rail:route ; pgr:start_vid ex:d ; pgr:end_vid ex:a ;
           pgr:seq ?seq ; pgr:edge ?edge ; pgr:geom ?g .
        {TRACKS} }} }} ORDER BY ?seq"#
        ),
    );
    assert_eq!(
        strs(&r),
        [
            ["t3", "LINESTRING(3 0,2 0)"],
            ["t2", "LINESTRING(2 0,1 0)"],
            ["t1", "LINESTRING(1 0,0 0)"],
            ["-", "-"],
        ]
    );
    // Only route-level columns: one row per route.
    let whole = |from: &str, to: &str, extra: &str| {
        rows(
            &e,
            &format!(
                r#"SELECT ?g ?list ?total WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function rail:route ; pgr:start_vid ex:{from} ; pgr:end_vid ex:{to} ; {extra}
               rail:routeGeometry ?g ; rail:routeAsList ?list ; pgr:agg_cost ?total .
            {TRACKS} }} }}"#
            ),
        )
    };
    assert_eq!(
        strs(&whole("d", "a", "")),
        [[
            "LINESTRING(3 0,2 0,1 0,0 0)",
            "http://example.com/t3 http://example.com/t2 http://example.com/t1",
            "3"
        ]]
    );
    assert_eq!(
        strs(&whole("a", "d", r#"pgr:constraint "?c_electrified" ;"#))[0][0],
        "LINESTRING(0 0,1 0,1 1,2 1,3 0)"
    );
    // t7's geometry does not continue from d: two parts.
    assert_eq!(
        strs(&whole("c", "f", ""))[0][0],
        "MULTILINESTRING((2 0,3 0),(5 5,6 6))"
    );
    // The route geometry is an ordinary geometry literal.
    let r = rows(
        &e,
        &format!(
            r#"SELECT ?len WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function rail:route ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
               pgr:route_geom ?g . {TRACKS} }} BIND(geof:length(?g) AS ?len) }}"#
        ),
    );
    assert_eq!(r.len(), 1);
    assert!(r[0][0].is_some());
}

#[test]
fn isochrone() {
    let e = engine();
    // Within 1.5 of a: a and b, half of t2, a quarter of t4 and 0.15 of t6.
    let r = rows(
        &e,
        &format!(
            r#"SELECT ?start ?g ?far WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function rail:isochrone ; pgr:start_vid ex:a ; pgr:distance 1.5 ;
           pgr:start_vid ?start ; pgr:geom ?g ; pgr:agg_cost ?far .
        {TRACKS} }} }}"#
        ),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], s("a"));
    assert_eq!(r[0][2], s("1"));
    let hull = r[0][1].clone().unwrap();
    assert!(hull.starts_with("POLYGON"), "{hull}");
    for corner in ["0 0", "1.5 0", "1 0.5"] {
        assert!(hull.contains(corner), "{corner} in {hull}");
    }
}

#[test]
fn time_dependent_routes() {
    let e = engine();
    let at = |departure: &str, extra: &str, periods: &str| {
        rows(
            &e,
            &format!(
                r#"SELECT ?node ?cost ?agg ?t WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function rail:routeAt ; pgr:start_vid ex:x ; pgr:end_vid ex:z ;
               pgr:departure {departure} ; {extra}
               pgr:seq ?seq ; pgr:node ?node ; pgr:cost ?cost ; pgr:agg_cost ?agg ; pgr:time ?t .
            {RUNS}
            {periods} }} }} ORDER BY ?seq"#
            ),
        )
    };
    let numeric = "{ SELECT ?edge ?t_from ?t_to WHERE { ?p a ex:Possession ; ex:edge ?edge ; ex:start ?t_from ; ex:end ?t_to } }";
    // At y at 10, r2 is closed until 30: waiting gets to z at 40, before r3 would (45).
    assert_eq!(
        strs(&at("0", "", numeric)),
        [
            ["x", "10", "0", "0"],
            ["y", "30", "10", "10"],
            ["z", "0", "40", "40"]
        ]
    );
    // Without waiting, only r3.
    assert_eq!(
        strs(&at("0", "pgr:wait false ;", numeric)),
        [["x", "45", "0", "0"], ["z", "0", "45", "45"]]
    );
    // After the possession: the plain route.
    assert_eq!(at("100", "", numeric).last().unwrap()[2], s("20"));
    // Without periods: static costs.
    assert_eq!(at("0", "", "").last().unwrap()[2], s("20"));
    // Date-times: times are written as date-times too.
    let dated = "{ SELECT ?edge ?t_from ?t_to WHERE { ?p a ex:DatedPossession ; ex:edge ?edge ; ex:start ?t_from ; ex:end ?t_to } }";
    let r = at("\"2024-01-01T08:00:00Z\"^^xsd:dateTime", "", dated);
    assert_eq!(
        strs(&r).iter().map(|r| (r[0], r[3])).collect::<Vec<_>>(),
        [
            ("x", "2024-01-01T08:00:00Z"),
            ("y", "2024-01-01T08:00:10Z"),
            ("z", "2024-01-01T08:00:40Z")
        ]
    );
    // A temporary speed restriction on r1 (cost 30 until 100): y at 30, after the closure.
    let tsr = "{ SELECT ?edge ?t_from ?t_to ?t_cost WHERE { { ?p a ex:Possession ; ex:edge ?edge ; ex:start ?t_from ; ex:end ?t_to } UNION { BIND(ex:r1 AS ?edge) BIND(0 AS ?t_from) BIND(100 AS ?t_to) BIND(30 AS ?t_cost) } } }";
    assert_eq!(
        strs(&at("0", "", tsr)),
        [
            ["x", "30", "0", "0"],
            ["y", "10", "30", "30"],
            ["z", "0", "40", "40"]
        ]
    );
}

#[test]
fn hierarchical_routes() {
    let e = engine();
    let coarse = "{ SELECT ?id ?source ?target ?cost WHERE { ?id a ex:Section ; ex:from ?source ; ex:to ?target ; ex:len ?cost } }";
    let fine = "{ SELECT ?fine_id ?fine_source ?fine_target ?fine_cost WHERE { ?fine_id a ex:Micro ; ex:from ?fine_source ; ex:to ?fine_target ; ex:len ?fine_cost } }";
    let mapping = |filter: &str| {
        format!(
            "{{ SELECT ?coarse_edge ?fine_edge WHERE {{ ?fine_edge ex:partOf ?coarse_edge {filter} }} }}"
        )
    };
    let hierarchical = |mapping: &str| {
        rows(
            &e,
            &format!(
                r#"SELECT ?edge ?coarse ?agg ?inside WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function rail:routeHierarchical ; pgr:start_vid ex:A ; pgr:end_vid ex:C ;
               pgr:fine_start_vid ex:a1 ; pgr:fine_end_vid ex:c1 ;
               pgr:seq ?seq ; pgr:edge ?edge ; pgr:coarse_edge ?coarse ; pgr:agg_cost ?agg ;
               pgr:corridor ?inside .
            {coarse} {fine} {mapping} }} }} ORDER BY ?seq"#
            ),
        )
    };
    // The coarse route is k1 k2; refined along f1..f4 (20), although f5 alone costs 12.
    assert_eq!(
        strs(&hierarchical(&mapping(""))),
        [
            ["f1", "k1", "0", "true"],
            ["f2", "k1", "5", "true"],
            ["f3", "k2", "10", "true"],
            ["f4", "k2", "15", "true"],
            ["-", "-", "20", "true"]
        ]
    );
    // Without f3 in the mapping, the corridor is broken: the fine network decides.
    assert_eq!(
        strs(&hierarchical(&mapping("FILTER(?fine_edge != ex:f3)"))),
        [["f5", "-", "0", "false"], ["-", "-", "12", "false"]]
    );
    // rail:refine takes the coarse route as rows, and the fine network as the edge set.
    let r = rows(
        &e,
        &format!(
            r#"SELECT ?edge WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function rail:refine ; pgr:start_vid ex:a1 ; pgr:end_vid ex:c1 ;
               pgr:seq ?seq ; pgr:edge ?edge .
            {{ SELECT ?id ?source ?target ?cost WHERE {{ ?id a ex:Micro ; ex:from ?source ; ex:to ?target ; ex:len ?cost }} }}
            {{ SELECT ?coarse_edge WHERE {{ VALUES ?coarse_edge {{ ex:k1 ex:k2 }} }} }}
            {} }} }} ORDER BY ?seq"#,
            mapping("")
        ),
    );
    assert_eq!(
        r.into_iter()
            .filter_map(|r| r[0].clone())
            .collect::<Vec<_>>(),
        ["f1", "f2", "f3", "f4"]
    );
}

#[test]
fn check_topology() {
    let e = engine();
    let r = rows(
        &e,
        r#"SELECT ?problem ?edge ?node ?value WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
        [] pgr:function rail:checkTopology ;
           pgr:seq ?seq ; pgr:problem ?problem ; pgr:edge ?edge ; pgr:node ?node ; pgr:value ?value .
        { SELECT ?id ?source ?target ?cost ?geom WHERE {
            ?id a ex:Bad ; ex:from ?source ; ex:to ?target .
            OPTIONAL { ?id ex:len ?cost } OPTIONAL { ?id ex:wkt ?geom } } }
        } } ORDER BY ?seq"#,
    );
    assert_eq!(
        strs(&r),
        [
            ["dangling_vertex", "b1", "p", "-"],
            ["dangling_vertex", "b4", "s", "-"],
            ["ends_not_coincident", "b2", "q", "0.5"],
            ["missing_cost", "b4", "-", "-"],
            ["missing_geometry", "b4", "-", "-"],
            ["self_loop", "b3", "r", "-"],
        ]
    );
}

#[test]
fn topology_from_geometries() {
    let e = engine();
    let lines = "{ SELECT ?id ?geom WHERE { ?id a ex:Line ; ex:wkt ?geom } }";
    // l3 ends 0.001 above l1: touching only with a tolerance.
    let separate = |function: &str, tolerance: &str| {
        rows(
            &e,
            &format!(
                r#"SELECT ?id ?sub ?g WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:{function} ; {tolerance}
               pgr:seq ?seq ; pgr:id ?id ; pgr:sub_id ?sub ; pgr:geom ?g . {lines} }} }} ORDER BY ?seq"#
            ),
        )
    };
    assert_eq!(
        strs(&separate("pgr_separateCrossing", "")),
        [
            ["l1", "1", "LINESTRING(0 0,2 0)"],
            ["l1", "2", "LINESTRING(2 0,4 0)"],
            ["l2", "1", "LINESTRING(2 -2,2 0)"],
            ["l2", "2", "LINESTRING(2 0,2 2)"],
        ]
    );
    assert!(separate("pgr_separateTouching", "").is_empty());
    assert_eq!(
        strs(&separate("pgr_separateTouching", "pgr:tolerance 0.01 ;")),
        [
            ["l1", "1", "LINESTRING(0 0,3 0)"],
            ["l1", "2", "LINESTRING(3 0,4 0)"]
        ]
    );
    // Noding everything: l1 in three pieces, l2 in two, l3 whole; with the tolerance l3's
    // end is snapped onto l1's vertex at (3 0): 7 vertices.
    let r = rows(
        &e,
        &format!(
            r#"SELECT ?old ?sub ?source ?target WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function pgr:pgr_nodeNetwork ; pgr:tolerance 0.01 ;
               pgr:seq ?seq ; pgr:old_id ?old ; pgr:sub_id ?sub ; pgr:source ?source ; pgr:target ?target .
            {lines} }} }} ORDER BY ?seq"#
        ),
    );
    assert_eq!(r.len(), 6);
    let vertices: std::collections::HashSet<_> = r
        .iter()
        .flat_map(|r| [r[2].clone(), r[3].clone()])
        .collect();
    assert_eq!(vertices.len(), 7);

    // Topology for edge geometries, then routed on directly.
    let r = rows(
        &e,
        r#"SELECT ?id ?source ?target WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
            [] pgr:function rail:buildTopology ;
               pgr:id ?id ; pgr:source ?source ; pgr:target ?target .
            { SELECT ?id ?geom WHERE { ?id a ex:Track ; ex:wkt ?geom } } } } ORDER BY ?id"#,
    );
    // Vertices are numbered as met: t1 (0 0)=1 (1 0)=2, t2 (2 0)=3, t3 (3 0)=4, t4 (2 1)=5,
    // t6 none new, t7 (5 5)=6 (6 6)=7.
    assert_eq!(
        strs(&r),
        [
            ["t1", "1", "2"],
            ["t2", "2", "3"],
            ["t3", "3", "4"],
            ["t4", "2", "5"],
            ["t5", "5", "4"],
            ["t6", "1", "4"],
            ["t7", "6", "7"],
        ]
    );

    // Counts: 7 edges, 7 vertices; (5 5) and (6 6) are dead ends of the isolated t7, so two
    // components.
    let r = rows(
        &e,
        r#"SELECT * WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
            [] pgr:function rail:analyzeGraph ;
               pgr:edges ?edges ; pgr:vertices ?vertices ; pgr:dead_ends ?dead ;
               pgr:isolated ?isolated ; pgr:components ?components ; pgr:intersections ?x .
            { SELECT ?id ?geom WHERE { ?id a ex:Track ; ex:wkt ?geom } } } }"#,
    );
    assert_eq!(r.len(), 1);
    let get = |name: &str| {
        let QueryResults::Solutions(s) = e
            .query(&format!(
                r#"{PREFIXES} SELECT ?v WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
                [] pgr:function rail:analyzeGraph ; pgr:{name} ?v .
                {{ SELECT ?id ?geom WHERE {{ ?id a ex:Track ; ex:wkt ?geom }} }} }} }}"#
            ))
            .unwrap()
        else {
            panic!()
        };
        match s.into_iter().next().unwrap().unwrap().get("v") {
            Some(Term::Literal(l)) => l.value().to_owned(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(get("edges"), "7");
    assert_eq!(get("vertices"), "7");
    assert_eq!(get("dead_ends"), "2");
    assert_eq!(get("isolated"), "1");
    assert_eq!(get("components"), "2");
    assert_eq!(get("intersections"), "0");

    // The tracks close to a point, nearest first.
    let r = rows(
        &e,
        r#"SELECT ?edge ?fraction ?side ?distance WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
            [] pgr:function pgr:pgr_findCloseEdges ;
               pgr:point "POINT(0.5 0.25)"^^geo:wktLiteral ; pgr:tolerance 0.4 ; pgr:cap 5 ;
               pgr:edge_id ?edge ; pgr:fraction ?fraction ; pgr:side ?side ; pgr:distance ?distance .
            { SELECT ?id ?geom WHERE { ?id a ex:Track ; ex:wkt ?geom } } } }"#,
    );
    // t1 and t6 both run through (0.5 0); t1 comes first by id order.
    assert_eq!(
        strs(&r),
        [
            ["t1", "0.5", "l", "0.25"],
            ["t6", "0.16666666666666666", "l", "0.25"]
        ]
    );
}

#[test]
fn pareto_routes() {
    let e = engine();
    // Time and energy per track: the direct broad-gauge t6 is slow but cheap in energy, the
    // detour b-e-d is fast but costly, and b-c-d lies between them.
    e.update(
        r#"PREFIX ex: <http://example.com/>
        INSERT DATA {
          ex:t1 ex:time 1 ; ex:energy 1 .  ex:t2 ex:time 1 ; ex:energy 2 .
          ex:t3 ex:time 4 ; ex:energy 2 .  ex:t4 ex:time 1 ; ex:energy 4 .
          ex:t5 ex:time 1 ; ex:energy 4 .  ex:t6 ex:time 9 ; ex:energy 2 .
          ex:t7 ex:time 1 ; ex:energy 1 .
        }"#,
    )
    .unwrap();
    let edges = r#"{ SELECT ?id ?source ?target ?cost ?reverse_cost ?c_time ?c_energy ?c_gauge WHERE {
        ?id a ex:Track ; ex:from ?source ; ex:to ?target ; ex:len ?cost ; ex:gauge ?c_gauge ;
            ex:time ?c_time ; ex:energy ?c_energy .
        BIND(?cost AS ?reverse_cost) } }"#;
    let q = |extra: &str, cols: &str| {
        rows(
            &e,
            &format!(
                r#"SELECT {cols} WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function rail:paretoRoutes ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
               pgr:criteria "c_time c_energy" ; {extra}
               pgr:path_id ?path_id ; pgr:total_c_time ?time ; pgr:total_c_energy ?energy ;
               pgr:route_list ?route .
            {edges} }} }} ORDER BY ?path_id"#
            ),
        )
    };
    // Per path (time, energy): b-e-d (3, 9), b-c-d (6, 5), t6 (9, 2); all Pareto-optimal.
    let all = q("", "?path_id ?time ?energy ?route");
    assert_eq!(
        strs(&all),
        [
            [
                "1",
                "3",
                "9",
                "http://example.com/t1 http://example.com/t4 http://example.com/t5"
            ],
            [
                "2",
                "6",
                "5",
                "http://example.com/t1 http://example.com/t2 http://example.com/t3"
            ],
            ["3", "9", "2", "http://example.com/t6"],
        ]
    );
    // The cap keeps the lexicographically first paths.
    let capped = q("pgr:max_paths 2 ;", "?path_id ?time");
    assert_eq!(strs(&capped), [["1", "3"], ["2", "6"]]);
    // Constraints apply: without broad gauge, t6 is gone.
    assert_eq!(
        q(r#"pgr:constraint "?c_gauge = 1435" ;"#, "?path_id").len(),
        2
    );
    // Criteria in another order, with the edge cost as one of them.
    let swapped = rows(
        &e,
        &format!(
            r#"SELECT ?path_id ?energy ?len WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function rail:paretoRoutes ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
               pgr:criteria "c_energy" ; pgr:criteria "cost" ;
               pgr:path_id ?path_id ; pgr:total_c_energy ?energy ; pgr:total_cost ?len .
            {edges} }} }} ORDER BY ?path_id"#
        ),
    );
    // (energy, length): t6 (2, 10), b-c-d (5, 3); b-e-d (9, 5) is dominated by b-c-d.
    assert_eq!(strs(&swapped), [["1", "2", "10"], ["2", "5", "3"]]);
    // Step rows: seq, node, edge and running costs.
    let steps = rows(
        &e,
        &format!(
            r#"SELECT ?seq ?path_id ?path_seq ?node ?edge ?time ?agg_time WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function rail:paretoRoutes ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
               pgr:criteria "c_time c_energy" ; pgr:max_paths 1 ;
               pgr:seq ?seq ; pgr:path_id ?path_id ; pgr:path_seq ?path_seq ; pgr:node ?node ;
               pgr:edge ?edge ; pgr:c_time ?time ; pgr:agg_c_time ?agg_time .
            {edges} }} }} ORDER BY ?seq"#
        ),
    );
    assert_eq!(
        strs(&steps),
        [
            ["1", "1", "1", "a", "t1", "1", "0"],
            ["2", "1", "2", "b", "t4", "1", "1"],
            ["3", "1", "3", "e", "t5", "1", "2"],
            ["4", "1", "4", "d", "-", "0", "3"],
        ]
    );
    // Errors: no criteria, a column the edge set does not project.
    for bad in [
        r#"pgr:function rail:paretoRoutes ; pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:path_id ?p ."#,
        r#"pgr:function rail:paretoRoutes ; pgr:criteria "c_speed" ; pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:path_id ?p ."#,
    ] {
        let q = format!(
            "{PREFIXES} SELECT * WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{ [] {bad} {edges} }} }}"
        );
        let failed = match e.query(&q) {
            Err(_) => true,
            Ok(QueryResults::Solutions(mut s)) => s.any(|r| r.is_err()),
            Ok(_) => false,
        };
        assert!(failed, "{bad}");
    }
}

#[test]
fn network_info() {
    let e = engine();
    e.update(
        r#"PREFIX ex: <http://example.com/> PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
        INSERT DATA { ex:tracks a pgr:Network ; pgr:edgeQuery """
          PREFIX ex: <http://example.com/>
          SELECT ?id ?source ?target ?cost ?c_gauge WHERE {
            ?id a ex:Track ; ex:from ?source ; ex:to ?target ; ex:len ?cost ; ex:gauge ?c_gauge } """ }"#,
    )
    .unwrap();
    let info = |e: &Engine| {
        let r = rows(
            e,
            r#"SELECT ?network ?vertices ?edges ?arcs ?columns ?hierarchy ?filters ?customized
                      ?build ?hierarchy_time ?cached ?persisted WHERE {
              SERVICE <http://w3id.org/tripletrail/pgr> {
                [] pgr:function pgr:networkInfo ; pgr:network ex:tracks ;
                   pgr:network ?network ; pgr:vertices ?vertices ; pgr:edges ?edges ;
                   pgr:arcs ?arcs ; pgr:columns ?columns ; pgr:hierarchy ?hierarchy ;
                   pgr:filters ?filters ; pgr:customized ?customized ; pgr:build_time ?build ;
                   pgr:hierarchy_time ?hierarchy_time ; pgr:cached ?cached ;
                   pgr:persisted ?persisted } }"#,
        );
        assert_eq!(r.len(), 1);
        r.into_iter().next().unwrap()
    };
    // Built by the call itself: a b c d e f, 7 tracks, one way each.
    let r = info(&e);
    assert_eq!(
        strs(std::slice::from_ref(&r))[0][..8],
        ["tracks", "6", "7", "7", "c_gauge", "false", "0", "0"]
    );
    assert!(r[8].as_deref().unwrap().parse::<f64>().unwrap() >= 0.0);
    assert_eq!(r[9], None);
    assert_eq!(
        (r[10].as_deref(), r[11].as_deref()),
        (Some("false"), Some("false"))
    );
    // A constrained query on the hierarchy caches a filter and customizes the hierarchy.
    let routed = rows(
        &e,
        r#"SELECT ?edge WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
            [] pgr:function rail:route ; pgr:network ex:tracks ; pgr:start_vid ex:a ;
               pgr:end_vid ex:d ; pgr:engine "ch" ; pgr:constraint "?c_gauge = 1435" ;
               pgr:edge ?edge } }"#,
    );
    assert!(!routed.is_empty());
    let r = info(&e);
    assert_eq!(r[5].as_deref(), Some("true"));
    assert_eq!((r[6].as_deref(), r[7].as_deref()), (Some("1"), Some("1")));
    assert!(r[9].as_deref().unwrap().parse::<f64>().unwrap() >= 0.0);
    assert_eq!(r[10].as_deref(), Some("true"));
    // A network the cache keeps on disk is reported as persisted.
    let dir = tripletrail_testdir::tempdir("network-info");
    let d = Engine::with_network_dir(e.store().clone(), &dir).unwrap();
    d.update(
        r#"PREFIX ex: <http://example.com/> PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
        INSERT DATA { ex:plain a pgr:Network ; pgr:edgeQuery """
          PREFIX ex: <http://example.com/>
          SELECT ?id ?source ?target ?cost WHERE {
            ?id a ex:Track ; ex:from ?source ; ex:to ?target ; ex:len ?cost } """ }"#,
    )
    .unwrap();
    let r = rows(
        &d,
        r#"SELECT ?persisted WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
            [] pgr:function pgr:networkInfo ; pgr:network ex:plain ; pgr:persisted ?persisted } }"#,
    );
    assert_eq!(strs(&r), [["true"]]);
}
