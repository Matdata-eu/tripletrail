//! Statistical aggregates (#190): tt:median, tt:percentile, tt:stddev, tt:variance, tt:mode,
//! tt:first, tt:last, tt:topK, and Jena's agg: names.

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::QueryResults;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:a ex:line ex:L1 ; ex:len 1 ; ex:speed 80.0 ; ex:type "ballast" .
ex:b ex:line ex:L1 ; ex:len 2 ; ex:speed 120.0 ; ex:type "slab" .
ex:c ex:line ex:L1 ; ex:len 4 ; ex:speed "160"^^xsd:double ; ex:type "ballast" .
ex:d ex:line ex:L1 ; ex:len 10 ; ex:speed 200.0 ; ex:type "slab" .
ex:e ex:line ex:L2 ; ex:len 7 ; ex:speed 60.5 ; ex:type "slab" .
ex:f ex:line ex:L2 ; ex:len "x" .
"#;

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e
}

/// The values of the query's one row, as text (empty when unbound).
fn row(e: &Engine, q: &str) -> Vec<String> {
    let q = format!(
        "PREFIX ex: <http://example.com/>\nPREFIX tt: <http://w3id.org/tripletrail/fn#>\nPREFIX agg: <http://jena.apache.org/ARQ/function/aggregate#>\n{q}"
    );
    let QueryResults::Solutions(s) = e.query(&q).unwrap() else {
        panic!()
    };
    let vars: Vec<_> = s.variables().to_vec();
    let rows: Vec<_> = s.map(Result::unwrap).collect();
    assert_eq!(rows.len(), 1, "{q}");
    vars.iter()
        .map(|v| rows[0].get(v).map_or(String::new(), ToString::to_string))
        .collect()
}

const DEC: &str = "^^<http://www.w3.org/2001/XMLSchema#decimal>";
const DBL: &str = "^^<http://www.w3.org/2001/XMLSchema#double>";

#[test]
fn exact_statistics_of_integers_are_decimals() {
    let e = engine();
    let r = row(
        &e,
        "SELECT (tt:median(?l) AS ?m) (tt:percentile(?l, 0.25) AS ?p) (tt:varPop(?l) AS ?v)
                (tt:variance(?l) AS ?s) (agg:var_samp(?l) AS ?j) (tt:topK(?l, 2) AS ?t)
         WHERE { ex:L1 ^ex:line/ex:len ?l }",
    );
    // 1 2 4 10: median 3, 25th percentile at 0.75 between 1 and 2.
    // Integral decimals are written as Jena writes them (as AVG gives them).
    assert_eq!(r[0], format!("\"3.0\"{DEC}"));
    assert_eq!(r[1], format!("\"1.75\"{DEC}"));
    // Mean 4.25; squared deviations 10.5625 + 5.0625 + 0.0625 + 33.0625 = 48.75.
    assert_eq!(r[2], format!("\"12.1875\"{DEC}"));
    assert_eq!(r[3], format!("\"16.25\"{DEC}"));
    assert_eq!(r[3], r[4]);
    assert_eq!(r[5], "\"10 4\"");
}

#[test]
fn a_double_makes_the_result_a_double_and_stddev_is_a_square_root() {
    let e = engine();
    let r = row(
        &e,
        "SELECT (tt:median(?s) AS ?m) (tt:stddevPop(?l) AS ?d) (agg:stdev(?l) AS ?j)
         WHERE { ex:L1 ^ex:line ?x . ?x ex:speed ?s ; ex:len ?l }",
    );
    assert_eq!(r[0], format!("\"140\"{DBL}"));
    let sd: f64 = r[1].split('"').nth(1).unwrap().parse().unwrap();
    assert!((sd - 12.1875f64.sqrt()).abs() < 1e-9, "{sd}");
    assert!(r[1].ends_with(DBL));
    let sample: f64 = r[2].split('"').nth(1).unwrap().parse().unwrap();
    assert!((sample - 16.25f64.sqrt()).abs() < 1e-9, "{sample}");
}

#[test]
fn mode_first_last_and_unbound_cases() {
    let e = engine();
    let r = row(
        &e,
        "SELECT (tt:mode(?t) AS ?m) (agg:mode(?t) AS ?j) (tt:first(?t) AS ?f) (tt:last(?t) AS ?l)
         WHERE { ?x ex:type ?t }",
    );
    assert_eq!(r[0], "\"slab\"");
    assert_eq!(r[0], r[1]);
    assert!(!r[2].is_empty() && !r[3].is_empty());
    // A value that is not a number, one value for a sample variance, an out-of-range
    // percentile: unbound.
    let r = row(
        &e,
        "SELECT (tt:median(?l) AS ?m) (tt:variance(?s) AS ?v) (tt:percentile(?l, 1.5) AS ?p)
                (tt:varPop(?s) AS ?pop)
         WHERE { ex:L2 ^ex:line ?x . ?x ex:len ?l . OPTIONAL { ?x ex:speed ?s } }",
    );
    assert_eq!(r[..3], ["", "", ""]);
    // An unbound value (from the OPTIONAL) is an error, as for SUM.
    assert_eq!(r[3], "");
    let r = row(
        &e,
        "SELECT (tt:varPop(?s) AS ?v) WHERE { ex:e ex:speed ?s }",
    );
    assert_eq!(r[0], format!("\"0.0\"{DEC}"));
}

#[test]
fn per_group_with_distinct() {
    let e = engine();
    let QueryResults::Solutions(s) = e
        .query(
            "PREFIX ex: <http://example.com/> PREFIX tt: <http://w3id.org/tripletrail/fn#>
             SELECT ?line (tt:percentile(DISTINCT ?l, 0.5) AS ?p) WHERE {
               ?x ex:line ?line ; ex:len ?l . FILTER(isNumeric(?l)) } GROUP BY ?line ORDER BY ?line",
        )
        .unwrap()
    else {
        panic!()
    };
    let rows: Vec<String> = s
        .map(|r| r.unwrap().get("p").unwrap().to_string())
        .collect();
    assert_eq!(rows, [format!("\"3.0\"{DEC}"), format!("\"7.0\"{DEC}")]);
}

#[test]
fn a_regular_function_may_not_take_arguments_as_an_aggregate() {
    let e = engine();
    assert!(
        e.query("PREFIX tt: <http://w3id.org/tripletrail/fn#> SELECT (COUNT(?x, 1) AS ?n) WHERE { ?x ?p ?o }")
            .is_err()
    );
    assert!(
        e.query(
            "PREFIX ex: <http://example.com/> SELECT (ex:nope(?x, 1) AS ?n) WHERE { ?x ?p ?o }"
        )
        .is_err()
    );
}

#[test]
fn large_integers_fall_back_to_a_double_when_the_decimal_overflows() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        "@prefix ex: <http://example.com/> .
         ex:x1 ex:big 1700000000000000 . ex:x2 ex:big 1800000000000000 .
         ex:x3 ex:big 1900000000000000 ."
            .as_bytes(),
    )
    .unwrap();
    let r = row(
        &e,
        "SELECT (tt:stddev(?b) AS ?sd) (tt:variance(?b) AS ?v) WHERE { ?x ex:big ?b }",
    );
    // Deviations ±1e14 and 0: sample variance 1e28, standard deviation 1e14.
    let sd: f64 = r[0].split('"').nth(1).unwrap().parse().unwrap();
    let v: f64 = r[1].split('"').nth(1).unwrap().parse().unwrap();
    assert!(r[0].ends_with(DBL) && r[1].ends_with(DBL), "{r:?}");
    assert!((sd - 1e14).abs() < 1.0, "{sd}");
    assert!((v / 1e28 - 1.0).abs() < 1e-9, "{v}");
}
