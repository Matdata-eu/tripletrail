//! The XPath (`fn:`, `math:`) and Jena (`afn:`) function libraries through the engine, as a
//! query written for Jena or Fuseki calls them (#231).

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::{Literal, Term};
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX fn: <http://www.w3.org/2005/xpath-functions#>
PREFIX math: <http://www.w3.org/2005/xpath-functions/math#>
PREFIX afn: <http://jena.apache.org/ARQ/function#>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
"#;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:s1 ex:name "Brussel-Zuid" ; ex:opened "1952-10-04"^^xsd:date ; ex:km 12.3456 .
ex:s2 ex:name "Antwerpen-Centraal" ; ex:opened "1905-08-11"^^xsd:date ; ex:km 3.5 .
ex:s3 ex:name "Gent-Sint-Pieters" ; ex:opened "1912-07-01"^^xsd:date ; ex:km 0.25 .
"#;

fn engine() -> Engine {
    let e = Engine::new().unwrap();
    e.load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    e
}

/// The rows of `?v` (a literal's value, an IRI's string).
fn values(e: &Engine, q: &str) -> Vec<Option<String>> {
    let QueryResults::Solutions(s) = e.query(&format!("{PREFIXES}{q}")).unwrap() else {
        panic!()
    };
    s.map(|r| {
        r.unwrap().get("v").map(|t| match t {
            Term::Literal(l) => l.value().to_owned(),
            other => other.to_string(),
        })
    })
    .collect()
}

/// The value of the expression `expr`, as `"lexical"^^<datatype>` or `None` for an error.
fn eval(e: &Engine, expr: &str) -> Option<String> {
    let QueryResults::Solutions(mut s) = e
        .query(&format!(
            "{PREFIXES} SELECT ?v WHERE {{ BIND({expr} AS ?v) }}"
        ))
        .unwrap()
    else {
        panic!()
    };
    s.next().unwrap().unwrap().get("v").map(ToString::to_string)
}

fn one(e: &Engine, expr: &str) -> String {
    eval(e, expr).unwrap_or_else(|| panic!("{expr} has no value"))
}

#[test]
fn string_functions_rewritten_to_builtins_work_everywhere() {
    let e = engine();
    // FILTER, BIND, ORDER BY and an aggregate all see the rewrite.
    let q = r#"SELECT (fn:upper-case(?n) AS ?v) WHERE {
        ?s ex:name ?n FILTER(fn:matches(?n, "^[ab]", "i") || fn:starts-with(?n, "Gent"))
    } ORDER BY fn:string-length(?n)"#;
    assert_eq!(
        values(&e, q),
        [
            Some("BRUSSEL-ZUID".into()),
            Some("GENT-SINT-PIETERS".into()),
            Some("ANTWERPEN-CENTRAAL".into())
        ]
    );
    let q = r#"SELECT (MAX(fn:substring-after(?n, "-")) AS ?v) WHERE { ?s ex:name ?n }"#;
    assert_eq!(values(&e, q), [Some("Zuid".into())]);
    let q = r#"SELECT ?v WHERE {
        ?s ex:name ?v FILTER EXISTS { ?s ex:opened ?d FILTER(fn:year-from-date(?d) < 1910) }
    }"#;
    assert_eq!(values(&e, q), [Some("Antwerpen-Centraal".into())]);
    assert_eq!(one(&e, r#"fn:replace("a-b-c", "-", "+")"#), r#""a+b+c""#);
    assert_eq!(one(&e, r#"fn:concat("a", "b", "c")"#), r#""abc""#);
    assert_eq!(one(&e, r#"fn:substring("motor car", 6)"#), r#"" car""#);
    assert_eq!(
        one(&e, r#"fn:boolean("x")"#),
        r#""true"^^<http://www.w3.org/2001/XMLSchema#boolean>"#
    );
    assert_eq!(
        one(&e, r#"fn:not(1)"#),
        r#""false"^^<http://www.w3.org/2001/XMLSchema#boolean>"#
    );
}

#[test]
fn updates_see_the_rewrite() {
    let e = engine();
    e.update(&format!(
        r#"{PREFIXES} INSERT {{ ?s ex:code ?c }} WHERE {{
            ?s ex:name ?n BIND(fn:lower-case(fn:substring-before(?n, "-")) AS ?c)
        }}"#
    ))
    .unwrap();
    let mut codes = values(&e, "SELECT ?v WHERE { ?s ex:code ?v }");
    codes.sort();
    assert_eq!(
        codes,
        [
            Some("antwerpen".into()),
            Some("brussel".into()),
            Some("gent".into())
        ]
    );
}

#[test]
fn string_functions() {
    let e = engine();
    assert_eq!(one(&e, r#"fn:normalize-space("  a \t b  ")"#), r#""a b""#);
    assert_eq!(one(&e, r#"fn:translate("bar", "abc", "ABC")"#), r#""BAr""#);
    assert_eq!(one(&e, "fn:codepoints-to-string(72, 105)"), r#""Hi""#);
    assert_eq!(
        one(&e, r#"fn:compare("abc", "abd")"#),
        r#""-1"^^<http://www.w3.org/2001/XMLSchema#integer>"#
    );
    assert_eq!(
        one(&e, r#"fn:iri-to-uri("http://e/a b")"#),
        r#""http://e/a%20b""#
    );
    assert_eq!(
        one(&e, r#"fn:string-join("a", "b", "c", ", ")"#),
        r#""a, b, c""#
    );
}

#[test]
fn numbers_and_math() {
    let e = engine();
    let double = |x: f64| Literal::from(x).to_string();
    assert_eq!(one(&e, "math:pi()"), double(std::f64::consts::PI));
    assert_eq!(one(&e, "math:sqrt(16)"), double(4.0));
    assert_eq!(one(&e, "math:pow(2, 10)"), double(1024.0));
    assert_eq!(one(&e, "math:log10(1000)"), double(3.0));
    assert_eq!(one(&e, "math:exp10(2)"), double(100.0));
    assert_eq!(one(&e, "math:atan2(0, -1)"), double(std::f64::consts::PI));
    assert_eq!(eval(&e, r#"math:sqrt("x")"#), None);
    assert_eq!(
        // A computed whole decimal keeps ".0", as Jena writes it; Oxigraph's store
        // (TRIPLETRAIL_STORE=oxigraph) writes it canonically, "2".
        one(&e, "fn:round-half-to-even(2.5)").replace(r#""2"^^"#, r#""2.0"^^"#),
        r#""2.0"^^<http://www.w3.org/2001/XMLSchema#decimal>"#
    );
    assert_eq!(
        one(&e, "fn:round-half-to-even(3.567812, 2)"),
        r#""3.57"^^<http://www.w3.org/2001/XMLSchema#decimal>"#
    );
    assert_eq!(
        one(&e, "fn:round(1234, -2)"),
        r#""1200"^^<http://www.w3.org/2001/XMLSchema#integer>"#
    );
    assert_eq!(
        one(&e, r##"fn:format-number(1234567.891, "#,##0.00")"##),
        r#""1,234,567.89""#
    );
    assert_eq!(one(&e, r#"fn:format-integer(7, "000")"#), r#""007""#);
    assert_eq!(
        one(&e, "fn:numeric-mod(10, 3)"),
        r#""1"^^<http://www.w3.org/2001/XMLSchema#integer>"#
    );
    assert_eq!(
        one(&e, "fn:numeric-integer-divide(-7, 2)"),
        r#""-3"^^<http://www.w3.org/2001/XMLSchema#integer>"#
    );
    assert_eq!(
        one(&e, "fn:max(3, 7.5, 2)"),
        r#""7.5"^^<http://www.w3.org/2001/XMLSchema#decimal>"#
    );
    assert_eq!(
        one(&e, "fn:abs(-2)"),
        r#""2"^^<http://www.w3.org/2001/XMLSchema#integer>"#
    );
    // In a query: the km of every station to two places.
    let mut kms = values(
        &e,
        r#"SELECT (fn:format-number(?km, "0.00") AS ?v) WHERE { ?s ex:km ?km }"#,
    );
    kms.sort();
    assert_eq!(
        kms,
        [
            Some("0.25".into()),
            Some("12.35".into()),
            Some("3.50".into())
        ]
    );
}

#[test]
fn dates_times_and_durations() {
    let e = engine();
    let ty = |v: &str, t: &str| format!(r#""{v}"^^<http://www.w3.org/2001/XMLSchema#{t}>"#);
    assert_eq!(
        one(
            &e,
            r#"fn:days-from-duration("P3DT10H"^^xsd:dayTimeDuration)"#
        ),
        ty("3", "integer")
    );
    assert_eq!(
        one(&e, r#"fn:years-from-duration("-P2Y6M"^^xsd:duration)"#),
        ty("-2", "integer")
    );
    assert_eq!(
        one(
            &e,
            r#"fn:seconds-from-duration("PT1M30.5S"^^xsd:dayTimeDuration)"#
        ),
        ty("30.5", "decimal")
    );
    assert_eq!(
        one(
            &e,
            r#"fn:adjust-dateTime-to-timezone("2026-11-09T10:00:00+01:00"^^xsd:dateTime, "-PT5H"^^xsd:dayTimeDuration)"#
        ),
        ty("2026-11-09T04:00:00-05:00", "dateTime")
    );
    assert_eq!(
        one(
            &e,
            r#"fn:adjust-dateTime-to-timezone("2026-11-09T10:00:00+01:00"^^xsd:dateTime)"#
        ),
        ty("2026-11-09T09:00:00Z", "dateTime")
    );
    assert_eq!(
        one(
            &e,
            r#"fn:dateTime("2026-11-09"^^xsd:date, "14:05:00"^^xsd:time)"#
        ),
        ty("2026-11-09T14:05:00", "dateTime")
    );
    assert_eq!(
        one(&e, r#"fn:hours-from-time("14:05:00"^^xsd:time)"#),
        ty("14", "integer")
    );
    assert_eq!(
        one(&e, r#"fn:month-from-date("2026-11-09"^^xsd:date)"#),
        ty("11", "integer")
    );
    assert_eq!(
        one(&e, "fn:implicit-timezone()"),
        ty("PT0S", "dayTimeDuration")
    );
}

#[test]
fn jena_functions() {
    let e = engine();
    assert_eq!(one(&e, "afn:localname(ex:s1)"), r#""s1""#);
    assert_eq!(one(&e, "afn:namespace(ex:s1)"), r#""http://example.com/""#);
    assert_eq!(one(&e, r#"afn:sprintf("%s_%03d", "km", 7)"#), r#""km_007""#);
    assert_eq!(
        one(&e, r#"afn:strjoin("/", "a", "b", ex:c)"#),
        r#""a/b/http://example.com/c""#
    );
    assert_eq!(one(&e, r#"afn:substr("abcdef", 1, 3)"#), r#""bc""#);
    assert_eq!(
        one(&e, "afn:min(3, 2)"),
        r#""2"^^<http://www.w3.org/2001/XMLSchema#integer>"#
    );
    assert_eq!(
        one(&e, r#"afn:sha1sum(ex:s1)"#),
        one(&e, r#"SHA1("http://example.com/s1")"#)
    );
    // The old namespace still works.
    assert_eq!(
        one(&e, "<http://jena.hpl.hp.com/ARQ/function#localname>(ex:s2)"),
        r#""s2""#
    );
    let q = r#"SELECT ?v WHERE { ?s ex:km ?km FILTER(?km > 10) BIND(afn:localname(?s) AS ?v) }"#;
    assert_eq!(values(&e, q), [Some("s1".into())]);
}

#[test]
fn integer_subtype_casts() {
    let e = engine();
    // The cast keeps its datatype (#379). Oxigraph's store writes derived integer types as
    // xsd:integer when it encodes a term.
    let (int, long) = if std::env::var("TRIPLETRAIL_STORE").as_deref() == Ok("oxigraph") {
        ("integer", "integer")
    } else {
        ("int", "long")
    };
    assert_eq!(
        one(&e, r#"xsd:int(" 42 ")"#),
        format!(r#""42"^^<http://www.w3.org/2001/XMLSchema#{int}>"#)
    );
    assert_eq!(
        one(&e, "xsd:long(3.9e0)"),
        format!(r#""3"^^<http://www.w3.org/2001/XMLSchema#{long}>"#)
    );
    assert_eq!(eval(&e, r#"xsd:byte("200")"#), None);
    assert_eq!(eval(&e, r#"xsd:unsignedByte("300")"#), None);
    assert_eq!(
        one(&e, "xsd:anyURI(ex:a)"),
        r#""http://example.com/a"^^<http://www.w3.org/2001/XMLSchema#anyURI>"#
    );
}

#[test]
fn the_service_description_lists_the_functions() {
    let names: Vec<String> = tripletrail::extension_functions()
        .into_iter()
        .map(|n| n.into_string())
        .collect();
    for f in [
        "http://www.w3.org/2005/xpath-functions#matches",
        "http://www.w3.org/2005/xpath-functions#round",
        "http://www.w3.org/2005/xpath-functions/math#sqrt",
        "http://jena.apache.org/ARQ/function#sprintf",
        "http://jena.apache.org/ARQ/function#now",
    ] {
        assert_eq!(names.iter().filter(|n| *n == f).count(), 1, "{f}");
    }
}
