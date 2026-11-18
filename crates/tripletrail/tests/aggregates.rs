//! Aggregates of geometry functions, computed in parallel, give what Oxigraph's evaluator
//! gives: the same queries with the function in a BIND (so the aggregate is of a variable,
//! which the engine leaves to the evaluator) are the reference.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
"#;

fn data() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.com/> .\n@prefix geo: <http://www.opengis.net/ont/geosparql#> .\n",
    );
    for i in 0..300 {
        let (x, y) = (10.0 + (i % 20) as f64 * 0.01, 60.0 + (i / 20) as f64 * 0.01);
        ttl += &format!(
            "ex:l{i} a ex:Line ; ex:zone ex:z{} ; geo:hasGeometry [ geo:asWKT \"LINESTRING({x} {y}, {} {})\"^^geo:wktLiteral ] .\n",
            i % 7,
            x + 0.004,
            y + 0.003
        );
    }
    // Not a geometry: an error for geof:metricLength.
    ttl += "ex:bad a ex:Line ; ex:zone ex:z3 ; geo:hasGeometry [ geo:asWKT \"oops\" ] .\n";
    ttl += "ex:z9 a ex:Zone .\n";
    ttl
}

fn rows(e: &Engine, q: &str) -> Vec<Vec<Option<f64>>> {
    let QueryResults::Solutions(s) = e.query(&format!("{PREFIXES}{q}")).unwrap() else {
        panic!()
    };
    let vars: Vec<_> = s.variables().to_vec();
    let mut out: Vec<Vec<Option<f64>>> = s
        .map(|r| {
            let r = r.unwrap();
            vars.iter()
                .map(|v| {
                    r.get(v).map(|t| match t {
                        tripletrail::oxigraph::model::Term::Literal(l) => {
                            l.value().parse().unwrap()
                        }
                        // Group keys: a number from the IRI.
                        t => t
                            .to_string()
                            .trim_end_matches('>')
                            .rsplit('z')
                            .next()
                            .unwrap()
                            .parse()
                            .unwrap(),
                    })
                })
                .collect()
        })
        .collect();
    out.sort_by(|a, b| a.partial_cmp(b).unwrap());
    out
}

fn same(a: &[Vec<Option<f64>>], b: &[Vec<Option<f64>>]) {
    assert_eq!(a.len(), b.len(), "{a:?} vs {b:?}");
    for (x, y) in a.iter().zip(b) {
        assert_eq!(x.len(), y.len());
        for (u, v) in x.iter().zip(y) {
            match (u, v) {
                (Some(u), Some(v)) => {
                    assert!((u - v).abs() <= 1e-6 * v.abs().max(1.0), "{a:?} vs {b:?}")
                }
                (u, v) => assert_eq!(u, v, "{a:?} vs {b:?}"),
            }
        }
    }
}

#[test]
fn parallel_aggregates_match_the_evaluator() {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let lines = "?l a ex:Line ; ex:zone ?z ; geo:hasGeometry/geo:asWKT ?w";
    let good = "FILTER(?l != ex:bad)";
    for (aggs, refs, filter) in [
        // Every aggregate, per zone, without the error.
        (
            "(SUM(geof:metricLength(?w)) AS ?s) (AVG(geof:metricLength(?w)) AS ?a) (MIN(geof:metricLength(?w)) AS ?lo) (MAX(geof:metricLength(?w)) AS ?hi) (COUNT(geof:metricLength(?w)) AS ?n)",
            "(SUM(?m) AS ?s) (AVG(?m) AS ?a) (MIN(?m) AS ?lo) (MAX(?m) AS ?hi) (COUNT(?m) AS ?n)",
            good,
        ),
        // With the error: SUM and AVG of its zone are unbound, COUNT skips it.
        (
            "(SUM(geof:metricLength(?w)) AS ?s) (AVG(geof:metricLength(?w)) AS ?a) (COUNT(geof:metricLength(?w)) AS ?n)",
            "(SUM(?m) AS ?s) (AVG(?m) AS ?a) (COUNT(?m) AS ?n)",
            "",
        ),
    ] {
        let q = |sel: &str, bind: &str| {
            format!("SELECT ?z {sel} WHERE {{ {lines} {bind} {filter} }} GROUP BY ?z")
        };
        same(
            &rows(&e, &q(aggs, "")),
            &rows(&e, &q(refs, "BIND(geof:metricLength(?w) AS ?m)")),
        );
    }
    // No GROUP BY, nested functions, HAVING and ORDER BY around the group.
    same(
        &rows(
            &e,
            &format!(
                "SELECT (SUM(geof:metricLength(geof:buffer(?w, 10, <http://www.opengis.net/def/uom/OGC/1.0/metre>))) AS ?s) WHERE {{ {lines} {good} }}"
            ),
        ),
        &rows(
            &e,
            &format!(
                "SELECT (SUM(?m) AS ?s) WHERE {{ {lines} {good} BIND(geof:metricLength(geof:buffer(?w, 10, <http://www.opengis.net/def/uom/OGC/1.0/metre>)) AS ?m) }}"
            ),
        ),
    );
    same(
        &rows(
            &e,
            &format!(
                "SELECT ?z (SUM(geof:metricLength(?w)) AS ?s) WHERE {{ {lines} {good} }} GROUP BY ?z HAVING (SUM(geof:metricLength(?w)) > 20000) ORDER BY ?s"
            ),
        ),
        &rows(
            &e,
            &format!(
                "SELECT ?z (SUM(?m) AS ?s) WHERE {{ {lines} {good} BIND(geof:metricLength(?w) AS ?m) }} GROUP BY ?z HAVING (SUM(?m) > 20000) ORDER BY ?s"
            ),
        ),
    );
    // No rows: one group without GROUP BY (SUM 0, MIN unbound), none with it.
    let none = "?l a ex:Nothing ; geo:hasGeometry/geo:asWKT ?w";
    same(
        &rows(
            &e,
            &format!(
                "SELECT (SUM(geof:metricLength(?w)) AS ?s) (MIN(geof:metricLength(?w)) AS ?lo) WHERE {{ {none} }}"
            ),
        ),
        &[vec![Some(0.0), None]],
    );
    assert!(rows(&e, &format!("SELECT ?z (SUM(geof:metricLength(?w)) AS ?s) WHERE {{ {none} ; ex:zone ?z }} GROUP BY ?z")).is_empty());
}
