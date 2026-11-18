//! PostGIS functions under `st:`, through SPARQL.

use tripletrail::Engine;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX st: <http://w3id.org/tripletrail/st#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
"#;

fn value(e: &Engine, expr: &str) -> Option<String> {
    let q = format!("{PREFIXES} SELECT ({expr} AS ?v) WHERE {{}}");
    let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
        panic!()
    };
    s.next().unwrap().unwrap().get("v").map(|t| match t {
        tripletrail::oxigraph::model::Term::Literal(l) => l.value().to_owned(),
        other => other.to_string(),
    })
}

fn wkt(s: &str) -> String {
    format!("\"{s}\"^^geo:wktLiteral")
}

fn check(e: &Engine, cases: &[(String, Option<&str>)]) {
    for (expr, expected) in cases {
        assert_eq!(value(e, expr).as_deref(), *expected, "{expr}");
    }
}

fn num(e: &Engine, expr: &str) -> f64 {
    value(e, expr)
        .unwrap_or_else(|| panic!("{expr} unbound"))
        .parse()
        .unwrap()
}

#[test]
fn accessors() {
    let e = Engine::new().unwrap();
    let line = wkt("LINESTRING(0 0, 1 0, 1 1)");
    let ring = wkt("LINESTRING(0 0, 1 0, 1 1, 0 0)");
    let holed = wkt("POLYGON((0 0, 10 0, 10 10, 0 10, 0 0), (1 1, 2 1, 2 2, 1 1))");
    let bowtie = wkt("POLYGON((0 0, 2 2, 2 0, 0 2, 0 0))");
    let srid = wkt("<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(1 2)");
    let multi = wkt("GEOMETRYCOLLECTION(POINT(1 1), LINESTRING(0 0, 1 1))");
    check(
        &e,
        &[
            (format!("st:ST_GeometryType({line})"), Some("ST_LineString")),
            (
                format!("st:ST_GeometryType({multi})"),
                Some("ST_GeometryCollection"),
            ),
            (format!("st:GeometryType({holed})"), Some("POLYGON")),
            (format!("st:ST_SRID({line})"), Some("4326")),
            (format!("st:ST_SRID({srid})"), Some("25833")),
            (format!("st:ST_PointN({line}, 2)"), Some("POINT(1 0)")),
            (format!("st:ST_PointN({line}, -1)"), Some("POINT(1 1)")),
            (format!("st:ST_PointN({line}, 4)"), None),
            (format!("st:ST_NumPoints({line})"), Some("3")),
            (format!("st:ST_NumInteriorRings({holed})"), Some("1")),
            (
                format!("st:ST_InteriorRingN({holed}, 1)"),
                Some("LINESTRING(1 1,2 1,2 2,1 1)"),
            ),
            (
                format!("st:ST_ExteriorRing({holed})"),
                Some("LINESTRING(0 0,10 0,10 10,0 10,0 0)"),
            ),
            (format!("st:ST_IsClosed({line})"), Some("false")),
            (format!("st:ST_IsClosed({ring})"), Some("true")),
            (format!("st:ST_IsRing({ring})"), Some("true")),
            (format!("st:ST_IsValid({holed})"), Some("true")),
            (format!("st:ST_IsValid({bowtie})"), Some("false")),
            (
                format!("st:ST_IsValidReason({holed})"),
                Some("Valid Geometry"),
            ),
            (format!("st:ST_IsCollection({multi})"), Some("true")),
            (format!("st:ST_IsCollection({line})"), Some("false")),
            (
                format!("st:ST_OrderingEquals({line}, {line})"),
                Some("true"),
            ),
            (format!("st:ST_NumGeometries({multi})"), Some("2")),
            (format!("st:ST_GeometryN({multi}, 1)"), Some("POINT(1 1)")),
            (format!("st:ST_Dimension({holed})"), Some("2")),
        ],
    );
    let reason = value(&e, &format!("st:ST_IsValidReason({bowtie})")).unwrap();
    assert_ne!(reason, "Valid Geometry");
}

#[test]
fn constructors_and_editors() {
    let e = Engine::new().unwrap();
    let a = wkt("POINT(0 0)");
    let b = wkt("POINT(1 1)");
    let line = wkt("LINESTRING(0 0, 1 0, 1 1)");
    let shell = wkt("LINESTRING(0 0, 4 0, 4 4, 0 0)");
    check(
        &e,
        &[
            (format!("st:ST_MakeLine({a}, {b})"), Some("LINESTRING(0 0,1 1)")),
            (format!("st:ST_MakeLine({line}, {b})"), Some("LINESTRING(0 0,1 0,1 1)")),
            (
                format!("st:ST_LineFromMultiPoint({})", wkt("MULTIPOINT((0 0),(2 0))")),
                Some("LINESTRING(0 0,2 0)"),
            ),
            (format!("st:ST_MakePolygon({shell})"), Some("POLYGON((0 0,4 0,4 4,0 0))")),
            (format!("st:ST_MakePolygon({line})"), None),
            (
                "st:ST_MakeEnvelope(0, 0, 2, 1)".to_owned(),
                Some("POLYGON((0 0,0 1,2 1,2 0,0 0))"),
            ),
            (
                "st:ST_MakeEnvelope(0, 0, 2, 1, 25833)".to_owned(),
                Some("<http://www.opengis.net/def/crs/EPSG/0/25833> POLYGON((0 0,0 1,2 1,2 0,0 0))"),
            ),
            ("st:ST_Point(1, 2)".to_owned(), Some("POINT(1 2)")),
            (
                r#"st:ST_GeomFromGeoJSON("{\"type\":\"Point\",\"coordinates\":[1,2]}")"#.to_owned(),
                Some("POINT(1 2)"),
            ),
            (
                r#"st:ST_GeomFromGML("<gml:Point xmlns:gml='http://www.opengis.net/gml'><gml:pos>1 2</gml:pos></gml:Point>")"#.to_owned(),
                Some("POINT(1 2)"),
            ),
            (
                r#"st:ST_GeomFromKML("<Point><coordinates>1,2</coordinates></Point>")"#.to_owned(),
                Some("POINT(1 2)"),
            ),
            (format!("st:ST_Collect({a}, {b})"), Some("MULTIPOINT((0 0),(1 1))")),
            (
                format!("st:ST_Collect({a}, {line})"),
                Some("GEOMETRYCOLLECTION(POINT(0 0),LINESTRING(0 0,1 0,1 1))"),
            ),
            (format!("st:ST_Multi({line})"), Some("MULTILINESTRING((0 0,1 0,1 1))")),
            (
                format!(
                    "st:ST_CollectionExtract({}, 1)",
                    wkt("GEOMETRYCOLLECTION(POINT(1 1), LINESTRING(0 0, 1 1))")
                ),
                Some("MULTIPOINT((1 1))"),
            ),
            (format!("st:ST_Points({line})"), Some("MULTIPOINT((0 0),(1 0),(1 1))")),
            (format!("st:ST_AddPoint({line}, {a})"), Some("LINESTRING(0 0,1 0,1 1,0 0)")),
            (format!("st:ST_AddPoint({line}, {b}, 0)"), Some("LINESTRING(1 1,0 0,1 0,1 1)")),
            (format!("st:ST_RemovePoint({line}, 1)"), Some("LINESTRING(0 0,1 1)")),
            (format!("st:ST_SetPoint({line}, -1, {a})"), Some("LINESTRING(0 0,1 0,0 0)")),
            (format!("st:ST_BoundingDiagonal({line})"), Some("LINESTRING(0 0,1 1)")),
            (format!("st:ST_FlipCoordinates({})", wkt("POINT(1 2)")), Some("POINT(2 1)")),
            (format!("st:ST_Translate({a}, 3, 4)"), Some("POINT(3 4)")),
            (format!("st:ST_Scale({b}, 2, 3)"), Some("POINT(2 3)")),
            (
                format!("st:ST_Expand({line}, 1)"),
                Some("POLYGON((-1 -1,-1 2,2 2,2 -1,-1 -1))"),
            ),
            (
                format!("st:ST_SetSRID({a}, 25833)"),
                Some("<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(0 0)"),
            ),
            (
                format!(
                    "st:ST_LineMerge({})",
                    wkt("MULTILINESTRING((0 0, 1 0), (2 0, 1 0), (2 0, 3 0))")
                ),
                Some("LINESTRING(0 0,1 0,2 0,3 0)"),
            ),
            (
                format!(
                    "st:ST_LineMerge({})",
                    wkt("MULTILINESTRING((0 0, 1 0), (1 0, 2 0), (1 0, 1 1))")
                ),
                Some("MULTILINESTRING((0 0,1 0),(1 0,2 0),(1 0,1 1))"),
            ),
            (format!("st:ST_ShortestLine({a}, {})", wkt("LINESTRING(-1 2, 1 2)")), Some("LINESTRING(0 0,0 2)")),
            (format!("st:ST_Segmentize({}, 1)", wkt("LINESTRING(0 0, 2 0)")), Some("LINESTRING(0 0,1 0,2 0)")),
            (
                format!(
                    "st:ST_ClipByBox2D({}, {})",
                    wkt("LINESTRING(-1 0.5, 2 0.5)"),
                    wkt("POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))")
                ),
                Some("MULTILINESTRING((0 0.5,1 0.5))"),
            ),
        ],
    );
    // Rotating (1, 0) a quarter turn counter-clockwise gives (0, 1).
    let rotated = value(
        &e,
        &format!(
            "st:ST_Rotate({}, {})",
            wkt("POINT(1 0)"),
            std::f64::consts::FRAC_PI_2
        ),
    )
    .unwrap();
    let xy: Vec<f64> = rotated
        .trim_start_matches("POINT(")
        .trim_end_matches(')')
        .split(' ')
        .map(|v| v.parse().unwrap())
        .collect();
    assert!(
        xy[0].abs() < 1e-12 && (xy[1] - 1.0).abs() < 1e-12,
        "{rotated}"
    );
    // Winding order.
    let cw = wkt("POLYGON((0 0, 0 1, 1 1, 1 0, 0 0))");
    let ccw = value(&e, &format!("st:ST_ForcePolygonCCW({cw})")).unwrap();
    assert_eq!(
        value(&e, &format!("st:ST_ForcePolygonCW({})", wkt(&ccw))).as_deref(),
        Some("POLYGON((0 0,0 1,1 1,1 0,0 0))")
    );
    assert_ne!(ccw, "POLYGON((0 0,0 1,1 1,1 0,0 0))");
    // Point on surface lies inside.
    let p = value(&e, &format!("st:ST_PointOnSurface({cw})")).unwrap();
    assert_eq!(
        value(&e, &format!("st:ST_Within({}, {cw})", wkt(&p))).as_deref(),
        Some("true")
    );
}

#[test]
fn measures() {
    let e = Engine::new().unwrap();
    let square = wkt("POLYGON((0 0, 2 0, 2 2, 0 2, 0 0), (0.5 0.5, 1 0.5, 1 1, 0.5 1, 0.5 0.5))");
    assert_eq!(num(&e, &format!("st:ST_Perimeter({square})")), 10.0);
    let az = num(
        &e,
        &format!(
            "st:ST_Azimuth({}, {})",
            wkt("POINT(0 0)"),
            wkt("POINT(1 0)")
        ),
    );
    assert!((az - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
    let az = num(
        &e,
        &format!(
            "st:ST_Azimuth({}, {})",
            wkt("POINT(0 0)"),
            wkt("POINT(0 -1)")
        ),
    );
    assert!((az - std::f64::consts::PI).abs() < 1e-12);
    assert_eq!(
        value(&e, &format!("st:ST_Azimuth({0}, {0})", wkt("POINT(0 0)"))),
        None
    );
    let a = wkt("LINESTRING(0 0, 1 0, 2 0)");
    let b = wkt("LINESTRING(0 1, 1 2, 2 1)");
    assert_eq!(num(&e, &format!("st:ST_HausdorffDistance({a}, {b})")), 2.0);
    assert_eq!(num(&e, &format!("st:ST_FrechetDistance({a}, {b})")), 2.0);
    assert!((num(&e, &format!("st:ST_MaxDistance({a}, {b})")) - 5f64.sqrt()).abs() < 1e-12);
    assert_eq!(
        value(&e, &format!("st:ST_DFullyWithin({a}, {b}, 3)")).as_deref(),
        Some("true")
    );
    assert_eq!(
        value(&e, &format!("st:ST_DFullyWithin({a}, {b}, 2)")).as_deref(),
        Some("false")
    );
    let p = wkt("POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))");
    let q = wkt("POLYGON((1 1, 3 1, 3 3, 1 3, 1 1))");
    assert_eq!(
        value(&e, &format!("st:ST_Relate({p}, {q})")).as_deref(),
        Some("212101212")
    );
    assert_eq!(
        value(&e, &format!("st:ST_Relate({p}, {q}, \"T*T***T**\")")).as_deref(),
        Some("true")
    );
    assert_eq!(
        value(&e, &format!("st:ST_Relate({p}, {q}, \"FF*FF****\")")).as_deref(),
        Some("false")
    );
    // Projected point: planar.
    let planar = wkt("<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(0 0)");
    let moved = value(
        &e,
        &format!(
            "st:ST_Project({planar}, 10, {})",
            std::f64::consts::FRAC_PI_2
        ),
    )
    .unwrap();
    assert!(moved.contains("POINT(10 "), "{moved}");
    // Geographic point: metres along the geodesic. 111 km north of the equator is about 1°.
    let north = value(
        &e,
        &format!("st:ST_Project({}, 111320, 0)", wkt("POINT(10 0)")),
    )
    .unwrap();
    let lat: f64 = north
        .trim_end_matches(')')
        .rsplit(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!((lat - 1.0).abs() < 0.01, "{north}");
}

#[test]
fn transform() {
    let e = Engine::new().unwrap();
    let oslo = wkt("POINT(10.75 59.91)");
    let utm = value(&e, &format!("st:ST_Transform({oslo}, 25833)")).unwrap();
    assert!(
        utm.starts_with("<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(2"),
        "{utm}"
    );
    let back = value(&e, &format!("st:ST_Transform({}, 4326)", wkt(&utm))).unwrap();
    let xy: Vec<f64> = back
        .trim_start_matches("POINT(")
        .trim_end_matches(')')
        .split(' ')
        .map(|v| v.parse().unwrap())
        .collect();
    assert!(
        (xy[0] - 10.75).abs() < 1e-7 && (xy[1] - 59.91).abs() < 1e-7,
        "{back}"
    );
}

fn xy(v: &str) -> (f64, f64) {
    let inner = v[v.find("POINT(").unwrap() + 6..].trim_end_matches(')');
    let mut it = inner.split(' ').map(|n| n.parse::<f64>().unwrap());
    (it.next().unwrap(), it.next().unwrap())
}

/// National CRSs with their datum shifts (#740): Belgian Lambert 72 and 2008, through
/// `st:ST_Transform` and `geof:transform`, as PROJ gives them (Brussels, Grand-Place).
#[test]
fn transform_national_crss() {
    let e = Engine::new().unwrap();
    let grand_place = wkt("POINT(4.352367 50.846717)");
    let near = |v: &str, x: f64, y: f64| {
        let (a, b) = xy(v);
        assert!((a - x).abs() < 0.01 && (b - y).abs() < 0.01, "{v}");
    };
    let l72 = value(&e, &format!("st:ST_Transform({grand_place}, 31370)")).unwrap();
    assert!(
        l72.starts_with("<http://www.opengis.net/def/crs/EPSG/0/31370> "),
        "{l72}"
    );
    near(&l72, 148_846.056, 170_701.569);
    let l08 = value(
        &e,
        &format!("geof:transform({grand_place}, <http://www.opengis.net/def/crs/EPSG/0/3812>)"),
    )
    .unwrap();
    near(&l08, 648_845.669, 670_701.837);
    // Lambert 72 to 2008 directly, and back to degrees.
    let via = value(&e, &format!("st:ST_Transform({}, 3812)", wkt(&l72))).unwrap();
    near(&via, 648_845.669, 670_701.837);
    let back = value(&e, &format!("st:ST_Transform({}, 4326)", wkt(&l72))).unwrap();
    let (lon, lat) = xy(&back);
    assert!(
        (lon - 4.352367).abs() < 1e-7 && (lat - 50.846717).abs() < 1e-7,
        "{back}"
    );
    // In the SRS catalogue.
    assert_eq!(
        value(&e, "st:srs(31370, \"srname\")").as_deref(),
        Some("BD72 / Belgian Lambert 72")
    );
}
