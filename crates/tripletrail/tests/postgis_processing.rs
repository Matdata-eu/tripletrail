//! PostGIS processing, validation, overlay and format functions under `st:`, through SPARQL.
//! Expected values come from the examples in the PostGIS reference where it has one.

use tripletrail::Engine;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX st: <http://w3id.org/tripletrail/st#>
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

fn close(e: &Engine, expr: &str, expected: f64) {
    let v = num(e, expr);
    assert!(
        (v - expected).abs() < 1e-6,
        "{expr} = {v}, expected {expected}"
    );
}

#[test]
fn formats() {
    let e = Engine::new().unwrap();
    let p = wkt("POINT(1 2)");
    let srid = wkt("<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(1 2)");
    let polyline = wkt("LINESTRING(-120.2 38.5,-120.95 40.7,-126.453 43.252)");
    check(
        &e,
        &[
            (format!("st:ST_AsEWKT({p})"), Some("SRID=4326;POINT(1 2)")),
            (
                format!("st:ST_AsEWKT({srid})"),
                Some("SRID=25833;POINT(1 2)"),
            ),
            (
                format!("st:ST_AsBinary({p})"),
                Some("0101000000000000000000F03F0000000000000040"),
            ),
            (
                format!("st:ST_AsHEXEWKB({p})"),
                Some("0101000020E6100000000000000000F03F0000000000000040"),
            ),
            (
                "st:ST_GeomFromWKB(\"0101000000000000000000F03F0000000000000040\")".into(),
                Some("POINT(1 2)"),
            ),
            (
                format!("st:ST_SRID(st:ST_GeomFromEWKB(st:ST_AsHEXEWKB({srid})))"),
                Some("25833"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_GeomFromWKB(st:ST_AsBinary({})))",
                    wkt("POLYGON((0 0,1 0,1 1,0 0))")
                ),
                Some("POLYGON((0 0,1 0,1 1,0 0))"),
            ),
            (
                format!(
                    "st:ST_Z(st:ST_GeomFromWKB(st:ST_AsBinary({})))",
                    wkt("POINT Z(1 2 3)")
                ),
                Some("3"),
            ),
            ("st:ST_GeomFromWKB(\"01\")".into(), None),
            (
                format!("st:ST_AsEncodedPolyline({polyline})"),
                Some("_p~iF~ps|U_ulLnnqC_mqNvxq`@"),
            ),
            (
                "st:ST_AsText(st:ST_LineFromEncodedPolyline(\"_p~iF~ps|U_ulLnnqC_mqNvxq`@\"))"
                    .into(),
                Some("LINESTRING(-120.2 38.5,-120.95 40.7,-126.453 43.252)"),
            ),
            (
                format!("st:ST_GeoHash({})", wkt("POINT(-126 48)")),
                Some("c0w3hf1s70w3hf1s70w3"),
            ),
            (
                format!("st:ST_GeoHash({}, 5)", wkt("POINT(-126 48)")),
                Some("c0w3h"),
            ),
            (
                format!("st:ST_AsLatLonText({})", wkt("POINT(-3.2342342 -2.32498)")),
                Some("2°19'29.928\"S 3°14'3.243\"W"),
            ),
            (
                format!(
                    "st:ST_AsLatLonText({}, \"D.DDDDD\")",
                    wkt("POINT(-3.2342342 -2.32498)")
                ),
                Some("-2.32498 -3.23423"),
            ),
            (
                format!("st:ST_AsSVG({})", wkt("POLYGON((0 0,0 1,1 1,1 0,0 0))")),
                Some("M 0 0 L 0 -1 1 -1 1 0 Z"),
            ),
            (format!("st:ST_AsSVG({p})"), Some("cx=\"1\" cy=\"-2\"")),
            (
                format!("st:ST_AsSVG({}, 1)", wkt("LINESTRING(1 2,3 5)")),
                Some("m 1 -2 l 2 -3"),
            ),
        ],
    );
    let center = value(
        &e,
        "st:ST_AsText(st:ST_PointFromGeoHash(\"9qqj7nmxncgyy4d0dbxqz0\"))",
    )
    .unwrap();
    assert!(center.starts_with("POINT(-115.17281"), "{center}");
    close(
        &e,
        "st:ST_XMin(st:ST_GeomFromGeoHash(\"c0w3h\"))",
        -126.03515625,
    );
}

#[test]
fn constructors_and_accessors() {
    let e = Engine::new().unwrap();
    let zm = wkt("POINT ZM(1 2 3 4)");
    let holed = wkt("POLYGON((0 0, 0 10, 10 10, 10 0, 0 0), (1 1, 2 1, 2 2, 1 1))");
    check(
        &e,
        &[
            (
                "st:ST_AsText(st:ST_PointFromText(\"POINT(1 2)\"))".into(),
                Some("POINT(1 2)"),
            ),
            ("st:ST_PointFromText(\"LINESTRING(0 0,1 1)\")".into(), None),
            (
                "st:ST_SRID(st:ST_LineFromText(\"LINESTRING(0 0,1 1)\", 25833))".into(),
                Some("25833"),
            ),
            (
                "st:ST_SRID(st:ST_GeomFromEWKT(\"SRID=25833;POINT(1 2)\"))".into(),
                Some("25833"),
            ),
            (format!("st:ST_Z({zm})"), Some("3")),
            (format!("st:ST_M({zm})"), Some("4")),
            (format!("st:ST_Zmflag({zm})"), Some("3")),
            (format!("st:ST_Zmflag({})", wkt("POINT(1 2)")), Some("0")),
            (format!("st:ST_HasArc({holed})"), Some("false")),
            (format!("st:ST_NRings({holed})"), Some("2")),
            (format!("st:ST_IsPolygonCW({holed})"), Some("true")),
            (format!("st:ST_IsPolygonCCW({holed})"), Some("false")),
            (
                format!(
                    "st:ST_AsText(st:ST_MakeBox2D({}, {}))",
                    wkt("POINT(0 0)"),
                    wkt("POINT(2 1)")
                ),
                Some("POLYGON((0 0,0 1,2 1,2 0,0 0))"),
            ),
            (
                format!(
                    "st:ST_SRID(st:ST_Polygon({}, 25833))",
                    wkt("LINESTRING(0 0,1 0,1 1,0 0)")
                ),
                Some("25833"),
            ),
            (
                format!("st:ST_AsText(st:ST_ForceCollection({}))", wkt("POINT(1 2)")),
                Some("GEOMETRYCOLLECTION(POINT(1 2))"),
            ),
        ],
    );
    close(
        &e,
        "st:ST_XMin(st:ST_TileEnvelope(2, 1, 1))",
        -10_018_754.171_394_622,
    );
    close(
        &e,
        "st:ST_YMax(st:ST_TileEnvelope(2, 1, 1))",
        10_018_754.171_394_622,
    );
    close(&e, "st:ST_YMin(st:ST_TileEnvelope(2, 1, 1))", 0.0);
}

#[test]
fn measures_and_relations() {
    let e = Engine::new().unwrap();
    let sq = wkt("POLYGON((0 0,10 0,10 10,0 10,0 0))");
    close(
        &e,
        &format!(
            "st:ST_Angle({}, {}, {})",
            wkt("POINT(0 0)"),
            wkt("POINT(10 10)"),
            wkt("POINT(20 0)")
        ),
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    close(
        &e,
        &format!(
            "st:ST_Angle({}, {})",
            wkt("LINESTRING(0 0, 0.3 0.7, 1 1)"),
            wkt("LINESTRING(0 0, 0.2 0.5, 1 0)")
        ),
        std::f64::consts::FRAC_PI_4,
    );
    check(
        &e,
        &[
            (
                format!("st:ST_ContainsProperly({sq}, {})", wkt("POINT(5 5)")),
                Some("true"),
            ),
            (
                format!(
                    "st:ST_ContainsProperly({sq}, {})",
                    wkt("LINESTRING(0 0,10 0)")
                ),
                Some("false"),
            ),
            (
                "st:ST_RelateMatch(\"101202FFF\", \"TTTTTTFFF\")".into(),
                Some("true"),
            ),
            (
                "st:ST_RelateMatch(\"101202FFF\", \"FTTTTTFFF\")".into(),
                Some("false"),
            ),
            (
                format!("st:ST_PointInsideCircle({}, 0, 0, 2)", wkt("POINT(1 1)")),
                Some("true"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_LongestLine({}, {}))",
                    wkt("POINT(0 0)"),
                    wkt("LINESTRING(1 0, 5 5)")
                ),
                Some("LINESTRING(0 0,5 5)"),
            ),
            (
                format!("st:ST_Length2D({})", wkt("LINESTRING(0 0,3 4)")),
                Some("5"),
            ),
        ],
    );
}

#[test]
fn editors() {
    let e = Engine::new().unwrap();
    check(
        &e,
        &[
            (
                format!(
                    "st:ST_AsText(st:ST_RemoveRepeatedPoints({}))",
                    wkt("LINESTRING(0 0,0 0,1 1,1 1,2 2)")
                ),
                Some("LINESTRING(0 0,1 1,2 2)"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_RemoveRepeatedPoints({}, 1.5))",
                    wkt("LINESTRING(0 0,1 0,2 0,5 0)")
                ),
                Some("LINESTRING(0 0,2 0,5 0)"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_SnapToGrid({}, 0.001))",
                    wkt("LINESTRING(1.1115678 2.123, 4.111111 3.2374897, 4.11112 3.23748667)")
                ),
                Some("LINESTRING(1.112 2.123,4.111 3.237)"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_Snap({}, {}, 0.5))",
                    wkt("LINESTRING(0 0, 10 0.1)"),
                    wkt("POINT(10 0)")
                ),
                Some("LINESTRING(0 0,10 0)"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_TransScale({}, 1, 1, 2, 3))",
                    wkt("POINT(1 2)")
                ),
                Some("POINT(4 9)"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_Affine({}, 0, -1, 1, 0, 10, 0))",
                    wkt("POINT(1 2)")
                ),
                Some("POINT(8 1)"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_ShiftLongitude({}))",
                    wkt("POINT(-120 30)")
                ),
                Some("POINT(240 30)"),
            ),
        ],
    );
}

#[test]
fn processing() {
    let e = Engine::new().unwrap();
    let bowtie = wkt("POLYGON((0 0, 2 2, 2 0, 0 2, 0 0))");
    check(
        &e,
        &[
            (
                format!("st:ST_GeometryType(st:ST_MakeValid({bowtie}))"),
                Some("ST_MultiPolygon"),
            ),
            (format!("st:ST_Area(st:ST_MakeValid({bowtie}))"), Some("2")),
            (
                format!("st:ST_IsValid(st:ST_MakeValid({bowtie}))"),
                Some("true"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_OffsetCurve({}, 1))",
                    wkt("LINESTRING(0 0,10 0)")
                ),
                Some("LINESTRING(0 1,10 1)"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_OffsetCurve({}, -1))",
                    wkt("LINESTRING(0 0,10 0)")
                ),
                Some("LINESTRING(0 -1,10 -1)"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_OffsetCurve({}, 1))",
                    wkt("LINESTRING(0 0,10 0,10 10)")
                ),
                Some("LINESTRING(0 1,9 1,9 10)"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_LineInterpolatePoints({}, 0.25))",
                    wkt("LINESTRING(0 0,10 0)")
                ),
                Some("MULTIPOINT((2.5 0),(5 0),(7.5 0),(10 0))"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_LineInterpolatePoints({}, 0.25, false))",
                    wkt("LINESTRING(0 0,10 0)")
                ),
                Some("POINT(2.5 0)"),
            ),
            (
                format!(
                    "st:ST_NumGeometries(st:ST_DelaunayTriangles({}))",
                    wkt("MULTIPOINT(0 0,1 0,0 1,1 1)")
                ),
                Some("2"),
            ),
            (
                format!(
                    "st:ST_NumGeometries(st:ST_DelaunayTriangles({}, 0, 1))",
                    wkt("MULTIPOINT(0 0,1 0,0 1,1 1)")
                ),
                Some("5"),
            ),
            (
                format!(
                    "st:ST_NumGeometries(st:ST_VoronoiPolygons({}))",
                    wkt("MULTIPOINT(0 0,2 0,1 2)")
                ),
                Some("3"),
            ),
            (
                format!(
                    "st:ST_NumPoints(st:ST_GeneratePoints({}, 5, 1))",
                    wkt("POLYGON((0 0,10 0,10 10,0 10,0 0))")
                ),
                Some("5"),
            ),
            (
                format!(
                    "st:ST_Area(st:ST_UnaryUnion({}))",
                    wkt("MULTIPOLYGON(((0 0,2 0,2 2,0 2,0 0)),((1 1,3 1,3 3,1 3,1 1)))")
                ),
                Some("7"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_OrientedEnvelope({}))",
                    wkt("MULTIPOINT(0 0,4 0)")
                ),
                Some("LINESTRING(0 0,4 0)"),
            ),
        ],
    );
    close(
        &e,
        &format!(
            "st:ST_Area(st:ST_OrientedEnvelope({}))",
            wkt("POLYGON((0 1,1 0,2 1,1 2,0 1))")
        ),
        2.0,
    );
    close(
        &e,
        &format!(
            "st:ST_X(st:ST_GeometricMedian({}))",
            wkt("MULTIPOINT((0 0), (1 1), (2 2), (200 200))")
        ),
        1.976_155_028_125_5,
    );
    let disc = format!(
        "st:ST_Buffer({}, 10)",
        wkt("<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(0 0)")
    );
    let pieces = format!("st:ST_Subdivide({disc}, 10)");
    assert!(num(&e, &format!("st:ST_NumGeometries({pieces})")) >= 4.0);
    let (whole, split) = (
        num(&e, &format!("st:ST_Area({disc})")),
        num(&e, &format!("st:ST_Area({pieces})")),
    );
    assert!((whole - split).abs() < 1e-6 * whole, "{whole} {split}");
    close(
        &e,
        &format!(
            "st:ST_Area(st:ST_SimplifyPreserveTopology({}, 0.5))",
            wkt("POLYGON((0 0,5 0.1,10 0,10 10,0 10,0 0))")
        ),
        100.0,
    );
}

#[test]
fn overlay() {
    let e = Engine::new().unwrap();
    check(
        &e,
        &[
            (
                format!(
                    "st:ST_AsText(st:ST_Split({}, {}))",
                    wkt("LINESTRING(0 0,10 0)"),
                    wkt("POINT(5 0)")
                ),
                Some("GEOMETRYCOLLECTION(LINESTRING(0 0,5 0),LINESTRING(5 0,10 0))"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_Split({}, {}))",
                    wkt("LINESTRING(0 0,10 0,10 10)"),
                    wkt("LINESTRING(5 -1,5 1)")
                ),
                Some("GEOMETRYCOLLECTION(LINESTRING(0 0,5 0),LINESTRING(5 0,10 0,10 10))"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_Node({}))",
                    wkt("MULTILINESTRING((0 0,10 10),(0 10,10 0))")
                ),
                Some("MULTILINESTRING((0 0,5 5),(5 5,10 10),(0 10,5 5),(5 5,10 0))"),
            ),
            (
                format!(
                    "st:ST_NumGeometries(st:ST_Node({}))",
                    wkt("MULTILINESTRING((0 0,10 0),(0 0,10 0))")
                ),
                Some("1"),
            ),
        ],
    );
}

#[test]
fn aggregates() {
    let e = Engine::new().unwrap();
    let agg = |f: &str| {
        let q = format!(
            "{PREFIXES} SELECT (st:ST_AsText(st:{f}(?g)) AS ?v) WHERE {{ VALUES ?g {{ {} {} }} }}",
            wkt("POINT(0 0)"),
            wkt("POINT(2 1)")
        );
        let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
            panic!()
        };
        match s.next().unwrap().unwrap().get("v") {
            Some(tripletrail::oxigraph::model::Term::Literal(l)) => l.value().to_owned(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(agg("ST_Extent"), "POLYGON((2 0,2 1,0 1,0 0,2 0))");
    assert_eq!(agg("ST_CollectAgg"), "MULTIPOINT((0 0),(2 1))");
    assert_eq!(
        agg("ST_UnionAgg"),
        "GEOMETRYCOLLECTION(POINT(0 0),POINT(2 1))"
    );
}
