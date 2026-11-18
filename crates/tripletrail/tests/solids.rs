//! Polyhedral surfaces, TINs and solids (SFCGAL-style 3-D functions) through SPARQL.

use tripletrail::Engine;
use tripletrail::oxigraph::model::Term;
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

fn text(e: &Engine, expr: &str) -> String {
    value(e, expr).unwrap_or_else(|| panic!("{expr} unbound"))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// The unit cube [0, 1]³ with outward faces.
const CUBE: &str = "POLYHEDRALSURFACE Z(\
    ((0 0 0,0 1 0,1 1 0,1 0 0,0 0 0)),\
    ((0 0 1,1 0 1,1 1 1,0 1 1,0 0 1)),\
    ((0 0 0,1 0 0,1 0 1,0 0 1,0 0 0)),\
    ((0 1 0,0 1 1,1 1 1,1 1 0,0 1 0)),\
    ((0 0 0,0 0 1,0 1 1,0 1 0,0 0 0)),\
    ((1 0 0,1 1 0,1 1 1,1 0 1,1 0 0)))";

/// A cube of side 1 from `(o, o, o)`, as an extruded square.
fn cube_at(o: f64) -> String {
    let p = o + 1.0;
    format!(
        "st:ST_Extrude({}, 0, 0, 1)",
        wkt(&format!(
            "POLYGON Z(({o} {o} {o},{p} {o} {o},{p} {p} {o},{o} {p} {o},{o} {o} {o}))"
        ))
    )
}

#[test]
fn volumes() {
    let e = Engine::new().unwrap();
    let cube = wkt(CUBE);
    // A surface is not a solid until made one, and has no volume until then (as in PostGIS).
    assert_eq!(text(&e, &format!("st:ST_IsSolid({cube})")), "false");
    assert_eq!(num(&e, &format!("st:ST_Volume({cube})")), 0.0);
    assert_eq!(text(&e, &format!("st:ST_IsClosed({cube})")), "true");
    let solid = format!("st:ST_MakeSolid({cube})");
    assert_eq!(text(&e, &format!("st:ST_IsSolid({solid})")), "true");
    assert!(close(num(&e, &format!("st:ST_Volume({solid})")), 1.0));
    assert!(close(num(&e, &format!("st:ST_3DArea({cube})")), 6.0));
    assert_eq!(num(&e, &format!("st:ST_ZMax({cube})")), 1.0);
    assert_eq!(num(&e, &format!("st:ST_ZMin({cube})")), 0.0);
    assert_eq!(num(&e, &format!("st:ST_Dimension({solid})")), 3.0);
    assert_eq!(num(&e, &format!("st:ST_Dimension({cube})")), 2.0);
    // 2-D functions see the footprint.
    let square = wkt("POLYGON((0 0,1 0,1 1,0 1,0 0))");
    assert!(close(
        num(&e, &format!("st:ST_Area({cube})")),
        num(&e, &format!("st:ST_Area({square})"))
    ));
    assert_eq!(
        text(
            &e,
            &format!("st:ST_Intersects({cube}, {})", wkt("POINT(0.5 0.5)"))
        ),
        "true"
    );

    // An extruded 2 x 3 rectangle, 4 high; the result of ST_Extrude is a solid.
    for ring in [
        "0 0,2 0,2 3,0 3,0 0", // counter-clockwise
        "0 0,0 3,2 3,2 0,0 0", // clockwise
    ] {
        let prism = format!(
            "st:ST_Extrude({}, 0, 0, 4)",
            wkt(&format!("POLYGON(({ring}))"))
        );
        assert_eq!(text(&e, &format!("st:ST_IsSolid({prism})")), "true");
        assert!(close(num(&e, &format!("st:ST_Volume({prism})")), 24.0));
        assert_eq!(num(&e, &format!("st:ST_NumPatches({prism})")), 6.0);
        assert!(close(
            num(&e, &format!("st:ST_3DArea({prism})")),
            2.0 * 6.0 + 4.0 * 10.0
        ));
    }
    // A slanted extrusion keeps the volume (Cavalieri), and a hole is cut out.
    let holed = wkt("POLYGON((0 0,4 0,4 4,0 4,0 0),(1 1,1 3,3 3,3 1,1 1))");
    assert!(close(
        num(
            &e,
            &format!("st:ST_Volume(st:ST_Extrude({holed}, 1, 2, 3))")
        ),
        12.0 * 3.0
    ));
    assert_eq!(
        num(
            &e,
            &format!("st:ST_NumPatches(st:ST_Extrude({holed}, 0, 0, 1))")
        ),
        10.0
    );
    // A point sweeps a (vertical) line, a line a surface.
    assert_eq!(
        text(
            &e,
            &format!("st:ST_Extrude({}, 0, 0, 5)", wkt("POINT(1 2)"))
        ),
        "LINESTRING Z(1 2 0,1 2 5)"
    );
    let wall = format!("st:ST_Extrude({}, 0, 0, 2)", wkt("LINESTRING(0 0,3 0,3 4)"));
    assert_eq!(num(&e, &format!("st:ST_NumPatches({wall})")), 2.0);
    assert_eq!(text(&e, &format!("st:ST_IsSolid({wall})")), "false");
    assert!(close(num(&e, &format!("st:ST_3DArea({wall})")), 14.0));
}

#[test]
fn boolean_operations_of_cubes() {
    let e = Engine::new().unwrap();
    let (a, b) = (cube_at(0.0), cube_at(0.5));
    let volume = |op: &str| num(&e, &format!("st:ST_Volume(st:{op}({a}, {b}))"));
    // Two unit cubes overlapping in a cube of side 0.5.
    assert!(
        close(volume("ST_3DUnion"), 1.875),
        "{}",
        volume("ST_3DUnion")
    );
    assert!(close(volume("ST_3DIntersection"), 0.125));
    assert!(close(volume("ST_3DDifference"), 0.875));
    assert_eq!(
        text(&e, &format!("st:ST_IsSolid(st:ST_3DUnion({a}, {b}))")),
        "true"
    );
    // Coplanar faces are merged: the intersection is a cube with six faces.
    assert_eq!(
        num(
            &e,
            &format!("st:ST_NumPatches(st:ST_3DIntersection({a}, {b}))")
        ),
        6.0
    );
    // The same with the cube given as a polyhedral surface made solid.
    let cube = format!("st:ST_MakeSolid({})", wkt(CUBE));
    assert!(close(
        num(
            &e,
            &format!("st:ST_Volume(st:ST_3DDifference({cube}, {b}))")
        ),
        0.875
    ));
    // Disjoint: an empty intersection, a union of both volumes.
    let far = cube_at(5.0);
    assert_eq!(
        text(
            &e,
            &format!("st:ST_IsEmpty(st:ST_3DIntersection({a}, {far}))")
        ),
        "true"
    );
    assert!(close(
        num(&e, &format!("st:ST_Volume(st:ST_3DUnion({a}, {far}))")),
        2.0
    ));
    // A line through a solid is cut to the part inside, or outside.
    let line = wkt("LINESTRING Z(-1 0.5 0.5,2 0.5 0.5)");
    assert_eq!(
        text(&e, &format!("st:ST_3DIntersection({a}, {line})")),
        "LINESTRING Z(0 0.5 0.5,1 0.5 0.5)"
    );
    assert_eq!(
        num(
            &e,
            &format!("st:ST_NumGeometries(st:ST_3DDifference({line}, {a}))")
        ),
        2.0
    );
    // Points inside or on the boundary are kept.
    assert_eq!(
        text(
            &e,
            &format!(
                "st:ST_3DIntersection({}, {a})",
                wkt("MULTIPOINT Z((0.5 0.5 0.5),(2 2 2),(1 0.5 0.5))")
            )
        ),
        "MULTIPOINT Z((0.5 0.5 0.5),(1 0.5 0.5))"
    );
    // A horizontal square across the cube is cut to the cube's section.
    let sheet = wkt("POLYGON Z((-1 -1 0.5,2 -1 0.5,2 2 0.5,-1 2 0.5,-1 -1 0.5))");
    assert!(close(
        num(
            &e,
            &format!("st:ST_3DArea(st:ST_3DIntersection({sheet}, {a}))")
        ),
        1.0
    ));
    assert!(close(
        num(
            &e,
            &format!("st:ST_3DArea(st:ST_3DDifference({sheet}, {a}))")
        ),
        8.0
    ));
    // Without solids, 2-D polygons combine in 2-D.
    assert!(close(
        num(
            &e,
            &format!(
                "st:ST_Area(st:ST_3DUnion({}, {}))",
                wkt("POLYGON((0 0,2 0,2 2,0 2,0 0))"),
                wkt("POLYGON((1 1,3 1,3 3,1 3,1 1))")
            )
        ),
        7.0
    ));
}

#[test]
fn convex_hull_3d() {
    let e = Engine::new().unwrap();
    // The corners of the unit cube, its centre, and points on its faces and edges.
    let cloud = wkt(
        "MULTIPOINT Z((0 0 0),(1 0 0),(0 1 0),(1 1 0),(0 0 1),(1 0 1),(0 1 1),\
        (1 1 1),(0.5 0.5 0.5),(0.5 0.5 0),(0.5 0.5 1),(0.5 0 0),(0.2 0.7 0.1))",
    );
    let hull = format!("st:ST_3DConvexHull({cloud})");
    assert_eq!(
        text(&e, &format!("st:ST_GeometryType({hull})")),
        "ST_PolyhedralSurface"
    );
    // Twelve triangles (two per face); points on the faces are not vertices.
    assert_eq!(num(&e, &format!("st:ST_NumPatches({hull})")), 12.0);
    assert!(close(
        num(&e, &format!("st:ST_Volume(st:ST_MakeSolid({hull}))")),
        1.0
    ));
    assert!(close(num(&e, &format!("st:ST_3DArea({hull})")), 6.0));
    assert_eq!(text(&e, &format!("st:ST_IsClosed({hull})")), "true");
    // A tetrahedron with a point inside.
    let tetra = wkt("MULTIPOINT Z((0 0 0),(6 0 0),(0 6 0),(0 0 6),(1 1 1))");
    assert!(close(
        num(
            &e,
            &format!("st:ST_Volume(st:ST_MakeSolid(st:ST_3DConvexHull({tetra})))")
        ),
        36.0
    ));
    // Degenerate inputs: coplanar points give a polygon, collinear ones a segment.
    assert_eq!(
        text(
            &e,
            &format!(
                "st:ST_GeometryType(st:ST_3DConvexHull({}))",
                wkt("MULTIPOINT Z((0 0 5),(1 0 5),(0 1 5),(0.2 0.2 5),(1 1 5))")
            )
        ),
        "ST_Polygon"
    );
    assert_eq!(
        text(
            &e,
            &format!(
                "st:ST_3DConvexHull({})",
                wkt("MULTIPOINT Z((1 1 1),(0 0 0),(2 2 2),(0.5 0.5 0.5))")
            )
        ),
        "LINESTRING Z(0 0 0,2 2 2)"
    );
}

#[test]
fn straight_skeleton() {
    let e = Engine::new().unwrap();
    // A 4 x 2 rectangle: four corner bisectors meeting a middle segment from (1 1) to (3 1).
    let rect = wkt("POLYGON((0 0,4 0,4 2,0 2,0 0))");
    let sk = format!("st:ST_StraightSkeleton({rect})");
    assert_eq!(num(&e, &format!("st:ST_NumGeometries({sk})")), 5.0);
    let len = num(&e, &format!("st:ST_Length({sk})"));
    assert!(close(len, 2.0 + 4.0 * 2f64.sqrt()), "{len}");
    for p in ["POINT(1 1)", "POINT(3 1)"] {
        assert_eq!(
            text(&e, &format!("st:ST_Intersects({sk}, {})", wkt(p))),
            "true"
        );
    }
    // An L of unit-wide arms: the reflex corner's bisector meets the corner's at (0.5 0.5),
    // from where the axes run to the ends of the arms.
    let l = wkt("POLYGON((0 0,2 0,2 1,1 1,1 2,0 2,0 0))");
    let sk = format!("st:ST_StraightSkeleton({l})");
    assert_eq!(num(&e, &format!("st:ST_NumGeometries({sk})")), 8.0);
    let len = num(&e, &format!("st:ST_Length({sk})"));
    assert!(close(len, 2.0 + 3.0 * 2f64.sqrt()), "{len}");
    for p in ["LINESTRING(0.5 0.5,1.5 0.5)", "LINESTRING(1 1,0.5 0.5)"] {
        assert_eq!(
            text(&e, &format!("st:ST_Covers({sk}, {})", wkt(p))),
            "true",
            "{p}"
        );
    }
    // A square with a square hole: the skeleton is the ring midway between them, plus the
    // corner bisectors.
    let frame = wkt("POLYGON((0 0,4 0,4 4,0 4,0 0),(1 1,1 3,3 3,3 1,1 1))");
    let len = num(
        &e,
        &format!("st:ST_Length(st:ST_StraightSkeleton({frame}))"),
    );
    // The middle ring, 4 x 3 = 12, and eight half-diagonals of length sqrt(0.5).
    assert!(close(len, 12.0 + 8.0 * 0.5f64.sqrt()), "{len}");
    // A rectangle with a notch exercises a split event.
    let notch = wkt("POLYGON((0 0,10 0,10 4,6 4,5 2,4 4,0 4,0 0))");
    let sk = num(
        &e,
        &format!("st:ST_NumGeometries(st:ST_StraightSkeleton({notch}))"),
    );
    assert!(sk >= 10.0, "{sk}");
}

#[test]
fn patches() {
    let e = Engine::new().unwrap();
    let cube = wkt(CUBE);
    assert_eq!(num(&e, &format!("st:ST_NumPatches({cube})")), 6.0);
    assert_eq!(num(&e, &format!("st:ST_NumGeometries({cube})")), 6.0);
    assert_eq!(
        text(&e, &format!("st:ST_PatchN({cube}, 3)")),
        "POLYGON Z((0 0 0,1 0 0,1 0 1,0 0 1,0 0 0))"
    );
    assert_eq!(
        text(&e, &format!("st:ST_GeometryN({cube}, 2)")),
        "POLYGON Z((0 0 1,1 0 1,1 1 1,0 1 1,0 0 1))"
    );
    assert!(value(&e, &format!("st:ST_PatchN({cube}, 7)")).is_none());
    assert!(value(&e, &format!("st:ST_PatchN({cube}, 0)")).is_none());
    // Not a polyhedral surface: no patches.
    assert!(
        value(
            &e,
            &format!("st:ST_NumPatches({})", wkt("POLYGON((0 0,1 0,1 1,0 0))"))
        )
        .is_none()
    );
    let tin = wkt("TIN Z(((0 0 0,1 0 0,0 1 1,0 0 0)),((1 0 0,1 1 1,0 1 1,1 0 0)))");
    assert_eq!(num(&e, &format!("st:ST_NumPatches({tin})")), 2.0);
    assert_eq!(
        text(&e, &format!("st:ST_PatchN({tin}, 2)")),
        "TRIANGLE Z((1 0 0,1 1 1,0 1 1,1 0 0))"
    );
    assert_eq!(text(&e, &format!("st:ST_GeometryType({tin})")), "ST_Tin");
    assert_eq!(
        text(&e, &format!("st:GeometryType({cube})")),
        "POLYHEDRALSURFACE"
    );
    assert_eq!(
        text(&e, &format!("geof:geometryType({tin})")),
        "http://www.opengis.net/ont/sf#TIN"
    );
    // ST_Tesselate returns a TIN, in 2-D for 2-D input.
    let square = wkt("POLYGON((0 0,1 0,1 1,0 1,0 0))");
    let t = format!("st:ST_Tesselate({square})");
    assert_eq!(text(&e, &format!("st:ST_GeometryType({t})")), "ST_Tin");
    assert_eq!(num(&e, &format!("st:ST_NumPatches({t})")), 2.0);
    assert!(text(&e, &format!("st:ST_AsText({t})")).starts_with("TIN((("));
    // ... and tesselates each face of a polyhedral surface in 3-D.
    let t = format!("st:ST_Tesselate({cube})");
    assert_eq!(num(&e, &format!("st:ST_NumPatches({t})")), 12.0);
    assert!(close(num(&e, &format!("st:ST_3DArea({t})")), 6.0));
    assert!(close(
        num(&e, &format!("st:ST_Volume(st:ST_MakeSolid({t}))")),
        1.0
    ));
}

#[test]
fn wkt_and_wkb_round_trips() {
    let e = Engine::new().unwrap();
    for (input, output) in [
        (CUBE, CUBE.replace("    ", "")),
        (
            "TIN Z (((0 0 0, 1 0 0, 0 1 1, 0 0 0)))",
            "TIN Z(((0 0 0,1 0 0,0 1 1,0 0 0)))".into(),
        ),
        (
            "TIN(((0 0,1 0,0 1,0 0)),((1 0,1 1,0 1,1 0)))",
            "TIN(((0 0,1 0,0 1,0 0)),((1 0,1 1,0 1,1 0)))".into(),
        ),
        (
            "TRIANGLE Z((0 0 1,1 0 2,0 1 3,0 0 1))",
            "TRIANGLE Z((0 0 1,1 0 2,0 1 3,0 0 1))".into(),
        ),
        (
            "polyhedralsurface z (((0 0 0,1 0 0,1 0 1,0 0 0),(0.2 0 0.1,0.8 0 0.7,0.8 0 0.1,0.2 0 0.1)))",
            "POLYHEDRALSURFACE Z(((0 0 0,1 0 0,1 0 1,0 0 0),(0.2 0 0.1,0.8 0 0.7,0.8 0 0.1,0.2 0 0.1)))".into(),
        ),
        ("TIN EMPTY", "TIN EMPTY".into()),
        ("POLYHEDRALSURFACE Z EMPTY", "POLYHEDRALSURFACE Z EMPTY".into()),
    ] {
        let lit = wkt(input);
        // Through the literal, ST_AsText, and WKB and EWKB.
        assert_eq!(text(&e, &format!("geof:asWKT({lit})")), output);
        assert_eq!(text(&e, &format!("st:ST_AsText({lit})")), output);
        assert_eq!(
            text(&e, &format!("st:ST_AsText(st:ST_GeomFromWKB(st:ST_AsBinary({lit})))")),
            output
        );
        assert_eq!(
            text(&e, &format!("st:ST_AsText(st:ST_GeomFromEWKB(st:ST_AsEWKB({lit})))")),
            output
        );
    }
    // The solid flag: the literal is SFCGAL's SOLID, which keeps it; ST_AsText prints a
    // polyhedral surface as PostGIS does, and WKB (like PostGIS's) drops the flag.
    let solid = text(&e, &format!("st:ST_MakeSolid({})", wkt(CUBE)));
    assert!(solid.starts_with("SOLID Z((((0 0 0,0 1 0,"), "{solid}");
    assert_eq!(text(&e, &format!("st:ST_IsSolid({})", wkt(&solid))), "true");
    assert!(close(
        num(&e, &format!("st:ST_Volume({})", wkt(&solid))),
        1.0
    ));
    assert_eq!(
        text(&e, &format!("st:ST_AsText({})", wkt(&solid))),
        CUBE.replace("    ", "")
    );
    assert_eq!(
        text(
            &e,
            &format!(
                "st:ST_IsSolid(st:ST_GeomFromWKB(st:ST_AsBinary({})))",
                wkt(&solid)
            )
        ),
        "false"
    );
    // ISO WKB type 1016 (TIN Z) and 1017 (TRIANGLE Z).
    let hex = text(
        &e,
        &format!(
            "st:ST_AsBinary({})",
            wkt("TRIANGLE Z((0 0 1,1 0 2,0 1 3,0 0 1))")
        ),
    );
    assert!(hex.starts_with("01F9030000"), "{hex}");
    // A CRS is kept, and EPSG:4326 coordinates are latitude first in the text.
    let ll = "<http://www.opengis.net/def/crs/EPSG/0/4326> TIN Z(((50 4 1,50 5 2,51 4 3,50 4 1)))";
    assert_eq!(text(&e, &format!("geof:asWKT({})", wkt(ll))), ll);
    // Invalid: an open triangle with too few points, a TIN patch that is not a triangle,
    // mixed dimensions, measures.
    for bad in [
        "TRIANGLE Z((0 0 0,1 0 0))",
        "TIN Z(((0 0 0,1 0 0,1 1 0,0 1 0,0 0 0)))",
        "POLYHEDRALSURFACE(((0 0,1 0 1,0 1,0 0)))",
        "TIN M(((0 0 0,1 0 0,0 1 0,0 0 0)))",
    ] {
        assert!(
            value(&e, &format!("st:ST_AsText({})", wkt(bad))).is_none(),
            "{bad}"
        );
    }
}
