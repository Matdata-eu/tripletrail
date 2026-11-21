//! The spatial FILTER rewrite gives the same answers as a full scan, and actually kicks in.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX st: <http://w3id.org/tripletrail/st#>
PREFIX uom: <http://www.opengis.net/def/uom/OGC/1.0/>
"#;

/// A 20 x 20 grid of points and short lines around 10°E 60°N, plus the same points in a
/// projected CRS, plus one invalid literal.
fn data() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..20 {
        for j in 0..20 {
            let (x, y) = (10.0 + i as f64 * 0.01, 60.0 + j as f64 * 0.005);
            ttl += &format!(
                "ex:p{i}_{j} geo:hasGeometry [ geo:asWKT \"POINT({x} {y})\"^^geo:wktLiteral ] .\n\
                 ex:l{i}_{j} geo:hasGeometry [ geo:asWKT \"LINESTRING({x} {y}, {} {})\"^^geo:wktLiteral ] .\n\
                 ex:u{i}_{j} geo:hasGeometry [ geo:asWKT \"<http://www.opengis.net/def/crs/EPSG/0/25833> POINT({} {})\"^^geo:wktLiteral ] .\n",
                x + 0.004,
                y + 0.002,
                i * 100,
                j * 100,
            );
        }
    }
    ttl += "ex:bad geo:hasGeometry [ geo:asWKT \"NOT WKT\"^^geo:wktLiteral ] .\n";
    ttl
}

fn sorted(e: &Engine, q: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = e.query(&format!("{PREFIXES}{q}")).unwrap() else {
        panic!()
    };
    let mut out: Vec<String> = s
        .map(|r| {
            r.unwrap()
                .iter()
                .map(|(_, t)| t.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out.sort();
    out
}

fn check(e: &Engine, q: &str, expect_rewrite: bool) -> usize {
    e.set_spatial_rewrite(false);
    let scan = sorted(e, q);
    e.set_spatial_rewrite(true);
    let indexed = sorted(e, q);
    assert_eq!(scan, indexed, "different results for {q}");
    let stats = e.last_spatial_stats();
    assert_eq!(
        stats.filters + stats.joins > 0,
        expect_rewrite,
        "rewrite expectation for {q}"
    );
    indexed.len()
}

const AREA: &str = r#""POLYGON((10.025 60.012, 10.075 60.012, 10.075 60.038, 10.025 60.038, 10.025 60.012))"^^geo:wktLiteral"#;

#[test]
fn predicates_match_full_scan() {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    for f in [
        "geof:sfIntersects",
        "geof:sfWithin",
        "geof:sfContains",
        "geof:sfTouches",
        "geof:sfCrosses",
        "st:ST_Intersects",
        "st:ST_Within",
    ] {
        check(
            &e,
            &format!(
                "SELECT ?f WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w FILTER({f}(?w, {AREA})) }}"
            ),
            true,
        );
    }
    // Constant first.
    let n = check(
        &e,
        &format!(
            "SELECT ?f WHERE {{ ?f geo:hasGeometry ?g . ?g geo:asWKT ?w FILTER(geof:sfContains({AREA}, ?w)) }}"
        ),
        true,
    );
    assert!(n > 0);
    // Disjoint is not indexable; still correct.
    check(
        &e,
        &format!(
            "SELECT ?f WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfDisjoint(?w, {AREA})) }}"
        ),
        false,
    );
}

#[test]
fn distances_match_full_scan() {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let p = r#""POINT(10.05 60.025)"^^geo:wktLiteral"#;
    let n = check(
        &e,
        &format!("SELECT ?w WHERE {{ ?g geo:asWKT ?w FILTER(geof:distance(?w, {p}) < 1500) }}"),
        true,
    );
    assert!(n > 0);
    check(
        &e,
        &format!(
            "SELECT ?w WHERE {{ ?g geo:asWKT ?w FILTER(geof:distance(?w, {p}, uom:kilometre) <= 1.5) }}"
        ),
        true,
    );
    check(
        &e,
        &format!(
            "SELECT ?w WHERE {{ ?g geo:asWKT ?w FILTER(2000 > geof:distance({p}, ?w, uom:metre)) }}"
        ),
        true,
    );
    check(
        &e,
        &format!("SELECT ?w WHERE {{ ?g geo:asWKT ?w FILTER(st:ST_DWithin(?w, {p}, 0.02)) }}"),
        true,
    );
    check(
        &e,
        &format!("SELECT ?w WHERE {{ ?g geo:asWKT ?w FILTER(st:ST_DWithinSphere(?w, {p}, 800)) }}"),
        true,
    );
    check(
        &e,
        &format!("SELECT ?w WHERE {{ ?g geo:asWKT ?w FILTER(st:ST_Distance(?w, {p}) < 0.01) }}"),
        true,
    );
    // Projected CRS: only geometries in that CRS are candidates.
    let u = r#""<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(500 500)"^^geo:wktLiteral"#;
    let n = check(
        &e,
        &format!("SELECT ?w WHERE {{ ?g geo:asWKT ?w FILTER(geof:distance(?w, {u}) <= 150) }}"),
        true,
    );
    assert_eq!(n, 9); // centre, 4 neighbours at 100, 4 diagonals at 141
}

#[test]
fn rewrite_respects_scope_and_composition() {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    // Conjunction: index on the spatial conjunct, the rest still filters.
    check(
        &e,
        &format!(
            "SELECT ?f WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w FILTER(STRSTARTS(STR(?f), \"http://example.com/l\") && geof:sfIntersects(?w, {AREA})) }}"
        ),
        true,
    );
    // Inside a sub-select and a UNION branch.
    check(
        &e,
        &format!(
            "SELECT * WHERE {{ {{ SELECT ?f WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfIntersects(?w, {AREA})) }} }} UNION {{ BIND(ex:x AS ?f) }} }}"
        ),
        true,
    );
    // ?w only bound in an OPTIONAL: not rewritten (the index cannot stand in for unbound rows).
    check(
        &e,
        &format!(
            "SELECT ?f WHERE {{ ?f geo:hasGeometry ?g OPTIONAL {{ ?g geo:asWKT ?w }} FILTER(geof:sfIntersects(?w, {AREA})) }}"
        ),
        false,
    );
    // ?w bound by BIND, not by geo:asWKT: not rewritten.
    check(
        &e,
        &format!("SELECT ?w WHERE {{ BIND({AREA} AS ?w) FILTER(geof:sfIntersects(?w, {AREA})) }}"),
        false,
    );
    // Variable on both sides: a join.
    check(
        &e,
        "SELECT (COUNT(*) AS ?n) WHERE { ex:p1_1 geo:hasGeometry/geo:asWKT ?a . ?g geo:asWKT ?w FILTER(geof:sfEquals(?w, ?a)) }",
        true,
    );
}

#[test]
fn index_follows_updates() {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let q = format!(
        "SELECT ?f WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w FILTER(geof:sfWithin(?w, {AREA})) }}"
    );
    let before = check(&e, &q, true);
    assert_eq!(e.spatial_index().unwrap().stats().skipped, 1);
    e.update(&format!("{PREFIXES} INSERT DATA {{ ex:new geo:hasGeometry [ geo:asWKT \"POINT(10.05 60.02)\"^^geo:wktLiteral ] }}")).unwrap();
    assert_eq!(check(&e, &q, true), before + 1);
}

/// Zones (3 x 3 squares of 0.02°) and 20 x 20 points, plus a feature with a default geometry
/// distinct from its other geometry, and one asserted relation triple.
fn join_data() -> String {
    let mut ttl = data();
    for i in 0..3 {
        for j in 0..3 {
            let (x, y) = (10.0 + i as f64 * 0.07, 60.0 + j as f64 * 0.03);
            ttl += &format!(
                "ex:z{i}_{j} a ex:Zone ; geo:hasGeometry [ geo:asWKT \"POLYGON(({x} {y}, {x2} {y}, {x2} {y2}, {x} {y2}, {x} {y}))\"^^geo:wktLiteral ] .\n",
                x2 = x + 0.02,
                y2 = y + 0.02,
            );
        }
    }
    for i in 0..20 {
        for j in 0..20 {
            ttl += &format!("ex:p{i}_{j} a ex:Spot .\n");
        }
    }
    ttl += "ex:moved a ex:Spot ; geo:hasDefaultGeometry ex:movedDefault ; geo:hasGeometry ex:movedOther .\n\
            ex:movedDefault geo:asWKT \"POINT(10.005 60.005)\"^^geo:wktLiteral .\n\
            ex:movedOther geo:asWKT \"POINT(50 50)\"^^geo:wktLiteral .\n\
            ex:far a ex:Spot ; geo:sfWithin ex:z0_0 .\n";
    ttl
}

fn join_engine() -> Engine {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, join_data().as_bytes()).unwrap();
    e
}

/// A join on term ids reads its sides' boxes from the store and parses only the geometries
/// that pair (#754): it doesn't build or wait for the spatial index, which takes minutes over
/// hundreds of millions of geometries.
#[test]
fn id_joins_do_not_need_the_index() {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, join_data().as_bytes()).unwrap();
    let q = "SELECT ?p ?z WHERE { ?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
             ?z a ex:Zone ; geo:hasGeometry ?zg . ?zg geo:asWKT ?zw FILTER(geof:sfWithin(?pw, ?zw)) }";
    let indexed = sorted(&e, q);
    assert!(!indexed.is_empty());
    let stats = e.last_spatial_stats();
    assert_eq!((stats.joins, stats.materialized), (1, 1));
    assert!(!e.spatial_index_ready());
    e.set_spatial_rewrite(false);
    assert_eq!(sorted(&e, q), indexed);
}

/// A join of few lines against many more polygons, on term ids: lines whose boxes cover
/// polygons they never come near, polygons they cross, and polygons within a distance (#2).
/// The candidates come from the lines' segments and are tested polygon by polygon; the
/// answers equal the full scan's.
#[test]
fn line_joins_on_ids_match_full_scan() {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..30 {
        for j in 0..30 {
            let (x, y) = (10.0 + i as f64 * 0.01, 60.0 + j as f64 * 0.01);
            ttl += &format!(
                "ex:a{i}_{j} a ex:Area ; geo:hasGeometry [ geo:asWKT \"POLYGON(({x} {y}, {x2} {y}, {x2} {y2}, {x} {y2}, {x} {y}))\"^^geo:wktLiteral ] .\n",
                x2 = x + 0.006,
                y2 = y + 0.006,
            );
        }
    }
    // L-shaped and diagonal lines, and a point.
    for k in 0..12 {
        let (x, y) = (10.0 + k as f64 * 0.023, 60.0 + k as f64 * 0.017);
        ttl += &format!(
            "ex:t{k} a ex:Track ; geo:hasGeometry [ geo:asWKT \"LINESTRING({x} {y}, {x2} {y}, {x2} {y2})\"^^geo:wktLiteral ] .\n\
             ex:d{k} a ex:Track ; geo:hasGeometry [ geo:asWKT \"LINESTRING({x} {y2}, {x2} {y})\"^^geo:wktLiteral ] .\n",
            x2 = x + 0.08,
            y2 = y + 0.05,
        );
    }
    ttl += "ex:p a ex:Track ; geo:hasGeometry [ geo:asWKT \"POINT(10.0503 60.0503)\"^^geo:wktLiteral ] .\n";
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let q = |f: &str| {
        format!(
            "SELECT ?t ?a WHERE {{ ?t a ex:Track ; geo:hasGeometry/geo:asWKT ?tw . \
             ?a a ex:Area ; geo:hasGeometry/geo:asWKT ?aw FILTER({f}) }}"
        )
    };
    for f in [
        "geof:sfIntersects(?tw, ?aw)",
        "geof:sfIntersects(?aw, ?tw)",
        "geof:sfCrosses(?tw, ?aw)",
        "geof:sfWithin(?tw, ?aw)",
        "geof:distance(?tw, ?aw, uom:metre) < 150",
        "st:ST_DWithinSphere(?tw, ?aw, 300)",
    ] {
        let n = check(&e, &q(f), true);
        assert!(n > 0, "{f}");
        let stats = e.last_spatial_stats();
        assert!(
            stats
                .decisions
                .iter()
                .any(|d| d.to_string().contains("on term ids")),
            "{f}: {:?}",
            stats.decisions
        );
    }
    check(
        &e,
        "SELECT (COUNT(*) AS ?n) WHERE { ?t a ex:Track ; geo:hasGeometry/geo:asWKT ?tw . \
         ?a a ex:Area ; geo:hasGeometry/geo:asWKT ?aw FILTER(geof:sfIntersects(?tw, ?aw)) }",
        true,
    );
}

#[test]
fn joins_match_full_scan() {
    let e = join_engine();
    let q = |f: &str| {
        format!(
            "SELECT ?p ?z WHERE {{ ?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
             ?z a ex:Zone ; geo:hasGeometry ?zg . ?zg geo:asWKT ?zw FILTER({f}) }}"
        )
    };
    let n = check(&e, &q("geof:sfWithin(?pw, ?zw)"), true);
    assert!(n > 0);
    assert_eq!(e.last_spatial_stats().joins, 1);
    check(&e, &q("geof:sfIntersects(?zw, ?pw)"), true);
    check(&e, &q("geof:ehInside(?pw, ?zw)"), true);
    let n = check(&e, &q("geof:distance(?pw, ?zw) < 600"), true);
    assert!(n > 0);
    check(&e, &q("st:ST_DWithinSphere(?pw, ?zw, 300)"), true);
    // A constant test narrows the join's side.
    let window = r#""POLYGON((10.005 60.005, 10.03 60.005, 10.03 60.03, 10.005 60.03, 10.005 60.005))"^^geo:wktLiteral"#;
    let composed = format!("geof:sfIntersects(?pw, {window}) && geof:sfWithin(?pw, ?zw)");
    let n = check(&e, &q(&composed), true);
    assert!(n > 0);
    assert_eq!(
        (e.last_spatial_stats().filters, e.last_spatial_stats().joins),
        (1, 1)
    );
    // Not separable (both sides hang off ?z): answered from all indexed pairs.
    let tangled = "SELECT ?z ?w1 WHERE { ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?w1 ; geo:hasGeometry/geo:asWKT ?w2 FILTER(geof:sfEquals(?w1, ?w2)) }";
    assert_eq!(check(&e, tangled, true), 9);
}

/// Counts over a spatial join's result table (read whole, or counted without reading the
/// rows) equal the full scan's, also for a variable and DISTINCT, and under a GROUP BY.
#[test]
fn join_counts_match_full_scan() {
    let e = join_engine();
    let body = "?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
                ?z a ex:Zone ; geo:hasGeometry ?zg . ?zg geo:asWKT ?zw FILTER(geof:sfWithin(?pw, ?zw))";
    for head in [
        "(COUNT(*) AS ?n)",
        "(COUNT(?z) AS ?n)",
        "(COUNT(DISTINCT ?z) AS ?n)",
        "(COUNT(DISTINCT ?p) AS ?n)",
    ] {
        check(&e, &format!("SELECT {head} WHERE {{ {body} }}"), true);
    }
    check(
        &e,
        &format!("SELECT ?z (COUNT(*) AS ?n) WHERE {{ {body} }} GROUP BY ?z"),
        true,
    );
    // Two spatial joins, two result tables in one pattern: counted by the evaluator.
    let two = "SELECT (COUNT(*) AS ?n) WHERE { \
               ?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
               ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfWithin(?pw, ?zw)) \
               ?z2 a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw2 . \
               ?y a ex:Zone ; geo:hasGeometry/geo:asWKT ?yw FILTER(geof:sfEquals(?zw2, ?yw)) }";
    check(&e, two, true);
    for head in [
        "SELECT ?z (COUNT(?p) AS ?n) WHERE { BODY } GROUP BY ?z",
        "SELECT ?z (COUNT(DISTINCT ?p) AS ?n) (COUNT(*) AS ?m) WHERE { BODY } GROUP BY ?z",
        "SELECT ?z (COUNT(?p) AS ?n) WHERE { BODY } GROUP BY ?z HAVING(COUNT(?p) > 1)",
        "SELECT ?z (COUNT(?p) AS ?n) WHERE { BODY } GROUP BY ?z ORDER BY DESC(?n) ?z LIMIT 2",
    ] {
        check(&e, &head.replace("BODY", body), true);
    }
    check(
        &e,
        &format!("SELECT ?z ?p WHERE {{ {body} }} LIMIT 5000"),
        true,
    );
    e.set_spatial_rewrite(true);
    let n = sorted(&e, &format!("SELECT (COUNT(*) AS ?n) WHERE {{ {body} }}"));
    assert_ne!(
        n,
        vec!["\"0\"^^<http://www.w3.org/2001/XMLSchema#integer>".to_string()]
    );
}

/// A group's FILTER applies over its OPTIONALs: a join test on variables the required part
/// binds is still answered from the index, and one that reads an optional variable is not.
#[test]
fn joins_under_optional_match_full_scan() {
    let e = join_engine();
    e.update(&format!(
        "{PREFIXES} INSERT DATA {{ ex:z0_0 ex:label \"first\" . ex:p0_0 ex:label \"spot\" .
            ex:far ex:alt \"POINT(10.005 60.005)\"^^geo:wktLiteral }}"
    ))
    .unwrap();
    let required = "?p a ex:Spot ; geo:hasGeometry/geo:asWKT ?pw . \
                    ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw .";
    for q in [
        format!(
            "SELECT ?p ?z ?l WHERE {{ {required} OPTIONAL {{ ?z ex:label ?l }} \
             FILTER(geof:sfWithin(?pw, ?zw)) }}"
        ),
        // Two OPTIONALs, a BIND and a second conjunct.
        format!(
            "SELECT ?p ?z ?l ?m ?k WHERE {{ {required} OPTIONAL {{ ?z ex:label ?l }} \
             OPTIONAL {{ ?p ex:label ?m }} BIND(STR(?p) AS ?k) \
             FILTER(geof:sfWithin(?pw, ?zw) && STRLEN(?k) > 0) }}"
        ),
    ] {
        let n = check(&e, &q, true);
        assert!(n > 0, "{q}");
        assert_eq!(e.last_spatial_stats().joins, 1, "{q}");
    }
    // The geometry comes from the OPTIONAL: rows without it must stay out of the test.
    let optional = "SELECT ?p ?z WHERE { ?p a ex:Spot . ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw . \
                    OPTIONAL { ?p geo:hasGeometry/geo:asWKT ?pw } FILTER(geof:sfWithin(?pw, ?zw)) }";
    assert!(check(&e, optional, false) > 0);
    // Bound in the required part only under an inner OPTIONAL, and by the outer OPTIONAL
    // where it is not: ex:far has only the outer one, and must come out.
    let nested = "SELECT ?p ?z WHERE { { ?p a ex:Spot OPTIONAL { ?p geo:hasGeometry/geo:asWKT ?pw } } \
                  ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw . \
                  OPTIONAL { ?p ex:alt ?pw } FILTER(geof:sfWithin(?pw, ?zw)) }";
    assert!(check(&e, nested, false) > 0);
    assert!(
        sorted(&e, &format!("{PREFIXES}{nested}"))
            .iter()
            .any(|r| r.starts_with("<http://example.com/far>")),
    );
}

#[test]
fn relation_triples_follow_the_query_rewrite_extension() {
    let e = join_engine();
    let triples = sorted(
        &e,
        "SELECT ?p ?z WHERE { ?p a ex:Spot . ?z a ex:Zone . ?p geo:sfWithin ?z }",
    );
    assert!(e.last_spatial_stats().triples == 1 && e.last_spatial_stats().joins == 1);
    // The same through explicit geometries, without any rewriting.
    e.set_spatial_rewrite(false);
    let explicit = sorted(
        &e,
        "SELECT DISTINCT ?p ?z WHERE { ?p a ex:Spot . ?z a ex:Zone .
           { ?p geo:hasDefaultGeometry/geo:asWKT ?pw } UNION
           { ?p geo:hasGeometry/geo:asWKT ?pw FILTER NOT EXISTS { ?p geo:hasDefaultGeometry ?d } }
           ?z geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfWithin(?pw, ?zw)) }",
    );
    e.set_spatial_rewrite(true);
    let mut expected = explicit;
    expected.push("<http://example.com/far> <http://example.com/z0_0>".into()); // asserted
    expected.sort();
    assert_eq!(triples, expected);
    // The default geometry wins over other geometries.
    assert!(
        triples
            .iter()
            .any(|r| r.starts_with("<http://example.com/moved> "))
    );
    // A constant on one side, and geometries rather than features.
    let one = sorted(
        &e,
        "SELECT ?p WHERE { ?p a ex:Spot ; geo:sfWithin ex:z1_1 }",
    );
    assert!(!one.is_empty());
    let geoms = sorted(
        &e,
        "SELECT ?g WHERE { ex:z1_1 geo:hasGeometry ?zg . ?p a ex:Spot ; geo:hasGeometry ?g . ?g geo:sfWithin ?zg }",
    );
    assert_eq!(geoms.len(), one.len());
    // Disjointness relations are rewritten too, just not through the index.
    let disjoint = sorted(
        &e,
        "SELECT ?z WHERE { ?z a ex:Zone . ex:z0_0 geo:sfDisjoint ?z }",
    );
    assert_eq!(disjoint.len(), 8);
}

#[test]
fn nearest_neighbours_match_full_sort() {
    // A grid in CRS84, plus two points in another CRS and an invalid literal: their distance
    // is an error, and SPARQL sorts errors first (in no particular order among themselves).
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..20 {
        for j in 0..20 {
            let (x, y) = (10.0 + i as f64 * 0.01, 60.0 + j as f64 * 0.005);
            ttl += &format!(
                "ex:p{i}_{j} geo:hasGeometry [ geo:asWKT \"POINT({x} {y})\"^^geo:wktLiteral ] .\n\
                 ex:l{i}_{j} geo:hasGeometry [ geo:asWKT \"LINESTRING({x} {y}, {} {})\"^^geo:wktLiteral ] .\n",
                x + 0.004,
                y + 0.002,
            );
        }
    }
    for (n, x) in [(0, 700), (1, 900)] {
        ttl += &format!(
            "ex:u{n} a ex:Zone ; geo:hasGeometry [ geo:asWKT \"<http://www.opengis.net/def/crs/EPSG/0/25833> POINT({x} 1300)\"^^geo:wktLiteral ] .\n"
        );
    }
    ttl += "ex:z0 a ex:Zone ; geo:hasGeometry [ geo:asWKT \"POLYGON((10.01 60.01, 10.02 60.01, 10.02 60.02, 10.01 60.01))\"^^geo:wktLiteral ] .\n\
            ex:z1 a ex:Zone ; geo:hasGeometry [ geo:asWKT \"POINT(10.3 60.3)\"^^geo:wktLiteral ] .\n\
            ex:bad geo:hasGeometry [ geo:asWKT \"NOT WKT\"^^geo:wktLiteral ] .\n";
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    // Off the grid, so no two valid geometries are at the same distance.
    let p = r#""POINT(10.0613 60.0371)"^^geo:wktLiteral"#;
    let u = r#""<http://www.opengis.net/def/crs/EPSG/0/25833> POINT(713 1288)"^^geo:wktLiteral"#;
    let queries = [
        (
            format!(
                "SELECT ?f WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w }} ORDER BY (geof:distance(?w, {p})) LIMIT 5"
            ),
            true,
        ),
        (
            format!(
                "SELECT ?f WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w }} ORDER BY (geof:distance(?w, {p})) LIMIT 3 OFFSET 4"
            ),
            true,
        ),
        (
            format!(
                "SELECT ?f ?d WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w BIND(geof:distance(?w, {p}, uom:kilometre) AS ?d) }} ORDER BY ?d LIMIT 7"
            ),
            true,
        ),
        (
            format!(
                "SELECT ?f (st:ST_DistanceSphere(?w, {p}) AS ?d) WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w }} ORDER BY ?d LIMIT 4"
            ),
            true,
        ),
        (
            format!(
                "SELECT DISTINCT ?f WHERE {{ ?f a ex:Zone ; geo:hasGeometry/geo:asWKT ?w }} ORDER BY (geof:distance(?w, {p})) LIMIT 2"
            ),
            true,
        ),
        (
            format!(
                "SELECT ?f WHERE {{ ?f a ex:Zone ; geo:hasGeometry/geo:asWKT ?w }} ORDER BY (st:ST_Distance(?w, {u})) LIMIT 3"
            ),
            true,
        ),
        // More than exist: falls back to the full query, same answer.
        (
            format!(
                "SELECT ?f WHERE {{ ?f a ex:Zone ; geo:hasGeometry/geo:asWKT ?w }} ORDER BY (geof:distance(?w, {p})) LIMIT 50"
            ),
            true,
        ),
        // Over a subselect with LIMIT: each bounded round would evaluate it again, so the
        // evaluator sorts its rows once (#359).
        (
            format!(
                "SELECT ?f WHERE {{ {{ SELECT ?f ?w WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w }} ORDER BY ?f LIMIT 200 }} }} ORDER BY (geof:distance(?w, {p})) LIMIT 5"
            ),
            false,
        ),
        (
            format!(
                "SELECT ?f WHERE {{ {{ SELECT ?f ?w WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w }} OFFSET 100 LIMIT 300 }} }} ORDER BY (geof:distance(?w, {p})) LIMIT 5"
            ),
            false,
        ),
        // Not the shape: descending order.
        (
            format!(
                "SELECT ?f WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w }} ORDER BY DESC(geof:distance(?w, {p})) LIMIT 3"
            ),
            false,
        ),
    ];
    for (q, indexed) in &queries {
        e.set_spatial_rewrite(false);
        let scan = sorted(&e, q);
        e.set_spatial_rewrite(true);
        let fast = sorted(&e, q);
        assert_eq!(scan, fast, "{q}");
        assert!(!fast.is_empty(), "{q}");
        assert_eq!(e.last_spatial_stats().filters > 0, *indexed, "{q}");
    }
}

#[test]
fn index_follows_geometry_writes() {
    for e in [Engine::new().unwrap(), Engine::new_tripletrail()] {
        e.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
        let q = r#"SELECT ?g WHERE { ?g geo:asWKT ?w
            FILTER(geof:sfIntersects(?w, "POLYGON((19.9 69.9, 20.1 69.9, 20.1 70.1, 19.9 70.1, 19.9 69.9))"^^geo:wktLiteral)) }"#;
        assert_eq!(check(&e, q, true), 0);
        let before = e.spatial_index().unwrap().stats().geometries;
        // Two geometries with the same literal, then one of them removed: the literal stays.
        e.update(&format!(
            r#"{PREFIXES} INSERT DATA {{ ex:n1 geo:asWKT "POINT(20 70)"^^geo:wktLiteral .
                ex:n2 geo:asWKT "POINT(20 70)"^^geo:wktLiteral .
                ex:n3 geo:asWKT "<http://www.opengis.net/def/crs/EPSG/0/3035> POINT(1 1)"^^geo:wktLiteral }}"#
        ))
        .unwrap();
        assert_eq!(e.spatial_index().unwrap().stats().geometries, before + 2);
        assert_eq!(check(&e, q, true), 2);
        e.update(&format!(
            r#"{PREFIXES} DELETE DATA {{ ex:n1 geo:asWKT "POINT(20 70)"^^geo:wktLiteral }}"#
        ))
        .unwrap();
        assert_eq!(check(&e, q, true), 1);
        e.update(&format!(
            r#"{PREFIXES} DELETE WHERE {{ ?g geo:asWKT "POINT(20 70)"^^geo:wktLiteral }}"#
        ))
        .unwrap();
        assert_eq!(check(&e, q, true), 0);
        assert_eq!(e.spatial_index().unwrap().stats().geometries, before + 1);
        // Bulk loads patch the index too.
        e.load_quads([tripletrail::oxigraph::model::Quad::new(
            tripletrail::oxigraph::model::NamedNode::new_unchecked("http://example.com/n4"),
            tripletrail::oxigraph::model::NamedNode::new_unchecked(
                "http://www.opengis.net/ont/geosparql#asWKT",
            ),
            tripletrail::oxigraph::model::Literal::new_typed_literal(
                "POINT(20.05 70.05)",
                tripletrail::oxigraph::model::NamedNode::new_unchecked(
                    "http://www.opengis.net/ont/geosparql#wktLiteral",
                ),
            ),
            tripletrail::oxigraph::model::GraphName::DefaultGraph,
        )])
        .unwrap();
        assert_eq!(check(&e, q, true), 1);
        assert_eq!(e.spatial_index().unwrap().stats().geometries, before + 2);
    }
}

/// One update that adds many geometries (put in the index in bulk), some of them twice or
/// already there, then one that removes some and adds others in the same request.
#[test]
fn index_follows_large_updates() {
    for e in [Engine::new().unwrap(), Engine::new_tripletrail()] {
        e.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
        let q = format!("SELECT ?g WHERE {{ ?g geo:asWKT ?w FILTER(geof:sfWithin(?w, {AREA})) }}");
        let before_rows = check(&e, &q, true);
        let before = e.spatial_index().unwrap().stats().geometries;
        // 3000 new points over the grid, each on two features; and one point already there.
        let mut insert = String::new();
        for i in 0..3000 {
            let (x, y) = (
                10.0 + (i % 60) as f64 * 0.0021,
                60.0 + (i / 60) as f64 * 0.0011,
            );
            insert += &format!(
                "ex:a{i} geo:asWKT \"POINT({x} {y})\"^^geo:wktLiteral . \
                 ex:b{i} geo:asWKT \"POINT({x} {y})\"^^geo:wktLiteral .\n"
            );
        }
        insert += "ex:again geo:asWKT \"POINT(10.05 60.02)\"^^geo:wktLiteral .\n";
        e.update(&format!("{PREFIXES} INSERT DATA {{ {insert} }}"))
            .unwrap();
        let added = e.spatial_index().unwrap().stats().geometries - before;
        let rows = check(&e, &q, true);
        assert!(rows > before_rows + 2 * 500, "{before_rows} -> {rows}");
        // The same as an index built from scratch.
        let fresh = Engine::new_tripletrail();
        fresh.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
        fresh
            .update(&format!("{PREFIXES} INSERT DATA {{ {insert} }}"))
            .unwrap();
        assert_eq!(
            added + before,
            fresh.spatial_index().unwrap().stats().geometries
        );
        // (Counts: the grid's geometries are blank nodes, named differently per engine.)
        assert_eq!(sorted(&fresh, &q).len(), rows);
        // Removes and adds in one request: the a points go, the b points (same literals) keep
        // them, the p1_ points go, and a new point comes.
        let mixed = format!(
            "{PREFIXES} DELETE {{ ?a geo:asWKT ?w }} WHERE {{ ?a geo:asWKT ?w FILTER(STRSTARTS(STR(?a), \"http://example.com/a\")) }} ;
             DELETE {{ ?h geo:asWKT ?w }} WHERE {{ ?g geo:hasGeometry ?h . ?h geo:asWKT ?w . FILTER(STRSTARTS(STR(?g), \"http://example.com/p1_\")) }} ;
             INSERT DATA {{ ex:c1 geo:asWKT \"POINT(10.05 60.03)\"^^geo:wktLiteral }}"
        );
        e.update(&mixed).unwrap();
        let rows = check(&e, &q, true);
        // The same as an index built after the writes.
        let fresh = Engine::new_tripletrail();
        fresh.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
        fresh
            .update(&format!("{PREFIXES} INSERT DATA {{ {insert} }}"))
            .unwrap();
        fresh.update(&mixed).unwrap();
        assert_eq!(
            e.spatial_index().unwrap().stats().geometries,
            fresh.spatial_index().unwrap().stats().geometries
        );
        assert_eq!(rows, sorted(&fresh, &q).len());
    }
}

/// Geometries on predicates other than `geo:asWKT` (as in GeoNames, LinkedGeoData or CORINE
/// exports), with values of those predicates that are not geometries.
fn other_predicates() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..20 {
        for j in 0..20 {
            let (x, y) = (10.0 + i as f64 * 0.01, 60.0 + j as f64 * 0.005);
            ttl += &format!(
                "ex:p{i}_{j} ex:wkt \"POINT({x} {y})\"^^geo:wktLiteral .\n\
                 ex:r{i}_{j} ex:shape \"POLYGON(({x} {y}, {} {y}, {} {}, {x} {}, {x} {y}))\"^^geo:wktLiteral .\n",
                x + 0.006,
                x + 0.006,
                y + 0.003,
                y + 0.003,
            );
        }
    }
    // Values that are not geometries: an error for every spatial function.
    ttl += "ex:s ex:wkt \"POINT(10.05 60.02)\" .\nex:t ex:wkt ex:p1_1 .\n";
    ttl
}

#[test]
fn geometries_on_any_predicate_are_indexed() {
    for e in [Engine::new().unwrap(), Engine::new_tripletrail()] {
        e.load(RdfFormat::Turtle, other_predicates().as_bytes())
            .unwrap();
        assert_eq!(e.spatial_index().unwrap().stats().geometries, 800);
        let constant =
            format!("SELECT ?f WHERE {{ ?f ex:wkt ?w FILTER(geof:sfIntersects(?w, {AREA})) }}");
        assert!(check(&e, &constant, true) > 0);
        // A variable predicate: every predicate is indexed.
        let any = format!("SELECT ?f ?p WHERE {{ ?f ?p ?w FILTER(geof:sfWithin(?w, {AREA})) }}");
        assert!(check(&e, &any, true) > 0);
        // A join between two predicates.
        let join = "SELECT ?p ?r WHERE { ?p ex:wkt ?a . ?r ex:shape ?b FILTER(geof:sfIntersects(?a, ?b)) }";
        assert!(check(&e, join, true) > 0);
        // Writes on another predicate patch the index.
        e.update(&format!(
            r#"{PREFIXES} INSERT DATA {{ ex:new ex:wkt "POINT(10.051 60.021)"^^geo:wktLiteral }}"#
        ))
        .unwrap();
        assert_eq!(e.spatial_index().unwrap().stats().geometries, 801);
        assert!(check(&e, &constant, true) > 0);
        // A predicate left out of the index is scanned instead.
        e.set_indexed_predicates(tripletrail::spatial::IndexedPredicates {
            only: None,
            except: ["http://example.com/wkt".to_owned()].into(),
        });
        check(&e, &constant, false);
        check(&e, &any, false);
        check(&e, join, false);
        assert_eq!(e.spatial_index().unwrap().stats().geometries, 400);
    }
}

#[test]
fn cancellation_covers_planning() {
    use tripletrail::oxigraph::sparql::CancellationToken;
    let e = join_engine();
    let token = CancellationToken::new();
    let cancelled = e.with_cancellation_token(token.clone());
    token.cancel();
    let join = "SELECT ?f ?z WHERE { ?f geo:hasGeometry/geo:asWKT ?w . ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfWithin(?w, ?zw)) }";
    let Err(err) = cancelled.query(&format!("{PREFIXES}{join}")) else {
        panic!("a cancelled query fails")
    };
    assert!(err.to_string().contains("cancelled"), "{err}");
    // The rewrite stopped before probing the index.
    let stats = cancelled.last_spatial_stats();
    assert_eq!(stats.joins + stats.candidates, 0);
    let err = cancelled
        .update(&format!(
            "{PREFIXES} INSERT {{ ?f ex:in ?z }} WHERE {{ ?f geo:hasGeometry/geo:asWKT ?w . ?z a ex:Zone ; geo:hasGeometry/geo:asWKT ?zw FILTER(geof:sfWithin(?w, ?zw)) }}"
        ))
        .expect_err("a cancelled update fails");
    assert!(err.to_string().contains("cancelled"), "{err}");
    // The engine without the token still answers.
    assert!(!sorted(&e, join).is_empty());
}

#[test]
fn writes_do_not_wait_for_the_index_build() {
    use std::time::{Duration, Instant};
    let e = Engine::new().unwrap();
    // Polygons are slow enough to index that the build takes a while.
    let mut nt = String::new();
    for i in 0..40_000 {
        let (x, y) = ((i % 200) as f64 * 0.01, (i / 200) as f64 * 0.01);
        nt += &format!(
            "<http://example.com/f{i}> <http://www.opengis.net/ont/geosparql#asWKT> \"POLYGON(({x} {y}, {} {y}, {} {}, {x} {}, {x} {y}))\"^^<http://www.opengis.net/ont/geosparql#wktLiteral> .\n",
            x + 0.005,
            x + 0.005,
            y + 0.005,
            y + 0.005
        );
    }
    e.load(RdfFormat::NTriples, nt.as_bytes()).unwrap();
    let started = Instant::now();
    let built = Instant::now();
    let builder = {
        let e = e.clone();
        std::thread::spawn(move || {
            e.spatial_index().unwrap();
            built.elapsed()
        })
    };
    std::thread::sleep(Duration::from_millis(20));
    e.update(
        "INSERT DATA { <http://example.com/new> <http://www.opengis.net/ont/geosparql#asWKT> \
         \"POINT(50 50)\"^^<http://www.opengis.net/ont/geosparql#wktLiteral> }",
    )
    .unwrap();
    let wrote = started.elapsed();
    let build = builder.join().unwrap();
    assert!(wrote < build, "insert {wrote:?}, build {build:?}");
    // The build the insert overtook was done again: the index has the new point.
    let found = sorted(
        &e,
        "SELECT ?f WHERE { ?f geo:asWKT ?w . \
         FILTER(geof:sfIntersects(?w, \"POLYGON((49 49, 51 49, 51 51, 49 51, 49 49))\"^^geo:wktLiteral)) }",
    );
    assert_eq!(found, ["<http://example.com/new>"]);
    assert!(e.spatial_index_ready());
}

#[test]
fn nearest_neighbours_on_another_predicate_and_in_few_rounds() {
    // 2 000 points on ex:geom (not geo:asWKT), one in ten of them a station.
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..2000 {
        let (x, y) = (
            10.0 + (i % 50) as f64 * 0.01,
            60.0 + (i / 50) as f64 * 0.005,
        );
        let class = if i % 10 == 0 { "Station" } else { "Signal" };
        ttl += &format!("ex:f{i} a ex:{class} ; ex:geom \"POINT({x} {y})\"^^geo:wktLiteral .\n");
    }
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let p = r#""POINT(10.2013 60.0771)"^^geo:wktLiteral"#;
    let q = format!(
        "SELECT ?f WHERE {{ ?f a ex:Station ; ex:geom ?w }} ORDER BY (geof:distance(?w, {p}, uom:metre)) LIMIT 10"
    );
    let check = |expect_index: bool| {
        e.set_spatial_rewrite(false);
        let scan = sorted(&e, &q);
        e.set_spatial_rewrite(true);
        let fast = sorted(&e, &q);
        assert_eq!(scan, fast);
        assert_eq!(fast.len(), 10);
        let rounds = e.last_spatial_stats().filters;
        if expect_index {
            // The first radius comes from the index and grows by the shortfall.
            assert!((1..=2).contains(&rounds), "{rounds} rounds");
        } else {
            assert_eq!(rounds, 0);
        }
    };
    check(true);
    // An object of ex:geom that is no geometry has an error distance and sorts first: the
    // index cannot answer that, so the full query runs (and the answer stays right).
    e.update(&format!(
        "{PREFIXES} INSERT DATA {{ ex:odd a ex:Station ; ex:geom \"not a geometry\" }}"
    ))
    .unwrap();
    check(false);
    e.update(&format!(
        "{PREFIXES} DELETE DATA {{ ex:odd a ex:Station ; ex:geom \"not a geometry\" }}"
    ))
    .unwrap();
    check(true);
}

/// A join whose side is a subselect with `LIMIT` (300 probes against all geometries) is
/// answered through the index, not one nested loop over all pairs.
#[test]
fn joins_with_a_limited_side_use_the_index() {
    let e = join_engine();
    for f in [
        "geof:distance(?a, ?b, uom:metre) < 300",
        "geof:sfIntersects(?a, ?b)",
    ] {
        for sub in [
            "SELECT ?a WHERE { ?x geo:asWKT ?a } ORDER BY ?a LIMIT 30",
            "SELECT ?a WHERE { ?x geo:asWKT ?a } ORDER BY DESC(?a) LIMIT 20 OFFSET 7",
            "SELECT DISTINCT ?a WHERE { ?x geo:asWKT ?a } ORDER BY ?a LIMIT 25",
        ] {
            let q = format!("SELECT ?a ?b WHERE {{ {{ {sub} }} ?y geo:asWKT ?b FILTER({f}) }}");
            assert!(check(&e, &q, true) > 0, "{q}");
            assert_eq!(e.last_spatial_stats().joins, 1, "{q}");
        }
        // Without an order, any 30 of the side's rows: the same ones here, as a scan reads
        // them in the same order (the scan comparison checks the answer).
        let q = format!(
            "SELECT (COUNT(DISTINCT ?a) AS ?n) WHERE {{ {{ SELECT ?a WHERE {{ ?x geo:asWKT ?a }} LIMIT 30 }} ?y geo:asWKT ?b FILTER({f}) }}"
        );
        check(&e, &q, true);
        // Each meets at least itself, but for the invalid literal (which rows the LIMIT
        // keeps depends on the store's ids).
        let n = sorted(&e, &q);
        assert!(
            n == ["\"30\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
                || n == ["\"29\"^^<http://www.w3.org/2001/XMLSchema#integer>"],
            "{q}: {n:?}"
        );
    }
}

#[test]
fn writing_quads_replaces_a_graph_in_one_transaction_and_keeps_the_index() {
    use tripletrail::Replace;
    use tripletrail::oxigraph::model::{GraphNameRef, Literal, NamedNode, Quad};
    let e = Engine::new_tripletrail();
    let wkt = "http://www.opengis.net/ont/geosparql#asWKT";
    let point = |s: &str, x: u32, g: &str| {
        Quad::new(
            NamedNode::new_unchecked(s),
            NamedNode::new_unchecked(wkt),
            Literal::new_typed_literal(
                format!("POINT({x} {x})"),
                NamedNode::new_unchecked("http://www.opengis.net/ont/geosparql#wktLiteral"),
            ),
            NamedNode::new_unchecked(g),
        )
    };
    e.write_quads(
        Replace::Nothing,
        vec![
            point("http://example.com/a", 1, "urn:a"),
            point("http://example.com/b", 5, "urn:b"),
        ],
    )
    .unwrap();
    e.spatial_index().unwrap();
    assert!(e.spatial_index_ready());
    // Replacing graph urn:a: its point goes, the new one comes, urn:b stays.
    let g = NamedNode::new_unchecked("urn:a");
    e.write_quads(
        Replace::Graph(GraphNameRef::NamedNode(g.as_ref())),
        vec![point("http://example.com/c", 9, "urn:a")],
    )
    .unwrap();
    assert!(
        e.spatial_index_ready(),
        "a small write keeps the spatial index"
    );
    let found = sorted(
        &e,
        "SELECT ?f WHERE { GRAPH ?g { ?f geo:asWKT ?w } \
         FILTER(geof:sfIntersects(?w, \"POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))\"^^geo:wktLiteral)) }",
    );
    assert_eq!(found, ["<http://example.com/b>", "<http://example.com/c>"]);
    e.write_quads(Replace::All, Vec::new()).unwrap();
    assert_eq!(
        sorted(&e, "SELECT ?s WHERE { GRAPH ?g { ?s ?p ?o } }"),
        Vec::<String>::new()
    );
}
