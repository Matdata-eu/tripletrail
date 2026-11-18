//! The ETL helpers of the `tt:` namespace and their vendor aliases through the engine (#233).

use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::Term;
use tripletrail::oxigraph::sparql::QueryResults;

const PREFIXES: &str = r#"
PREFIX ex: <http://example.com/>
PREFIX tt: <http://w3id.org/tripletrail/fn#>
PREFIX spif: <http://spinrdf.org/spif#>
PREFIX ofn: <http://www.ontotext.com/sparql/functions/>
PREFIX fn: <http://www.w3.org/2005/xpath-functions#>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
"#;

fn engine() -> Engine {
    let e = Engine::new().unwrap();
    e.load(
        RdfFormat::Turtle,
        r#"@prefix ex: <http://example.com/> .
        ex:a ex:line "L 12" ; ex:km 3 ; ex:date "31.10.2026" .
        ex:b ex:line "L 12" ; ex:km 3 ; ex:date "01.11.2026" .
        ex:c ex:line "L 12" ; ex:km "3" ; ex:date "garbage" ."#
            .as_bytes(),
    )
    .unwrap();
    e
}

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

fn column(e: &Engine, q: &str) -> Vec<Option<Term>> {
    let QueryResults::Solutions(s) = e.query(&format!("{PREFIXES}{q}")).unwrap() else {
        panic!()
    };
    s.map(|r| r.unwrap().get("v").cloned()).collect()
}

#[test]
fn skolem_iris_are_stable_and_tell_terms_apart() {
    let e = engine();
    let q = r#"SELECT ?s (tt:skolem(ex:pos_, ?l, ?k) AS ?v) WHERE { ?s ex:line ?l ; ex:km ?k } ORDER BY ?s"#;
    let v = column(&e, q);
    // ex:a and ex:b have the same line and km; ex:c's km is a string, not an integer.
    assert_eq!(v[0], v[1]);
    assert_ne!(v[0], v[2]);
    let Some(Term::NamedNode(iri)) = &v[0] else {
        panic!("{v:?}")
    };
    assert!(iri.as_str().starts_with("http://example.com/pos_"));
    assert_eq!(iri.as_str().len(), "http://example.com/pos_".len() + 32);
    // Same on another run.
    assert_eq!(column(&e, q), v);
    // A relative prefix is resolved against the query's base.
    let r = e
        .query(&format!(
            "BASE <http://base.org/> {PREFIXES} SELECT (tt:skolem(\"x/\", 1) AS ?v) WHERE {{}}"
        ))
        .unwrap();
    let QueryResults::Solutions(mut r) = r else {
        panic!()
    };
    let t = r.next().unwrap().unwrap().get("v").cloned();
    assert!(
        matches!(&t, Some(Term::NamedNode(n)) if n.as_str().starts_with("http://base.org/x/")),
        "{t:?}"
    );
}

#[test]
fn templates_build_iris_and_strings() {
    let e = engine();
    assert_eq!(
        one(
            &e,
            r#"tt:iri("http://example.com/line/{0}/km/{1}", "L 12", 3)"#
        ),
        "<http://example.com/line/L%2012/km/3>"
    );
    assert_eq!(
        one(&e, r#"spif:buildURI("http://example.com/{?1}", "a/b")"#),
        "<http://example.com/a/b>"
    );
    assert_eq!(
        one(&e, r#"spif:buildString("{?1}-{?2}", "L 12", ex:x)"#),
        r#""L 12-http://example.com/x""#
    );
    assert_eq!(one(&e, r#"tt:format("{}:{}", "a", "b")"#), r#""a:b""#);
}

#[test]
fn dates_with_patterns() {
    let e = engine();
    let q =
        r#"SELECT ?s (tt:parseDate(?d, "dd.MM.yyyy") AS ?v) WHERE { ?s ex:date ?d } ORDER BY ?s"#;
    let v: Vec<Option<String>> = column(&e, q)
        .into_iter()
        .map(|t| t.map(|t| t.to_string()))
        .collect();
    assert_eq!(
        v,
        [
            Some(r#""2026-10-31"^^<http://www.w3.org/2001/XMLSchema#date>"#.into()),
            Some(r#""2026-11-01"^^<http://www.w3.org/2001/XMLSchema#date>"#.into()),
            None
        ]
    );
    assert_eq!(
        one(
            &e,
            r#"spif:parseDate("2026-10-31 14:05", "yyyy-MM-dd HH:mm", "+01:00")"#
        ),
        r#""2026-10-31T14:05:00+01:00"^^<http://www.w3.org/2001/XMLSchema#dateTime>"#
    );
    assert_eq!(
        one(
            &e,
            r#"tt:formatDate("2026-11-09T14:05:00Z"^^xsd:dateTime, "EEEE d MMMM yyyy, HH:mm")"#
        ),
        r#""Monday 9 November 2026, 14:05""#
    );
    assert_eq!(
        one(
            &e,
            r#"fn:format-date("2026-11-09"^^xsd:date, "[D01]/[M01]/[Y0001]")"#
        ),
        r#""09/11/2026""#
    );
    assert_eq!(
        one(
            &e,
            r#"fn:format-dateTime("2026-11-09T14:05:00Z"^^xsd:dateTime, "[FNn] [D1o] [MNn] [Y] [H01]:[m01] [Z]")"#
        ),
        r#""Monday 9 November 2026 14:05 +00:00""#
    );
    assert_eq!(
        one(
            &e,
            r#"tt:daysBetween("2026-10-31"^^xsd:date, "2026-11-09T08:00:00"^^xsd:dateTime)"#
        ),
        r#""9"^^<http://www.w3.org/2001/XMLSchema#integer>"#
    );
    assert_eq!(
        one(
            &e,
            r#"ofn:monthsBetween("2026-01-31"^^xsd:date, "2026-03-30"^^xsd:date)"#
        ),
        r#""1"^^<http://www.w3.org/2001/XMLSchema#integer>"#
    );
    assert_eq!(
        one(
            &e,
            r#"tt:yearsBetween("1952-10-04"^^xsd:date, "2026-10-03"^^xsd:date)"#
        ),
        r#""73"^^<http://www.w3.org/2001/XMLSchema#integer>"#
    );
}

#[test]
fn similarity_and_case() {
    let e = engine();
    assert_eq!(
        one(&e, r#"tt:levenshtein("Brussel-Zuid", "Brussels-Zuid")"#),
        r#""1"^^<http://www.w3.org/2001/XMLSchema#integer>"#
    );
    let jw = one(&e, r#"tt:jaroWinkler("MARTHA", "MARHTA")"#);
    assert!(jw.starts_with("\"0.96"), "{jw}");
    assert_eq!(
        one(
            &e,
            r#"tt:colognePhonetic("Meyer") = tt:colognePhonetic("Maier")"#
        ),
        r#""true"^^<http://www.w3.org/2001/XMLSchema#boolean>"#
    );
    assert_eq!(one(&e, r#"tt:leftPad("7", 3, "0")"#), r#""007""#);
    assert_eq!(one(&e, r#"tt:rightPad("ab", 4)"#), r#""ab  ""#);
    assert_eq!(
        one(&e, r#"tt:camelCase("track section id")"#),
        r#""trackSectionId""#
    );
    assert_eq!(
        one(&e, r#"tt:snakeCase("TrackSection ID")"#),
        r#""track_section_id""#
    );
    assert_eq!(
        one(&e, r#"spif:titleCase("gent sint-pieters")"#),
        r#""Gent Sint-pieters""#
    );
    assert_eq!(
        one(&e, r#"tt:normalizeWhitespace("  a \t b ")"#),
        r#""a b""#
    );
}

#[test]
fn the_service_description_lists_them_without_the_helpers() {
    let names: Vec<String> = tripletrail::extension_functions()
        .into_iter()
        .map(|n| n.into_string())
        .collect();
    for f in [
        "http://w3id.org/tripletrail/fn#skolem",
        "http://w3id.org/tripletrail/fn#parseDate",
        "http://spinrdf.org/spif#buildURI",
        "http://www.w3.org/2005/xpath-functions#format-dateTime",
    ] {
        assert!(names.iter().any(|n| n == f), "{f}");
    }
    assert!(!names.iter().any(|n| n.ends_with("#skolemKey")));
}
