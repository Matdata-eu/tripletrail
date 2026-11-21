//! FILTERs and `SUM` on the values of inline numbers and dates (issue #170) give the same
//! solutions as the evaluator: on the tripletrail store they run on ids, on Oxigraph's store
//! through the evaluator. (Oxigraph's store keeps decimals in canonical form, so the
//! filter tests compare the subjects, and whole computed decimals are compared as "3.0".)

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use oxigraph::store::Store;
use tripletrail::Engine;

fn rows(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = engine.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    let mut out: Vec<String> = solutions
        .map(|s| {
            let s = s.unwrap();
            // Oxigraph's store writes a computed whole decimal as "3", TripleTrail as Jena does
            // ("3.0", #513): compared in Jena's form.
            let mut cells: Vec<String> = s
                .iter()
                .map(|(v, t)| match t {
                    oxigraph::model::Term::Literal(l)
                        if l.datatype() == oxigraph::model::vocab::xsd::DECIMAL
                            && !l.value().contains('.') =>
                    {
                        format!("{v}=\"{}.0\"^^<{}>", l.value(), l.datatype().as_str())
                    }
                    t => format!("{v}={t}"),
                })
                .collect();
            cells.sort();
            cells.join(" ")
        })
        .collect();
    out.sort();
    out
}

fn data() -> String {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    for i in 0..2000_i64 {
        let s = format!("ex:s{i}");
        ttl.push_str(&format!("{s} a ex:C{} .\n", i % 4));
        ttl.push_str(&format!("{s} ex:int {} .\n", (i * 37) % 101 - 50));
        let frac = ["", ".5", ".25", ".125", ".0", ".50"][i as usize % 6];
        ttl.push_str(&format!(
            "{s} ex:dec \"{}{}{frac}\"^^xsd:decimal .\n",
            if i % 3 == 0 { "-" } else { "" },
            (i * 13) % 977
        ));
        let (y, m, d) = (1995 + i % 20, 1 + i % 12, 1 + i % 28);
        ttl.push_str(&format!("{s} ex:date \"{y}-{m:02}-{d:02}\"^^xsd:date .\n"));
        let tz = ["", "Z", "+02:00", "-05:30"][i as usize % 4];
        ttl.push_str(&format!(
            "{s} ex:when \"{y}-{m:02}-{d:02}T{:02}:{:02}:{:02}{tz}\"^^xsd:dateTime .\n",
            i % 24,
            i % 60,
            (i * 7) % 60
        ));
        // Values that are not inline or not numbers: the ids give up or say error.
        let odd = match i % 9 {
            0 => "\"7\"^^xsd:int".to_owned(),
            1 => "\"1.5E2\"^^xsd:double".to_owned(),
            2 => "\"text\"".to_owned(),
            3 => "ex:iri".to_owned(),
            4 => "\"2.5\"^^xsd:double".to_owned(),
            _ => format!("{}", i % 13),
        };
        ttl.push_str(&format!("{s} ex:mixed {odd} .\n"));
        if i % 5 == 0 {
            ttl.push_str(&format!("{s} ex:maybe {} .\n", i % 17));
        }
        // Signed zeros, equal to the evaluator (#64).
        if i % 50 == 7 {
            let zero = ["0.0", "-0.0", "0", "-0", "1.5", "-1.5"][i as usize / 50 % 6];
            ttl.push_str(&format!("{s} ex:zdec \"{zero}\"^^xsd:decimal .\n"));
            ttl.push_str(&format!("{s} ex:zdbl \"{zero}\"^^xsd:double .\n"));
        }
        // Doubles whose sums are exact in any order, so the evaluator's sum is the exact one
        // (#62); in ex:num, mixed with integers and decimals.
        let dbl = format!(
            "\"{}{}{}\"^^xsd:double",
            if i % 4 == 1 { "-" } else { "" },
            (i * 29) % 1013,
            [".25", "", ".5", ".125", ".0", ".75"][i as usize % 6]
        );
        ttl.push_str(&format!("{s} ex:dbl {dbl} .\n"));
        let num = match i % 3 {
            0 => dbl,
            1 => format!("{}", i % 41),
            _ => format!("{}.5", i % 23),
        };
        ttl.push_str(&format!("{s} ex:num {num} .\n"));
    }
    ttl
}

fn engines() -> (Engine, Engine) {
    let ours = Engine::new_tripletrail();
    ours.load(RdfFormat::Turtle, data().as_bytes()).unwrap();
    let reference = Engine::from_store(Store::new().unwrap());
    reference
        .load(RdfFormat::Turtle, data().as_bytes())
        .unwrap();
    (ours, reference)
}

const P: &str = "PREFIX ex: <http://example.org/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> ";

#[test]
fn filters_on_numbers_and_dates_match_the_evaluator() {
    let (ours, reference) = engines();
    let filters = [
        "?v > 10",
        "?v >= -3 && ?v < 20",
        "?v = 7",
        "?v = 7.0",
        "?v != 7",
        "?v * 2 > 30",
        "?v + 1.5 <= 0",
        "ABS(?v) < 5",
        "-?v > 10",
        "ROUND(?v) = 3",
        "CEIL(?v) > FLOOR(?v)",
        "?v > \"100.25\"^^xsd:decimal",
        "?v < 1.5E2",
        "YEAR(?v) = 2005",
        "MONTH(?v) >= 6 && DAY(?v) < 10",
        "HOURS(?v) = 13",
        "MINUTES(?v) < 5",
        "?v >= \"2005-06-01\"^^xsd:date",
        "?v < \"2005-06-01T00:00:00Z\"^^xsd:dateTime",
        "?v < \"2005-06-01T00:00:00\"^^xsd:dateTime",
        "?v > ?w",
    ];
    for p in [
        "ex:int", "ex:dec", "ex:date", "ex:when", "ex:mixed", "ex:maybe",
    ] {
        for f in filters {
            let q = format!(
                "{P}SELECT ?s WHERE {{ ?s a ex:C1 . ?s {p} ?v OPTIONAL {{ ?s ex:maybe ?w }} FILTER({f}) }}"
            );
            assert_eq!(rows(&ours, &q), rows(&reference, &q), "{q}");
            let count = format!(
                "{P}SELECT (COUNT(*) AS ?n) WHERE {{ ?s {p} ?v OPTIONAL {{ ?s ex:maybe ?w }} FILTER({f}) }}"
            );
            assert_eq!(rows(&ours, &count), rows(&reference, &count), "{count}");
        }
    }
}

#[test]
fn sums_match_the_evaluator() {
    let (ours, reference) = engines();
    let sums = [
        "?v",
        "ABS(?v)",
        "CEIL(?v)",
        "FLOOR(?v)",
        "ROUND(?v)",
        "?v + ?v",
        "?v * 3",
        "?v - 0.75",
        "?v > 0",
        "YEAR(?v)",
        "MONTH(?v)",
        "DAY(?v)",
        "HOURS(?v)",
    ];
    for p in [
        "ex:int", "ex:dec", "ex:date", "ex:when", "ex:mixed", "ex:maybe", "ex:dbl", "ex:num",
    ] {
        for e in sums {
            for q in [
                format!(
                    "{P}SELECT ?c (AVG({e}) AS ?avg) (SUM({e}) AS ?sum) WHERE {{ ?s {p} ?v ; a ?c }} GROUP BY ?c"
                ),
                format!("{P}SELECT (AVG({e}) AS ?avg) WHERE {{ ?s {p} ?v FILTER(false) }}"),
                format!("{P}SELECT (SUM({e}) AS ?sum) WHERE {{ ?s {p} ?v }}"),
                format!(
                    "{P}SELECT ?c (SUM({e}) AS ?sum) (COUNT(*) AS ?n) WHERE {{ ?s {p} ?v ; a ?c }} GROUP BY ?c"
                ),
                format!(
                    "{P}SELECT (SUM({e}) AS ?sum) WHERE {{ ?s a ex:C2 OPTIONAL {{ ?s {p} ?v }} }}"
                ),
                format!("{P}SELECT (SUM({e}) AS ?sum) WHERE {{ ?s {p} ?v FILTER(false) }}"),
            ] {
                assert_eq!(rows(&ours, &q), rows(&reference, &q), "{q}");
            }
        }
    }
}

/// Doubles are summed exactly and rounded once (#62), where the evaluator rounds after each
/// addition: ten times 0.1 is 1, not 0.9999999999999999, whatever the order of the rows.
#[test]
fn double_sums_are_rounded_once() {
    let ours = Engine::new_tripletrail();
    let mut ttl = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..10 {
        ttl.push_str(&format!(
            "ex:s{i} ex:v \"0.1\"^^<http://www.w3.org/2001/XMLSchema#double> ; ex:g ex:g{} .\n",
            i % 2
        ));
    }
    ours.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let double = |v: &str| format!("\"{v}\"^^<http://www.w3.org/2001/XMLSchema#double>");
    assert_eq!(
        rows(
            &ours,
            &format!("{P}SELECT (SUM(?v) AS ?sum) (AVG(?v) AS ?avg) WHERE {{ ?s ex:v ?v }}")
        ),
        [format!("?avg={} ?sum={}", double("0.1"), double("1"))]
    );
    assert_eq!(
        rows(
            &ours,
            &format!("{P}SELECT ?g (SUM(?v) AS ?sum) WHERE {{ ?s ex:v ?v ; ex:g ?g }} GROUP BY ?g")
        ),
        [
            format!("?g=<http://example.org/g0> ?sum={}", double("0.5")),
            format!("?g=<http://example.org/g1> ?sum={}", double("0.5")),
        ]
    );
}

/// The solutions of `query` in their order, decimals and doubles by value (Oxigraph's store
/// keeps them in canonical form).
fn ordered(engine: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = engine.query(query).unwrap() else {
        panic!("not a SELECT")
    };
    solutions
        .map(|s| {
            let s = s.unwrap();
            s.iter()
                .map(|(v, t)| match t {
                    oxigraph::model::Term::Literal(l)
                        if l.datatype() == oxigraph::model::vocab::xsd::DECIMAL
                            || l.datatype() == oxigraph::model::vocab::xsd::DOUBLE =>
                    {
                        // Oxigraph's store writes a decimal `-0` as `0`.
                        let value = l.value().parse::<f64>().unwrap() + 0.0;
                        format!("{v}={value}^^{}", l.datatype())
                    }
                    t => format!("{v}={t}"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// `ORDER BY` of inline numbers and dates under a `LIMIT` is ordered on ids (#64): the same
/// rows in the same order as the evaluator, also with `OFFSET`, `DESC`, a second key, unbound
/// values, and columns that mix kinds (left to the evaluator).
#[test]
fn top_rows_of_numbers_and_dates_match_the_evaluator() {
    let (ours, reference) = engines();
    for p in [
        "ex:int", "ex:dec", "ex:dbl", "ex:num", "ex:date", "ex:when", "ex:mixed", "ex:zdec",
        "ex:zdbl",
    ] {
        for (order, slice) in [
            ("DESC(?v) ?s", "LIMIT 7"),
            ("?v ?s", "LIMIT 5 OFFSET 3"),
            ("DESC(?v) DESC(?s)", "LIMIT 40"),
            ("?c DESC(?v) ?s", "LIMIT 12"),
        ] {
            for q in [
                format!("{P}SELECT ?s ?v WHERE {{ ?s {p} ?v ; a ?c }} ORDER BY {order} {slice}"),
                format!(
                    "{P}SELECT ?s ?v WHERE {{ ?s a ?c OPTIONAL {{ ?s {p} ?v FILTER(?v != 0) }} }} ORDER BY {order} {slice}"
                ),
                // Equal values of different kinds (`7` and `"7"^^xsd:int`) tie in any order.
                if ["ex:mixed", "ex:num", "ex:zdec", "ex:zdbl"].contains(&p) {
                    String::new()
                } else {
                    format!(
                        "{P}SELECT DISTINCT ?v WHERE {{ ?s {p} ?v ; a ?c }} ORDER BY DESC(?v) {slice}"
                    )
                },
            ] {
                if q.is_empty() {
                    continue;
                }
                assert_eq!(ordered(&ours, &q), ordered(&reference, &q), "{q}");
            }
        }
    }
    // Doubles are ordered on ids: the plan reads the first rows from a result table.
    let plan = ours
        .explain(&format!(
            "{P}SELECT ?s ?v WHERE {{ ?s ex:dbl ?v }} ORDER BY DESC(?v) LIMIT 5"
        ))
        .unwrap();
    assert!(plan.contains("urn:tripletrail:table:"), "{plan}");
}
