//! PostGIS P2 functions (SFCGAL-style operations, curves, X3D, coverages) through SPARQL.

use tripletrail::Engine;
use tripletrail::oxigraph::model::Term;
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
        Term::Literal(l) => l.value().to_owned(),
        other => other.to_string(),
    })
}

fn wkt(s: &str) -> String {
    format!("\"{s}\"^^geo:wktLiteral")
}

fn num(e: &Engine, expr: &str) -> f64 {
    value(e, expr)
        .unwrap_or_else(|| panic!("{expr} unbound"))
        .parse()
        .unwrap()
}

#[test]
fn sfcgal_style_functions() {
    let e = Engine::new().unwrap();
    let square = wkt("POLYGON((0 0,1 0,1 1,0 1,0 0))");
    let seg = wkt("LINESTRING(0 0,2 0)");
    assert_eq!(
        num(
            &e,
            &format!("st:ST_Area(st:ST_MinkowskiSum({square}, {seg}))")
        ),
        3.0
    );
    let grid = wkt("MULTIPOINT((0 0),(1 0),(2 0),(0 1),(1 1),(2 1),(0 2),(1 2),(2 2))");
    assert_eq!(
        num(&e, &format!("st:ST_Area(st:ST_AlphaShape({grid}, 1))")),
        4.0
    );
    assert_eq!(
        num(&e, &format!("st:ST_Area(st:ST_OptimalAlphaShape({grid}))")),
        4.0
    );
    assert_eq!(
        num(
            &e,
            &format!("st:ST_NumGeometries(st:ST_Tesselate({square}))")
        ),
        2.0
    );
    assert!(
        num(
            &e,
            &format!("st:ST_NumGeometries(st:ST_ConstrainedDelaunayTriangles({square}))")
        ) >= 2.0
    );
    assert_eq!(
        value(&e, &format!("st:ST_Orientation({square})")).as_deref(),
        Some("-1")
    );
    assert_eq!(
        value(
            &e,
            &format!(
                "st:ST_IsPlanar({})",
                wkt("POLYGON Z((0 0 0,1 0 1,1 1 1,0 1 0,0 0 0))")
            )
        )
        .as_deref(),
        Some("true")
    );
    assert_eq!(
        value(
            &e,
            &format!(
                "st:ST_IsPlanar({})",
                wkt("POLYGON Z((0 0 0,1 0 1,1 1 0,0 1 1,0 0 0))")
            )
        )
        .as_deref(),
        Some("false")
    );
    let tilted = num(
        &e,
        &format!(
            "st:ST_3DArea({})",
            wkt("POLYGON Z((0 0 0,1 0 1,1 1 1,0 1 0,0 0 0))")
        ),
    );
    assert!((tilted - 2f64.sqrt()).abs() < 1e-12, "{tilted}");
    // The medial axis of a long rectangle runs along its middle.
    let axis = num(
        &e,
        &format!(
            "st:ST_Length(st:ST_ApproximateMedialAxis({}))",
            wkt("POLYGON((0 0,10 0,10 2,0 2,0 0))")
        ),
    );
    assert!(axis > 7.0 && axis < 14.0, "{axis}");
}

#[test]
fn curves_and_x3d() {
    let e = Engine::new().unwrap();
    let line = wkt("LINESTRING(0 0,1 1)");
    assert_eq!(
        value(&e, &format!("st:ST_NumCurves({line})")).as_deref(),
        Some("1")
    );
    assert_eq!(
        value(&e, &format!("st:ST_AsText(st:ST_CurveToLine({line}))")).as_deref(),
        Some("LINESTRING(0 0,1 1)")
    );
    assert_eq!(
        value(
            &e,
            &format!("st:ST_AsX3D({})", wkt("LINESTRING Z(1 2 3,4 5 6)"))
        )
        .as_deref(),
        Some("<LineSet vertexCount='2'><Coordinate point='1 2 3 4 5 6' /></LineSet>")
    );
    assert_eq!(
        value(
            &e,
            &format!("st:ST_AsX3D({})", wkt("POLYGON((0 0,1 0,1 1,0 0))"))
        )
        .as_deref(),
        Some(
            "<IndexedFaceSet convex='false' coordIndex='0 1 2 -1'><Coordinate point='0 0 0 1 0 0 1 1 0' /></IndexedFaceSet>"
        )
    );
}

#[test]
fn coverages() {
    let e = Engine::new().unwrap();
    e.update(
        r#"PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX ex: <http://example.com/>
        INSERT DATA {
            ex:a geo:asWKT "POLYGON((0 0,1 0,1 1,0 1,0 0))"^^geo:wktLiteral .
            ex:b geo:asWKT "POLYGON((1 0,2 0,2 1,1 1,1 0))"^^geo:wktLiteral .
            ex:c geo:asWKT "POLYGON((1.5 0,3 0,3 1,1.5 1,1.5 0))"^^geo:wktLiteral .
        }"#,
    )
    .unwrap();
    let q = format!(
        "{PREFIXES} SELECT (st:ST_Area(st:ST_CoverageUnion(?g)) AS ?a) WHERE {{ ?f geo:asWKT ?g }}"
    );
    let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
        panic!()
    };
    let Some(Term::Literal(a)) = s.next().unwrap().unwrap().get("a").cloned() else {
        panic!()
    };
    assert_eq!(a.value().parse::<f64>().unwrap(), 3.0);
    let rows = |f: &str| -> Vec<(String, bool)> {
        let q = format!(
            "{PREFIXES} SELECT ?f ?out WHERE {{ SERVICE <http://w3id.org/tripletrail/st> {{
                [] st:function st:{f} ; st:geom ?out .
                {{ SELECT ?f ?geom WHERE {{ ?f geo:asWKT ?geom }} }} }} }} ORDER BY ?f"
        );
        let QueryResults::Solutions(s) = e.query(&q).unwrap() else {
            panic!()
        };
        s.map(|r| {
            let r = r.unwrap();
            (r.get("f").unwrap().to_string(), r.get("out").is_some())
        })
        .collect()
    };
    let invalid = rows("ST_CoverageInvalidEdges");
    assert_eq!(invalid.len(), 3);
    assert!(invalid[0].0.ends_with("a>") && !invalid[0].1);
    assert!(invalid[1].1 && invalid[2].1);
    let q = format!(
        "{PREFIXES} SELECT (SUM(st:ST_Area(?out)) AS ?total) WHERE {{ SERVICE <http://w3id.org/tripletrail/st> {{
            [] st:function st:ST_CoverageClean ; st:geom ?out .
            {{ SELECT ?f ?geom WHERE {{ ?f geo:asWKT ?geom }} }} }} }}"
    );
    let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
        panic!()
    };
    let Some(Term::Literal(t)) = s.next().unwrap().unwrap().get("total").cloned() else {
        panic!()
    };
    assert!((t.value().parse::<f64>().unwrap() - 3.0).abs() < 1e-9);
}

#[test]
fn box_constructors() {
    let e = Engine::new().unwrap();
    let text = |s: &str| format!("\"{s}\"");
    // box2d: the envelope polygon, as ST_MakeBox2D writes it; 2-D even for 3-D input.
    assert_eq!(
        value(&e, &format!("st:Box2D({})", wkt("LINESTRING(0 5,4 1)"))).as_deref(),
        Some("POLYGON((0 1,0 5,4 5,4 1,0 1))")
    );
    assert_eq!(
        value(
            &e,
            &format!("st:Box2D({})", wkt("LINESTRING Z(0 5 2,4 1 3)"))
        )
        .as_deref(),
        Some("POLYGON((0 1,0 5,4 5,4 1,0 1))")
    );
    let utm = "<http://www.opengis.net/def/crs/EPSG/0/25832>";
    assert_eq!(
        value(
            &e,
            &format!("st:Box2D({})", wkt(&format!("{utm} POINT(1 2)")))
        )
        .as_deref(),
        Some(format!("{utm} POLYGON((1 2,1 2,1 2,1 2,1 2))").as_str())
    );
    assert_eq!(
        value(&e, &format!("st:Box2D({})", text("BOX(3 4,1 2)"))).as_deref(),
        Some("POLYGON((1 2,1 4,3 4,3 2,1 2))")
    );
    assert_eq!(
        value(&e, &format!("st:Box2D({})", text("BOX3D(1 2 3,4 5 6)"))).as_deref(),
        Some("POLYGON((1 2,1 5,4 5,4 2,1 2))")
    );
    // box3d: the line from the lowest to the highest corner, as ST_3DExtent.
    assert_eq!(
        value(
            &e,
            &format!("st:Box3D({})", wkt("LINESTRING Z(0 5 -1,4 4 10)"))
        )
        .as_deref(),
        Some("LINESTRING Z(0 4 -1,4 5 10)")
    );
    assert_eq!(
        value(&e, &format!("st:Box3D({})", wkt("POINT(1 2)"))).as_deref(),
        Some("LINESTRING Z(1 2 0,1 2 0)")
    );
    assert_eq!(
        value(
            &e,
            &format!(
                "st:Box3D({})",
                wkt(&format!("{utm} POLYGON((0 0,2 0,2 3,0 0))"))
            )
        )
        .as_deref(),
        Some(format!("{utm} LINESTRING Z(0 0 0,2 3 0)").as_str())
    );
    assert_eq!(
        value(&e, &format!("st:Box3D({})", text("BOX3D(4 5 6,1 2 3)"))).as_deref(),
        Some("LINESTRING Z(1 2 3,4 5 6)")
    );
    assert_eq!(
        value(&e, &format!("st:Box3D({})", text("BOX(1 2,3 4)"))).as_deref(),
        Some("LINESTRING Z(1 2 0,3 4 0)")
    );
    // A box2d of a box3d.
    assert_eq!(
        value(
            &e,
            &format!("st:Box2D(st:Box3D({}))", wkt("POINT Z(1 2 3)"))
        )
        .as_deref(),
        Some("POLYGON((1 2,1 2,1 2,1 2,1 2))")
    );
    // Not a geometry or a box; empty.
    assert_eq!(value(&e, &format!("st:Box2D({})", text("BOX(1 2)"))), None);
    assert_eq!(value(&e, &format!("st:Box3D({})", text("nonsense"))), None);
    assert_eq!(
        value(&e, &format!("st:Box2D({})", wkt("POINT EMPTY"))),
        None
    );
}

#[test]
fn srs_lookup() {
    let e = Engine::new().unwrap();
    let wkt_of = value(&e, "st:srs(\"EPSG:25832\")").unwrap();
    assert!(
        wkt_of.starts_with(r#"PROJCS["ETRS89 / UTM zone 32N""#),
        "{wkt_of}"
    );
    assert_eq!(
        value(
            &e,
            "st:srs(<http://www.opengis.net/def/crs/EPSG/0/3035>, \"proj4text\")"
        )
        .as_deref(),
        Some(
            "+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +ellps=GRS80 +towgs84=0,0,0,0,0,0,0 +units=m +no_defs"
        )
    );
    assert_eq!(
        value(&e, "st:srs(32633, \"srname\")").as_deref(),
        Some("WGS 84 / UTM zone 33N")
    );
    assert_eq!(
        value(&e, "st:srs(\"EPSG:3006\", \"point_ne\")").as_deref(),
        Some("POINT(24.17 69.07)")
    );
    assert_eq!(value(&e, "st:srs(\"EPSG:4269\")"), None);
    assert_eq!(
        value(&e, "st:srs(\"EPSG:2154\", \"srname\")").as_deref(),
        Some("RGF93 v1 / Lambert-93")
    );
}
