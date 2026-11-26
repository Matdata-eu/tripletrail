//! Periods (#300): `tt:period` for every temporal literal, its accessors, Allen's relations,
//! `tt:periodIntersects` and `tt:periodContains`, the same under both executors. The
//! relations are checked against a truth table made from PostgreSQL's `tstzrange` operators
//! (`data/period_relations.sql`).

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

const PREFIXES: &str = "PREFIX ex: <http://example.com/>
PREFIX tt: <http://w3id.org/tripletrail/fn#>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
";

const EXECUTORS: [&str; 2] = [
    "#pragma tt.executor ids\n",
    "#pragma tt.executor spareval\n",
];

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        br#"@prefix ex: <http://example.com/> .
        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
        ex:a ex:from "2022-12-11"^^xsd:date ; ex:until "2078-12-31"^^xsd:date .
        ex:b ex:from "2024-01-01T00:00:00Z"^^xsd:dateTimeStamp .
        ex:c ex:until "2020"^^xsd:gYear ."#
            .as_slice(),
    )
    .unwrap();
    e
}

/// The rows of `?v` for `query`, the same under both executors.
fn column(e: &Engine, query: &str) -> Vec<Option<String>> {
    let rows = |pragma: &str| -> Vec<Option<String>> {
        let QueryResults::Solutions(s) = e.query(&format!("{pragma}{PREFIXES}{query}")).unwrap()
        else {
            panic!("SELECT")
        };
        s.map(|r| r.unwrap().get("v").map(ToString::to_string))
            .collect()
    };
    let ids = rows(EXECUTORS[0]);
    assert_eq!(ids, rows(EXECUTORS[1]), "{query}");
    ids
}

/// The value of an expression, or `None` when it has none (an error).
fn eval(e: &Engine, expr: &str) -> Option<String> {
    column(e, &format!("SELECT ?v WHERE {{ BIND({expr} AS ?v) }}"))
        .pop()
        .unwrap()
}

/// The value of an expression, a boolean as `true` or `false`.
fn one(e: &Engine, expr: &str) -> String {
    let v = eval(e, expr).unwrap_or_else(|| panic!("{expr} has no value"));
    v.strip_suffix("\"^^<http://www.w3.org/2001/XMLSchema#boolean>")
        .map_or(v.clone(), |b| b.trim_start_matches('"').to_owned())
}

fn period(lexical: &str) -> String {
    format!("\"{lexical}\"^^<http://w3id.org/tripletrail/fn#period>")
}

#[test]
fn every_temporal_literal_is_a_period() {
    let e = engine();
    for (value, expected) in [
        (
            "\"2026-11-26\"^^xsd:date",
            "2026-11-26T00:00:00Z/2026-11-27T00:00:00Z",
        ),
        (
            "\"2026-11-26+01:00\"^^xsd:date",
            "2026-11-25T23:00:00Z/2026-11-26T23:00:00Z",
        ),
        (
            "\"2024-02\"^^xsd:gYearMonth",
            "2024-02-01T00:00:00Z/2024-03-01T00:00:00Z",
        ),
        (
            "\"2023-02\"^^xsd:gYearMonth",
            "2023-02-01T00:00:00Z/2023-03-01T00:00:00Z",
        ),
        (
            "\"2024\"^^xsd:gYear",
            "2024-01-01T00:00:00Z/2025-01-01T00:00:00Z",
        ),
        (
            "\"-0044\"^^xsd:gYear",
            "-0044-01-01T00:00:00Z/-0043-01-01T00:00:00Z",
        ),
        (
            "\"2026-11-26T10:30:00.25+02:00\"^^xsd:dateTime",
            "2026-11-26T08:30:00.25Z/2026-11-26T08:30:00.251Z",
        ),
        (
            "\"2026-11-26T10:30:00Z\"^^xsd:dateTimeStamp",
            "2026-11-26T10:30:00Z/2026-11-26T10:30:00.001Z",
        ),
        (
            "\"2024-02-29\"",
            "2024-02-29T00:00:00Z/2024-03-01T00:00:00Z",
        ),
        (
            "\"2022-12-11/2078-12-31\"",
            "2022-12-11T00:00:00Z/2079-01-01T00:00:00Z",
        ),
        (&period("2022-12-11/.."), "2022-12-11T00:00:00Z/.."),
    ] {
        let p = one(&e, &format!("tt:period({value})"));
        assert_eq!(p, period(expected), "{value}");
    }
    for value in [
        "\"2023-02-29\"^^xsd:date",
        "\"2026-11-26T10:30:00\"^^xsd:dateTimeStamp",
        "\"2030/2020\"",
        "\"tomorrow\"",
        "42",
        "ex:a",
    ] {
        assert_eq!(eval(&e, &format!("tt:period({value})")), None, "{value}");
    }
}

#[test]
fn a_period_is_built_from_two_sides() {
    let e = engine();
    // "Until 2078-12-31" includes that day; an instant as the end is not in the period.
    assert_eq!(
        column(
            &e,
            "SELECT ?v WHERE { ?s ex:from|ex:until [] OPTIONAL { ?s ex:from ?f } \
             OPTIONAL { ?s ex:until ?u } BIND(STR(tt:period(?f, ?u)) AS ?v) } \
             GROUP BY ?s ?v ORDER BY ?v"
        ),
        [
            Some("\"../2021-01-01T00:00:00Z\"".to_owned()),
            Some("\"2022-12-11T00:00:00Z/2079-01-01T00:00:00Z\"".to_owned()),
            Some("\"2024-01-01T00:00:00Z/..\"".to_owned()),
        ]
    );
    assert_eq!(
        one(
            &e,
            "STR(tt:period(\"2026-11-26T08:00:00Z\"^^xsd:dateTime, \"2026-11-26T09:00:00Z\"^^xsd:dateTime))"
        ),
        "\"2026-11-26T08:00:00Z/2026-11-26T09:00:00Z\""
    );
    // The same instant on both sides is an empty period, which has no value.
    assert_eq!(
        eval(
            &e,
            "tt:period(\"2026-11-26T08:00:00Z\"^^xsd:dateTime, \"2026-11-26T08:00:00Z\"^^xsd:dateTime)"
        ),
        None
    );
    // Both sides open: the whole timeline.
    assert_eq!(
        column(&e, "SELECT ?v WHERE { BIND(STR(tt:period(?x, ?y)) AS ?v) }"),
        [Some("\"../..\"".to_owned())]
    );
}

#[test]
fn values_without_a_timezone_are_read_at_an_offset() {
    let e = engine();
    let day = "\"2026-11-26\"^^xsd:date";
    assert_eq!(
        one(&e, &format!("STR(tt:period({day}))")),
        "\"2026-11-26T00:00:00Z/2026-11-27T00:00:00Z\""
    );
    // One call's offset.
    assert_eq!(
        one(&e, &format!("STR(tt:period({day}, \"+01:00\"))")),
        "\"2026-11-25T23:00:00Z/2026-11-26T23:00:00Z\""
    );
    assert_eq!(
        one(
            &e,
            &format!("STR(tt:period({day}, \"2026-11-28\"^^xsd:date, \"-05:00\"))")
        ),
        "\"2026-11-26T05:00:00Z/2026-11-29T05:00:00Z\""
    );
    // The engine's default: every function reads at it, unless a value or a call has its own.
    e.set_time_offset(60);
    assert_eq!(
        one(&e, &format!("STR(tt:period({day}))")),
        "\"2026-11-25T23:00:00Z/2026-11-26T23:00:00Z\""
    );
    assert_eq!(
        one(&e, "STR(tt:period(\"2026-11-26Z\"^^xsd:date))"),
        "\"2026-11-26T00:00:00Z/2026-11-27T00:00:00Z\""
    );
    assert_eq!(
        one(&e, &format!("STR(tt:period({day}, \"Z\"))")),
        "\"2026-11-26T00:00:00Z/2026-11-27T00:00:00Z\""
    );
    // 23:30 UTC on the 25th is on the 26th in Brussels.
    assert_eq!(
        one(
            &e,
            &format!("tt:periodContains({day}, \"2026-11-25T23:30:00Z\"^^xsd:dateTime)")
        ),
        "true"
    );
    e.set_time_offset(0);
    assert_eq!(
        one(
            &e,
            &format!("tt:periodContains({day}, \"2026-11-25T23:30:00Z\"^^xsd:dateTime)")
        ),
        "false"
    );
}

#[test]
fn accessors_give_the_sides_and_the_length() {
    let e = engine();
    let p = period("2022-12-11/2078-12-31");
    assert_eq!(
        one(&e, &format!("tt:periodStart({p})")),
        "\"2022-12-11T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTimeStamp>"
    );
    assert_eq!(
        one(&e, &format!("STR(tt:periodEnd({p}))")),
        "\"2079-01-01T00:00:00Z\""
    );
    assert_eq!(
        one(&e, "tt:periodDuration(\"2024\"^^xsd:gYear)"),
        "\"P366D\"^^<http://www.w3.org/2001/XMLSchema#dayTimeDuration>"
    );
    assert_eq!(
        one(
            &e,
            "STR(tt:periodDuration(\"2026-11-26T10:00:00Z\"^^xsd:dateTime))"
        ),
        "\"PT0.001S\""
    );
    let open = period("2022-12-11/..");
    assert_eq!(eval(&e, &format!("tt:periodEnd({open})")), None);
    assert_eq!(eval(&e, &format!("tt:periodDuration({open})")), None);
    assert_eq!(one(&e, &format!("tt:isOpenEnd({open})")), "true");
    assert_eq!(one(&e, &format!("tt:isOpenStart({open})")), "false");
    assert_eq!(
        one(
            &e,
            "tt:isInstant(\"2026-11-26T10:00:00Z\"^^xsd:dateTimeStamp)"
        ),
        "true"
    );
    assert_eq!(one(&e, "tt:isInstant(\"2026-11-26\"^^xsd:date)"), "false");
}

#[test]
fn valid_on_a_day_is_one_call() {
    let e = engine();
    // Without periods a date and a date-time do not compare: no row.
    assert_eq!(
        column(
            &e,
            "SELECT ?v WHERE { ?s ex:from ?f FILTER(?f <= \"2026-11-26\"^^xsd:date) BIND(?s AS ?v) }"
        ),
        [Some("<http://example.com/a>".to_owned())]
    );
    // With them, every way the sides are written and the open ones.
    assert_eq!(
        column(
            &e,
            "SELECT ?v WHERE { ?v ex:from|ex:until [] OPTIONAL { ?v ex:from ?f } \
             OPTIONAL { ?v ex:until ?u } \
             FILTER(tt:periodContains(tt:period(?f, ?u), \"2026-11-26\"^^xsd:date)) } \
             GROUP BY ?v ORDER BY ?v"
        ),
        [
            Some("<http://example.com/a>".to_owned()),
            Some("<http://example.com/b>".to_owned()),
        ]
    );
    assert_eq!(
        one(
            &e,
            "tt:periodIntersects(\"2020\"^^xsd:gYear, \"2020-12-31T23:59:59.999Z/2021\")"
        ),
        "true"
    );
    assert_eq!(
        one(
            &e,
            "tt:periodMeets(\"2020\"^^xsd:gYear, \"2021-01-01\"^^xsd:date)"
        ),
        "true"
    );
}

#[test]
fn relations_match_postgresql() {
    let table = include_str!("data/period_relations.tsv");
    let mut lines = table.lines();
    let header: Vec<&str> = lines.next().unwrap().split('\t').collect();
    let relations = &header[2..];
    let rows: Vec<Vec<&str>> = lines.map(|l| l.split('\t').collect()).collect();
    let mut names: Vec<&str> = rows.iter().map(|r| r[0]).collect();
    names.dedup();
    let e = Engine::new_tripletrail();
    let mut ttl = String::from("@prefix ex: <http://example.com/> .\n");
    for (i, n) in names.iter().enumerate() {
        ttl.push_str(&format!(
            "ex:p{i} ex:name \"{n}\" ; ex:p \"{n}\"^^<http://w3id.org/tripletrail/fn#period> .\n"
        ));
    }
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    let binds: String = relations
        .iter()
        .enumerate()
        .map(|(i, r)| format!(" BIND(tt:{r}(?a, ?b) AS ?r{i})"))
        .collect();
    let query = format!(
        "{PREFIXES}SELECT * WHERE {{ ?x ex:name ?an ; ex:p ?a . ?y ex:name ?bn ; ex:p ?b .{binds} }}"
    );
    for pragma in EXECUTORS {
        let QueryResults::Solutions(s) = e.query(&format!("{pragma}{query}")).unwrap() else {
            panic!("SELECT")
        };
        let mut got = std::collections::BTreeMap::new();
        for row in s {
            let row = row.unwrap();
            let name = |v: &str| match row.get(v) {
                Some(oxigraph::model::Term::Literal(l)) => l.value().to_owned(),
                other => panic!("{v}: {other:?}"),
            };
            let held: Vec<String> = (0..relations.len())
                .map(|i| match row.get(format!("r{i}").as_str()) {
                    Some(oxigraph::model::Term::Literal(l)) if l.value() == "true" => "1".into(),
                    Some(oxigraph::model::Term::Literal(l)) if l.value() == "false" => "0".into(),
                    other => panic!("{}: {other:?}", relations[i]),
                })
                .collect();
            got.insert((name("an"), name("bn")), held);
        }
        assert_eq!(got.len(), rows.len(), "{pragma}");
        for r in &rows {
            let held = &got[&(r[0].to_owned(), r[1].to_owned())];
            for (i, relation) in relations.iter().enumerate() {
                assert_eq!(held[i], r[2 + i], "{pragma}{relation}({}, {})", r[0], r[1]);
            }
        }
    }
}

#[test]
fn the_service_description_lists_the_public_names_only() {
    let names: Vec<String> = tripletrail::extension_functions()
        .into_iter()
        .map(|n| n.into_string())
        .collect();
    assert!(names.contains(&"http://w3id.org/tripletrail/fn#periodIntersects".to_owned()));
    assert!(names.contains(&"http://w3id.org/tripletrail/fn#period".to_owned()));
    assert!(
        !names
            .iter()
            .any(|n| n.contains("#_period") || n.contains("#_is"))
    );
}
