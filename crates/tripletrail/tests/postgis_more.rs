//! PostGIS Z/M, measure, 3-D, bounding-box and shape functions under `st:`, through SPARQL.
//! Expected values come from the examples in the PostGIS reference where it has one.

use tripletrail::Engine;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
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
fn z_and_m_constructors_and_editors() {
    let e = Engine::new().unwrap();
    check(
        &e,
        &[
            (
                "st:ST_MakePoint(1, 2, 1.5)".into(),
                Some("POINT Z(1 2 1.5)"),
            ),
            (
                "st:ST_MakePoint(1, 2, 3, 4)".into(),
                Some("POINT ZM(1 2 3 4)"),
            ),
            ("st:ST_MakePointM(1, 2, 4)".into(), Some("POINT M(1 2 4)")),
            (
                "st:ST_PointZM(1, 2, 3, 4)".into(),
                Some("POINT ZM(1 2 3 4)"),
            ),
            (
                format!("st:ST_HasZ({})", wkt("POINT Z(1 2 3)")),
                Some("true"),
            ),
            (
                format!("st:ST_HasM({})", wkt("POINT Z(1 2 3)")),
                Some("false"),
            ),
            (
                format!("st:ST_Force3D({})", wkt("POINT(1 2)")),
                Some("POINT Z(1 2 0)"),
            ),
            (
                format!("st:ST_Force4D({}, 5, 6)", wkt("POINT Z(1 2 3)")),
                Some("POINT ZM(1 2 3 6)"),
            ),
            (
                format!(
                    "st:ST_Scroll({}, {})",
                    wkt("LINESTRING ZM(0 0 0 1,10 0 2 0,5 5 4 2,0 0 0 1)"),
                    wkt("POINT ZM(5 5 4 2)")
                ),
                Some("LINESTRING ZM(5 5 4 2,0 0 0 1,10 0 2 0,5 5 4 2)"),
            ),
            (
                format!("st:ST_LineExtend({}, 5, 6)", wkt("LINESTRING(0 0,0 10)")),
                Some("LINESTRING(0 -6,0 0,0 10,0 15)"),
            ),
            (
                format!(
                    "st:ST_Normalize({})",
                    wkt(
                        "GEOMETRYCOLLECTION(POINT(2 3),MULTILINESTRING((0 0,1 1),(2 2,3 3)),POLYGON((0 10,0 0,10 0,10 10,0 10),(4 2,2 2,2 4,4 4,4 2),(6 8,8 8,8 6,6 6,6 8)))"
                    )
                ),
                Some(
                    "GEOMETRYCOLLECTION(POLYGON((0 0,0 10,10 10,10 0,0 0),(6 6,8 6,8 8,6 8,6 6),(2 2,4 2,4 4,2 4,2 2)),MULTILINESTRING((2 2,3 3),(0 0,1 1)),POINT(2 3))",
                ),
            ),
            (
                format!(
                    "st:ST_CollectionHomogenize({})",
                    wkt("GEOMETRYCOLLECTION(POINT(0 0),POINT(1 1))")
                ),
                Some("MULTIPOINT((0 0),(1 1))"),
            ),
            (
                format!(
                    "st:ST_RemoveSmallParts({}, 50, 50)",
                    wkt(
                        "MULTIPOLYGON(((60 160,120 160,120 220,60 220,60 160),(70 170,70 210,110 210,110 170,70 170)),((85 75,155 75,155 145,85 145,85 75)),((50 110,70 110,70 130,50 130,50 110)))"
                    )
                ),
                Some(
                    "MULTIPOLYGON(((60 160,120 160,120 220,60 220,60 160)),((85 75,155 75,155 145,85 145,85 75)))",
                ),
            ),
            (
                format!("st:ST_ReducePrecision({}, 0.1)", wkt("POINT(1.412 19.323)")),
                Some("POINT(1.4 19.3)"),
            ),
            (
                format!(
                    "st:ST_AsText(st:ST_QuantizeCoordinates({}, 4))",
                    wkt("POINT(100.123456 0)")
                ),
                Some("POINT(100.12345504760742 0)"),
            ),
        ],
    );
    close(
        &e,
        &format!(
            "st:ST_Y(st:ST_RotateX({}, {}))",
            wkt("POINT Z(1 2 3)"),
            std::f64::consts::FRAC_PI_2
        ),
        -3.0,
    );
    assert_eq!(
        num(&e, &format!("st:ST_MemSize({})", wkt("POINT(0 0)"))),
        32.0
    );
}

#[test]
fn measures_and_linear_referencing() {
    let e = Engine::new().unwrap();
    check(
        &e,
        &[
            (
                format!("st:ST_AddMeasure({}, 1, 4)", wkt("LINESTRING(1 0,2 0,4 0)")),
                Some("LINESTRING M(1 0 1,2 0 2,4 0 4)"),
            ),
            (
                format!(
                    "st:ST_LocateAlong({}, 3)",
                    wkt("MULTILINESTRING M((1 2 3,3 4 2,9 4 3),(1 2 3,5 4 5))")
                ),
                Some("MULTIPOINT M((1 2 3),(9 4 3),(1 2 3))"),
            ),
            (
                format!(
                    "st:ST_LocateBetween({}, 1.5, 3)",
                    wkt("MULTILINESTRING M((1 2 3,3 4 2,9 4 3),(1 2 3,5 4 5))")
                ),
                Some("GEOMETRYCOLLECTION M(LINESTRING M(1 2 3,3 4 2,9 4 3),POINT M(1 2 3))"),
            ),
            (
                format!(
                    "st:ST_LocateBetweenElevations({}, 2, 4)",
                    wkt("LINESTRING Z(1 2 3,4 5 6)")
                ),
                Some("MULTILINESTRING Z((1 2 3,2 3 4))"),
            ),
            (
                format!(
                    "st:ST_IsValidTrajectory({})",
                    wkt("LINESTRING M(0 0 1,0 1 2)")
                ),
                Some("true"),
            ),
            (
                format!(
                    "st:ST_IsValidTrajectory({})",
                    wkt("LINESTRING M(0 0 1,0 1 0)")
                ),
                Some("false"),
            ),
        ],
    );
    close(
        &e,
        &format!(
            "st:ST_InterpolatePoint({}, {})",
            wkt("LINESTRING M(0 0 0,10 0 20)"),
            wkt("POINT(5 5)")
        ),
        10.0,
    );
    let (a, b) = (
        wkt("LINESTRING M(0 0 0,10 0 10)"),
        wkt("LINESTRING M(10 1 0,0 1 10)"),
    );
    close(&e, &format!("st:ST_ClosestPointOfApproach({a}, {b})"), 5.0);
    close(&e, &format!("st:ST_DistanceCPA({a}, {b})"), 1.0);
    check(
        &e,
        &[(format!("st:ST_CPAWithin({a}, {b}, 1.5)"), Some("true"))],
    );
}

#[test]
fn three_d() {
    let e = Engine::new().unwrap();
    close(
        &e,
        &format!(
            "st:ST_3DDistance({}, {})",
            wkt("POINT Z(0 0 0)"),
            wkt("POINT Z(1 2 2)")
        ),
        3.0,
    );
    close(
        &e,
        &format!("st:ST_3DLength({})", wkt("LINESTRING Z(0 0 0,1 2 2,1 5 6)")),
        8.0,
    );
    // A segment above a horizontal square: the distance is its height.
    close(
        &e,
        &format!(
            "st:ST_3DDistance({}, {})",
            wkt("LINESTRING Z(2 2 5,3 3 5)"),
            wkt("POLYGON Z((0 0 1,10 0 1,10 10 1,0 10 1,0 0 1))")
        ),
        4.0,
    );
    check(
        &e,
        &[
            (
                format!(
                    "st:ST_3DIntersects({}, {})",
                    wkt("LINESTRING Z(2 2 5,3 3 -5)"),
                    wkt("POLYGON Z((0 0 1,10 0 1,10 10 1,0 10 1,0 0 1))")
                ),
                Some("true"),
            ),
            (
                format!(
                    "st:ST_3DShortestLine({}, {})",
                    wkt("POINT Z(0 0 0)"),
                    wkt("LINESTRING Z(-5 3 4,5 3 4)")
                ),
                Some("LINESTRING Z(0 0 0,0 3 4)"),
            ),
            (
                format!(
                    "st:ST_3DLineInterpolatePoint({}, 0.5)",
                    wkt("LINESTRING Z(0 0 0,0 10 10)")
                ),
                Some("POINT Z(0 5 5)"),
            ),
        ],
    );
}

#[test]
fn bounding_box_operators() {
    let e = Engine::new().unwrap();
    let (a, b) = (wkt("LINESTRING(1 2,3 4)"), wkt("LINESTRING(5 6,7 8)"));
    check(
        &e,
        &[
            (format!("st:bboxLeftOf({a}, {b})"), Some("true")),
            (format!("st:bboxRightOf({a}, {b})"), Some("false")),
            (format!("st:bboxIntersects({a}, {b})"), Some("false")),
            (format!("st:bboxBelow({a}, {b})"), Some("true")),
            (format!("st:geomEquals({a}, {a})"), Some("true")),
            (
                format!("st:bboxContains({}, {a})", wkt("POINT(0 0)")),
                Some("false"),
            ),
            (
                format!("geof:latitude({})", wkt("POINT(10 60)")),
                Some("60"),
            ),
        ],
    );
    close(&e, &format!("st:bboxDistance({a}, {b})"), 8f64.sqrt());
}

#[test]
fn shapes() {
    let e = Engine::new().unwrap();
    check(
        &e,
        &[
            (
                format!(
                    "st:ST_LineCrossingDirection({}, {})",
                    wkt("LINESTRING(25 169,89 114,40 70,86 43)"),
                    wkt("LINESTRING(171 154,20 140,71 74,161 53)")
                ),
                Some("3"),
            ),
            (
                format!(
                    "st:ST_SharedPaths({}, {})",
                    wkt("MULTILINESTRING((26 125,26 200,126 200,126 125,26 125),(51 150,101 150,76 175,51 150))"),
                    wkt("LINESTRING(151 100,126 156.25,126 125,90 161,76 175)")
                ),
                Some(
                    "GEOMETRYCOLLECTION(MULTILINESTRING((126 156.25,126 125),(101 150,90 161),(90 161,76 175)),MULTILINESTRING EMPTY)",
                ),
            ),
            (
                format!("st:ST_AsTWKB({})", wkt("LINESTRING(1 1,5 5)")),
                Some("02000202020808"),
            ),
            (
                "st:ST_AsText(st:ST_GeomFromTWKB(st:ST_AsTWKB(\"LINESTRING(1 1,5 5)\"^^geo:wktLiteral)))"
                    .into(),
                Some("LINESTRING(1 1,5 5)"),
            ),
            (
                format!(
                    "st:ST_NumGeometries(st:ST_TriangulatePolygon({}))",
                    wkt("POLYGON((0 0,1 0,1 1,0 1,0 0))")
                ),
                Some("2"),
            ),
        ],
    );
    close(
        &e,
        &format!(
            "st:ST_MinimumClearance({})",
            wkt("POLYGON((0 0,1 0,1 1,0.5 3.2e-4,0 0))")
        ),
        0.00032,
    );
    close(
        &e,
        &format!(
            "st:ST_Area(st:ST_BuildArea({}))",
            wkt(
                "MULTILINESTRING((0 0,10 0,10 10,0 10,0 0),(2 2,4 2,4 4,2 4,2 2),(2.5 2.5,3 2.5,3 3,2.5 2.5))"
            )
        ),
        100.0 - 4.0 + 0.125,
    );
    close(
        &e,
        "st:ST_Area(st:ST_Hexagon(1, 0, 0))",
        3.0 * 3f64.sqrt() / 2.0,
    );
    close(
        &e,
        &format!(
            "st:ST_Area(st:ST_WrapX({}, 180, -360))",
            wkt("POLYGON((170 0,190 0,190 10,170 10,170 0))")
        ),
        200.0,
    );
    close(
        &e,
        &format!(
            "st:ST_X(st:ST_MaximumInscribedCircle({}))",
            wkt("POLYGON((0 0,10 0,10 4,0 4,0 0))")
        ),
        5.0,
    );
    // The hull only grows: a notched square fills its notch.
    let notched = wkt("POLYGON((0 0,10 0,10 10,6 10,5 5,4 10,0 10,0 0))");
    close(
        &e,
        &format!("st:ST_Area(st:ST_SimplifyPolygonHull({notched}, 0.7))"),
        100.0,
    );
    // Aggregates: polygonize and intersection clusters.
    let q = format!(
        "{PREFIXES} SELECT (st:ST_NumGeometries(st:ST_PolygonizeAgg(?g)) AS ?n) \
         (st:ST_NumGeometries(st:ST_ClusterIntersecting(?g)) AS ?c) WHERE {{ VALUES ?g {{ {} {} {} }} }}",
        wkt("LINESTRING(0 0,1 0,1 1)"),
        wkt("LINESTRING(1 1,0 1,0 0)"),
        wkt("LINESTRING(5 5,6 6)"),
    );
    let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
        panic!()
    };
    let row = s.next().unwrap().unwrap();
    let get = |v: &str| match row.get(v) {
        Some(tripletrail::oxigraph::model::Term::Literal(l)) => l.value().to_owned(),
        other => panic!("{other:?}"),
    };
    assert_eq!(get("n"), "1");
    assert_eq!(get("c"), "2");
}
