//! End-to-end tests of SHACL-SPARQL constraints and SHACL-AF rules as edge filters
//! (`pgr:shapes`), and of `rail:shaclRules`: trucks under bridges whose clearance, less a
//! safety margin set by the bridge's owner, must be above the truck.

use std::time::Duration;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::{NamedNode, Term};
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX osm: <https://wiki.openstreetmap.org/wiki/Key:>
PREFIX sh: <http://www.w3.org/ns/shacl#>
PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
PREFIX rail: <http://w3id.org/tripletrail/rail#>
"#;

/// Roads, usable both ways:
///
/// ```text
///   A -s1(1)- B -s2(1)- C -s3(1)- D      s2 passes under a 4.5 m bridge
///             B -s4(2)- E -s5(2)- C
///   A -s6(4)- F -s7(4)- D
/// ```
const ROADS: &str = r#"
@prefix ex: <http://example.com/> .
@prefix osm: <https://wiki.openstreetmap.org/wiki/Key:> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
ex:s1 a ex:Road ; ex:from ex:A ; ex:to ex:B ; ex:len 1 ; ex:geom "POINT(1 1)"^^geo:wktLiteral .
ex:s2 a ex:Road ; ex:from ex:B ; ex:to ex:C ; ex:len 1 ; ex:passesUnder ex:bridge .
ex:s3 a ex:Road ; ex:from ex:C ; ex:to ex:D ; ex:len 1 .
ex:s4 a ex:Road ; ex:from ex:B ; ex:to ex:E ; ex:len 2 .
ex:s5 a ex:Road ; ex:from ex:E ; ex:to ex:C ; ex:len 2 .
ex:s6 a ex:Road ; ex:from ex:A ; ex:to ex:F ; ex:len 4 ; ex:geom "POINT(5 5)"^^geo:wktLiteral .
ex:s7 a ex:Road ; ex:from ex:F ; ex:to ex:D ; ex:len 4 .
ex:bridge osm:maxheight 4.5 ; ex:owner ex:agency .
ex:agency ex:safetyMargin 0.3 .
ex:tall ex:height 4.0 .
"#;

const DECLARE: &str = r#"
@prefix ex: <http://example.com/> .
@prefix osm: <https://wiki.openstreetmap.org/wiki/Key:> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:prefixes sh:declare
    [ sh:prefix "ex" ; sh:namespace "http://example.com/"^^xsd:anyURI ] ,
    [ sh:prefix "osm" ; sh:namespace "https://wiki.openstreetmap.org/wiki/Key:"^^xsd:anyURI ] .
"#;

/// A SPARQL-based constraint: the clearance less the owner's margin must be above 4 m.
const SPARQL_SHAPES: &str = r#"
ex:Clearance a sh:NodeShape ; sh:targetClass ex:Road ;
    sh:sparql [ sh:prefixes ex:prefixes ; sh:select """
        SELECT $this ?bridge WHERE {
            $this ex:passesUnder ?bridge .
            ?bridge osm:maxheight ?h ; ex:owner/ex:safetyMargin ?m .
            FILTER (?h - ?m <= 4.0)
        }""" ] .
"#;

/// A SPARQL-based constraint component with the vehicle as its parameter.
const COMPONENT_SHAPES: &str = r#"
ex:ClearanceFor a sh:ConstraintComponent ;
    sh:parameter [ sh:path ex:vehicle ] ;
    sh:nodeValidator [ a sh:SPARQLSelectValidator ; sh:prefixes ex:prefixes ; sh:select """
        SELECT $this WHERE {
            $this ex:passesUnder ?bridge .
            ?bridge osm:maxheight ?h ; ex:owner/ex:safetyMargin ?m .
            $vehicle ex:height ?vh .
            FILTER (?h - ?m <= ?vh)
        }""" ] .
ex:ForTall a sh:NodeShape ; sh:targetClass ex:Road ; ex:vehicle ex:tall .
"#;

/// Rules: a triple rule copies the bridge's clearance onto the road, then a SPARQL rule marks
/// roads too low (clearance less margin); a Core constraint keeps the marked roads out.
const RULE_SHAPES: &str = r#"
ex:Derive a sh:NodeShape ; sh:targetClass ex:Road ;
    sh:rule [ a sh:SPARQLRule ; sh:order 1 ; sh:prefixes ex:prefixes ; sh:construct """
        CONSTRUCT { $this ex:tooLow true } WHERE {
            $this ex:clearance ?c ; ex:passesUnder/ex:owner/ex:safetyMargin ?m .
            FILTER (?c - ?m <= 4.0)
        }""" ] ;
    sh:rule [ a sh:TripleRule ; sh:order 0 ; sh:subject sh:this ; sh:predicate ex:clearance ;
        sh:object [ sh:path ( ex:passesUnder osm:maxheight ) ] ] ;
    sh:property [ sh:path ex:tooLow ; sh:maxCount 0 ] .
"#;

/// A geo function in a SPARQL constraint: roads with a geometry must lie in a zone.
const ZONE_SHAPES: &str = r#"
ex:Zone a sh:NodeShape ; sh:targetClass ex:Road ;
    sh:sparql [ sh:select """
        PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
        SELECT $this WHERE {
            $this <http://example.com/geom> ?g .
            FILTER (!geof:sfWithin(?g, "POLYGON((2 2, 10 2, 10 10, 2 10, 2 2))"^^geo:wktLiteral))
        }""" ] .
"#;

fn graph_iri(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{local}"))
}

fn engine() -> Engine {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, ROADS.as_bytes()).unwrap();
    for (g, ttl) in [
        ("sparqlShapes", SPARQL_SHAPES),
        ("componentShapes", COMPONENT_SHAPES),
        ("ruleShapes", RULE_SHAPES),
        ("zoneShapes", ZONE_SHAPES),
    ] {
        e.load_into(
            graph_iri(g),
            RdfFormat::Turtle,
            format!("{DECLARE}{ttl}").as_bytes(),
        )
        .unwrap();
    }
    e
}

fn rows(e: &Engine, q: &str) -> Result<Vec<Vec<Option<String>>>, String> {
    let QueryResults::Solutions(s) = e
        .query(&format!("{PREFIXES}{q}"))
        .map_err(|err| err.to_string())?
    else {
        panic!()
    };
    let vars = s.variables().to_vec();
    s.map(|r| {
        let r = r.map_err(|err| err.to_string())?;
        Ok(vars
            .iter()
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
            .collect())
    })
    .collect()
}

/// The edges of the route from A to D, under `params`.
fn drive(e: &Engine, params: &str) -> Vec<String> {
    let q = format!(
        r#"SELECT ?edge WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function rail:route ; pgr:start_vid ex:A ; pgr:end_vid ex:D ; {params}
           pgr:seq ?seq ; pgr:edge ?edge .
        {{ SELECT ?id ?source ?target ?cost ?reverse_cost WHERE {{
            ?id a ex:Road ; ex:from ?source ; ex:to ?target ; ex:len ?cost .
            BIND(?cost AS ?reverse_cost) }} }} }} }} ORDER BY ?seq"#
    );
    rows(e, &q)
        .unwrap_or_else(|err| panic!("{err}\n{q}"))
        .into_iter()
        .filter_map(|r| r[0].clone())
        .collect()
}

/// The owner raises its safety margin: 4.5 - 0.6 is below 4 m.
fn raise_margin(e: &Engine) {
    e.update(&format!(
        "{PREFIXES} DELETE DATA {{ ex:agency ex:safetyMargin 0.3 }} ; INSERT DATA {{ ex:agency ex:safetyMargin 0.6 }}"
    ))
    .unwrap();
}

#[test]
fn sparql_constraints_filter_edges() {
    for shapes in ["sparqlShapes", "componentShapes"] {
        let e = engine();
        let p = format!("pgr:shapes ex:{shapes} ;");
        // 4.5 - 0.3 = 4.2 m: the truck fits under the bridge.
        assert_eq!(drive(&e, &p), ["s1", "s2", "s3"], "{shapes}");
        // Cached.
        assert_eq!(drive(&e, &p), ["s1", "s2", "s3"], "{shapes}");
        assert!(e.last_routing_stats().cache_hit);
        assert_eq!(e.last_routing_stats().constraint, Duration::ZERO);
        // The margin only appears in the query, yet changing it drops the filtered graph.
        raise_margin(&e);
        assert_eq!(drive(&e, &p), ["s1", "s4", "s5", "s3"], "{shapes}");
        assert!(e.last_routing_stats().constraint > Duration::ZERO);
    }
    // With the vehicle lower, the component lets it under again.
    let e = engine();
    raise_margin(&e);
    e.update(&format!(
        "{PREFIXES} DELETE DATA {{ ex:tall ex:height 4.0 }} ; INSERT DATA {{ ex:tall ex:height 3.5 }}"
    ))
    .unwrap();
    assert_eq!(
        drive(&e, "pgr:shapes ex:componentShapes ;"),
        ["s1", "s2", "s3"]
    );
}

#[test]
fn sparql_constraints_call_geo_functions() {
    let e = engine();
    // s1 lies outside the zone: the long way round.
    assert_eq!(drive(&e, "pgr:shapes ex:zoneShapes ;"), ["s6", "s7"]);
}

#[test]
fn rules_derive_values_for_constraints() {
    let e = engine();
    let p = "pgr:shapes ex:ruleShapes ;";
    assert_eq!(drive(&e, p), ["s1", "s2", "s3"]);
    raise_margin(&e);
    assert_eq!(drive(&e, p), ["s1", "s4", "s5", "s3"]);
    // The inferred triples stayed in memory.
    let QueryResults::Boolean(found) = e
        .query(&format!(
            "{PREFIXES} ASK {{ {{ ?s ex:clearance ?c }} UNION {{ GRAPH ?g {{ ?s ex:clearance ?c }} }} }}"
        ))
        .unwrap()
    else {
        panic!()
    };
    assert!(!found);
}

#[test]
fn rules_can_be_run_and_stored() {
    let e = engine();
    raise_margin(&e);
    let q = r#"SELECT ?s ?p ?o WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
        [] pgr:function rail:shaclRules ; pgr:shapes ex:ruleShapes ;
           pgr:subject ?s ; pgr:predicate ?p ; pgr:object ?o . } } ORDER BY ?p"#;
    let triples = rows(&e, q).unwrap();
    assert_eq!(triples.len(), 2, "{triples:?}");
    assert_eq!(triples[0][0].as_deref(), Some("s2"));
    assert_eq!(triples[0][1].as_deref(), Some("clearance"));
    assert_eq!(triples[1][1].as_deref(), Some("tooLow"));
    assert_eq!(triples[1][2].as_deref(), Some("true"));
    e.update(&format!(
        r#"{PREFIXES} INSERT {{ GRAPH ex:inferred {{ ?s ?p ?o }} }} WHERE {{
        SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function rail:shaclRules ; pgr:shapes ex:ruleShapes ;
           pgr:subject ?s ; pgr:predicate ?p ; pgr:object ?o . }} }}"#
    ))
    .unwrap();
    let stored = rows(&e, "SELECT * WHERE { GRAPH ex:inferred { ?s ?p ?o } }").unwrap();
    assert_eq!(stored.len(), 2);
    // Errors: no shapes graph, or a graph without rules.
    let run = |params: &str| {
        rows(
            &e,
            &format!(
                r#"SELECT ?s WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
                [] pgr:function rail:shaclRules ; {params} pgr:subject ?s . }} }}"#
            ),
        )
    };
    assert!(run("").is_err());
    assert!(run("pgr:shapes ex:sparqlShapes ;").is_err());
}

/// Timings on a generated grid (`cargo test --release -p tripletrail --test shacl_sparql --
/// --ignored --nocapture`, `GRID=400` by default: 319,200 edges, every seventh under a
/// bridge): the same height check as a Core shape, a SPARQL constraint and a rule.
#[test]
#[ignore]
fn sparql_on_a_large_grid() {
    use std::fmt::Write;
    let n: usize = std::env::var("GRID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(400);
    let e = Engine::new().unwrap();
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix osm: <https://wiki.openstreetmap.org/wiki/Key:> .\n",
    );
    let mut edges = 0;
    for y in 0..n {
        for x in 0..n {
            for (dx, dy, dir) in [(1, 0, "h"), (0, 1, "v")] {
                if x + dx >= n || y + dy >= n {
                    continue;
                }
                write!(
                    ttl,
                    "ex:r{x}_{y}{dir} ex:from ex:v{x}_{y} ; ex:to ex:v{}_{} ; ex:len {} ",
                    x + dx,
                    y + dy,
                    1 + (x * 7 + y * 13) % 5
                )
                .unwrap();
                if edges % 7 == 1 {
                    write!(
                        ttl,
                        "; ex:passesUnder ex:b{edges} . ex:b{edges} osm:maxheight {}.{} ",
                        3 + edges % 2,
                        edges % 10
                    )
                    .unwrap();
                }
                ttl.push_str(".\n");
                edges += 1;
            }
        }
    }
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let shapes = [
        (
            "core",
            r#"ex:S sh:targetSubjectsOf ex:from ;
                sh:property [ sh:path ( ex:passesUnder osm:maxheight ) ; sh:minExclusive 3.6 ] ."#,
        ),
        (
            "sparql",
            r#"ex:S sh:targetSubjectsOf ex:from ; sh:sparql [ sh:prefixes ex:prefixes ; sh:select """
                SELECT $this WHERE { $this ex:passesUnder/osm:maxheight ?h FILTER (?h <= 3.6) }""" ] ."#,
        ),
        (
            "rule",
            r#"ex:S sh:targetSubjectsOf ex:passesUnder ;
                sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate ex:clearance ;
                    sh:object [ sh:path ( ex:passesUnder osm:maxheight ) ] ] .
            ex:T sh:targetSubjectsOf ex:from ;
                sh:property [ sh:path ex:clearance ; sh:minExclusive 3.6 ] ."#,
        ),
    ];
    let edge_set = r#"{ SELECT ?id ?source ?target ?cost WHERE {
        ?id ex:from ?source ; ex:to ?target ; ex:len ?cost } }"#;
    let q = |params: &str| {
        format!(
            r#"SELECT ?edge WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
            [] pgr:function rail:route ; pgr:start_vid ex:v0_0 ; pgr:end_vid ex:v{m}_{m} ; pgr:directed false ;
               {params} pgr:edge ?edge . {edge_set} }} }}"#,
            m = n - 1
        )
    };
    println!("{edges} edges, store {}", e.store().kind());
    rows(&e, &q("")).unwrap();
    let mut routes = Vec::new();
    for (name, ttl) in shapes {
        e.load_into(
            graph_iri(name),
            RdfFormat::Turtle,
            format!("{DECLARE}{ttl}").as_bytes(),
        )
        .unwrap();
        let params = format!("pgr:shapes ex:{name} ;");
        let r = rows(&e, &q(&params)).unwrap();
        let s = e.last_routing_stats();
        println!(
            "{name}: {} rows, filter {:?} ({:.2} µs per edge)",
            r.len(),
            s.constraint,
            s.constraint.as_secs_f64() * 1e6 / edges as f64
        );
        routes.push(r);
    }
    assert!(routes.windows(2).all(|w| w[0] == w[1]));
}

#[test]
fn failing_queries_are_errors() {
    let e = engine();
    e.load_into(
        graph_iri("badShapes"),
        RdfFormat::Turtle,
        r#"@prefix sh: <http://www.w3.org/ns/shacl#> .
        <http://example.com/Bad> sh:targetClass <http://example.com/Road> ;
            sh:sparql [ sh:select "SELECT $this WHERE { $this <http://example.com/len> ?l FILTER (?l > " ] ."#
            .as_bytes(),
    )
    .unwrap();
    let q = r#"SELECT ?edge WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
        [] pgr:function rail:route ; pgr:start_vid ex:A ; pgr:end_vid ex:D ; pgr:shapes ex:badShapes ;
           pgr:edge ?edge .
        { SELECT ?id ?source ?target ?cost WHERE { ?id ex:from ?source ; ex:to ?target ; ex:len ?cost } } } }"#;
    assert!(rows(&e, q).is_err());
}
