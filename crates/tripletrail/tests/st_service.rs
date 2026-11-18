//! PostGIS set-returning and window functions through `SERVICE <http://w3id.org/tripletrail/st>`.

use tripletrail::Engine;
use tripletrail::oxigraph::model::Term;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX st: <http://w3id.org/tripletrail/st#>
PREFIX ex: <http://example.com/>
"#;

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

#[test]
fn dump_functions() {
    let e = Engine::new().unwrap();
    let r = rows(
        &e,
        r#"SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:ST_Dump ; st:geom "MULTIPOINT((1 2),(3 4))"^^geo:wktLiteral ;
               st:path ?path ; st:geom ?g .
        }"#,
        &["path", "g"],
    );
    assert_eq!(
        r,
        vec![vec!["{1}", "POINT(1 2)"], vec!["{2}", "POINT(3 4)"]]
    );
    let r = rows(
        &e,
        r#"SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:ST_DumpPoints ; st:geom "POLYGON((0 0,1 0,1 1,0 0))"^^geo:wktLiteral ;
               st:path ?path ; st:geom ?g .
        }"#,
        &["path", "g"],
    );
    assert_eq!(r.len(), 4);
    assert_eq!(r[0], vec!["{1,1}", "POINT(0 0)"]);
    let r = rows(
        &e,
        r#"SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:ST_DumpSegments ; st:geom "LINESTRING(0 0,1 0,1 1)"^^geo:wktLiteral ;
               st:path ?path ; st:geom ?g .
        }"#,
        &["path", "g"],
    );
    assert_eq!(
        r,
        vec![
            vec!["{1}", "LINESTRING(0 0,1 0)"],
            vec!["{2}", "LINESTRING(1 0,1 1)"]
        ]
    );
    let r = rows(
        &e,
        r#"SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:ST_DumpRings ;
               st:geom "POLYGON((0 0,9 0,9 9,0 9,0 0),(1 1,2 1,2 2,1 1))"^^geo:wktLiteral ;
               st:path ?path ; st:geom ?g .
        }"#,
        &["path"],
    );
    assert_eq!(r, vec![vec!["{0}"], vec!["{1}"]]);
}

#[test]
fn input_rows_pass_through() {
    let e = Engine::new().unwrap();
    e.update(
        r#"PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX ex: <http://example.com/>
        INSERT DATA {
            ex:a geo:asWKT "MULTILINESTRING((0 0,1 1),(2 2,3 3))"^^geo:wktLiteral .
            ex:b geo:asWKT "LINESTRING(5 5,6 6)"^^geo:wktLiteral .
        }"#,
    )
    .unwrap();
    let r = rows(
        &e,
        r#"SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:ST_Dump ; st:path ?path ; st:geom ?part .
            { SELECT ?f ?geom WHERE { ?f geo:asWKT ?geom } }
        }"#,
        &["f", "path"],
    );
    assert_eq!(
        r,
        vec![
            vec!["http://example.com/a", "{1}"],
            vec!["http://example.com/a", "{2}"],
            vec!["http://example.com/b", "{}"],
        ]
    );
}

#[test]
fn grids() {
    let e = Engine::new().unwrap();
    let r = rows(
        &e,
        r#"SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:ST_SquareGrid ; st:size 1 ;
               st:bounds "LINESTRING(0.5 0.5,2.5 1.5)"^^geo:wktLiteral ;
               st:i ?i ; st:j ?j .
        }"#,
        &["i", "j"],
    );
    assert_eq!(r.len(), 6, "{r:?}");
    let r = rows(
        &e,
        r#"SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:ST_HexagonGrid ; st:size 1 ;
               st:bounds "POINT(0 0)"^^geo:wktLiteral ; st:geom ?g ; st:i ?i ; st:j ?j .
        }"#,
        &["i", "j"],
    );
    assert!(r.contains(&vec!["0".to_owned(), "0".to_owned()]), "{r:?}");
}

#[test]
fn clusters() {
    let e = Engine::new().unwrap();
    e.update(
        r#"PREFIX geo: <http://www.opengis.net/ont/geosparql#>
        PREFIX ex: <http://example.com/>
        INSERT DATA {
            ex:p1 geo:asWKT "POINT(0 0)"^^geo:wktLiteral .
            ex:p2 geo:asWKT "POINT(0 1)"^^geo:wktLiteral .
            ex:p3 geo:asWKT "POINT(1 0)"^^geo:wktLiteral .
            ex:p4 geo:asWKT "POINT(10 10)"^^geo:wktLiteral .
            ex:p5 geo:asWKT "POINT(10 11)"^^geo:wktLiteral .
            ex:p6 geo:asWKT "POINT(50 50)"^^geo:wktLiteral .
        }"#,
    )
    .unwrap();
    let input = "{ SELECT ?f ?geom WHERE { ?f geo:asWKT ?geom } }";
    let r = rows(
        &e,
        &format!(
            "SERVICE <http://w3id.org/tripletrail/st> {{
                [] st:function st:ST_ClusterDBSCAN ; st:eps 1.5 ; st:minpoints 2 ;
                   st:cluster_id ?c . {input} }}"
        ),
        &["f", "c"],
    );
    let id = |name: &str| r.iter().find(|x| x[0].ends_with(name)).unwrap()[1].clone();
    assert_eq!(id("p1"), id("p2"));
    assert_eq!(id("p1"), id("p3"));
    assert_eq!(id("p4"), id("p5"));
    assert_ne!(id("p1"), id("p4"));
    assert_eq!(id("p6"), "-");
    let r = rows(
        &e,
        &format!(
            "SERVICE <http://w3id.org/tripletrail/st> {{
                [] st:function st:ST_ClusterKMeans ; st:k 3 ; st:cluster_id ?c . {input} }}"
        ),
        &["f", "c"],
    );
    let id = |name: &str| r.iter().find(|x| x[0].ends_with(name)).unwrap()[1].clone();
    assert_eq!(id("p1"), id("p3"));
    assert_eq!(id("p4"), id("p5"));
    assert_ne!(id("p1"), id("p6"));
    let r = rows(
        &e,
        &format!(
            "SERVICE <http://w3id.org/tripletrail/st> {{
                [] st:function st:ST_ClusterWithinWin ; st:distance 2 ; st:cluster_id ?c . {input} }}"
        ),
        &["f", "c"],
    );
    let distinct: std::collections::HashSet<_> = r.iter().map(|x| x[1].clone()).collect();
    assert_eq!(distinct.len(), 3);
    let r = rows(
        &e,
        &format!(
            "SERVICE <http://w3id.org/tripletrail/st> {{
                [] st:function st:ST_ClusterWithin ; st:distance 2 ; st:geom ?cluster . {input} }}"
        ),
        &["cluster"],
    );
    assert_eq!(r.len(), 3);
}

#[test]
fn srs_tables() {
    let e = Engine::new().unwrap();
    // postgis_srs('EPSG', '25832').
    let cols = [
        "auth_name",
        "auth_srid",
        "srname",
        "proj4text",
        "point_sw",
        "point_ne",
    ];
    let one = |params: &str| {
        rows(
            &e,
            &format!(
                "SERVICE <http://w3id.org/tripletrail/st> {{
                    [] st:function st:srs ; {params} ;
                       st:auth_name ?auth_name ; st:auth_srid ?auth_srid ; st:srname ?srname ;
                       st:srtext ?srtext ; st:proj4text ?proj4text ; st:point_sw ?point_sw ;
                       st:point_ne ?point_ne }}"
            ),
            &cols,
        )
    };
    let expected = [
        "EPSG",
        "25832",
        "ETRS89 / UTM zone 32N",
        "+proj=utm +zone=32 +ellps=GRS80 +towgs84=0,0,0,0,0,0,0 +units=m +no_defs",
        "POINT(6 32.88)",
        "POINT(12 84.73)",
    ];
    assert_eq!(
        one(r#"st:auth_name "EPSG" ; st:auth_srid "25832""#),
        [expected]
    );
    assert_eq!(one(r#"st:code "EPSG:25832""#), [expected]);
    assert_eq!(
        one("st:code <http://www.opengis.net/def/crs/EPSG/0/25832>"),
        [expected]
    );
    assert!(one(r#"st:code "EPSG:4269""#).is_empty());
    // postgis_srs_all: every supported CRS.
    let all = rows(
        &e,
        "SERVICE <http://w3id.org/tripletrail/st> { [] st:function st:srs ; st:auth_srid ?code }",
        &["code"],
    );
    // National CRSs (#740): 65 more.
    assert_eq!(all.len(), 5 + 11 + 60 + 60 + 1 + 65);
    // postgis_srs_search: the projected CRSs whose area of use holds the bounds, smallest
    // area first; per input row from a sub-select.
    let found = rows(
        &e,
        r#"SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:srsSearch ; st:bounds "POLYGON((7 50,8 50,8 51,7 51,7 50))"^^geo:wktLiteral ;
               st:auth_srid ?code ; st:srname ?name }"#,
        &["code", "name"],
    );
    let mut codes: Vec<&str> = found.iter().map(|r| r[0].as_str()).collect();
    codes.sort();
    assert_eq!(
        codes,
        [
            "2154", "25832", "27572", "3034", "3035", "32632", "3857", "3950", "4647"
        ]
    );
    let per_row = rows(
        &e,
        r#"SERVICE <http://w3id.org/tripletrail/st> {
            [] st:function st:srsSearch ; st:auth_srid ?code .
            { SELECT ?f ?geom WHERE {
                VALUES (?f ?geom) {
                  ("stockholm" "POINT(18.07 59.33)"^^geo:wktLiteral)
                  ("utm" "<http://www.opengis.net/def/crs/EPSG/0/25832> POINT(500000 5600000)"^^geo:wktLiteral)
                } } } }"#,
        &["f", "code"],
    );
    let has = |f: &str, c: &str| per_row.iter().any(|r| r[0] == f && r[1] == c);
    assert!(has("stockholm", "3006") && has("stockholm", "32634"));
    assert!(has("utm", "25832") && !has("utm", "3006"));
}

/// A GeoSPARQL relation triple in a service's sub-select is rewritten as in the main query
/// (#857): it found nothing, since the services evaluated their sub-selects unrewritten.
#[test]
fn relation_triples_in_service_sub_selects_are_answered() {
    for e in [Engine::new_tripletrail(), Engine::new_oxigraph().unwrap()] {
        e.load(
            tripletrail::oxigraph::io::RdfFormat::Turtle,
            r#"@prefix geo: <http://www.opengis.net/ont/geosparql#> .
               @prefix ex: <http://example.com/> .
               ex:p geo:hasGeometry [ geo:asWKT "POINT(1 1)"^^geo:wktLiteral ] .
               ex:q geo:hasGeometry [ geo:asWKT "POINT(9 9)"^^geo:wktLiteral ] .
               ex:z geo:hasGeometry [ geo:asWKT "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"^^geo:wktLiteral ] ."#
                .as_bytes(),
        )
        .unwrap();
        // A relation triple, and a jena-geosparql property function.
        for pattern in [
            "?a geo:sfWithin ex:z",
            "?a <http://jena.apache.org/spatial#withinBox> (0 0 3 3)",
        ] {
            let features = rows(
                &e,
                &format!("{pattern} . ?a geo:hasGeometry/geo:asWKT ?g"),
                &["a"],
            );
            assert!(
                features.contains(&vec!["http://example.com/p".to_owned()]),
                "{features:?}"
            );
            let window = rows(
                &e,
                &format!(
                    "SERVICE <http://w3id.org/tripletrail/window> {{
                       [] <http://w3id.org/tripletrail/window#orderBy> ?a ;
                          <http://w3id.org/tripletrail/window#rowNumber> ?n .
                       {{ SELECT ?a WHERE {{ {pattern} . ?a geo:hasGeometry/geo:asWKT ?g }} }} }}"
                ),
                &["a"],
            );
            assert_eq!(window, features, "{pattern}");
            let st = rows(
                &e,
                &format!(
                    "SERVICE <http://w3id.org/tripletrail/st> {{
                       [] st:function st:ST_Dump ; st:geom ?part .
                       {{ SELECT ?a ?geom WHERE {{ {pattern} . ?a geo:hasGeometry/geo:asWKT ?geom }} }} }}"
                ),
                &["a"],
            );
            assert_eq!(st, features, "{pattern}");
        }
    }
}
