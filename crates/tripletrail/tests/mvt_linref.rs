//! Vector tiles (`st:ST_AsMVTGeom`, `st:ST_AsMVT`), `rail:` linear referencing, alpha shapes,
//! conflation and `st:ST_3DExtent`, through SPARQL. Tiles are decoded back with the small
//! protobuf reader below to check the MVT 2.1 encoding.

use tripletrail::Engine;
use tripletrail::oxigraph::model::Term;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX st: <http://w3id.org/tripletrail/st#>
PREFIX rail: <http://w3id.org/tripletrail/rail#>
PREFIX ex: <http://example.com/>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
"#;

const UTM: &str = "<http://www.opengis.net/def/crs/EPSG/0/25832>";
const MERC: &str = "<http://www.opengis.net/def/crs/EPSG/0/3857>";

fn value(e: &Engine, expr: &str) -> Option<String> {
    let q = format!("{PREFIXES} SELECT ({expr} AS ?v) WHERE {{}}");
    one(e, &q, "v")
}

fn one(e: &Engine, q: &str, var: &str) -> Option<String> {
    let QueryResults::Solutions(mut s) = e.query(q).unwrap() else {
        panic!()
    };
    s.next().unwrap().unwrap().get(var).map(|t| match t {
        Term::Literal(l) => l.value().to_owned(),
        other => other.to_string(),
    })
}

fn rows(e: &Engine, body: &str, vars: &[&str]) -> Vec<Vec<String>> {
    let q = format!("{PREFIXES} SELECT * WHERE {{ {body} }}");
    let QueryResults::Solutions(s) = e.query(&q).unwrap() else {
        panic!()
    };
    let mut out: Vec<Vec<String>> = s
        .map(|s| {
            let s = s.unwrap();
            vars.iter()
                .map(|v| match s.get(*v) {
                    Some(Term::Literal(l)) => l.value().to_owned(),
                    Some(Term::NamedNode(n)) => n.as_str().to_owned(),
                    Some(t) => t.to_string(),
                    None => "-".into(),
                })
                .collect()
        })
        .collect();
    out.sort();
    out
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

fn close(e: &Engine, expr: &str, expected: f64, tol: f64) {
    let v = num(e, expr);
    assert!(
        (v - expected).abs() <= tol,
        "{expr} = {v}, expected {expected}"
    );
}

// --- A minimal MVT decoder -----------------------------------------------------------------

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }
    fn done(&self) -> bool {
        self.i >= self.b.len()
    }
    fn varint(&mut self) -> u64 {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.b[self.i];
            self.i += 1;
            v |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return v;
            }
            shift += 7;
        }
    }
    /// (field, wire type, varint value or payload).
    fn field(&mut self) -> (u64, u64, Field<'a>) {
        let key = self.varint();
        let (field, wire) = (key >> 3, key & 7);
        let v = match wire {
            0 => Field::Varint(self.varint()),
            1 => {
                let v = f64::from_le_bytes(self.b[self.i..self.i + 8].try_into().unwrap());
                self.i += 8;
                Field::Double(v)
            }
            2 => {
                let len = self.varint() as usize;
                let s = &self.b[self.i..self.i + len];
                self.i += len;
                Field::Bytes(s)
            }
            other => panic!("unexpected wire type {other}"),
        };
        (field, wire, v)
    }
}

enum Field<'a> {
    Varint(u64),
    Double(f64),
    Bytes(&'a [u8]),
}

fn packed(b: &[u8]) -> Vec<u32> {
    let mut r = Reader::new(b);
    let mut out = Vec::new();
    while !r.done() {
        out.push(r.varint() as u32);
    }
    out
}

#[derive(Debug, Default)]
struct DFeature {
    id: Option<u64>,
    tags: Vec<u32>,
    kind: u64,
    geometry: Vec<u32>,
}

#[derive(Debug, Default)]
struct DLayer {
    version: u64,
    name: String,
    features: Vec<DFeature>,
    keys: Vec<String>,
    values: Vec<String>,
    extent: u64,
}

fn decode(hex: &str) -> Vec<DLayer> {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let mut tile = Reader::new(&bytes);
    let mut layers = Vec::new();
    while !tile.done() {
        let (f, _, Field::Bytes(lb)) = tile.field() else {
            panic!("tile field")
        };
        assert_eq!(f, 3);
        let mut layer = DLayer {
            extent: 4096,
            ..Default::default()
        };
        let mut r = Reader::new(lb);
        while !r.done() {
            match r.field() {
                (15, 0, Field::Varint(v)) => layer.version = v,
                (1, 2, Field::Bytes(s)) => layer.name = String::from_utf8(s.to_vec()).unwrap(),
                (3, 2, Field::Bytes(s)) => layer.keys.push(String::from_utf8(s.to_vec()).unwrap()),
                (5, 0, Field::Varint(v)) => layer.extent = v,
                (4, 2, Field::Bytes(s)) => {
                    let mut vr = Reader::new(s);
                    let v = match vr.field() {
                        (1, 2, Field::Bytes(s)) => {
                            format!("s:{}", String::from_utf8(s.to_vec()).unwrap())
                        }
                        (3, 1, Field::Double(d)) => format!("d:{d}"),
                        (5, 0, Field::Varint(v)) => format!("u:{v}"),
                        (6, 0, Field::Varint(v)) => {
                            format!("i:{}", ((v >> 1) as i64) ^ -((v & 1) as i64))
                        }
                        (7, 0, Field::Varint(v)) => format!("b:{}", v != 0),
                        (f, w, _) => panic!("value field {f}/{w}"),
                    };
                    assert!(vr.done(), "one value per Value message");
                    layer.values.push(v);
                }
                (2, 2, Field::Bytes(fb)) => {
                    let mut feature = DFeature::default();
                    let mut fr = Reader::new(fb);
                    while !fr.done() {
                        match fr.field() {
                            (1, 0, Field::Varint(v)) => feature.id = Some(v),
                            (2, 2, Field::Bytes(s)) => feature.tags = packed(s),
                            (3, 0, Field::Varint(v)) => feature.kind = v,
                            (4, 2, Field::Bytes(s)) => feature.geometry = packed(s),
                            (f, w, _) => panic!("feature field {f}/{w}"),
                        }
                    }
                    layer.features.push(feature);
                }
                (f, w, _) => panic!("layer field {f}/{w}"),
            }
        }
        layers.push(layer);
    }
    layers
}

fn unzigzag(v: u32) -> i64 {
    i64::from((v >> 1) as i32 ^ -((v & 1) as i32))
}

/// The paths of a feature's geometry in absolute tile coordinates, checking the command
/// structure the spec requires for its type. Rings come back without their closing point.
fn paths(f: &DFeature) -> Vec<Vec<(i64, i64)>> {
    let g = &f.geometry;
    let (mut x, mut y) = (0i64, 0i64);
    let mut out: Vec<Vec<(i64, i64)>> = Vec::new();
    let mut i = 0;
    while i < g.len() {
        let (id, count) = (g[i] & 7, (g[i] >> 3) as usize);
        i += 1;
        match id {
            1 => {
                if f.kind != 1 {
                    assert_eq!(count, 1, "MoveTo of a line or ring moves to one point");
                }
                for _ in 0..count {
                    x += unzigzag(g[i]);
                    y += unzigzag(g[i + 1]);
                    i += 2;
                    out.push(vec![(x, y)]);
                }
            }
            2 => {
                assert!(f.kind == 2 || f.kind == 3, "LineTo in a point feature");
                assert!(count >= 1);
                for _ in 0..count {
                    let (dx, dy) = (unzigzag(g[i]), unzigzag(g[i + 1]));
                    assert!(dx != 0 || dy != 0, "LineTo with a zero delta");
                    x += dx;
                    y += dy;
                    i += 2;
                    out.last_mut().unwrap().push((x, y));
                }
            }
            7 => {
                assert_eq!(f.kind, 3, "ClosePath outside a polygon");
                assert_eq!(count, 1);
                assert!(
                    out.last().unwrap().len() >= 3,
                    "a ring has at least 3 points"
                );
            }
            other => panic!("unknown command {other}"),
        }
    }
    if f.kind == 3 {
        // Every ring ends with ClosePath.
        let closes = (0..g.len()).filter(|&k| g[k] == 15).count();
        assert!(closes >= out.len());
    }
    out
}

/// Surveyor's formula in tile coordinates.
fn area(ring: &[(i64, i64)]) -> i64 {
    let n = ring.len();
    (0..n)
        .map(|k| {
            let (a, b) = (ring[k], ring[(k + 1) % n]);
            a.0 * b.1 - b.0 * a.1
        })
        .sum::<i64>()
}

// --- Vector tiles --------------------------------------------------------------------------

#[test]
fn as_mvt_geom() {
    let e = Engine::new().unwrap();
    let bounds = wkt(&format!(
        "{MERC} POLYGON((0 0,4096 0,4096 4096,0 4096,0 0))"
    ));
    let g = |w: &str, extra: &str| {
        value(
            &e,
            &format!(
                "st:ST_AsMVTGeom({}, {bounds}{extra})",
                wkt(&format!("{MERC} {w}"))
            ),
        )
    };
    // Y is flipped and coordinates are snapped.
    assert_eq!(g("POINT(10.4 4000.2)", "").as_deref(), Some("POINT(10 96)"));
    // Clipped to the tile plus the buffer.
    assert_eq!(
        g("LINESTRING(-1000 2048,5000 2048)", "").as_deref(),
        Some("LINESTRING(-256 2048,4352 2048)")
    );
    assert_eq!(
        g("LINESTRING(-1000 2048,5000 2048)", ", 4096, 0").as_deref(),
        Some("LINESTRING(0 2048,4096 2048)")
    );
    assert_eq!(
        g("LINESTRING(-1000 2048,5000 2048)", ", 4096, 0, false").as_deref(),
        Some("LINESTRING(-1000 2048,5000 2048)")
    );
    // Another extent scales.
    assert_eq!(
        g("POINT(2048 1024)", ", 256").as_deref(),
        Some("POINT(128 192)")
    );
    // Outside the buffered tile, or collapsing on the grid: no geometry.
    assert_eq!(g("POINT(9000 9000)", ""), None);
    assert_eq!(g("POLYGON((1 1,1.2 1,1.2 1.2,1 1))", ""), None);
    // Polygons keep their area; exterior rings have positive area in tile coordinates.
    let p = g("POLYGON((0 0,10 0,10 5,0 5,0 0))", "").unwrap();
    close(&e, &format!("st:ST_Area({})", wkt(&p)), 50.0, 0.0);
    assert_eq!(
        value(&e, &format!("st:ST_IsPolygonCCW({})", wkt(&p))).as_deref(),
        Some("true")
    );
    // A geographic geometry goes to the CRS of the bounds: (0, 0) is the centre of tile 0/0/0.
    assert_eq!(
        value(
            &e,
            &format!(
                "st:ST_AsMVTGeom({}, st:ST_TileEnvelope(0, 0, 0))",
                wkt("POINT(0 0)")
            )
        )
        .as_deref(),
        Some("POINT(2048 2048)")
    );
}

#[test]
fn as_mvt_aggregate() {
    let e = Engine::new().unwrap();
    let q = format!(
        r#"{PREFIXES} SELECT (st:ST_AsMVT(?t) AS ?mvt) WHERE {{
            VALUES ?g {{ {} {} {} }}
            BIND(st:ST_AsMVTGeom(?g, {}) AS ?t)
        }}"#,
        wkt(&format!("{MERC} POINT(25 4079)")),
        wkt(&format!("{MERC} LINESTRING(2 4094,2 4086,10 4086)")),
        wkt(&format!(
            "{MERC} POLYGON((100 100,200 100,200 200,100 200,100 100),(120 120,120 180,180 180,180 120,120 120))"
        )),
        wkt(&format!(
            "{MERC} POLYGON((0 0,4096 0,4096 4096,0 4096,0 0))"
        )),
    );
    let hex = one(&e, &q, "mvt").unwrap();
    let layers = decode(&hex);
    assert_eq!(layers.len(), 1);
    let l = &layers[0];
    assert_eq!((l.version, l.name.as_str(), l.extent), (2, "default", 4096));
    assert_eq!(l.features.len(), 3);
    let mut kinds: Vec<u64> = l.features.iter().map(|f| f.kind).collect();
    kinds.sort();
    assert_eq!(kinds, [1, 2, 3]);
    for f in &l.features {
        let p = paths(f);
        match f.kind {
            // The spec's examples: point (25, 17), line (2,2)-(2,10)-(10,10).
            1 => {
                assert_eq!(f.geometry, [9, 50, 34]);
                assert_eq!(p, [vec![(25, 17)]]);
            }
            2 => assert_eq!(f.geometry, [9, 4, 4, 18, 0, 16, 16, 0]),
            3 => {
                assert_eq!(p.len(), 2);
                assert!(area(&p[0]) > 0, "exterior ring clockwise on screen");
                assert!(area(&p[1]) < 0, "interior ring counter-clockwise on screen");
                assert_eq!(area(&p[0]), 2 * 100 * 100);
            }
            _ => unreachable!(),
        }
    }
    // No rows: an empty tile.
    let q = format!("{PREFIXES} SELECT (st:ST_AsMVT(?g) AS ?mvt) WHERE {{ VALUES ?g {{ }} }}");
    assert_eq!(one(&e, &q, "mvt").as_deref(), Some(""));
}

#[test]
fn as_mvt_service_with_attributes() {
    let e = Engine::new().unwrap();
    e.update(&format!(
        r#"{PREFIXES} INSERT DATA {{
            ex:a ex:n 7 ; ex:name "north" ; ex:speed 120 ; ex:electrified true ;
                 geo:hasGeometry [ geo:asWKT {} ] .
            ex:b ex:n 8 ; ex:name "south" ; ex:speed 80.5 ;
                 geo:hasGeometry [ geo:asWKT {} ] .
            ex:c ex:n 9 ; ex:name "far" ; geo:hasGeometry [ geo:asWKT {} ] .
        }}"#,
        wkt("LINESTRING(0 0,1 1)"),
        wkt("POLYGON((-1 -1,-0.5 -1,-0.5 -0.5,-1 -0.5,-1 -1))"),
        wkt("POINT(100 60)"),
    ))
    .unwrap();
    let body = r#"SERVICE <http://w3id.org/tripletrail/st> {
        [] st:function st:ST_AsMVT ; st:name "tracks" ; st:extent 4096 ; st:buffer 0 ;
           st:bounds ?bounds ; st:feature_id_name "n" ; st:mvt ?mvt .
        { SELECT ?n ?geom ?name ?speed ?electrified WHERE {
            ?f ex:n ?n ; ex:name ?name ; geo:hasGeometry/geo:asWKT ?geom .
            OPTIONAL { ?f ex:speed ?speed } OPTIONAL { ?f ex:electrified ?electrified } } }
    }"#;
    // Tile 1/1/0 covers the north-east quarter of the world; 2/1/1 and 2/2/1 the centre.
    let tile = |z: i64, x: i64, y: i64| {
        let q = format!(
            "{PREFIXES} SELECT ?mvt WHERE {{ {} }}",
            body.replace("?bounds", &format!("st:ST_TileEnvelope({z}, {x}, {y})"))
        );
        // st:bounds must be constant: substitute the envelope's value.
        let env = value(&e, &format!("st:ST_TileEnvelope({z}, {x}, {y})")).unwrap();
        let q = q.replace(
            &format!("st:ST_TileEnvelope({z}, {x}, {y})"),
            &format!("\"{env}\"^^geo:wktLiteral"),
        );
        decode(&one(&e, &q, "mvt").unwrap())
    };
    let layers = tile(2, 2, 1);
    assert_eq!(layers.len(), 1);
    let l = &layers[0];
    assert_eq!(l.name, "tracks");
    // Only the line reaches into the tile north-east of (0, 0), with the buffer.
    assert_eq!(l.features.len(), 1);
    let f = &l.features[0];
    assert_eq!(f.id, Some(7));
    assert_eq!(f.kind, 2);
    let props: Vec<(String, String)> = f
        .tags
        .chunks(2)
        .map(|kv| {
            (
                l.keys[kv[0] as usize].clone(),
                l.values[kv[1] as usize].clone(),
            )
        })
        .collect();
    assert_eq!(
        props,
        [
            ("name".to_owned(), "s:north".to_owned()),
            ("speed".to_owned(), "u:120".to_owned()),
            ("electrified".to_owned(), "b:true".to_owned()),
        ]
    );
    let p = paths(f);
    assert_eq!(p[0][0], (0, 4096));
    // The square south-west of (0, 0) in tile 2/1/2: a polygon with positive area.
    let l = &tile(2, 1, 2)[0];
    let poly: Vec<&DFeature> = l.features.iter().filter(|f| f.kind == 3).collect();
    assert_eq!(poly.len(), 1);
    assert_eq!(poly[0].id, Some(8));
    let rings = paths(poly[0]);
    assert!(area(&rings[0]) > 0);
    assert!(l.values.contains(&"d:80.5".to_owned()));
    // An empty tile when nothing is in it.
    let q = format!(
        "{PREFIXES} SELECT ?mvt WHERE {{ {} }}",
        body.replace(
            "?bounds",
            &wkt(&format!(
                "{MERC} POLYGON((0 -9000000,1000 -9000000,1000 -8999000,0 -8999000,0 -9000000))"
            ))
        )
    );
    assert_eq!(one(&e, &q, "mvt").as_deref(), Some(""));
}

// --- Linear referencing --------------------------------------------------------------------

#[test]
fn locate_and_intrinsic() {
    let e = Engine::new().unwrap();
    let line = wkt(&format!("{UTM} LINESTRING(0 0,100 0,100 100)"));
    assert_eq!(
        value(&e, &format!("rail:locate({line}, 0.75)")).as_deref(),
        Some(format!("{UTM} POINT(100 50)").as_str())
    );
    assert_eq!(value(&e, &format!("rail:locate({line}, 1.5)")), None);
    close(
        &e,
        &format!(
            "rail:intrinsicAt({line}, {})",
            wkt(&format!("{UTM} POINT(50 10)"))
        ),
        0.25,
        1e-12,
    );
    // Geodesic length on CRS84: a meridian degree is about twice a degree of longitude at
    // 61° N, so the middle of the line is on the first segment.
    let geo_line = wkt("LINESTRING(10 60,10 61,11 61)");
    let lat = num(&e, &format!("geof:latitude(rail:locate({geo_line}, 0.5))"));
    assert!(lat > 60.7 && lat < 60.8, "{lat}");
    close(
        &e,
        &format!("rail:intrinsicAt({geo_line}, rail:locate({geo_line}, 0.3))"),
        0.3,
        1e-9,
    );
    close(
        &e,
        &format!(
            "geof:metricLength(rail:offsetGeom({geo_line}, 0, 0.5, 0)) / geof:metricLength({geo_line})"
        ),
        0.5,
        1e-3,
    );
    // Z and M are interpolated.
    assert_eq!(
        value(
            &e,
            &format!(
                "rail:locate({}, 0.5)",
                wkt(&format!("{UTM} LINESTRING Z(0 0 10,100 0 20)"))
            )
        )
        .as_deref(),
        Some(format!("{UTM} POINT Z(50 0 15)").as_str())
    );
}

#[test]
fn kilometres() {
    let e = Engine::new().unwrap();
    let line = wkt(&format!("{UTM} LINESTRING(0 0,1000 0)"));
    let table = "\"0:12.300 0.5:12.950 1:13.600\"";
    close(
        &e,
        &format!("rail:kmAt({line}, 0.25, {table})"),
        12.625,
        1e-9,
    );
    close(
        &e,
        &format!("rail:intrinsicAtKm({line}, 13.275, {table})"),
        0.75,
        1e-9,
    );
    assert_eq!(
        value(&e, &format!("rail:intrinsicAtKm({line}, 14, {table})")),
        None
    );
    // A jump in kilometres and decreasing kilometres.
    let jump = "\"0:10.0 0.5:10.5 0.5:11.0 1:11.5\"";
    close(&e, &format!("rail:kmAt({line}, 0.5, {jump})"), 11.0, 1e-9);
    assert_eq!(
        value(&e, &format!("rail:intrinsicAtKm({line}, 10.7, {jump})")),
        None
    );
    close(
        &e,
        &format!("rail:intrinsicAtKm({line}, 4.25, \"0:5;1:4\")"),
        0.75,
        1e-9,
    );
    // The line's own M as kilometres.
    let m_line = wkt(&format!(
        "{UTM} LINESTRING M(0 0 12.3,500 0 12.8,1000 0 13.6)"
    ));
    close(&e, &format!("rail:kmAt({m_line}, 0.5)"), 12.8, 1e-9);
    close(
        &e,
        &format!("rail:intrinsicAtKm({m_line}, 13.2)"),
        0.75,
        1e-9,
    );
    // Kilometre posts as a separate measured line beside the track.
    let posts = wkt(&format!("{UTM} LINESTRING M(0 3 5,1000 3 6)"));
    close(&e, &format!("rail:kmAt({line}, 0.3, {posts})"), 5.3, 1e-9);
    // A malformed table is an error.
    assert_eq!(
        value(&e, &format!("rail:kmAt({line}, 0.3, \"1:4 0:5\")")),
        None
    );
}

#[test]
fn geometry_between_kilometres() {
    let e = Engine::new().unwrap();
    // Two elements in order; the kilometres cross the element boundary.
    let elements = wkt(&format!(
        "{UTM} MULTILINESTRING M((0 0 0,100 0 1),(100 0 1,200 0 2))"
    ));
    assert_eq!(
        value(&e, &format!("rail:geomBetweenKm({elements}, 0.5, 1.5)")).as_deref(),
        Some(format!("{UTM} LINESTRING M(50 0 0.5,100 0 1,150 0 1.5)").as_str())
    );
    let plain = wkt(&format!(
        "{UTM} MULTILINESTRING((0 0,100 0),(100 0,200 0,200 100))"
    ));
    let table = "\"0:10 0.5:10.2 1:10.3\"";
    assert_eq!(
        value(
            &e,
            &format!("rail:geomBetweenKm({plain}, 10.25, 10.1, {table})")
        )
        .as_deref(),
        Some(format!("{UTM} LINESTRING(75 0,100 0,200 0,200 25)").as_str())
    );
    // Elements that do not touch give one piece each.
    let gap = wkt(&format!("{UTM} MULTILINESTRING((0 0,100 0),(110 0,210 0))"));
    assert_eq!(
        value(
            &e,
            &format!("rail:geomBetweenKm({gap}, 0.5, 1.5, \"0:0 1:2\")")
        )
        .as_deref(),
        Some(format!("{UTM} MULTILINESTRING((50 0,100 0),(110 0,160 0))").as_str())
    );
}

#[test]
fn offset_geometry() {
    let e = Engine::new().unwrap();
    let line = wkt(&format!("{UTM} LINESTRING(0 0,100 0)"));
    assert_eq!(
        value(&e, &format!("rail:offsetGeom({line}, 0.25, 0.75, 5)")).as_deref(),
        Some(format!("{UTM} LINESTRING(25 5,75 5)").as_str())
    );
    assert_eq!(
        value(&e, &format!("rail:offsetGeom({line}, 0.25, 0.75, -5)")).as_deref(),
        Some(format!("{UTM} LINESTRING(25 -5,75 -5)").as_str())
    );
    // A position gives the point beside it.
    assert_eq!(
        value(&e, &format!("rail:offsetGeom({line}, 0.5, 0.5, 5)")).as_deref(),
        Some(format!("{UTM} POINT(50 5)").as_str())
    );
    // Metres on CRS84.
    let geo_line = wkt("LINESTRING(10 60,10.01 60)");
    close(
        &e,
        &format!("geof:distance(rail:offsetGeom({geo_line}, 0, 1, 10), {geo_line})"),
        10.0,
        0.05,
    );
}

// --- Aggregates, alpha shape, conflation ---------------------------------------------------

#[test]
fn extent_3d() {
    let e = Engine::new().unwrap();
    let q = |values: &str| {
        one(
            &e,
            &format!(
                "{PREFIXES} SELECT (st:ST_3DExtent(?g) AS ?v) WHERE {{ VALUES ?g {{ {values} }} }}"
            ),
            "v",
        )
    };
    assert_eq!(
        q(&format!(
            "{} {}",
            wkt("POINT Z(1 2 3)"),
            wkt("LINESTRING Z(0 5 -1,4 4 10)")
        ))
        .as_deref(),
        Some("LINESTRING Z(0 2 -1,4 5 10)")
    );
    assert_eq!(
        q(&format!("{} {}", wkt("POINT(1 2)"), wkt("POINT(3 4)"))).as_deref(),
        Some("LINESTRING Z(1 2 0,3 4 0)")
    );
}

#[test]
fn alpha_shape() {
    let e = Engine::new().unwrap();
    // A 5 x 5 grid of points one apart, with the middle row's points far to the east too.
    let mut pts: Vec<String> = Vec::new();
    for x in 0..5 {
        for y in 0..5 {
            pts.push(format!("({x} {y})"));
        }
    }
    pts.push("(20 2)".into());
    let mp = wkt(&format!("MULTIPOINT({})", pts.join(",")));
    // Small alpha: the 4 x 4 square of the grid; the far point is left out.
    close(
        &e,
        &format!("st:ST_Area(rail:alphaShape({mp}, 1))"),
        16.0,
        1e-9,
    );
    // Large alpha: the convex hull.
    close(
        &e,
        &format!("st:ST_Area(rail:alphaShape({mp}, 1e6))"),
        num(&e, &format!("st:ST_Area(st:ST_ConvexHull({mp}))")),
        1e-9,
    );
    assert_eq!(value(&e, &format!("rail:alphaShape({mp}, 0)")), None);
}

#[test]
fn conflate() {
    let e = Engine::new().unwrap();
    e.update(&format!(
        r#"{PREFIXES} INSERT DATA {{
            ex:t1 a ex:Target ; geo:hasGeometry [ geo:asWKT {} ] .
            ex:t2 a ex:Target ; geo:hasGeometry [ geo:asWKT {} ] .
            ex:s1 a ex:Source ; geo:hasGeometry [ geo:asWKT {} ] .
            ex:s2 a ex:Source ; geo:hasGeometry [ geo:asWKT {} ] .
            ex:s3 a ex:Source ; geo:hasGeometry [ geo:asWKT {} ] .
        }}"#,
        wkt(&format!("{UTM} LINESTRING(0 0,1000 0)")),
        wkt(&format!("{UTM} LINESTRING(0 500,1000 500)")),
        wkt(&format!("{UTM} LINESTRING(0 3,500 3,1000 2)")),
        wkt(&format!("{UTM} LINESTRING(500 -2,1500 -2)")),
        wkt(&format!("{UTM} LINESTRING(0 250,1000 250)")),
    ))
    .unwrap();
    let service = |params: &str| {
        format!(
            r#"SERVICE <http://w3id.org/tripletrail/st> {{
                [] st:function rail:conflate ; st:tolerance 10 ; {params}
                   st:source ?s ; st:target ?t ; st:distance ?d ; st:overlap ?o .
                {{ SELECT ?id ?geom WHERE {{ ?id a ex:Source ; geo:hasGeometry/geo:asWKT ?geom }} }}
                {{ SELECT ?id ?geom WHERE {{ ?id a ex:Target ; geo:hasGeometry/geo:asWKT ?geom }} }}
            }}"#
        )
    };
    let r = rows(&e, &service(""), &["s", "t", "d", "o"]);
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(
        r[0][..2],
        ["http://example.com/s1", "http://example.com/t1"]
    );
    // Fréchet on the lines densified to half the tolerance: about the 3 m between them.
    let d: f64 = r[0][2].parse().unwrap();
    assert!((3.0..4.0).contains(&d), "{d}");
    assert_eq!(r[0][3].parse::<f64>().unwrap(), 1.0);
    assert_eq!(
        r[1][..2],
        ["http://example.com/s2", "http://example.com/t1"]
    );
    assert!((r[1][3].parse::<f64>().unwrap() - 0.51).abs() < 0.01);
    // Hausdorff and a minimum overlap.
    let r = rows(
        &e,
        &service("st:method \"hausdorff\" ; st:min_overlap 0.9 ;"),
        &["s", "t", "d"],
    );
    assert_eq!(r.len(), 1);
    assert_eq!(
        r[0][..2],
        ["http://example.com/s1", "http://example.com/t1"]
    );
    let d: f64 = r[0][2].parse().unwrap();
    assert!((3.0..4.0).contains(&d), "{d}");
}
