//! `GROUP BY ?g` with `COUNT` over a star (#298): a facet count of a few groups over many
//! rows, the same answers under both executors.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;

const PREFIXES: &str = "PREFIX ex: <http://example.com/>\n";

/// Tracks in two countries with one of six gauges (some with two), and other things.
fn engine(tracks: usize) -> Engine {
    let mut d = String::from("@prefix ex: <http://example.com/> .\n");
    for i in 0..tracks {
        let country = if i % 3 == 0 { "ex:BE" } else { "ex:DE" };
        d += &format!(
            "ex:t{i} a ex:Track ; ex:country {country} ; ex:gauge ex:g{} .\n",
            i % 6
        );
        if i % 10 == 0 {
            d += &format!("ex:t{i} ex:gauge ex:g{} .\n", (i + 1) % 6);
        }
        d += &format!("ex:o{i} a ex:Other ; ex:country {country} .\n");
    }
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    e
}

fn rows(e: &Engine, query: &str) -> Vec<String> {
    let QueryResults::Solutions(s) = e.query(&format!("{PREFIXES}{query}")).unwrap() else {
        panic!("SELECT")
    };
    let mut out: Vec<String> = s
        .map(|r| {
            let r = r.unwrap();
            r.iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out.sort();
    out
}

const IDS: &str = "#pragma tt.executor ids\n";
const ORACLE: &str = "#pragma tt.executor spareval\n#pragma tt.id-rewrites off\n";

const FACET: &str = "SELECT ?g (COUNT(?t) AS ?n) WHERE { ?t a ex:Track ; ex:country ex:DE ; ex:gauge ?g } GROUP BY ?g";

#[test]
fn facet_counts_match_the_evaluator() {
    let e = engine(3000);
    for q in [
        FACET,
        "SELECT ?g (COUNT(*) AS ?n) WHERE { ?t a ex:Track ; ex:gauge ?g } GROUP BY ?g",
        "SELECT ?g (COUNT(DISTINCT ?t) AS ?n) WHERE { ?t a ex:Track ; ex:country ?c ; ex:gauge ?g } GROUP BY ?g",
        "SELECT ?c ?g (COUNT(?t) AS ?n) WHERE { ?t a ex:Track ; ex:country ?c ; ex:gauge ?g } GROUP BY ?c ?g",
        "SELECT ?c (COUNT(?t) AS ?n) WHERE { ?t ex:country ?c } GROUP BY ?c",
        "SELECT ?g (COUNT(?t) AS ?n) WHERE { ?t a ex:Track ; ex:country ex:XX ; ex:gauge ?g } GROUP BY ?g",
    ] {
        assert_eq!(
            rows(&e, &format!("{IDS}{q}")),
            rows(&e, &format!("{ORACLE}{q}")),
            "{q}"
        );
    }
}

/// Timings on a larger copy: `cargo test --release --test facet_counts -- --ignored --nocapture`.
#[test]
#[ignore]
fn timings() {
    let e = engine(
        std::env::var("TRACKS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(500_000),
    );
    let q = std::env::var("Q").unwrap_or_else(|_| FACET.into());
    for _ in 0..std::env::var("N")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(5)
    {
        let start = std::time::Instant::now();
        let answer = rows(&e, &format!("{IDS}#pragma tt.result-cache off\n{q}"));
        eprintln!("{:?} {}", start.elapsed(), answer.len());
    }
}
