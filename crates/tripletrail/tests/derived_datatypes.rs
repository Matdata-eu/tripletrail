//! #379: a literal made in a query keeps its datatype and lexical form, also for a type
//! derived from one the evaluator computes with (`xsd:long`, `xsd:dateTimeStamp`), and
//! still compares and computes by its value.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

fn one(e: &Engine, expression: &str) -> Option<String> {
    let q = format!("PREFIX xsd: <{XSD}> SELECT ?v WHERE {{ BIND({expression} AS ?v) }}");
    let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
        panic!("not a SELECT")
    };
    let row = s.next().unwrap().unwrap();
    row.get("v").map(|t| t.to_string().replace(XSD, "xsd:"))
}

#[test]
fn strdt_keeps_the_datatype_and_the_lexical_form() {
    let e = Engine::new_tripletrail();
    for (expression, expected) in [
        (
            r#"STRDT("2026-08-03T00:00:00+02:00", xsd:dateTimeStamp)"#,
            r#""2026-08-03T00:00:00+02:00"^^<xsd:dateTimeStamp>"#,
        ),
        (
            r#"STRDT("5", xsd:unsignedInt)"#,
            r#""5"^^<xsd:unsignedInt>"#,
        ),
        (r#"STRDT("5", xsd:long)"#, r#""5"^^<xsd:long>"#),
        (
            r#"STRDT("5", xsd:nonNegativeInteger)"#,
            r#""5"^^<xsd:nonNegativeInteger>"#,
        ),
        (r#"STRDT("01", xsd:integer)"#, r#""01"^^<xsd:integer>"#),
        (r#"STRDT("x", xsd:token)"#, r#""x"^^<xsd:token>"#),
        (r#"STRDT("5", xsd:integer)"#, r#""5"^^<xsd:integer>"#),
        (r#""5"^^xsd:long"#, r#""5"^^<xsd:long>"#),
        (r#"DATATYPE(STRDT("5", xsd:long))"#, "<xsd:long>"),
        (r#"STR(STRDT("01", xsd:integer))"#, r#""01""#),
    ] {
        assert_eq!(
            one(&e, expression).as_deref(),
            Some(expected),
            "{expression}"
        );
    }
}

#[test]
fn literals_as_written_compute_and_compare_by_value() {
    let e = Engine::new_tripletrail();
    for (expression, expected) in [
        (r#"STRDT("5", xsd:long) + 1"#, r#""6"^^<xsd:integer>"#),
        (r#"-"5"^^xsd:long"#, r#""-5"^^<xsd:integer>"#),
        (r#""5"^^xsd:long = 5"#, r#""true"^^<xsd:boolean>"#),
        (r#""01"^^xsd:integer = 1"#, r#""true"^^<xsd:boolean>"#),
        (
            r#""5"^^xsd:long < "6"^^xsd:short"#,
            r#""true"^^<xsd:boolean>"#,
        ),
        (r#"ABS("-5"^^xsd:long)"#, r#""5"^^<xsd:integer>"#),
        (r#"isNumeric("5"^^xsd:long)"#, r#""true"^^<xsd:boolean>"#),
        (
            r#"YEAR(STRDT("2026-08-03T00:00:00+02:00", xsd:dateTimeStamp))"#,
            r#""2026"^^<xsd:integer>"#,
        ),
        (r#"IF("0"^^xsd:long, "y", "n")"#, r#""n""#),
        // A cast to a string writes the canonical form (XPath), STR the lexical form.
        (r#"xsd:string("01"^^xsd:integer)"#, r#""1""#),
        (r#"xsd:integer("01"^^xsd:integer)"#, r#""1"^^<xsd:integer>"#),
    ] {
        assert_eq!(
            one(&e, expression).as_deref(),
            Some(expected),
            "{expression}"
        );
    }
}

#[test]
fn casts_to_derived_types() {
    let e = Engine::new_tripletrail();
    for (expression, expected) in [
        (
            r#"xsd:dateTimeStamp("2026-08-03T00:00:00+02:00")"#,
            Some(r#""2026-08-03T00:00:00+02:00"^^<xsd:dateTimeStamp>"#),
        ),
        // No timezone: not a dateTimeStamp.
        (r#"xsd:dateTimeStamp("2026-08-03T00:00:00")"#, None),
        (r#"xsd:long("42")"#, Some(r#""42"^^<xsd:long>"#)),
        (r#"xsd:long(3.9e0)"#, Some(r#""3"^^<xsd:long>"#)),
        (
            r#"xsd:unsignedByte(255)"#,
            Some(r#""255"^^<xsd:unsignedByte>"#),
        ),
        (r#"xsd:unsignedByte(256)"#, None),
        (r#"xsd:positiveInteger(0)"#, None),
        (r#"xsd:short(3.0)"#, Some(r#""3"^^<xsd:short>"#)),
    ] {
        assert_eq!(one(&e, expression).as_deref(), expected, "{expression}");
    }
}

#[test]
fn an_update_writes_the_derived_datatype() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"<http://e/a> <http://e/text> "2026-08-03T00:00:00+02:00" ."#.as_bytes(),
    )
    .unwrap();
    e.update(&format!(
        "PREFIX xsd: <{XSD}> INSERT {{ ?s <http://e/at> ?t }} WHERE {{ ?s <http://e/text> ?x BIND(STRDT(?x, xsd:dateTimeStamp) AS ?t) }}"
    ))
    .unwrap();
    let QueryResults::Solutions(mut s) = e
        .query("SELECT (DATATYPE(?t) AS ?d) WHERE { ?s <http://e/at> ?t }")
        .unwrap()
    else {
        panic!("not a SELECT")
    };
    let d = s.next().unwrap().unwrap().get("d").unwrap().to_string();
    assert_eq!(d, format!("<{XSD}dateTimeStamp>"));
}

fn rows(e: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = e
        .query(&format!(
            "PREFIX xsd: <{XSD}> PREFIX e: <http://e/> {query}"
        ))
        .unwrap()
    else {
        panic!("not a SELECT")
    };
    s.map(|r| {
        r.unwrap()
            .iter()
            .map(|(v, t)| format!("{}={}", v.as_str(), t.to_string().replace(XSD, "xsd:")))
            .collect::<Vec<_>>()
            .join(" ")
    })
    .collect()
}

#[test]
fn stored_derived_literals_are_read_by_value_and_written_as_stored() {
    let e = Engine::new_tripletrail();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
        <http://e/a> <http://e/s> "hello world" ; <http://e/int> "5"^^xsd:int ;
            <http://e/pad> "007"^^xsd:integer ; <http://e/b> "1"^^xsd:boolean .
        <http://e/c> <http://e/int> "12"^^xsd:int ."#
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(
        rows(
            &e,
            "SELECT (SUBSTR(?s, 1, ?n) AS ?r) { e:a e:s ?s ; e:int ?n }"
        ),
        [r#"r="hello""#]
    );
    assert_eq!(
        rows(
            &e,
            "SELECT (SUBSTR(?s, 1, ?n) AS ?r) { e:a e:s ?s ; e:pad ?n }"
        ),
        [r#"r="hello w""#]
    );
    assert_eq!(
        rows(&e, "SELECT ?x { ?x e:b ?b FILTER(?b) }"),
        ["x=<http://e/a>"]
    );
    assert_eq!(
        rows(
            &e,
            "SELECT (STR(?n) AS ?s) (DATATYPE(?n) AS ?d) { e:a e:pad ?n }"
        ),
        [r#"s="007" d=<xsd:integer>"#]
    );
    assert_eq!(
        rows(&e, "SELECT (DATATYPE(?n) AS ?d) { e:a e:int ?n }"),
        ["d=<xsd:int>"]
    );
    assert_eq!(
        rows(&e, "SELECT (SUM(?n) AS ?s) { ?x e:int ?n }"),
        [r#"s="17"^^<xsd:integer>"#]
    );
    assert_eq!(
        rows(&e, "SELECT ?x { ?x e:int ?n FILTER(?n > \"6\"^^xsd:long) }"),
        ["x=<http://e/c>"]
    );
    assert_eq!(
        rows(&e, "SELECT ?x { ?x e:int ?n } ORDER BY DESC(?n)"),
        ["x=<http://e/c>", "x=<http://e/a>"]
    );
}
