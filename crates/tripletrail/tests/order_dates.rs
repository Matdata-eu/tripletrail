//! `ORDER BY` over dates and times with and without a timezone (#78): XSD leaves a zoned and
//! an unzoned value less than 14 hours apart unordered, which made the sort's comparison
//! inconsistent, and the sort panicked. A value without a timezone is read as UTC, and
//! values at the same instant are ordered by their text.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

fn engine() -> Engine {
    let mut ttl = String::from(
        "@prefix ex: <http://example.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
    );
    let zones = ["", "Z", "+02:00", "-05:00", "+14:00", "-14:00", "+00:00"];
    let mut i = 0;
    for day in 1..=3 {
        for hour in [0, 1, 2, 5, 11, 13, 19, 23] {
            for zone in zones {
                ttl.push_str(&format!(
                    "ex:s{i} ex:dt \"2020-01-0{day}T{hour:02}:00:00{zone}\"^^xsd:dateTime ; \
                     ex:d \"2020-01-0{day}{zone}\"^^xsd:date ; \
                     ex:t \"{hour:02}:30:00{zone}\"^^xsd:time .\n"
                ));
                i += 1;
            }
        }
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
    e
}

fn column(e: &Engine, query: &str) -> Vec<String> {
    let query = format!("PREFIX ex: <http://example.org/>\n{query}");
    let QueryResults::Solutions(s) = e.query(&query).unwrap() else {
        unreachable!("SELECT")
    };
    s.map(|row| row.unwrap().iter().next().unwrap().1.to_string())
        .collect()
}

/// The instant of a lexical dateTime, `None` without a timezone read as UTC, in seconds.
fn instant(text: &str) -> i64 {
    let value = text.split('"').nth(1).unwrap();
    let (date, time) = value.split_once('T').unwrap();
    let day: i64 = date[8..10].parse().unwrap();
    let hour: i64 = time[0..2].parse().unwrap();
    let zone = &time[8..];
    let offset = match zone {
        "" | "Z" => 0,
        z => {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            sign * (z[1..3].parse::<i64>().unwrap() * 60 + z[4..6].parse::<i64>().unwrap())
        }
    };
    (day * 24 + hour) * 3600 - offset * 60
}

#[test]
fn mixed_timezones_sort_without_panicking_in_a_total_order() {
    let e = engine();
    for (property, n) in [("dt", 168), ("d", 21), ("t", 56)] {
        for order in ["?v", "DESC(?v)"] {
            for tail in ["", " LIMIT 20"] {
                let q = format!(
                    "SELECT DISTINCT ?v WHERE {{ ?x ex:{property} ?v }} ORDER BY {order}{tail}"
                );
                let got = column(&e, &q);
                let off = column(&e, &format!("#pragma tt.id-rewrites off\n{q}"));
                assert_eq!(got, off, "{q}");
                assert_eq!(got.len(), if tail.is_empty() { n } else { 20 }, "{q}");
            }
        }
    }
    // Ascending by instant, a value without a timezone read as UTC. (Values of the same
    // instant come by their text; `Z` and `+00:00` are the same value, in either order.)
    let instants = |rows: &[String]| rows.iter().map(|r| instant(r)).collect::<Vec<_>>();
    let got = column(&e, "SELECT DISTINCT ?v WHERE { ?x ex:dt ?v } ORDER BY ?v");
    assert!(instants(&got).is_sorted(), "{got:#?}");
    let mut desc = column(
        &e,
        "SELECT DISTINCT ?v WHERE { ?x ex:dt ?v } ORDER BY DESC(?v)",
    );
    desc.reverse();
    assert!(instants(&desc).is_sorted(), "{desc:#?}");
    // Of the same instant, the one without a timezone first.
    let at = |text: &str| got.iter().position(|r| r.contains(text)).unwrap();
    assert!(at("\"2020-01-01T00:00:00\"") < at("\"2020-01-01T02:00:00+02:00\""));
}

/// `MIN` and `MAX` order the same way.
#[test]
fn min_and_max_read_a_value_without_a_timezone_as_utc() {
    let e = Engine::new_tripletrail();
    let q = |values: &str| {
        column(
            &e,
            &format!(
                "PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
                 SELECT (MAX(?v) AS ?m) WHERE {{ VALUES ?v {{ {values} }} }}"
            ),
        )
    };
    assert_eq!(
        q(r#""2020-01-01T03:00:00"^^xsd:dateTime "2020-01-01T02:00:00+02:00"^^xsd:dateTime"#),
        vec![r#""2020-01-01T03:00:00"^^<http://www.w3.org/2001/XMLSchema#dateTime>"#]
    );
    assert_eq!(
        q(r#""2020-01-01T01:00:00"^^xsd:dateTime "2020-01-01T02:00:00+02:00"^^xsd:dateTime"#),
        vec![r#""2020-01-01T01:00:00"^^<http://www.w3.org/2001/XMLSchema#dateTime>"#]
    );
}
