//! `ORDER BY ?v LIMIT k` with `?v` the value of one triple pattern (#297): the rows of the
//! best values are joined first, and the answer is the same as the whole join's.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

const PREFIXES: &str = "PREFIX ex: <http://example.com/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
";

/// Tracks and other things with speeds (many tied), labels on most tracks.
fn data(tracks: usize) -> String {
    let mut d = String::new();
    for i in 0..tracks {
        let speed = [40, 60, 80, 100, 120, 160, 200, 250][(i * 7919) % 8];
        d += &format!("ex:t{i} a ex:Track ; ex:speed {speed} .\n");
        if i % 5 != 0 {
            d += &format!("ex:t{i} rdfs:label \"track {i}\" .\n");
        }
        // Other things are slower: the lowest speeds are not tracks'.
        d += &format!("ex:o{i} a ex:Other ; ex:speed {} .\n", i % 50);
    }
    d
}

fn engine(tracks: usize) -> Engine {
    let e = Engine::new_tripletrail();
    let d = format!(
        "@prefix ex: <http://example.com/> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n{}",
        data(tracks)
    );
    e.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    e
}

fn rows(e: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = e.query(&format!("{PREFIXES}{query}")).unwrap() else {
        panic!("SELECT")
    };
    s.map(|r| {
        let r = r.unwrap();
        r.iter()
            .map(|(v, t)| format!("{v}={t}"))
            .collect::<Vec<_>>()
            .join(" ")
    })
    .collect()
}

const IDS: &str = "#pragma tt.executor ids\n";
const ORACLE: &str = "#pragma tt.executor spareval\n#pragma tt.id-rewrites off\n";

#[test]
fn the_best_values_give_the_same_rows_as_the_whole_join() {
    let e = engine(5000);
    for (order, limit) in [
        ("DESC(?v) ?t", "LIMIT 20"),
        ("?v ?t", "LIMIT 20"),
        ("DESC(?v) ?t", "LIMIT 7 OFFSET 300"),
        ("DESC(?v) DESC(?t)", "LIMIT 600"),
        ("?v DESC(?label)", "LIMIT 1"),
        ("DESC(?v) ?label", "LIMIT 260"),
    ] {
        for executor in [IDS, "#pragma tt.executor spareval\n"] {
            let q = |pragma: &str| {
                format!(
                    "{pragma}SELECT ?t ?v ?label WHERE {{ ?t a ex:Track ; ex:speed ?v ; rdfs:label ?label }} ORDER BY {order} {limit}"
                )
            };
            assert_eq!(
                rows(&e, &q(executor)),
                rows(&e, &q(ORACLE)),
                "{order} {limit}"
            );
        }
    }
}

#[test]
fn ties_on_the_order_keep_rows_of_the_best_values() {
    let e = engine(5000);
    let q = |pragma: &str| {
        format!(
            "{pragma}SELECT ?v WHERE {{ ?t a ex:Track ; ex:speed ?v ; rdfs:label ?label }} ORDER BY DESC(?v) LIMIT 30"
        )
    };
    let got = rows(&e, &q(IDS));
    assert_eq!(got, rows(&e, &q(ORACLE)));
    assert_eq!(got.len(), 30);
    assert!(got.iter().all(|r| r.contains("250")), "{got:?}");
}

#[test]
fn a_filter_and_distinct_are_kept() {
    let e = engine(5000);
    for q in [
        "SELECT ?t ?v WHERE { ?t a ex:Track ; ex:speed ?v ; rdfs:label ?l FILTER(?v < 200) } ORDER BY DESC(?v) ?t LIMIT 15",
        // A filter that keeps few of the best values: more are picked, or the whole join.
        "SELECT ?t ?v WHERE { ?t a ex:Track ; ex:speed ?v ; rdfs:label ?l FILTER(STRENDS(?l, \"77\")) } ORDER BY DESC(?v) ?t LIMIT 15",
        "SELECT DISTINCT ?v WHERE { ?t a ex:Track ; ex:speed ?v ; rdfs:label ?l } ORDER BY DESC(?v) LIMIT 3",
        "SELECT DISTINCT ?v WHERE { ?t a ex:Track ; ex:speed ?v } ORDER BY ?v LIMIT 100",
        "SELECT ?t ?v WHERE { ?t a ex:Other ; ex:speed ?v } ORDER BY ?v ?t LIMIT 30",
        "SELECT DISTINCT ?t WHERE { ?t a ex:Other ; ex:speed ?v } ORDER BY ?v ?t LIMIT 30",
        // Nothing passes.
        "SELECT ?t WHERE { ?t a ex:Track ; ex:speed ?v ; rdfs:label ?l FILTER(?v > 1000) } ORDER BY DESC(?v) LIMIT 5",
    ] {
        for executor in [IDS, "#pragma tt.executor spareval\n"] {
            assert_eq!(
                rows(&e, &format!("{executor}{q}")),
                rows(&e, &format!("{ORACLE}{q}")),
                "{q}"
            );
        }
    }
}

#[test]
fn decimals_doubles_and_negative_numbers_order_by_value() {
    let mut d = String::from("@prefix ex: <http://example.com/> .\n");
    for i in 0..3000i64 {
        let x = (i * 7919) % 3001 - 1500;
        let len = match i % 3 {
            0 => format!("{x}"),
            1 => format!("{x}.5"),
            _ => format!("{x}.25e0"),
        };
        d += &format!("ex:t{i} a ex:Track ; ex:len {len} ; ex:n {i} .\n");
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    for order in ["DESC(?v) ?t", "?v ?t"] {
        for limit in ["LIMIT 5", "LIMIT 50 OFFSET 100"] {
            let q = format!(
                "SELECT ?t ?v WHERE {{ ?t a ex:Track ; ex:len ?v ; ex:n ?n }} ORDER BY {order} {limit}"
            );
            assert_eq!(
                rows(&e, &format!("{IDS}{q}")),
                rows(&e, &format!("{ORACLE}{q}")),
                "{q}"
            );
        }
    }
}

#[test]
fn a_predicate_with_other_objects_and_a_write_since() {
    let e = engine(5000);
    let q = "SELECT ?t ?v WHERE { ?t a ex:Track ; ex:speed ?v ; rdfs:label ?l } ORDER BY DESC(?v) ?t LIMIT 3";
    let before = rows(&e, &format!("{IDS}{q}"));
    // A faster track, written after the values were read: it comes first.
    e.update("PREFIX ex: <http://example.com/> PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> INSERT DATA { ex:fast a ex:Track ; ex:speed 999 ; rdfs:label \"fast\" }").unwrap();
    let after = rows(&e, &format!("{IDS}{q}"));
    assert_eq!(after, rows(&e, &format!("{ORACLE}{q}")));
    assert!(after[0].contains("fast"), "{after:?}");
    assert_eq!(after[1..], before[..2]);
    // A string among the numbers orders apart from them.
    e.update("PREFIX ex: <http://example.com/> PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> INSERT DATA { ex:odd a ex:Track ; ex:speed \"fast\" ; rdfs:label \"odd\" }").unwrap();
    for order in ["DESC(?v) ?t", "?v ?t"] {
        let q = format!(
            "SELECT ?t ?v WHERE {{ ?t a ex:Track ; ex:speed ?v ; rdfs:label ?l }} ORDER BY {order} LIMIT 3"
        );
        assert_eq!(
            rows(&e, &format!("{IDS}{q}")),
            rows(&e, &format!("{ORACLE}{q}")),
            "{q}"
        );
    }
}

#[test]
fn one_triple_pattern_gives_the_same_rows_as_all_its_triples() {
    // #360: the values of one pattern, as of a star.
    let e = engine(5000);
    for q in [
        "SELECT ?t ?v WHERE { ?t ex:speed ?v } ORDER BY DESC(?v) ?t LIMIT 20",
        "SELECT ?t ?v WHERE { ?t ex:speed ?v } ORDER BY ?v DESC(?t) LIMIT 7 OFFSET 30",
        "SELECT ?v WHERE { ?t ex:speed ?v } ORDER BY DESC(?v) LIMIT 25",
        "SELECT DISTINCT ?v WHERE { ?t ex:speed ?v } ORDER BY ?v LIMIT 10",
        "SELECT ?t WHERE { ?t ex:speed ?v FILTER(?v < 100) } ORDER BY DESC(?v) ?t LIMIT 12",
        "SELECT ?t ?v WHERE { ?t ex:speed ?v } ORDER BY DESC(?v) LIMIT 1",
    ] {
        for executor in [IDS, "#pragma tt.executor spareval\n"] {
            assert_eq!(
                rows(&e, &format!("{executor}{q}")),
                rows(&e, &format!("{ORACLE}{q}")),
                "{q}"
            );
        }
    }
}

#[test]
fn ties_on_the_one_key_of_one_pattern_come_by_subject() {
    // 5000 tracks at 8 speeds: 625 tie on the best one, of which 20 are kept. Joined from
    // the best values only, the tied rows come in the order of all the triples ordered.
    let e = engine(5000);
    let q = "SELECT ?t ?v WHERE { ?t ex:speed ?v } ORDER BY DESC(?v) LIMIT 20";
    let got = rows(&e, &format!("{IDS}{q}"));
    assert_eq!(got.len(), 20);
    assert!(got.iter().all(|r| r.contains("250")), "{got:?}");
    // An order on `?v + 0` is the same order, but not on the value of a pattern: all the
    // triples are joined and ordered.
    let whole = rows(
        &e,
        &format!("{IDS}SELECT ?t ?v WHERE {{ ?t ex:speed ?v }} ORDER BY DESC(?v + 0) LIMIT 20"),
    );
    assert_eq!(got, whole);
    let tracks = rows(&e, &format!("{IDS}SELECT ?t WHERE {{ ?t ex:speed 250 }}"));
    assert!(
        got.iter()
            .all(|r| tracks.iter().any(|t| r.starts_with(t.as_str())))
    );
}

#[test]
fn a_sub_select_takes_the_best_values_too() {
    // #360: counted, and joined with the rest of the query.
    let e = engine(5000);
    for q in [
        "SELECT (COUNT(*) AS ?n) WHERE { { SELECT ?t ?v ?l WHERE { ?t a ex:Track ; ex:speed ?v ; rdfs:label ?l } ORDER BY DESC(?v) LIMIT 20 } }",
        "SELECT (COUNT(*) AS ?n) WHERE { { SELECT ?t ?v WHERE { ?t ex:speed ?v } ORDER BY DESC(?v) LIMIT 20 } }",
        "SELECT (COUNT(DISTINCT ?v) AS ?n) WHERE { { SELECT ?t ?v WHERE { ?t ex:speed ?v } ORDER BY ?v LIMIT 300 } }",
        "SELECT ?t ?l WHERE { { SELECT ?t ?v WHERE { ?t ex:speed ?v } ORDER BY DESC(?v) ?t LIMIT 5 } ?t rdfs:label ?l } ORDER BY ?t",
        "SELECT ?t ?v WHERE { { SELECT ?t ?v WHERE { ?t a ex:Other ; ex:speed ?v } ORDER BY ?v ?t LIMIT 8 OFFSET 2 } } ORDER BY ?t",
    ] {
        for executor in [IDS, "#pragma tt.executor spareval\n"] {
            assert_eq!(
                rows(&e, &format!("{executor}{q}")),
                rows(&e, &format!("{ORACLE}{q}")),
                "{q}"
            );
        }
    }
}

/// Timings on a larger copy: `cargo test --release --test top_values -- --ignored --nocapture`.
#[test]
#[ignore]
fn timings() {
    let e = engine(300_000);
    let q = std::env::var("Q").unwrap_or_else(|_| "SELECT ?t ?v ?label WHERE { ?t a ex:Track ; ex:speed ?v ; rdfs:label ?label } ORDER BY DESC(?v) LIMIT 20".into());
    let mut answer = Vec::new();
    for _ in 0..std::env::var("N")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(3)
    {
        let start = std::time::Instant::now();
        answer = rows(&e, &format!("{IDS}#pragma tt.result-cache off\n{q}"));
        eprintln!("{:?}", start.elapsed());
    }
    eprintln!("{}", answer.len());
}
