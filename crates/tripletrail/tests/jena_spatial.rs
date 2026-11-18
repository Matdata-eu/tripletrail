//! jena-geosparql's `spatial:` property functions (#827): the answers Jena 5.2 gives on the
//! same data.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

const DATA: &str = r#"@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix ex: <http://example.org/> .
ex:oslo geo:hasGeometry ex:g1 . ex:g1 geo:asWKT "POINT(10.7522 59.9139)"^^geo:wktLiteral .
ex:drammen geo:hasGeometry ex:g2 . ex:g2 geo:asWKT "POINT(10.2045 59.7439)"^^geo:wktLiteral .
ex:lillestrom geo:hasGeometry ex:g3 . ex:g3 geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(59.9560 11.0492)"^^geo:wktLiteral .
ex:hamar geo:hasGeometry ex:g4 . ex:g4 geo:asWKT "POINT(11.0680 60.7945)"^^geo:wktLiteral .
ex:bergen geo:hasGeometry ex:g5 . ex:g5 geo:asWKT "POINT(5.3221 60.3913)"^^geo:wktLiteral .
ex:trondheim geo:hasGeometry ex:g6 . ex:g6 geo:asWKT "POINT(10.3951 63.4305)"^^geo:wktLiteral .
ex:gardermobanen geo:hasGeometry ex:g7 . ex:g7 geo:asWKT "LINESTRING(10.7522 59.9139, 11.0492 59.9560, 11.1004 60.1939)"^^geo:wktLiteral .
ex:oslofjord geo:hasGeometry ex:g8 . ex:g8 geo:asWKT "POLYGON((10.4 59.0, 10.8 59.0, 10.8 59.85, 10.4 59.85, 10.4 59.0))"^^geo:wktLiteral .
"#;

const PREFIXES: &str = "PREFIX spatial: <http://jena.apache.org/spatial#>
PREFIX units: <http://www.opengis.net/def/uom/OGC/1.0/>
PREFIX ex: <http://example.org/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
";

fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = engine.query(&format!("{PREFIXES}{query}")).unwrap()
    else {
        panic!("not a SELECT")
    };
    let mut rows: Vec<String> = solutions
        .map(|s| {
            s.unwrap()
                .iter()
                .map(|(_, t)| match t {
                    oxigraph::model::Term::Literal(l) => l.value().to_owned(),
                    t => t
                        .to_string()
                        .replace("<http://example.org/", "")
                        .replace('>', ""),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    rows.sort();
    rows
}

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e
}

#[test]
fn spatial_property_functions_give_jenas_answers() {
    let e = engine();
    let cases: &[(&str, &[&str])] = &[
        (
            "SELECT ?f { ?f spatial:nearby (59.91 10.75 40) }",
            &[
                "drammen",
                "gardermobanen",
                "lillestrom",
                "oslo",
                "oslofjord",
            ],
        ),
        (
            "SELECT ?f { ?f spatial:withinCircle (59.91 10.75 20000 units:metre) }",
            &["gardermobanen", "lillestrom", "oslo", "oslofjord"],
        ),
        (
            r#"SELECT ?f { ?f spatial:nearbyGeom ("POINT(11.07 60.79)"^^geo:wktLiteral 60 units:kilometre) }"#,
            &["hamar"],
        ),
        // Searching for features, Jena tests their bounding boxes: it also gives oslofjord,
        // whose bounding box only meets the box (#838).
        (
            "SELECT ?f { ?f spatial:withinBox (59.5 10 60.5 11.5) }",
            &[
                "drammen",
                "gardermobanen",
                "lillestrom",
                "oslo",
                "oslofjord",
            ],
        ),
        (
            r#"SELECT ?f { ?f spatial:withinBoxGeom ("POLYGON((10 59.5, 11.5 59.5, 11.5 60.5, 10 60.5, 10 59.5))"^^geo:wktLiteral) }"#,
            &[
                "drammen",
                "gardermobanen",
                "lillestrom",
                "oslo",
                "oslofjord",
            ],
        ),
        // A bound subject is tested: within the box.
        (
            "SELECT ?f { VALUES ?f { ex:oslofjord ex:oslo ex:bergen } ?f spatial:withinBox (59.5 10 60.5 11.5) }",
            &["oslo"],
        ),
        (
            r#"SELECT ?f { VALUES ?f { ex:oslofjord ex:oslo } ?f spatial:withinBoxGeom ("POLYGON((10 59.5, 11.5 59.5, 11.5 60.5, 10 60.5, 10 59.5))"^^geo:wktLiteral) }"#,
            &["oslo"],
        ),
        (
            "SELECT ?x { ex:oslofjord spatial:withinBox (59.5 10 60.5 11.5) BIND(1 AS ?x) }",
            &[],
        ),
        (
            "SELECT ?x { ex:oslo spatial:withinBox (59.5 10 60.5 11.5) BIND(1 AS ?x) }",
            &["1"],
        ),
        (
            "SELECT ?f { ?f spatial:intersectBox (59.9 11.0 60.0 11.2) }",
            &["gardermobanen", "lillestrom"],
        ),
        (
            r#"SELECT ?f { ?f spatial:intersectBoxGeom ("LINESTRING(5 60, 6 61)"^^geo:wktLiteral) }"#,
            &["bergen"],
        ),
        (
            "SELECT ?f { ?f spatial:north (60.5 10) }",
            &["hamar", "trondheim"],
        ),
        (
            "SELECT ?f { ?f spatial:south (59.8 10) }",
            &["drammen", "oslofjord"],
        ),
        (
            "SELECT ?f { ?f spatial:east (60 11) }",
            &["gardermobanen", "hamar", "lillestrom"],
        ),
        (
            r#"SELECT ?f { ?f spatial:westGeom ("POINT(10.3 60)"^^geo:wktLiteral) }"#,
            &["bergen", "drammen"],
        ),
        // A subject bound before the call is tested, not searched for.
        (
            "SELECT ?f { VALUES ?f { ex:oslo ex:bergen } ?f spatial:nearby (59.91 10.75 40) }",
            &["oslo"],
        ),
        (
            "SELECT * { ex:oslo spatial:nearby (59.91 10.75 40) }",
            &[""],
        ),
        (
            "SELECT * { ex:bergen spatial:nearby (59.91 10.75 40) }",
            &[],
        ),
        // The arguments may come from the row.
        (
            "SELECT ?f { VALUES (?lat ?r) { (59.91 1) (63.43 5) } ?f spatial:nearby (?lat 10.75 ?r) }",
            &["gardermobanen", "oslo"],
        ),
        // A limit takes that many features.
        (
            "SELECT (COUNT(*) AS ?n) { ?f spatial:north (59 10 3) }",
            &["3"],
        ),
    ];
    for (query, expected) in cases {
        assert_eq!(rows(&e, query), *expected, "{query}");
    }
}

/// A feature without geometries is found by its `wgs84_pos:lat` and `wgs84_pos:long`.
#[test]
fn features_with_a_latitude_and_longitude_are_found() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix w: <http://www.w3.org/2003/01/geo/wgs84_pos#> .
        <http://example.org/oslo> w:lat 59.9139 ; w:long 10.7522 .
        <http://example.org/bergen> w:lat 60.3913 ; w:long 5.3221 ."#
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(
        rows(&e, "SELECT ?f { ?f spatial:nearby (59.91 10.75 10) }"),
        ["oslo"]
    );
    assert_eq!(rows(&e, "SELECT ?f { ?f spatial:west (60 8) }"), ["bergen"]);
}

/// With radii of thousands of kilometres, `nearby` finds every feature that Jena's distance
/// puts within the radius, among long lines across the globe (#836: a pre-test on
/// `geof:distance` dropped some, since it can be far above Jena's distance for long lines).
#[test]
fn nearby_with_large_radii_finds_every_feature_within_them() {
    let mut seed = 7u64;
    let mut next = move |lo: f64, hi: f64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        lo + (hi - lo) * ((seed >> 11) as f64 / (1u64 << 53) as f64)
    };
    let mut data = String::from("@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n");
    for i in 0..300 {
        let (mut x, mut y) = (next(-150.0, 150.0), next(-75.0, 75.0));
        let mut points = Vec::new();
        for _ in 0..2 + (next(0.0, 5.0) as usize) {
            points.push(format!("{x:.3} {y:.3}"));
            x = (x + next(-25.0, 25.0)).clamp(-179.0, 179.0);
            y = (y + next(-15.0, 15.0)).clamp(-85.0, 85.0);
        }
        data.push_str(&format!(
            "<http://example.org/f{i}> geo:hasGeometry [ geo:asWKT \"LINESTRING({})\"^^geo:wktLiteral ] .\n",
            points.join(", ")
        ));
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    for _ in 0..30 {
        let (lon, lat) = (next(-179.0, 179.0), next(-85.0, 85.0));
        let km = [300.0, 1000.0, 2500.0, 4000.0, 6000.0][next(0.0, 5.0) as usize];
        let nearby = rows(
            &e,
            &format!("SELECT ?f {{ ?f spatial:nearby ({lat:?} {lon:?} {km:?} units:kilometre) }}"),
        );
        let within = rows(
            &e,
            &format!(
                "PREFIX spatialf: <http://jena.apache.org/function/spatial#>
                 SELECT DISTINCT ?f {{ ?f geo:hasGeometry/geo:asWKT ?w
                   FILTER(spatialf:distance(?w, \"POINT({lon:?} {lat:?})\"^^geo:wktLiteral, units:kilometre) < {km:?}) }}"
            ),
        );
        assert_eq!(nearby, within, "nearby ({lat} {lon} {km} km)");
    }
}
