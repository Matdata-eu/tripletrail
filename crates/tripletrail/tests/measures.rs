//! Areas computed ahead on every core (#703): the answers are those the evaluator gives
//! without it (the Oxigraph store, whose geometries are parsed per call).

use std::fmt::Write;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

fn sorted(e: &Engine, q: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = e.query(q).unwrap() else {
        panic!("not a SELECT")
    };
    let mut out: Vec<String> = s
        .map(|r| {
            r.unwrap()
                .iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out.sort();
    out
}

#[test]
fn areas_computed_ahead_are_the_evaluators() {
    // 1500 small squares near 60° N in a named graph, 1200 in the default graph, a few in
    // a projected CRS, and a line (area 0) and a point.
    let mut d = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    let square = |x: f64, y: f64, s: f64| {
        format!(
            "POLYGON(({x} {y}, {} {y}, {} {}, {x} {}, {x} {y}))",
            x + s,
            x + s,
            y + s,
            y + s
        )
    };
    d.push_str("ex:g {\n");
    for i in 0..1500 {
        let (x, y) = (
            10.0 + f64::from(i % 50) * 0.01,
            60.0 + f64::from(i / 50) * 0.01,
        );
        writeln!(
            d,
            "ex:a{i} geo:asWKT \"{}\"^^geo:wktLiteral .",
            square(x, y, 0.004)
        )
        .unwrap();
    }
    d.push_str("}\n");
    for i in 0..1200 {
        let (x, y) = (
            5.0 + f64::from(i % 40) * 0.02,
            50.0 + f64::from(i / 40) * 0.02,
        );
        writeln!(
            d,
            "ex:b{i} geo:asWKT \"{}\"^^geo:wktLiteral .",
            square(x, y, 0.01)
        )
        .unwrap();
    }
    for i in 0..5 {
        writeln!(
            d,
            "ex:p{i} geo:asWKT \"<http://www.opengis.net/def/crs/EPSG/0/25832> {}\"^^geo:wktLiteral .",
            square(500000.0 + f64::from(i) * 100.0, 6000000.0, 50.0)
        )
        .unwrap();
    }
    d.push_str("ex:l geo:asWKT \"LINESTRING(1 1, 2 2)\"^^geo:wktLiteral .\n");
    d.push_str("ex:q geo:asWKT \"POINT(1 1)\"^^geo:wktLiteral .\n");
    let prefixes = "PREFIX ex: <http://example.org/> PREFIX geo: <http://www.opengis.net/ont/geosparql#> PREFIX geof: <http://www.opengis.net/def/function/geosparql/>";
    let queries = [
        "SELECT ?f (geof:metricArea(?w) AS ?a) WHERE { GRAPH ex:g { ?f geo:asWKT ?w } }",
        "SELECT ?f ?a WHERE { ?f geo:asWKT ?w BIND(geof:area(?w) AS ?a) }",
        "SELECT ?f (geof:metricSize(?w) * 2 AS ?a) WHERE { ?f geo:asWKT ?w }",
        "SELECT ?f (geof:size(?w) AS ?a) WHERE { ?f geo:asWKT ?w FILTER(STRSTARTS(STR(?f), \"http://example.org/b\")) }",
        // A graph the store doesn't hold.
        "SELECT ?f (geof:metricArea(?w) AS ?a) WHERE { GRAPH ex:none { ?f geo:asWKT ?w } }",
    ];
    let reference = Engine::new_oxigraph().unwrap();
    reference.load(RdfFormat::TriG, d.as_bytes()).unwrap();
    let engine = Engine::new_tripletrail();
    engine.load(RdfFormat::TriG, d.as_bytes()).unwrap();
    for q in queries {
        let q = format!("{prefixes} {q}");
        let before = tripletrail::measures_warmed();
        let got = sorted(&engine, &q);
        let warmed = tripletrail::measures_warmed() - before;
        assert_eq!(got, sorted(&reference, &q), "{q}");
        // And again, from the areas the geometries keep.
        assert_eq!(got, sorted(&engine, &q), "{q}");
        if q.contains("ex:g {") {
            assert!(warmed >= 1500, "{warmed} {q}");
        }
    }
    // Under a LIMIT the evaluator may need a few areas: none are computed ahead, unless an
    // ORDER BY needs every row; nor for a pattern that would be evaluated twice.
    let fresh = Engine::new_tripletrail();
    fresh.load(RdfFormat::TriG, d.as_bytes()).unwrap();
    for (q, ahead) in [
        (
            "SELECT (geof:metricArea(?w) AS ?a) WHERE { GRAPH ex:g { ?f geo:asWKT ?w } } LIMIT 10",
            false,
        ),
        (
            "SELECT (geof:metricArea(?w) AS ?a) WHERE { GRAPH ex:g { ?f geo:asWKT ?w } } OFFSET 20 LIMIT 10",
            false,
        ),
        (
            "SELECT * WHERE { { SELECT ?f (geof:area(?w) AS ?a) WHERE { ?f geo:asWKT ?w } LIMIT 5 } }",
            false,
        ),
        (
            "SELECT ?f (geof:area(?w) AS ?a) WHERE { ?f geo:asWKT ?w . ?f geo:asWKT ?w2 }",
            false,
        ),
        (
            "SELECT (geof:metricArea(?w) AS ?a) WHERE { GRAPH ex:g { ?f geo:asWKT ?w } } ORDER BY DESC(?a) LIMIT 3",
            true,
        ),
    ] {
        let q = format!("{prefixes} {q}");
        let before = tripletrail::measures_warmed();
        let got = sorted(&fresh, &q);
        let warmed = tripletrail::measures_warmed() - before;
        assert_eq!(warmed > 0, ahead, "{warmed} {q}");
        if !q.contains("LIMIT") || q.contains("ORDER BY") {
            assert_eq!(got, sorted(&reference, &q), "{q}");
        }
    }
}
