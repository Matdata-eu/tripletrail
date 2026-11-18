//! FlatGeobuf, Geobuf, MARC21, `ST_Letters`, `ST_IsValidDetail`, transformation pipelines and
//! the aliases (`ST_GMLToSQL`, `ST_EstimatedExtent`) through SPARQL. Expected values are worked
//! out by hand from the specifications, or checked with the reference FlatGeobuf crate.

use flatgeobuf::{
    ColumnType, FallibleStreamingIterator, FgbCrs, FgbReader, FgbWriter, FgbWriterOptions,
    GeometryType,
};
use geozero::{ColumnValue, FeatureProperties, PropertyProcessor, ToWkt};
use std::io::Cursor;
use tripletrail::Engine;
use tripletrail::oxigraph::model::Term;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX st: <http://w3id.org/tripletrail/st#>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
PREFIX ex: <http://example.com/>
"#;

fn show(t: &Term) -> String {
    match t {
        Term::Literal(l) => l.value().to_owned(),
        Term::NamedNode(n) => n.as_str().to_owned(),
        other => other.to_string(),
    }
}

fn value(e: &Engine, expr: &str) -> Option<String> {
    let q = format!("{PREFIXES} SELECT ({expr} AS ?v) WHERE {{}}");
    let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
        panic!()
    };
    s.next().unwrap().unwrap().get("v").map(show)
}

fn num(e: &Engine, expr: &str) -> f64 {
    value(e, expr)
        .unwrap_or_else(|| panic!("{expr} unbound"))
        .parse()
        .unwrap()
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
                .map(|v| s.get(*v).map_or("-".into(), show))
                .collect()
        })
        .collect();
    out.sort();
    out
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn wkt(s: &str) -> String {
    format!("\"{s}\"^^geo:wktLiteral")
}

const FEATURES: &str = r#"{ SELECT ?geom ?name ?n WHERE {
    VALUES (?geom ?name ?n) {
        ("POINT(1 2)"^^geo:wktLiteral "a" 1)
        ("LINESTRING(0 0,3 4)"^^geo:wktLiteral "b" 2)
        ("POLYGON((10 10,12 10,12 12,10 10))"^^geo:wktLiteral "c" UNDEF)
    } } }"#;

/// The FlatGeobuf the `st` SERVICE writes for [`FEATURES`].
fn service_fgb(e: &Engine, index: bool) -> Vec<u8> {
    let r = rows(
        e,
        &format!(
            "SERVICE <http://w3id.org/tripletrail/st> {{
                [] st:function st:ST_AsFlatGeobuf ; st:index {index} ; st:flatgeobuf ?fgb .
                {FEATURES}
            }}"
        ),
        &["fgb"],
    );
    assert_eq!(r.len(), 1);
    hex(&r[0][0])
}

#[test]
fn flatgeobuf_is_read_by_the_reference_reader() {
    let e = Engine::new().unwrap();
    for index in [false, true] {
        let bytes = service_fgb(&e, index);
        // Magic bytes: "fgb", version 3, "fgb", 0.
        assert_eq!(to_hex(&bytes[..8]), "6667620366676200");
        let reader = FgbReader::open(Cursor::new(bytes.clone())).unwrap();
        let header = reader.header();
        assert_eq!(header.features_count(), 3);
        assert_eq!(header.geometry_type(), GeometryType::Unknown);
        assert_eq!(header.index_node_size(), if index { 16 } else { 0 });
        assert_eq!(header.crs().unwrap().code(), 4326);
        let cols: Vec<(String, ColumnType)> = header
            .columns()
            .unwrap()
            .iter()
            .map(|c| (c.name().to_owned(), c.type_()))
            .collect();
        assert_eq!(
            cols,
            [
                ("name".to_owned(), ColumnType::String),
                ("n".to_owned(), ColumnType::Long)
            ]
        );
        let env: Vec<f64> = header.envelope().unwrap().iter().collect();
        assert_eq!(env, [0.0, 0.0, 12.0, 12.0]);
        let mut features = reader.select_all().unwrap();
        let mut got = Vec::new();
        while let Some(f) = features.next().unwrap() {
            got.push((
                f.property::<String>("name").unwrap(),
                f.property::<i64>("n").ok(),
                f.to_wkt().unwrap(),
            ));
        }
        got.sort();
        assert_eq!(
            got,
            [
                ("a".into(), Some(1), "POINT(1 2)".into()),
                ("b".into(), Some(2), "LINESTRING(0 0,3 4)".into()),
                (
                    "c".into(),
                    None,
                    "POLYGON((10 10,12 10,12 12,10 10))".into()
                ),
            ]
        );
        if index {
            // The packed R-tree answers a box query.
            let reader = FgbReader::open(Cursor::new(bytes)).unwrap();
            let mut hits = reader.select_bbox(9.0, 9.0, 13.0, 13.0).unwrap();
            let mut names = Vec::new();
            while let Some(f) = hits.next().unwrap() {
                names.push(f.property::<String>("name").unwrap());
            }
            assert_eq!(names, ["c"]);
        }
    }
}

#[test]
fn flatgeobuf_aggregate_and_z() {
    let e = Engine::new().unwrap();
    let q = format!(
        "{PREFIXES} SELECT (st:ST_AsFlatGeobuf(?g) AS ?f) WHERE {{ VALUES ?g {{ {} {} }} }}",
        wkt("MULTIPOLYGON(((0 0,1 0,1 1,0 0)),((5 5,6 5,6 6,5 5)))"),
        wkt("MULTIPOLYGON(((7 7,8 7,8 8,7 7)))")
    );
    let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
        panic!()
    };
    let t = s.next().unwrap().unwrap().get("f").cloned().unwrap();
    let Term::Literal(l) = &t else { panic!() };
    assert_eq!(
        l.datatype().as_str(),
        "http://www.w3.org/2001/XMLSchema#hexBinary"
    );
    let mut r = FgbReader::open(Cursor::new(hex(l.value())))
        .unwrap()
        .select_all()
        .unwrap();
    let mut got = Vec::new();
    while let Some(f) = r.next().unwrap() {
        got.push(f.to_wkt().unwrap());
    }
    assert_eq!(
        got,
        [
            "MULTIPOLYGON(((0 0,1 0,1 1,0 0)),((5 5,6 5,6 6,5 5)))",
            "MULTIPOLYGON(((7 7,8 7,8 8,7 7)))"
        ]
    );

    // Z and M survive our own round trip.
    let r = rows(
        &e,
        r#"SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:ST_AsFlatGeobuf ; st:flatgeobuf ?fgb .
            { SELECT ?geom WHERE { VALUES ?geom { "LINESTRING ZM (0 0 5 1,3 4 6 2)"^^geo:wktLiteral } } }
        }"#,
        &["fgb"],
    );
    let back = rows(
        &e,
        &format!(
            r#"SERVICE <http://w3id.org/tripletrail/st> {{
                [] st:function st:ST_FromFlatGeobuf ; st:data "{}"^^xsd:hexBinary ; st:geom ?g .
            }}"#,
            r[0][0]
        ),
        &["g"],
    );
    assert_eq!(back, [["LINESTRING ZM(0 0 5 1,3 4 6 2)"]]);
}

#[test]
fn from_flatgeobuf_reads_the_reference_writer() {
    let mut w = FgbWriter::create_with_options(
        "test",
        GeometryType::Unknown,
        FgbWriterOptions {
            write_index: true,
            detect_type: false,
            promote_to_multi: false,
            crs: FgbCrs {
                code: 3857,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    w.add_column("name", ColumnType::String, |_, _| {});
    w.add_column("pop", ColumnType::Int, |_, _| {});
    w.add_column("ok", ColumnType::Bool, |_, _| {});
    for (g, name, pop, ok) in [
        ("POINT(100 200)", "x", 5, true),
        (
            "POLYGON((0 0,4 0,4 4,0 4,0 0),(1 1,2 1,2 2,1 1))",
            "y",
            -7,
            false,
        ),
        ("MULTILINESTRING((0 0,1 1),(2 2,3 3))", "z", 9, true),
    ] {
        w.add_feature_geom(geozero::wkt::Wkt(g), |f| {
            f.property(0, "name", &ColumnValue::String(name)).unwrap();
            f.property(1, "pop", &ColumnValue::Int(pop)).unwrap();
            f.property(2, "ok", &ColumnValue::Bool(ok)).unwrap();
        })
        .unwrap();
    }
    let mut bytes = Vec::new();
    w.write(&mut bytes).unwrap();
    let e = Engine::new().unwrap();
    let r = rows(
        &e,
        &format!(
            r#"SERVICE <http://w3id.org/tripletrail/st> {{
                [] st:function st:ST_FromFlatGeobuf ; st:data "{}"^^xsd:hexBinary ;
                   st:fid ?fid ; st:geom ?g ; st:name ?name ; st:pop ?pop ; st:ok ?ok .
            }}"#,
            to_hex(&bytes)
        ),
        &["name", "pop", "ok", "g"],
    );
    let crs = "<http://www.opengis.net/def/crs/EPSG/0/3857> ";
    assert_eq!(
        r,
        [
            vec![
                "x".to_owned(),
                "5".into(),
                "true".into(),
                format!("{crs}POINT(100 200)")
            ],
            vec![
                "y".to_owned(),
                "-7".into(),
                "false".into(),
                format!("{crs}POLYGON((0 0,4 0,4 4,0 4,0 0),(1 1,2 1,2 2,1 1))")
            ],
            vec![
                "z".to_owned(),
                "9".into(),
                "true".into(),
                format!("{crs}MULTILINESTRING((0 0,1 1),(2 2,3 3))")
            ],
        ]
    );
}

#[test]
fn from_flatgeobuf_round_trip_and_insert() {
    let e = Engine::new().unwrap();
    let data = to_hex(&service_fgb(&e, true));
    let body = format!(
        r#"SERVICE <http://w3id.org/tripletrail/st> {{
            [] st:function st:ST_FromFlatGeobuf ; st:data "{data}"^^xsd:hexBinary ;
               st:fid ?fid ; st:geom ?geom ; st:name ?name ; st:n ?n .
        }}"#
    );
    let r = rows(&e, &body, &["name", "n", "geom"]);
    assert_eq!(
        r,
        [
            ["a", "1", "POINT(1 2)"],
            ["b", "2", "LINESTRING(0 0,3 4)"],
            ["c", "-", "POLYGON((10 10,12 10,12 12,10 10))"],
        ]
    );
    // ST_FromFlatGeobufToTable: there are no tables; the rows become triples with an update.
    e.update(&format!(
        "{PREFIXES} INSERT {{ ?f ex:name ?name ; geo:hasGeometry [ geo:asWKT ?geom ] }} WHERE {{
            {body}
            BIND(IRI(CONCAT('http://example.com/f', STR(?fid))) AS ?f)
        }}"
    ))
    .unwrap();
    let r = rows(
        &e,
        "?f ex:name ?name ; geo:hasGeometry/geo:asWKT ?g . FILTER(st:ST_Intersects(?g, \"POINT(11.5 10.5)\"^^geo:wktLiteral))",
        &["name"],
    );
    assert_eq!(r, [["c"]]);
}

#[test]
fn geobuf_by_hand() {
    let e = Engine::new().unwrap();
    let agg = |g: &str| {
        let q = format!(
            "{PREFIXES} SELECT (st:ST_AsGeobuf(?g) AS ?b) WHERE {{ VALUES ?g {{ {} }} }}",
            wkt(g)
        );
        let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
            panic!()
        };
        show(s.next().unwrap().unwrap().get("b").unwrap())
    };
    // Data { precision: 0, feature_collection { features { geometry { type: POINT, coords: [1, 2] } } } }
    assert_eq!(agg("POINT(1 2)"), "1800220A0A080A0608001A020204");
    // A polygon: the closing point is left out, coordinates are deltas, zigzag-encoded.
    assert_eq!(
        agg("POLYGON((0 0,1 0,1 1,0 0))"),
        "1800220E0A0C0A0A08041A06000002000002"
    );
    // The SERVICE form: keys ["name"], precision 1, a string value, properties [0, 0].
    let r = rows(
        &e,
        r#"SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:ST_AsGeobuf ; st:geobuf ?b .
            { SELECT ?geom ?name WHERE { VALUES (?geom ?name) { ("POINT(1.5 2)"^^geo:wktLiteral "a") } } }
        }"#,
        &["b"],
    );
    assert_eq!(
        r,
        [["0A046E616D65180122130A110A0608001A021E286A030A016172020000"]]
    );
}

#[test]
fn marc21() {
    let e = Engine::new().unwrap();
    // The PostGIS documentation example.
    assert_eq!(
        value(
            &e,
            &format!(
                "st:ST_AsMARC21({}, 'hdddmmss.sss')",
                wkt("POINT(-4.504289 54.253312)")
            )
        )
        .unwrap(),
        concat!(
            r#"<record xmlns="http://www.loc.gov/MARC21/slim">"#,
            r#"<datafield tag="034" ind1="1" ind2=" "><subfield code="a">a</subfield>"#,
            r#"<subfield code="d">W0043015.440</subfield><subfield code="e">W0043015.440</subfield>"#,
            r#"<subfield code="f">N0541511.923</subfield><subfield code="g">N0541511.923</subfield>"#,
            r#"</datafield></record>"#
        )
    );
    let box_ = wkt("POLYGON((-4 50,-3 50,-3 51.5,-4 50))");
    let x = value(&e, &format!("st:ST_AsMARC21({box_})")).unwrap();
    assert!(x.contains(r#"<subfield code="d">W0040000</subfield><subfield code="e">W0030000</subfield><subfield code="f">N0513000</subfield><subfield code="g">N0500000</subfield>"#), "{x}");
    let x = value(&e, &format!("st:ST_AsMARC21({box_}, 'ddd.dd')")).unwrap();
    assert!(
        x.contains(r#"<subfield code="d">-004.00</subfield>"#),
        "{x}"
    );
    // Back: a box is the envelope, equal coordinates a point; several fields a multi-geometry.
    assert_eq!(
        value(
            &e,
            &format!("st:ST_AsText(st:ST_GeomFromMARC21(st:ST_AsMARC21({box_})))")
        )
        .unwrap(),
        "POLYGON((-4 50,-4 51.5,-3 51.5,-3 50,-4 50))"
    );
    assert_eq!(
        value(
            &e,
            &format!(
                "st:ST_AsText(st:ST_GeomFromMARC21(st:ST_AsMARC21({}, 'hddd.dddddd')))",
                wkt("MULTIPOINT((1.5 2.25),(-3 -4))")
            )
        )
        .unwrap(),
        "MULTIPOINT((1.5 2.25),(-3 -4))"
    );
    // A Library of Congress style record, in decimal degrees with signs and decimal minutes.
    let rec = r#"<record xmlns="http://www.loc.gov/MARC21/slim"><leader>x</leader><datafield tag="245" ind1="0" ind2="0"><subfield code="a">Map</subfield></datafield><datafield tag="034" ind1="1" ind2=" "><subfield code="a">a</subfield><subfield code="d">+079.5</subfield><subfield code="e">E08630.0000</subfield><subfield code="f">N0200000</subfield><subfield code="g">-018.25</subfield></datafield></record>"#;
    assert_eq!(
        value(&e, &format!("st:ST_AsText(st:ST_GeomFromMARC21('{rec}'))")).unwrap(),
        "POLYGON((79.5 -18.25,79.5 20,86.5 20,86.5 -18.25,79.5 -18.25))"
    );
    assert_eq!(value(&e, "st:ST_GeomFromMARC21('<record/>')"), None);
}

#[test]
fn letters() {
    let e = Engine::new().unwrap();
    let hi = "st:ST_Letters('Hi')";
    assert_eq!(
        value(&e, &format!("st:GeometryType({hi})")).unwrap(),
        "MULTIPOLYGON"
    );
    // H, the stem of i and its dot.
    assert_eq!(num(&e, &format!("st:ST_NumGeometries({hi})")), 3.0);
    assert!(num(&e, &format!("st:ST_XMin({hi})")).abs() < 1e-9);
    assert!(num(&e, &format!("st:ST_YMin({hi})")).abs() < 1e-9);
    assert!((num(&e, &format!("st:ST_YMax({hi})")) - 100.0).abs() < 1e-9);
    // H is 60 design units (51.6) plus the stroke (14) wide, then a gap of 12, then i (14).
    assert!((num(&e, &format!("st:ST_XMax({hi})")) - 91.6).abs() < 1e-6);
    assert_eq!(value(&e, &format!("st:ST_IsValid({hi})")).unwrap(), "true");
    // Every built-in character renders.
    let all = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789.,:;!?-+=()/_#*";
    assert!(value(&e, &format!("st:ST_Area(st:ST_Letters('{all}'))")).is_some());
    assert_eq!(value(&e, "st:ST_Letters('é')"), None);

    // A font of one square glyph, as WKT and as hex WKB.
    let square = "POLYGON((0 0,10 0,10 10,0 10,0 0))";
    let font = format!("'{{\"a\": \"{square}\"}}'");
    let aa = format!("st:ST_Letters('a a', {font})");
    assert_eq!(num(&e, &format!("st:ST_NumGeometries({aa})")), 2.0);
    // Second glyph: after the first (10), a gap (1.2) and a space (4).
    assert!((num(&e, &format!("st:ST_XMax({aa})")) - 25.2).abs() < 1e-9);
    let wkb = value(&e, &format!("st:ST_AsBinary({})", wkt(square))).unwrap();
    let font = format!("'{{\"b\": \"{wkb}\"}}'");
    assert_eq!(
        num(&e, &format!("st:ST_Area(st:ST_Letters('bb', {font}))")),
        200.0
    );
    assert_eq!(value(&e, &format!("st:ST_Letters('c', {font})")), None);
}

#[test]
fn is_valid_detail() {
    let e = Engine::new().unwrap();
    let r = rows(
        &e,
        r#"SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:ST_IsValidDetail ; st:valid ?valid ; st:reason ?reason ;
               st:location ?location .
            { SELECT ?id ?geom WHERE { VALUES (?id ?geom) {
                (1 "POLYGON((0 0,2 2,2 0,0 2,0 0))"^^geo:wktLiteral)
                (2 "POLYGON((0 0,1 0,1 1,0 0))"^^geo:wktLiteral)
                (3 "MULTIPOLYGON(((0 0,2 0,2 2,0 2,0 0)),((1 1,3 1,3 3,1 3,1 1)))"^^geo:wktLiteral)
            } } }
        }"#,
        &["id", "valid", "reason", "location"],
    );
    assert_eq!(
        r,
        [
            [
                "1",
                "false",
                "exterior ring has a self-intersection",
                "POINT(1 1)"
            ],
            ["2", "true", "-", "-"],
            [
                "3",
                "false",
                "polygons at indices 0 and 1 overlap",
                "POINT(2 1)"
            ],
        ]
    );
    assert_eq!(
        value(
            &e,
            &format!(
                "st:ST_IsValidDetail({})",
                wkt("POLYGON((0 0,2 2,2 0,0 2,0 0))")
            )
        )
        .unwrap(),
        "exterior ring has a self-intersection"
    );
    assert_eq!(
        value(
            &e,
            &format!("st:ST_IsValidDetail({}, 0)", wkt("POINT(0 0)"))
        ),
        None
    );
}

#[test]
fn transform_pipelines() {
    let e = Engine::new().unwrap();
    let p = wkt("POINT(2 49)");
    // The PostGIS documentation examples: POINT(426857.987717275 5427937.52346616).
    for pipeline in [
        "urn:ogc:def:coordinateOperation:EPSG::16031",
        "+proj=pipeline +step +proj=unitconvert +xy_in=deg +xy_out=rad +step +proj=utm +zone=31 +ellps=WGS84",
    ] {
        let t = format!("st:ST_TransformPipeline({p}, '{pipeline}')");
        assert!((num(&e, &format!("st:ST_X({t})")) - 426_857.987_717_275).abs() < 1e-3);
        assert!((num(&e, &format!("st:ST_Y({t})")) - 5_427_937.523_466_16).abs() < 1e-3);
        assert_eq!(num(&e, &format!("st:ST_SRID({t})")), 32631.0);
        let back = format!("st:ST_InverseTransformPipeline({t}, '{pipeline}', 4326)");
        assert!((num(&e, &format!("st:ST_X({back})")) - 2.0).abs() < 1e-7);
        assert!((num(&e, &format!("st:ST_Y({back})")) - 49.0).abs() < 1e-7);
        assert_eq!(num(&e, &format!("st:ST_SRID({back})")), 4326.0);
    }
    // Same as ST_Transform to Web Mercator; to_srid labels the result.
    let merc = format!(
        "st:ST_TransformPipeline({p}, '+proj=pipeline +step +proj=unitconvert +xy_in=deg +xy_out=rad +step +proj=webmerc +ellps=WGS84', 3857)"
    );
    assert_eq!(
        value(&e, &format!("st:ST_AsText({merc})")),
        value(&e, &format!("st:ST_AsText(st:ST_Transform({p}, 3857))"))
    );
    // Axis swap, then the inverse of the whole pipeline with +inv.
    assert_eq!(
        value(
            &e,
            &format!("st:ST_AsText(st:ST_TransformPipeline({p}, '+proj=axisswap +order=2,1'))")
        )
        .unwrap(),
        "POINT(49 2)"
    );
    // Datum shifts need PROJ's database and grids.
    assert_eq!(
        value(
            &e,
            &format!("st:ST_TransformPipeline({p}, '+proj=pipeline +step +proj=helmert +x=1')")
        ),
        None
    );
}

#[test]
fn aliases() {
    let e = Engine::new().unwrap();
    let gml = r#"'<gml:Point xmlns:gml="http://www.opengis.net/gml/3.2" srsName="http://www.opengis.net/def/crs/OGC/1.3/CRS84"><gml:pos>1 2</gml:pos></gml:Point>'"#;
    assert_eq!(
        value(&e, &format!("st:ST_AsText(st:ST_GMLToSQL({gml}))")).unwrap(),
        "POINT(1 2)"
    );
    assert_eq!(
        value(&e, &format!("st:ST_GMLToSQL({gml})")),
        value(&e, &format!("st:ST_GeomFromGML({gml})"))
    );
    let q = |agg: &str| {
        let q = format!(
            "{PREFIXES} SELECT (st:ST_AsText(st:{agg}(?g)) AS ?b) WHERE {{ VALUES ?g {{ {} {} }} }}",
            wkt("POINT(1 2)"),
            wkt("LINESTRING(3 -1,5 0)")
        );
        let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
            panic!()
        };
        show(s.next().unwrap().unwrap().get("b").unwrap())
    };
    assert_eq!(q("ST_EstimatedExtent"), q("ST_Extent"));
    assert_eq!(q("ST_EstimatedExtent"), "POLYGON((5 -1,5 2,1 2,1 -1,5 -1))");
}
