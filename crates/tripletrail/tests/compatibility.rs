//! End-to-end tests of SHACL edge filters (`pgr:shapes`) and vehicle compatibility rules
//! (`pgr:vehicle`, `pgr:rules`): trucks on roads with weight, height and length limits,
//! hand-written shapes on tracks, and the RINF profile on a tiny ERA-shaped network.

use std::time::Duration;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::{NamedNode, Term};
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX osm: <https://wiki.openstreetmap.org/wiki/Key:>
PREFIX era: <http://data.europa.eu/949/>
PREFIX sh: <http://www.w3.org/ns/shacl#>
PREFIX compat: <http://w3id.org/tripletrail/compat#>
PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
PREFIX rail: <http://w3id.org/tripletrail/rail#>
"#;

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

fn graph_iri(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://example.com/{local}"))
}

// ---- trucks on roads ----

/// Roads, usable both ways:
///
/// ```text
///   A -s1(1)- B -s2(1)- C -s3(1)- D      s2 passes under a 3.8 m bridge
///             B -s4(2)- E -s5(2)- C      s4: 12 t limit; s5: 15 m limit
///   A -s6(4)- F -s7(4)- D                no limits
/// ```
const ROADS: &str = r#"
@prefix ex: <http://example.com/> .
@prefix osm: <https://wiki.openstreetmap.org/wiki/Key:> .
ex:s1 a ex:Road ; ex:from ex:A ; ex:to ex:B ; ex:len 1 .
ex:s2 a ex:Road ; ex:from ex:B ; ex:to ex:C ; ex:len 1 ; ex:passesUnder ex:lowBridge .
ex:s3 a ex:Road ; ex:from ex:C ; ex:to ex:D ; ex:len 1 ; osm:maxheight 4.5 .
ex:s4 a ex:Road ; ex:from ex:B ; ex:to ex:E ; ex:len 2 ; osm:maxweight 12 .
ex:s5 a ex:Road ; ex:from ex:E ; ex:to ex:C ; ex:len 2 ; osm:maxlength 15 .
ex:s6 a ex:Road ; ex:from ex:A ; ex:to ex:F ; ex:len 4 .
ex:s7 a ex:Road ; ex:from ex:F ; ex:to ex:D ; ex:len 4 .
ex:lowBridge a ex:Bridge ; osm:maxheight 3.8 .

# Vehicles: weight (t), height and length (m).
ex:car   ex:weight 1.5 ; ex:height 1.6 ; ex:length 4.5 .
ex:low   ex:weight 30 ; ex:height 3.5 ; ex:length 12 .
ex:tall  ex:weight 10 ; ex:height 4.0 ; ex:length 10 .
ex:heavy ex:weight 30 ; ex:height 4.0 ; ex:length 10 .
ex:long  ex:weight 10 ; ex:height 4.0 ; ex:length 18 .
"#;

/// The rules: limits on the road itself or, for the height, on a bridge it passes under.
const ROAD_RULES: &str = r#"
@prefix ex: <http://example.com/> .
@prefix osm: <https://wiki.openstreetmap.org/wiki/Key:> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix compat: <http://w3id.org/tripletrail/compat#> .
ex:weightRule a compat:Rule ; compat:vehiclePath ex:weight ; compat:edgePath osm:maxweight ;
    compat:constraint sh:minInclusive .
ex:heightRule a compat:Rule ; compat:vehiclePath ex:height ;
    compat:edgePath [ sh:alternativePath ( osm:maxheight ( ex:passesUnder osm:maxheight ) ) ] ;
    compat:constraint sh:minInclusive .
ex:lengthRule a compat:Rule ; compat:vehiclePath ex:length ; compat:edgePath osm:maxlength ;
    compat:constraint sh:minInclusive .
"#;

const ROAD_EDGES: &str = r#"
{ SELECT ?id ?source ?target ?cost ?reverse_cost WHERE {
    ?id a ex:Road ; ex:from ?source ; ex:to ?target ; ex:len ?cost .
    BIND(?cost AS ?reverse_cost) } }"#;

fn road_engine() -> Engine {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, ROADS.as_bytes()).unwrap();
    e.load_into(
        graph_iri("roadRules"),
        RdfFormat::Turtle,
        ROAD_RULES.as_bytes(),
    )
    .unwrap();
    e
}

/// The edges of the route from A to D, under `params`.
fn drive(e: &Engine, params: &str) -> Vec<String> {
    let q = format!(
        r#"SELECT ?edge WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function rail:route ; pgr:start_vid ex:A ; pgr:end_vid ex:D ; {params}
           pgr:seq ?seq ; pgr:edge ?edge .
        {ROAD_EDGES} }} }} ORDER BY ?seq"#
    );
    rows(e, &q)
        .unwrap_or_else(|err| panic!("{err}\n{q}"))
        .into_iter()
        .filter_map(|r| r[0].clone())
        .collect()
}

fn vehicle(v: &str) -> String {
    format!("pgr:vehicle ex:{v} ; pgr:rules ex:roadRules ;")
}

#[test]
fn trucks_take_compatible_roads() {
    let e = road_engine();
    for engine in ["dijkstra", "ch"] {
        let route = |v: &str| drive(&e, &format!("{} pgr:engine \"{engine}\" ;", vehicle(v)));
        // Fits everywhere: the short way.
        assert_eq!(route("car"), ["s1", "s2", "s3"], "{engine}");
        // 30 t but low: under the bridge, which has no weight limit.
        assert_eq!(route("low"), ["s1", "s2", "s3"], "{engine}");
        // 4 m is over the 3.8 m bridge: around it, through E.
        assert_eq!(route("tall"), ["s1", "s4", "s5", "s3"], "{engine}");
        // Tall and 30 t: s4's 12 t limit too, so the long way.
        assert_eq!(route("heavy"), ["s6", "s7"], "{engine}");
        // Tall and 18 m: s5's 15 m limit, so the long way.
        assert_eq!(route("long"), ["s6", "s7"], "{engine}");
    }
    // An unknown vehicle has no values: no rule is checked.
    assert_eq!(drive(&e, &vehicle("nobody")), ["s1", "s2", "s3"]);
}

#[test]
fn compatibility_filters_are_cached_and_invalidated() {
    let e = road_engine();
    assert_eq!(drive(&e, &vehicle("tall")), ["s1", "s4", "s5", "s3"]);
    let first = e.last_routing_stats();
    assert!(first.constraint > Duration::ZERO);
    // Again: the filtered graph is cached with the network.
    assert_eq!(drive(&e, &vehicle("tall")), ["s1", "s4", "s5", "s3"]);
    let stats = e.last_routing_stats();
    assert!(stats.cache_hit);
    assert_eq!(stats.constraint, Duration::ZERO);

    // The vehicle changes: it is 3.6 m now and fits under the bridge.
    e.update(&format!(
        "{PREFIXES} DELETE DATA {{ ex:tall ex:height 4.0 }} ; INSERT DATA {{ ex:tall ex:height 3.6 }}"
    ))
    .unwrap();
    assert_eq!(drive(&e, &vehicle("tall")), ["s1", "s2", "s3"]);
    // The data changes: the bridge is lower.
    e.update(&format!(
        "{PREFIXES} DELETE DATA {{ ex:lowBridge osm:maxheight 3.8 }} ; INSERT DATA {{ ex:lowBridge osm:maxheight 3.5 }}"
    ))
    .unwrap();
    assert_eq!(drive(&e, &vehicle("tall")), ["s1", "s4", "s5", "s3"]);
    // The rules change: no height rule.
    e.update(&format!(
        "{PREFIXES} DELETE WHERE {{ GRAPH ex:roadRules {{ ex:heightRule ?p ?o }} }}"
    ))
    .unwrap();
    assert_eq!(drive(&e, &vehicle("tall")), ["s1", "s2", "s3"]);
}

#[test]
fn compiled_shapes_can_be_materialised() {
    let e = road_engine();
    let shapes = r#"SELECT ?s ?p ?o WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
        [] pgr:function rail:compatibilityShapes ; pgr:vehicle ex:heavy ; pgr:rules ex:roadRules ;
           pgr:shape ex:HeavyShape ; pgr:subject ?s ; pgr:predicate ?p ; pgr:object ?o . } }"#;
    let triples = rows(&e, shapes).unwrap();
    let has = |p: &str, o: &str| {
        triples
            .iter()
            .any(|t| t[1].as_deref() == Some(p) && t[2].as_deref() == Some(o))
    };
    assert!(has("http://www.w3.org/ns/shacl#minInclusive", "30"));
    // (4.0 as written, or as Oxigraph's store writes the decimal)
    assert!(
        has("http://www.w3.org/ns/shacl#minInclusive", "4.0")
            || has("http://www.w3.org/ns/shacl#minInclusive", "4")
    );
    assert!(has(
        "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
        "http://www.w3.org/ns/shacl#NodeShape"
    ));
    // Into a named graph, then used as plain shapes: the same route.
    e.update(&format!(
        r#"{PREFIXES} INSERT {{ GRAPH ex:heavyShapes {{ ?s ?p ?o }} }} WHERE {{
        SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function rail:compatibilityShapes ; pgr:vehicle ex:heavy ; pgr:rules ex:roadRules ;
           pgr:shape ex:HeavyShape ; pgr:subject ?s ; pgr:predicate ?p ; pgr:object ?o . }} }}"#
    ))
    .unwrap();
    assert_eq!(
        drive(&e, "pgr:shapes ex:heavyShapes ; pgr:shape ex:HeavyShape ;"),
        ["s6", "s7"]
    );
}

/// Weight classes, ordered by a rank (the tonnes each allows), named on the roads; a vehicle
/// gives its class, or its weight.
const CLASSES: &str = r#"
@prefix ex: <http://example.com/> .
ex:s2 ex:weightClass ex:W7_5 .
ex:s5 ex:weightClass ex:W12 .
ex:van ex:weightClass ex:W3_5 .
ex:lorry ex:weightClass ex:W12 .
"#;

const CLASS_RULES: &str = r#"
@prefix ex: <http://example.com/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix compat: <http://w3id.org/tripletrail/compat#> .
ex:W3_5 ex:tonnes 3.5 . ex:W7_5 ex:tonnes 7.5 . ex:W12 ex:tonnes 12 . ex:W40 ex:tonnes 40 .
ex:classRule a compat:Rule ; compat:vehiclePath ex:weightClass ; compat:edgePath ex:weightClass ;
    compat:constraint sh:minInclusive ; compat:rank ex:tonnes .
ex:weightClassRule a compat:Rule ; compat:vehiclePath ex:weight ; compat:edgePath ex:weightClass ;
    compat:constraint sh:minInclusive ; compat:rank ex:tonnes .
"#;

#[test]
fn weight_classes_are_ordered() {
    let e = road_engine();
    e.load(RdfFormat::Turtle, CLASSES.as_bytes()).unwrap();
    e.load_into(
        graph_iri("classRules"),
        RdfFormat::Turtle,
        CLASS_RULES.as_bytes(),
    )
    .unwrap();
    let route = |v: &str| {
        drive(
            &e,
            &format!("pgr:vehicle ex:{v} ; pgr:rules ex:classRules ;"),
        )
    };
    // 3.5 t class: s2's 7.5 t class is above it.
    assert_eq!(route("van"), ["s1", "s2", "s3"]);
    // 12 t class: not on s2, s5's 12 t class is enough.
    assert_eq!(route("lorry"), ["s1", "s4", "s5", "s3"]);
    // 30 t by weight against the classes' tonnes: neither s2 nor s5.
    assert_eq!(route("heavy"), ["s6", "s7"]);
}

// ---- hand-written shapes ----

/// Tracks, usable both ways (as in the rail routing tests):
///
/// ```text
///   a -t1- b -t2- c -t3- d      (1 each; t3 not electrified)
///         b -t4- e -t5- d       (2 each)
///   a -t6- d                    (10, broad gauge)
/// ```
const TRACKS: &str = r#"
@prefix ex: <http://example.com/> .
ex:t1 a ex:Track ; ex:from ex:a ; ex:to ex:b ; ex:len 1 ; ex:gauge 1435 ; ex:electrified true .
ex:t2 a ex:Track ; ex:from ex:b ; ex:to ex:c ; ex:len 1 ; ex:gauge 1435 ; ex:electrified true .
ex:t3 a ex:Track ; ex:from ex:c ; ex:to ex:d ; ex:len 1 ; ex:gauge 1435 .
ex:t4 a ex:Track ; ex:from ex:b ; ex:to ex:e ; ex:len 2 ; ex:gauge 1435 ; ex:electrified true .
ex:t5 a ex:MainTrack ; ex:from ex:e ; ex:to ex:d ; ex:len 2 ; ex:gauge 1435 ; ex:electrified true .
ex:t6 a ex:Track ; ex:from ex:a ; ex:to ex:d ; ex:len 10 ; ex:gauge 1668 ; ex:electrified true .
ex:MainTrack <http://www.w3.org/2000/01/rdf-schema#subClassOf> ex:Track .
"#;

const SHAPES: &str = r#"
@prefix ex: <http://example.com/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
# Every track (t5 through its subclass): standard gauge, electrified.
ex:Electric a sh:NodeShape ; sh:targetClass ex:Track ;
    sh:property [ sh:path ex:gauge ; sh:hasValue 1435 ] ;
    sh:property [ sh:path ex:electrified ; sh:minCount 1 ; sh:in ( true ) ] .
# Without targets: only used when named with pgr:shape.
ex:Broad a sh:NodeShape ; sh:property [ sh:path ex:gauge ; sh:minInclusive 1500 ] .
"#;

fn track_engine() -> Engine {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, TRACKS.as_bytes()).unwrap();
    e.load_into(graph_iri("shapes"), RdfFormat::Turtle, SHAPES.as_bytes())
        .unwrap();
    e
}

/// The edges of the route from a to d, under `params`; `edges` is the edge sub-select.
fn ride(e: &Engine, params: &str, edges: &str) -> Result<Vec<String>, String> {
    Ok(rows(
        e,
        &format!(
            r#"SELECT ?edge WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function rail:route ; pgr:start_vid ex:a ; pgr:end_vid ex:d ; {params}
           pgr:seq ?seq ; pgr:edge ?edge .
        {edges} }} }} ORDER BY ?seq"#
        ),
    )?
    .into_iter()
    .filter_map(|r| r[0].clone())
    .collect())
}

const TRACK_EDGES: &str = r#"
{ SELECT ?id ?source ?target ?cost ?reverse_cost WHERE {
    ?id ex:from ?source ; ex:to ?target ; ex:len ?cost . BIND(?cost AS ?reverse_cost) } }"#;

/// Edges named apart from their tracks, with the track as a column.
const SEGMENT_EDGES: &str = r#"
{ SELECT ?id ?source ?target ?cost ?reverse_cost ?c_track WHERE {
    ?c_track ex:from ?source ; ex:to ?target ; ex:len ?cost . BIND(?cost AS ?reverse_cost)
    BIND(IRI(CONCAT(STR(?c_track), "-seg")) AS ?id) } }"#;

#[test]
fn shapes_filter_edges() {
    let e = track_engine();
    assert_eq!(ride(&e, "", TRACK_EDGES).unwrap(), ["t1", "t2", "t3"]);
    for engine in ["dijkstra", "ch"] {
        // By the shapes' targets: t3 is not electrified, t6 is broad gauge.
        let p = format!("pgr:shapes ex:shapes ; pgr:engine \"{engine}\" ;");
        assert_eq!(ride(&e, &p, TRACK_EDGES).unwrap(), ["t1", "t4", "t5"]);
        // A named shape, whatever its targets: only t6 is broad.
        let p = format!("pgr:shapes ex:shapes ; pgr:shape ex:Broad ; pgr:engine \"{engine}\" ;");
        assert_eq!(ride(&e, &p, TRACK_EDGES).unwrap(), ["t6"]);
    }
    // The focus node is a column: the same filter on differently named edges.
    assert_eq!(
        ride(
            &e,
            r#"pgr:shapes ex:shapes ; pgr:focus "c_track" ;"#,
            SEGMENT_EDGES
        )
        .unwrap(),
        ["t1-seg", "t4-seg", "t5-seg"]
    );
    // With ?id as the focus, the segments are no shape's target: nothing is filtered.
    assert_eq!(
        ride(&e, "pgr:shapes ex:shapes ;", SEGMENT_EDGES).unwrap(),
        ["t1-seg", "t2-seg", "t3-seg"]
    );
    // With a constraint as well, both apply.
    assert!(
        ride(
            &e,
            r#"pgr:shapes ex:shapes ; pgr:shape ex:Broad ; pgr:constraint "false" ;"#,
            TRACK_EDGES
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn shapes_follow_the_shapes_graph() {
    let e = track_engine();
    let p = "pgr:shapes ex:shapes ;";
    assert_eq!(ride(&e, p, TRACK_EDGES).unwrap(), ["t1", "t4", "t5"]);
    // A new constraint in the shapes graph: t4 is too long.
    e.update(&format!(
        "{PREFIXES} INSERT DATA {{ GRAPH ex:shapes {{ ex:Electric sh:property [ sh:path ex:len ; sh:maxInclusive 1 ] }} }}"
    ))
    .unwrap();
    assert!(ride(&e, p, TRACK_EDGES).unwrap().is_empty());
    // The data changes: t3 is electrified now.
    e.update(&format!(
        "{PREFIXES} INSERT DATA {{ ex:t3 ex:electrified true }}"
    ))
    .unwrap();
    assert_eq!(ride(&e, p, TRACK_EDGES).unwrap(), ["t1", "t2", "t3"]);
}

#[test]
fn shape_errors() {
    let e = track_engine();
    let fails = |params: &str, edges: &str| ride(&e, params, edges).is_err();
    assert!(fails(
        r#"pgr:shapes ex:shapes ; pgr:focus "c_nothing" ;"#,
        TRACK_EDGES
    ));
    assert!(fails("pgr:shapes ex:noSuchGraph ;", TRACK_EDGES));
    assert!(fails(
        "pgr:shapes ex:shapes ; pgr:shape ex:NoSuchShape ;",
        TRACK_EDGES
    ));
    assert!(fails("pgr:shape ex:Broad ;", TRACK_EDGES));
    assert!(fails("pgr:vehicle ex:car ;", TRACK_EDGES));
    assert!(fails(
        "pgr:vehicle ex:car ; pgr:rules ex:noSuchGraph ;",
        TRACK_EDGES
    ));
}

// ---- RINF ----

/// Sections of line between operational points, each with one running track:
///
/// ```text
///   A -AB(10)- B -BC(10)- C -CD(5)- D      AB, BC: 1435 mm, 25 kV 50 Hz; CD: not electrified
///   A -AC(15)- C                           1435 mm, 3 kV DC
///   C -CE(5)- E                            1668 mm, 3 kV DC
/// ```
///
/// The gauge and energy supply concepts stand for those of the ERA vocabularies.
const RINF: &str = r#"
@prefix era: <http://data.europa.eu/949/> .
@prefix ex: <http://example.com/> .
@prefix gauge: <http://data.europa.eu/949/concepts/nominal-track-gauges/> .
@prefix ess: <http://data.europa.eu/949/concepts/energy-supply-systems/> .
@prefix cls: <http://data.europa.eu/949/concepts/contact-line-systems/> .
@prefix lclc: <http://data.europa.eu/949/concepts/load-capability-line-categories/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:AB a era:SectionOfLine ; era:opStart ex:A ; era:opEnd ex:B ; era:lengthOfSectionOfLine 10.0 .
ex:BC a era:SectionOfLine ; era:opStart ex:B ; era:opEnd ex:C ; era:lengthOfSectionOfLine 10.0 .
ex:CD a era:SectionOfLine ; era:opStart ex:C ; era:opEnd ex:D ; era:lengthOfSectionOfLine 5.0 .
ex:AC a era:SectionOfLine ; era:opStart ex:A ; era:opEnd ex:C ; era:lengthOfSectionOfLine 15.0 .
ex:CE a era:SectionOfLine ; era:opStart ex:C ; era:opEnd ex:E ; era:lengthOfSectionOfLine 5.0 .
ex:AB-1 a era:RunningTrack ; era:isPartOf ex:AB ; era:wheelSetGauge gauge:1435 ;
    era:contactLineSystem [ era:energySupplySystem ess:AC25kV50Hz ; era:maxTrainCurrent 500 ] .
ex:BC-1 a era:RunningTrack ; era:isPartOf ex:BC ; era:wheelSetGauge gauge:1435 ;
    era:contactLineSystem [ era:energySupplySystem ess:AC25kV50Hz ; era:maxTrainCurrent 500 ] .
ex:CD-1 a era:RunningTrack ; era:isPartOf ex:CD ; era:wheelSetGauge gauge:1435 ;
    era:contactLineSystem [ era:contactLineSystemType cls:40 ] .
ex:AC-1 a era:RunningTrack ; era:isPartOf ex:AC ; era:wheelSetGauge gauge:1435 ;
    era:contactLineSystem [ era:energySupplySystem ess:DC3kV ; era:maxTrainCurrent 300 ] .
ex:CE-1 a era:RunningTrack ; era:isPartOf ex:CE ; era:wheelSetGauge gauge:1668 ;
    era:contactLineSystem [ era:energySupplySystem ess:DC3kV ] .

# Load capabilities (EN 15528 line categories 40 = C2, 90 = D4): AC is C2; AB is D4 up to
# 100 km/h and C2 up to 120 km/h; BC is D4 at any speed; CD has none.
ex:AC-1 era:trackLoadCapability [ era:loadCapabilityLineCategory lclc:40 ; era:loadCapabilitySpeed 100 ] .
ex:AB-1 era:trackLoadCapability [ era:loadCapabilityLineCategory lclc:90 ; era:loadCapabilitySpeed 100 ] ,
    [ era:loadCapabilityLineCategory lclc:40 ; era:loadCapabilitySpeed 120 ] .
ex:BC-1 era:trackLoadCapability [ era:loadCapabilityLineCategory lclc:90 ] .

# Vehicle types.
ex:ac a era:VehicleType ; era:wheelSetGauge gauge:1435 ; era:energySupplySystem ess:AC25kV50Hz .
ex:dc a era:VehicleType ; era:wheelSetGauge gauge:1435 ; era:energySupplySystem ess:DC3kV .
ex:dcHungry a era:VehicleType ; era:wheelSetGauge gauge:1435 ; era:energySupplySystem ess:DC3kV ;
    era:configDependentParameter [ era:catenaryMaxRatedCurrent 400 ] .
ex:biMode a era:VehicleType ; era:wheelSetGauge gauge:1435 ;
    era:energySupplySystem ess:AC25kV50Hz, cls:40 .
ex:diesel a era:VehicleType ; era:wheelSetGauge gauge:1435 .
ex:iberian a era:VehicleType ; era:wheelSetGauge gauge:1668 ; era:energySupplySystem ess:DC3kV .
ex:dualGauge a era:VehicleType ; era:wheelSetGauge gauge:1435, gauge:1668 ;
    era:energySupplySystem ess:DC3kV .
# Diesel freight vehicle types by line category (20 = B1, 90 = D4), axle load (kg), speed.
ex:dieselB1 a era:VehicleType ; era:wheelSetGauge gauge:1435 ; era:compatibleWithLineCategory lclc:20 .
ex:dieselD4 a era:VehicleType ; era:wheelSetGauge gauge:1435 ; era:compatibleWithLineCategory lclc:90 .
ex:dieselD4at100 a era:VehicleType ; era:wheelSetGauge gauge:1435 ;
    era:compatibleWithLineCategory lclc:90 ;
    era:vehicleTypeMaximumSpeedAndCantDeficiency [ era:vehicleTypeMaximumSpeed 100 ] .
ex:dieselD4at120 a era:VehicleType ; era:wheelSetGauge gauge:1435 ;
    era:compatibleWithLineCategory lclc:90 ;
    era:vehicleTypeMaximumSpeedAndCantDeficiency [ era:vehicleTypeMaximumSpeed 120 ] .
ex:diesel22t a era:VehicleType ; era:wheelSetGauge gauge:1435 ; era:staticAxleLoadNormalPayload 22000.0 .
ex:diesel19t a era:VehicleType ; era:wheelSetGauge gauge:1435 ; era:staticAxleLoadNormalPayload 19000.0 .
"#;

const SECTIONS: &str = r#"
{ SELECT ?id ?source ?target ?cost ?reverse_cost ?c_track WHERE {
    ?id a era:SectionOfLine ; era:opStart ?source ; era:opEnd ?target ;
        era:lengthOfSectionOfLine ?km .
    ?c_track era:isPartOf ?id .
    BIND(?km * 1000 AS ?cost) BIND(?cost AS ?reverse_cost) } }"#;

fn rinf_route(e: &Engine, vehicle: &str, from: &str, to: &str) -> Vec<String> {
    let q = format!(
        r#"SELECT ?edge WHERE {{ SERVICE <http://w3id.org/tripletrail/pgr> {{
        [] pgr:function rail:route ; pgr:start_vid ex:{from} ; pgr:end_vid ex:{to} ;
           pgr:vehicle ex:{vehicle} ; pgr:rules ex:rinf ; pgr:focus "c_track" ;
           pgr:seq ?seq ; pgr:edge ?edge .
        {SECTIONS} }} }} ORDER BY ?seq"#
    );
    rows(e, &q)
        .unwrap_or_else(|err| panic!("{err}\n{q}"))
        .into_iter()
        .filter_map(|r| r[0].clone())
        .collect()
}

#[test]
fn rinf_profile_blocks_incompatible_lines() {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, RINF.as_bytes()).unwrap();
    e.load_into(
        graph_iri("rinf"),
        RdfFormat::Turtle,
        include_str!("../../../profiles/rinf-compatibility.ttl").as_bytes(),
    )
    .unwrap();
    // 25 kV: not on the 3 kV direct line, so through B.
    assert_eq!(rinf_route(&e, "ac", "A", "C"), ["AB", "BC"]);
    // 3 kV: the direct line.
    assert_eq!(rinf_route(&e, "dc", "A", "C"), ["AC"]);
    // 3 kV drawing 400 A: the direct line allows 300 A, and the others are 25 kV.
    assert!(rinf_route(&e, "dcHungry", "A", "C").is_empty());
    // No electric vehicle reaches D (no contact line), but a bi-mode or a diesel does.
    assert!(rinf_route(&e, "ac", "A", "D").is_empty());
    assert_eq!(rinf_route(&e, "biMode", "A", "D"), ["AB", "BC", "CD"]);
    assert_eq!(rinf_route(&e, "diesel", "A", "D"), ["AC", "CD"]);
    // Gauge: standard gauge vehicles do not reach E, an Iberian one only runs there.
    assert!(rinf_route(&e, "dc", "A", "E").is_empty());
    assert!(rinf_route(&e, "iberian", "A", "E").is_empty());
    assert_eq!(rinf_route(&e, "iberian", "C", "E"), ["CE"]);
    assert_eq!(rinf_route(&e, "dualGauge", "A", "E"), ["AC", "CE"]);
    // Load capability: a D4 vehicle avoids the C2 line AC, a B1 one takes it.
    assert_eq!(rinf_route(&e, "dieselD4", "A", "D"), ["AB", "BC", "CD"]);
    assert_eq!(rinf_route(&e, "dieselB1", "A", "D"), ["AC", "CD"]);
    // At 100 km/h AB carries D4; at 120 km/h only C2, so no way to D.
    assert_eq!(
        rinf_route(&e, "dieselD4at100", "A", "D"),
        ["AB", "BC", "CD"]
    );
    assert!(rinf_route(&e, "dieselD4at120", "A", "D").is_empty());
    // By axle load: 22 t needs D2 or up (not C2's 20 t), 19 t fits C2.
    assert_eq!(rinf_route(&e, "diesel22t", "A", "D"), ["AB", "BC", "CD"]);
    assert_eq!(rinf_route(&e, "diesel19t", "A", "D"), ["AC", "CD"]);
}

// ---- performance ----

/// Validation time per edge and cached query time on a generated road grid, for the report
/// in the README. `cargo test --release -p tripletrail --test compatibility -- --ignored --nocapture`
#[test]
#[ignore]
fn compatibility_on_a_large_grid() {
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
                let id = format!("ex:r{x}_{y}{dir}");
                write!(
                    ttl,
                    "{id} ex:from ex:v{x}_{y} ; ex:to ex:v{}_{} ; ex:len {} ",
                    x + dx,
                    y + dy,
                    1 + (x * 7 + y * 13) % 5
                )
                .unwrap();
                match edges % 7 {
                    0 => write!(ttl, "; osm:maxweight {} ", 7 + edges % 40).unwrap(),
                    1 => write!(
                        ttl,
                        "; ex:passesUnder ex:b{edges} . ex:b{edges} osm:maxheight {}.{} ",
                        3 + edges % 2,
                        edges % 10
                    )
                    .unwrap(),
                    2 => write!(ttl, "; osm:maxlength {} ", 10 + edges % 15).unwrap(),
                    _ => {}
                }
                ttl.push_str(".\n");
                edges += 1;
            }
        }
    }
    ttl.push_str("ex:truck ex:weight 20 ; ex:height 3.6 ; ex:length 16 .\n");
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    e.load_into(
        graph_iri("roadRules"),
        RdfFormat::Turtle,
        ROAD_RULES.as_bytes(),
    )
    .unwrap();
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
    let time = |label: &str, params: &str| {
        let start = std::time::Instant::now();
        let r = rows(&e, &q(params)).unwrap();
        let total = start.elapsed();
        let s = e.last_routing_stats();
        println!(
            "{label}: {} rows, total {total:?}, build {:?}, filter {:?}, search {:?}",
            r.len(),
            s.build.edge_query + s.build.build,
            s.constraint,
            s.search
        );
        s
    };
    println!("{edges} edges, store {}", e.store().kind());
    time("unfiltered", "");
    time("unfiltered, cached", "");
    let s = time(
        "truck, first",
        "pgr:vehicle ex:truck ; pgr:rules ex:roadRules ;",
    );
    println!(
        "validation per edge: {:.2} µs",
        s.constraint.as_secs_f64() * 1e6 / edges as f64
    );
    time(
        "truck, cached",
        "pgr:vehicle ex:truck ; pgr:rules ex:roadRules ;",
    );
    time(
        "truck, ch, first",
        "pgr:vehicle ex:truck ; pgr:rules ex:roadRules ; pgr:engine \"ch\" ;",
    );
    time(
        "truck, ch, cached",
        "pgr:vehicle ex:truck ; pgr:rules ex:roadRules ; pgr:engine \"ch\" ;",
    );
}
