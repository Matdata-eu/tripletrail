//! Regression tests from the evaluation of tripletrail for a rail infrastructure manager's knowledge graph
//! post-processing (issues #15 to #23).

use std::time::Instant;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX st: <http://w3id.org/tripletrail/st#>
PREFIX spatialf: <http://jena.apache.org/function/spatial#>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
"#;

fn rows(e: &Engine, q: &str) -> Vec<Vec<Option<String>>> {
    let QueryResults::Solutions(s) = e.query(&format!("{PREFIXES}{q}")).unwrap() else {
        panic!("not a SELECT")
    };
    let vars = s.variables().to_vec();
    s.map(|r| {
        let r = r.unwrap();
        vars.iter()
            .map(|v| r.get(v).map(|t| t.to_string()))
            .collect()
    })
    .collect()
}

fn one(e: &Engine, q: &str) -> Option<String> {
    rows(e, q)
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next().flatten())
}

// #15
fn locations(n: usize) -> Engine {
    let mut ttl = String::from("@prefix ex: <http://example.com/> .\n");
    for i in 0..n {
        ttl.push_str(&format!(
            "ex:track{i} a ex:Track ; ex:loc ex:loc{i} .\n\
             ex:loc{i} ex:ane ex:ane{i} .\n\
             ex:ane{i} ex:on ex:ne{i} .\n\
             ex:ne{i} ex:begin ex:t{i} .\n"
        ));
        if i % 2 == 0 {
            ttl.push_str(&format!("ex:speed{i} a ex:SpeedSection ; ex:loc ex:sloc{i} .\nex:sloc{i} ex:ane ex:sane{i} .\nex:sane{i} ex:on ex:ne{i} .\n"));
        }
    }
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    e
}

fn seconds(e: &Engine, q: &str) -> f64 {
    let t = Instant::now();
    let _ = rows(e, q);
    t.elapsed().as_secs_f64()
}

#[test]
fn issue15_not_exists_uses_the_bound_variable() {
    let q = "SELECT (COUNT(*) AS ?n) WHERE {
        ?track a ex:Track ; ex:loc/ex:ane/ex:on ?ne .
        FILTER NOT EXISTS { ?s a ex:SpeedSection ; ex:loc/ex:ane/ex:on ?ne . }
    }";
    let e = locations(400);
    assert_eq!(
        one(&e, q).as_deref(),
        Some("\"200\"^^<http://www.w3.org/2001/XMLSchema#integer>")
    );
    let small = seconds(&locations(100), q);
    let large = seconds(&e, q);
    assert!(large < small * 8.0, "100: {small:.3} s, 400: {large:.3} s");
}

#[test]
fn issue16_transform_keeps_z() {
    let e = Engine::new().unwrap();
    let t = one(
        &e,
        r#"SELECT (geof:transform("<http://www.opengis.net/def/crs/EPSG/0/25832> POINT Z(600000 6600000 12.5)"^^geo:wktLiteral,
                   <http://www.opengis.net/def/crs/OGC/1.3/CRS84>) AS ?t) {}"#,
    )
    .expect("transform result");
    assert!(t.contains("POINT Z") && t.contains(" 12.5"), "{t}");
}

#[test]
fn issue16_line_substring_keeps_z() {
    let e = Engine::new().unwrap();
    let t = one(
        &e,
        r#"SELECT (st:ST_LineSubstring("LINESTRING Z(0 0 10, 10 0 20)"^^geo:wktLiteral, 0.0, 0.5) AS ?t) {}"#,
    )
    .expect("substring result");
    assert!(t.contains("LINESTRING Z") && t.contains(" 15"), "{t}");
}

#[test]
fn issue16_line_interpolate_point_keeps_z() {
    let e = Engine::new().unwrap();
    let t = one(
        &e,
        r#"SELECT (st:ST_LineInterpolatePoint("LINESTRING Z(0 0 10, 10 0 20)"^^geo:wktLiteral, 0.5) AS ?t) {}"#,
    )
    .expect("interpolate result");
    assert!(t.contains("POINT Z") && t.contains(" 15"), "{t}");
}

#[test]
fn issue17_nan_m_ordinate_is_accepted() {
    let e = Engine::new().unwrap();
    let t = one(
        &e,
        r#"SELECT (geof:transform("<http://www.opengis.net/def/crs/EPSG/0/25832> POINT ZM(612849.62 6741447.486 126.789 nan)"^^geo:wktLiteral,
                   <http://www.opengis.net/def/crs/OGC/1.3/CRS84>) AS ?t) {}"#,
    );
    assert!(
        t.is_some(),
        "a WKT with M = nan is rejected (GEOS/shapely read it)"
    );
}

#[test]
fn issue18_update_uses_spatial_rewrite() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..20_000 {
        ttl.push_str(&format!(
            "ex:p{i} geo:hasGeometry ex:g{i} . ex:g{i} geo:asWKT \"POINT({} {})\"^^geo:wktLiteral .\n",
            i % 200,
            i / 200
        ));
    }
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let filter = r#"?p geo:hasGeometry/geo:asWKT ?w
        FILTER(geof:sfWithin(?w, "POLYGON((0 0, 5 0, 5 5, 0 5, 0 0))"^^geo:wktLiteral))"#;
    let _ = rows(&e, &format!("SELECT ?p WHERE {{ {filter} }}"));
    assert_eq!(
        e.last_spatial_stats().filters,
        1,
        "the SELECT uses the index"
    );
    let _ = rows(&e, "SELECT * {}");
    e.update(&format!(
        "{PREFIXES} INSERT {{ ?p ex:near ex:origin }} WHERE {{ {filter} }}"
    ))
    .unwrap();
    assert_eq!(
        e.last_spatial_stats().filters,
        1,
        "the same WHERE in an UPDATE scans every geometry"
    );
}

#[test]
fn issue19_transform_accepts_crs_as_string_or_anyuri() {
    let e = Engine::new().unwrap();
    for crs in [
        r#""http://www.opengis.net/def/crs/OGC/1.3/CRS84""#,
        r#""http://www.opengis.net/def/crs/OGC/1.3/CRS84"^^xsd:anyURI"#,
    ] {
        let t = one(
            &e,
            &format!(
                r#"SELECT (geof:transform("<http://www.opengis.net/def/crs/EPSG/0/25832> POINT(600000 6600000)"^^geo:wktLiteral, {crs}) AS ?t) {{}}"#
            ),
        );
        assert!(t.is_some(), "CRS given as {crs}");
    }
}

#[test]
fn issue19_jena_transform_srs_alias() {
    let e = Engine::new().unwrap();
    let t = e.query(&format!(
        r#"{PREFIXES} SELECT (spatialf:transformSRS("<http://www.opengis.net/def/crs/EPSG/0/25832> POINT(600000 6600000)"^^geo:wktLiteral,
              "http://www.opengis.net/def/crs/OGC/1.3/CRS84") AS ?t) {{}}"#
    ));
    assert!(t.is_ok(), "{:?}", t.err());
}

#[test]
fn issue22_ordered_make_line_aggregate() {
    let e = Engine::new().unwrap();
    let t = one(
        &e,
        r#"SELECT (st:ST_MakeLine(?seg) AS ?line) WHERE {
            { SELECT ?seg WHERE {
                VALUES (?seq ?seg) { (2 "LINESTRING(10 0, 20 0)"^^geo:wktLiteral) (1 "LINESTRING(0 0, 10 0)"^^geo:wktLiteral) }
            } ORDER BY ?seq }
        }"#,
    );
    assert_eq!(
        t.as_deref(),
        Some("\"LINESTRING(0 0,10 0,20 0)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>")
    );
}

#[test]
fn issue23_index_sees_data_written_through_store() {
    let e = Engine::new().unwrap();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix geo: <http://www.opengis.net/ont/geosparql#> . <http://example.com/a> geo:asWKT "POINT(1 1)"^^geo:wktLiteral ."#.as_bytes(),
    )
    .unwrap();
    let q = r#"SELECT (COUNT(*) AS ?n) WHERE { ?g geo:asWKT ?w FILTER(geof:sfWithin(?w, "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))"^^geo:wktLiteral)) }"#;
    assert_eq!(
        one(&e, q).as_deref(),
        Some("\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>")
    );
    e.store()
        .load(
            tripletrail::oxigraph::io::RdfParser::from_format(RdfFormat::Turtle)
                .with_default_graph(tripletrail::oxigraph::model::NamedNode::new_unchecked("http://example.com/step")),
            r#"@prefix geo: <http://www.opengis.net/ont/geosparql#> . <http://example.com/b> geo:asWKT "POINT(2 2)"^^geo:wktLiteral ."#.as_bytes(),
        )
        .unwrap();
    // Writes through the store bypass the engine: tell it.
    e.invalidate();
    let q = r#"SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?x { ?g geo:asWKT ?w } FILTER(geof:sfWithin(?w, "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))"^^geo:wktLiteral)) }"#;
    assert_eq!(
        one(&e, q).as_deref(),
        Some("\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>")
    );
}

#[test]
fn issue23_load_into_named_graph() {
    let e = Engine::new().unwrap();
    let q = r#"SELECT (COUNT(*) AS ?n) WHERE { GRAPH ex:step { ?g geo:asWKT ?w } FILTER(geof:sfWithin(?w, "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))"^^geo:wktLiteral)) }"#;
    assert_eq!(
        one(&e, q).as_deref(),
        Some("\"0\"^^<http://www.w3.org/2001/XMLSchema#integer>")
    );
    e.load_into(
        tripletrail::oxigraph::model::NamedNode::new_unchecked("http://example.com/step"),
        RdfFormat::Turtle,
        r#"@prefix geo: <http://www.opengis.net/ont/geosparql#> . <http://example.com/b> geo:asWKT "POINT(2 2)"^^geo:wktLiteral ."#.as_bytes(),
    )
    .unwrap();
    assert_eq!(
        one(&e, q).as_deref(),
        Some("\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>")
    );
}

#[test]
fn issue15_correlated_exists_keeps_its_results() {
    let e = locations(20);
    // EXISTS and NOT EXISTS split the tracks; a nested group and a UNION inside still work.
    let count = |q: &str| {
        one(
            &e,
            &format!(
                "SELECT (COUNT(*) AS ?n) WHERE {{ ?track a ex:Track ; ex:loc/ex:ane/ex:on ?ne . {q} }}"
            ),
        )
    };
    let n = |v: u32| {
        Some(format!(
            "\"{v}\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        ))
    };
    assert_eq!(
        count("FILTER EXISTS { ?s a ex:SpeedSection ; ex:loc/ex:ane/ex:on ?ne . }"),
        n(10)
    );
    assert_eq!(
        count("FILTER NOT EXISTS { ?s a ex:SpeedSection ; ex:loc/ex:ane/ex:on ?ne . }"),
        n(10)
    );
    assert_eq!(
        count(
            "FILTER EXISTS { { ?s a ex:SpeedSection } UNION { ?s a ex:Track } ?s ex:loc/ex:ane/ex:on ?ne . }"
        ),
        n(20)
    );
    assert_eq!(
        count("FILTER EXISTS { ?ne ex:begin ?t . OPTIONAL { ?x ex:on ?ne } FILTER(BOUND(?x)) }"),
        n(20)
    );
}

#[test]
fn issue16_z_survives_reverse_and_substring() {
    let e = Engine::new().unwrap();
    let t = one(
        &e,
        r#"SELECT (st:ST_Reverse("LINESTRING Z(0 0 10, 10 0 20)"^^geo:wktLiteral) AS ?t) {}"#,
    );
    assert_eq!(
        t.as_deref(),
        Some("\"LINESTRING Z(10 0 20,0 0 10)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>")
    );
    let t = one(
        &e,
        r#"SELECT (st:ST_LineSubstring("LINESTRING ZM(0 0 10 1, 10 0 20 2)"^^geo:wktLiteral, 0.5, 1.0) AS ?t) {}"#,
    );
    assert_eq!(
        t.as_deref(),
        Some(
            "\"LINESTRING ZM(5 0 15 1.5,10 0 20 2)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>"
        )
    );
    // 2-D in, 2-D out.
    let t = one(
        &e,
        r#"SELECT (st:ST_LineSubstring("LINESTRING(0 0, 10 0)"^^geo:wktLiteral, 0.0, 0.5) AS ?t) {}"#,
    );
    assert_eq!(
        t.as_deref(),
        Some("\"LINESTRING(0 0,5 0)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>")
    );
    // PostGIS keeps the centroid 2-D.
    let t = one(
        &e,
        r#"SELECT (st:ST_Centroid("LINESTRING Z(0 0 10, 10 0 20)"^^geo:wktLiteral) AS ?t) {}"#,
    );
    assert_eq!(
        t.as_deref(),
        Some("\"POINT(5 0)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>")
    );
}

#[test]
fn issue17_nan_is_kept_and_nan_xy_rejected() {
    let e = Engine::new().unwrap();
    let t = one(
        &e,
        r#"SELECT (st:ST_Reverse("LINESTRING ZM(0 0 1 nan, 1 0 2 NaN)"^^geo:wktLiteral) AS ?t) {}"#,
    );
    assert_eq!(
        t.as_deref(),
        Some(
            "\"LINESTRING ZM(1 0 2 NaN,0 0 1 NaN)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>"
        )
    );
    let t = one(
        &e,
        r#"SELECT (geof:dimension("POINT(nan 1)"^^geo:wktLiteral) AS ?t) {}"#,
    );
    assert_eq!(t, None);
}

#[test]
fn issue18_update_result_is_unchanged_by_the_index() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..1000 {
        ttl.push_str(&format!("ex:p{i} geo:hasGeometry ex:g{i} . ex:g{i} geo:asWKT \"POINT({} {})\"^^geo:wktLiteral .\n", i % 50, i / 50));
    }
    let insert = r#"INSERT { ?p ex:near ex:origin } WHERE { ?p geo:hasGeometry/geo:asWKT ?w
        FILTER(geof:sfWithin(?w, "POLYGON((0 0, 5 0, 5 5, 0 5, 0 0))"^^geo:wktLiteral)) }"#;
    let count = "SELECT (COUNT(*) AS ?n) WHERE { ?p ex:near ex:origin }";
    let mut counts = Vec::new();
    for indexed in [true, false] {
        let e = Engine::new().unwrap();
        e.set_spatial_rewrite(indexed);
        e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
        e.update(&format!("{PREFIXES}{insert}")).unwrap();
        counts.push(one(&e, count));
    }
    assert_eq!(counts[0], counts[1]);
    assert_eq!(
        counts[0].as_deref(),
        Some("\"16\"^^<http://www.w3.org/2001/XMLSchema#integer>")
    );
}

#[test]
fn issue19_jena_functions_with_string_units() {
    let e = Engine::new().unwrap();
    let t = one(
        &e,
        r#"SELECT (spatialf:distance("POINT(10 59)"^^geo:wktLiteral, "POINT(10 60)"^^geo:wktLiteral, "http://www.opengis.net/def/uom/OGC/1.0/kilometre") AS ?d) {}"#,
    )
    .unwrap();
    let d: f64 = t.split('"').nth(1).unwrap().parse().unwrap();
    assert!((d - 111.0).abs() < 1.0, "{d}");
}

#[test]
fn issue22_make_line_function_and_aggregate_with_z() {
    let e = Engine::new().unwrap();
    // The two-argument function still works.
    let t = one(
        &e,
        r#"SELECT (st:ST_MakeLine("POINT(0 0)"^^geo:wktLiteral, "POINT(1 1)"^^geo:wktLiteral) AS ?l) {}"#,
    );
    assert_eq!(
        t.as_deref(),
        Some("\"LINESTRING(0 0,1 1)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>")
    );
    // The aggregate per group, keeping Z, under both names.
    for name in ["ST_MakeLine", "ST_MakeLineAgg"] {
        let r = rows(
            &e,
            &format!(
                r#"SELECT ?loc (st:{name}(?seg) AS ?line) WHERE {{
                    {{ SELECT ?loc ?seg WHERE {{
                        VALUES (?loc ?seq ?seg) {{
                            (ex:a 2 "LINESTRING Z(10 0 1, 20 0 2)"^^geo:wktLiteral)
                            (ex:a 1 "LINESTRING Z(0 0 0, 10 0 1)"^^geo:wktLiteral)
                            (ex:b 1 "LINESTRING(5 5, 6 6)"^^geo:wktLiteral)
                        }}
                    }} ORDER BY ?loc ?seq }}
                }} GROUP BY ?loc ORDER BY ?loc"#
            ),
        );
        assert_eq!(
            r[0][1].as_deref(),
            Some(
                "\"LINESTRING Z(0 0 0,10 0 1,20 0 2)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>"
            ),
            "{name}"
        );
        assert_eq!(
            r[1][1].as_deref(),
            Some("\"LINESTRING(5 5,6 6)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>"),
            "{name}"
        );
    }
}

// #57
#[test]
fn issue57_triples_after_a_bind_look_up_the_bound_variable() {
    // 04-add-default-speedsections: the triple after the BIND made the patterns before it a
    // LATERAL right side, planned without ?ne: a scan of all tracks for every ex:begin triple.
    let q = "SELECT (COUNT(*) AS ?n) WHERE {
        ?track a ex:Track ; ex:loc/ex:ane/ex:on ?ne .
        BIND(STR(?ne) AS ?id)
        ?ne ex:begin ?b .
        OPTIONAL { ?ne ex:end ?e }
    }";
    let e = locations(4000);
    assert_eq!(
        one(&e, q).as_deref(),
        Some("\"4000\"^^<http://www.w3.org/2001/XMLSchema#integer>")
    );
    let small = seconds(&locations(1000), q);
    let large = seconds(&e, q);
    assert!(
        large < small * 8.0,
        "1000: {small:.3} s, 4000: {large:.3} s"
    );
}

#[test]
fn issue57_union_after_a_join_looks_up_the_bound_variable() {
    // 99-associate-netentity-with-subnetwork: a UNION branch linked to the rest only through
    // ?loc started from a scan of its type for every row.
    let q = "SELECT (COUNT(*) AS ?n) WHERE {
        ?track a ex:Track ; ex:loc ?loc .
        { ?loc ex:ane ?a . ?a ex:on ?ne } UNION { ?s a ex:SpeedSection ; ex:loc ?loc }
    }";
    let e = locations(4000);
    assert_eq!(
        one(&e, q).as_deref(),
        Some("\"4000\"^^<http://www.w3.org/2001/XMLSchema#integer>")
    );
    let small = seconds(&locations(1000), q);
    let large = seconds(&e, q);
    assert!(
        large < small * 8.0,
        "1000: {small:.3} s, 4000: {large:.3} s"
    );
}

#[test]
fn issue57_spatial_filter_after_binds_is_a_spatial_join() {
    // 03-create-area-locations-for-etcs-areas: FILTER(geof:sfContains(?area, ?line)) in a
    // group ending with BINDs. The BINDs hid the join from the spatial rewrite, which
    // answered it from all pairs of geometries in the store (out of memory on the rail KG).
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..4 {
        let x = i * 10;
        ttl.push_str(&format!(
            "ex:area{i} a ex:Area ; geo:hasGeometry [ geo:asWKT \"POLYGON(({x} 0, {} 0, {} 10, {x} 10, {x} 0))\"^^geo:wktLiteral ] .\n",
            x + 10,
            x + 10
        ));
    }
    for i in 0..400 {
        let (x, y) = (f64::from(i % 40) + 0.2, f64::from(i / 40) + 0.2);
        ttl.push_str(&format!(
            "ex:ne{i} a ex:NetElement ; geo:hasGeometry [ geo:asWKT \"LINESTRING({x} {y}, {} {})\"^^geo:wktLiteral ] .\n",
            x + 0.5,
            y + 0.5
        ));
    }
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let q = format!(
        "{PREFIXES}SELECT (COUNT(*) AS ?n) WHERE {{
            ?area a ex:Area ; geo:hasGeometry/geo:asWKT ?a .
            ?ne a ex:NetElement ; geo:hasGeometry/geo:asWKT ?l .
            BIND(STR(?ne) AS ?id)
            BIND(IRI(CONCAT(STR(?area), \"/location\")) AS ?location)
            FILTER(geof:sfContains(?a, ?l))
        }}"
    );
    assert_eq!(
        one(&e, &q[PREFIXES.len()..]).as_deref(),
        Some("\"400\"^^<http://www.w3.org/2001/XMLSchema#integer>")
    );
    // The join's two sides are found (with their sizes), not one opaque pattern.
    let explained = e.explain(&q).unwrap();
    assert!(explained.contains(r#""joins":1"#), "{explained}");
    assert!(!explained.contains("(?)"), "{explained}");
}

/// The order-independent fingerprint of a store's quads: their count and the wrapping sum of
/// a fixed hash of each one's N-Quads text.
fn fingerprint(e: &Engine) -> (usize, u64) {
    use std::hash::{Hash, Hasher};
    let mut n = 0;
    let mut sum = 0u64;
    for q in e.store().quads_for_pattern(None, None, None, None) {
        let mut h = std::hash::DefaultHasher::new();
        q.unwrap().to_string().hash(&mut h);
        sum = sum.wrapping_add(h.finish());
        n += 1;
    }
    (n, sum)
}

fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .map_or(0, |kb| kb / 1024)
}

/// #406 on the rail KG: the post-process updates run in an overlay of the KG leave the KG as it
/// was, and give the store a copy of it gets.
/// `DIM_KG=kg.ttl DIM_UPDATES=dir cargo test --release -p tripletrail --test dim_kg overlay -- --ignored --nocapture`
#[test]
#[ignore]
fn overlay_runs_the_post_process_updates_apart_from_the_kg() {
    let (Ok(kg), Ok(dir)) = (std::env::var("DIM_KG"), std::env::var("DIM_UPDATES")) else {
        eprintln!("set DIM_KG and DIM_UPDATES");
        return;
    };
    let t = Instant::now();
    let base = Engine::new_tripletrail();
    base.load(
        RdfFormat::Turtle,
        std::io::BufReader::new(std::fs::File::open(&kg).unwrap()),
    )
    .unwrap();
    let tripletrail::store::Dataset::TripleTrail(store) = base.store() else {
        unreachable!()
    };
    let store = store.clone();
    println!(
        "loaded {} quads in {:.1?}, {} MB",
        store.len(),
        t.elapsed(),
        rss_mb()
    );
    let before = fingerprint(&base);
    let mut updates: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "rq" || x == "ru"))
        .collect();
    updates.sort();
    let run = |e: &Engine, label: &str| {
        let rss = rss_mb();
        let t = Instant::now();
        for u in &updates {
            let started = Instant::now();
            let text = std::fs::read_to_string(u).unwrap();
            e.update(&text)
                .unwrap_or_else(|err| panic!("{}: {err}", u.display()));
            let s = started.elapsed().as_secs_f64();
            if s > 2.0 {
                println!(
                    "  {label} {}: {s:.1} s",
                    u.file_name().unwrap().to_string_lossy()
                );
            }
        }
        println!(
            "{label}: {} updates in {:.1?}, {} quads, RSS {} -> {} MB",
            updates.len(),
            t.elapsed(),
            e.store().len().unwrap_or(0),
            rss,
            rss_mb()
        );
    };
    let overlay = Engine::from_store(store.overlay());
    run(&overlay, "overlay");
    if let tripletrail::store::Dataset::TripleTrail(o) = overlay.store() {
        println!("overlay changes at most {} quads", o.unmerged_len());
    }
    assert_eq!(fingerprint(&base), before, "the KG is unchanged");
    let copy = Engine::from_store(store.fork());
    run(&copy, "copy");
    assert_eq!(fingerprint(&base), before, "the KG is unchanged");
    assert_eq!(
        fingerprint(&overlay),
        fingerprint(&copy),
        "the overlay answers as the copy"
    );
    for q in [
        "SELECT (COUNT(*) AS ?n) { ?s a ?c }",
        "SELECT ?c (COUNT(*) AS ?n) { ?s a ?c } GROUP BY ?c ORDER BY ?c",
        "SELECT ?g (COUNT(*) AS ?n) { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g ORDER BY ?g",
    ] {
        assert_eq!(rows(&overlay, q), rows(&copy, q), "{q}");
    }
}

/// `spatialf:transformSRS` writes X and Y with 6 decimals and keeps Z, as Jena (Fuseki 5.6
/// gives "POINT Z(60.430484 9.476223 154.748)"); the rail KG mints gps02 coordinate IRIs from it.
#[test]
fn jena_transform_srs_writes_six_decimals() {
    let e = Engine::new().unwrap();
    let t = one(
        &e,
        r#"SELECT (spatialf:transformSRS("<http://www.opengis.net/def/crs/EPSG/0/25832> POINT Z(526216.596 6699449.551 154.748)"^^geo:wktLiteral,
              "http://www.opengis.net/def/crs/EPSG/0/4326") AS ?t) {}"#,
    );
    assert_eq!(
        t.as_deref(),
        Some(
            "\"<http://www.opengis.net/def/crs/EPSG/0/4326> POINT Z(60.430484 9.476223 154.748)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral>"
        )
    );
}

/// `xsd:dateTime(...) + duration`: the optimizer orders the operands of `+`, and a duration
/// before a dateTime was an error (the rail KG's Oslo dates of `time:inXSDDate` were missing).
#[test]
fn date_time_plus_duration_in_either_order() {
    let e = Engine::new().unwrap();
    let t = one(
        &e,
        r#"SELECT (CONCAT(STR(xsd:dateTime(CONCAT(SUBSTR(STR(?ts), 1, 10), "T12:00:00")) + STRDT(?shift, xsd:dayTimeDuration)), "|",
                          STR(STRDT("P1D", xsd:dayTimeDuration) + xsd:dateTime("2026-09-30T12:00:00"))) AS ?t)
           { VALUES (?ts ?shift) { ("2026-09-30T22:00:00+00:00"^^xsd:dateTimeStamp "P1D") } }"#,
    );
    assert_eq!(
        t.as_deref(),
        Some("\"2026-10-01T12:00:00|2026-10-01T12:00:00\"")
    );
}
