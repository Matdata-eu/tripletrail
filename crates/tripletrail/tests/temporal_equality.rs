//! `=` and `!=` on dates and times: a value with a timezone and one without, less than 14
//! hours apart, compare as indeterminate, so the comparison is an error (W3C open-world date-2).

use tripletrail::Engine;
use tripletrail::oxigraph::sparql::QueryResults;

/// `?a = ?b` and `?a != ?b`, or `None` for an error.
fn compare(e: &Engine, a: &str, b: &str) -> (Option<String>, Option<String>) {
    let q = format!(
        "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
        SELECT ?eq ?ne WHERE {{ BIND({a} AS ?a) BIND({b} AS ?b) BIND(?a = ?b AS ?eq) BIND(?a != ?b AS ?ne) }}"
    );
    let QueryResults::Solutions(mut s) = e.query(&q).unwrap() else {
        panic!("not a SELECT")
    };
    let row = s.next().unwrap().unwrap();
    let value = |v: &str| {
        row.get(v)
            .map(|t| t.to_string().split('"').nth(1).unwrap().to_owned())
    };
    (value("eq"), value("ne"))
}

fn yes() -> (Option<String>, Option<String>) {
    (Some("true".into()), Some("false".into()))
}

fn no() -> (Option<String>, Option<String>) {
    (Some("false".into()), Some("true".into()))
}

#[test]
fn dates_with_and_without_a_timezone() {
    let e = Engine::new_tripletrail();
    let date = |s: &str| format!("\"{s}\"^^xsd:date");
    // Indeterminate: within 14 hours of each other.
    for (a, b) in [
        ("2006-08-23", "2006-08-23Z"),
        ("2006-08-23", "2006-08-23+00:00"),
        ("2006-08-23", "2006-08-23-05:00"),
    ] {
        assert_eq!(compare(&e, &date(a), &date(b)), (None, None), "{a} {b}");
    }
    assert_eq!(compare(&e, &date("2006-08-23"), &date("2006-08-23")), yes());
    assert_eq!(
        compare(&e, &date("2006-08-23Z"), &date("2006-08-23+00:00")),
        yes()
    );
    assert_eq!(compare(&e, &date("2006-08-23"), &date("2001-01-01Z")), no());
    assert_eq!(
        compare(&e, &date("2006-08-23Z"), &date("2006-08-24Z")),
        no()
    );
    // More than 14 hours apart: determinate.
    assert_eq!(compare(&e, &date("2006-08-23"), &date("2006-08-25Z")), no());
}

#[test]
fn date_times_and_times() {
    let e = Engine::new_tripletrail();
    let dt = |s: &str| format!("\"{s}\"^^xsd:dateTime");
    assert_eq!(
        compare(&e, &dt("2006-08-23T09:00:00"), &dt("2006-08-23T09:00:00Z")),
        (None, None)
    );
    assert_eq!(
        compare(
            &e,
            &dt("2006-08-23T09:00:00+01:00"),
            &dt("2006-08-23T08:00:00Z")
        ),
        yes()
    );
    assert_eq!(
        compare(&e, &dt("2006-08-23T09:00:00"), &dt("2006-09-23T09:00:00Z")),
        no()
    );
    // A date and a dateTime are different types.
    assert_eq!(
        compare(&e, &dt("2006-08-23T00:00:00Z"), "\"2006-08-23Z\"^^xsd:date"),
        no()
    );
    let time = |s: &str| format!("\"{s}\"^^xsd:time");
    assert_eq!(
        compare(&e, &time("09:00:00"), &time("09:00:00Z")),
        (None, None)
    );
    assert_eq!(
        compare(&e, &time("09:00:00Z"), &time("10:00:00+01:00")),
        yes()
    );
}
