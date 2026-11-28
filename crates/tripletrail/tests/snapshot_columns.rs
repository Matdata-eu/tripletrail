//! A query reads the numeric value columns (#71, #297) of its own snapshot (#328): a write
//! between the snapshot and the moment the column is read takes nothing from its answer.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use tripletrail::Engine;
use tripletrail::store::{Dataset, Snapshot};

const PREFIXES: &str = "PREFIX ex: <http://example.com/>\n";
const EXECUTORS: [&str; 2] = [
    "#pragma tt.executor ids\n",
    "#pragma tt.executor spareval\n",
];
/// The best value (top-k) and the one value in the range.
const TOP_K: &str =
    "SELECT ?t ?v WHERE { ?t a ex:Track ; ex:speed ?v } ORDER BY DESC(?v) ?t LIMIT 3";
const RANGE: &str =
    "SELECT ?t ?v WHERE { ?t a ex:Track ; ex:speed ?v FILTER(?v >= 990 && ?v <= 1000) }";

fn engine() -> Engine {
    let mut d = String::from("@prefix ex: <http://example.com/> .\n");
    for i in 0..2000 {
        d += &format!(
            "ex:t{i} a ex:Track ; ex:speed {} .\n",
            40 + (i * 7919) % 200
        );
    }
    d += "ex:fast a ex:Track ; ex:speed 999 .\n";
    let e = Engine::new_tripletrail();
    e.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    e
}

fn snapshot(e: &Engine) -> Snapshot {
    let Dataset::TripleTrail(store) = e.store() else {
        panic!("the tripletrail store")
    };
    store.snapshot()
}

fn rows(results: QueryResults<'static>) -> Vec<String> {
    let QueryResults::Solutions(s) = results else {
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

#[test]
fn a_query_on_an_older_snapshot_reads_its_values() {
    for executor in EXECUTORS {
        let e = engine();
        let q = |query: &str| format!("{executor}{PREFIXES}{query}");
        let top = rows(e.query(&q(TOP_K)).unwrap());
        let range = rows(e.query(&q(RANGE)).unwrap());
        assert!(top[0].contains("fast"), "{executor}{top:?}");
        assert_eq!(range.len(), 1, "{executor}{range:?}");
        // The columns are built now, and patched by the write below.
        let held = snapshot(&e);
        e.update(&format!("{PREFIXES}DELETE DATA {{ ex:fast ex:speed 999 }}"))
            .unwrap();
        assert_eq!(
            rows(e.query_on(held.clone(), &q(TOP_K)).unwrap()),
            top,
            "{executor}"
        );
        assert_eq!(
            rows(e.query_on(held.clone(), &q(RANGE)).unwrap()),
            range,
            "{executor}"
        );
        // A query on the store's data does not see it any more.
        let now = rows(e.query(&q(TOP_K)).unwrap());
        assert!(!now[0].contains("fast"), "{executor}{now:?}");
        assert!(rows(e.query(&q(RANGE)).unwrap()).is_empty(), "{executor}");
        // And the old snapshot again, after the column was read for the new one.
        assert_eq!(
            rows(e.query_on(held, &q(TOP_K)).unwrap()),
            top,
            "{executor}"
        );
    }
}

#[test]
fn a_query_on_a_newer_snapshot_reads_a_value_added_since() {
    for executor in EXECUTORS {
        let e = engine();
        let q = |query: &str| format!("{executor}{PREFIXES}{query}");
        let old = snapshot(&e);
        e.update(&format!(
            "{PREFIXES}INSERT DATA {{ ex:faster a ex:Track ; ex:speed 995 . ex:fast ex:speed 1500 }}"
        ))
        .unwrap();
        let top = rows(e.query(&q(TOP_K)).unwrap());
        assert!(
            top[0].contains("fast") && top[0].contains("1500"),
            "{top:?}"
        );
        assert_eq!(rows(e.query(&q(RANGE)).unwrap()).len(), 2, "{executor}");
        // The column is now the newer data's; the old snapshot still gets its own answer.
        let old_top = rows(e.query_on(old.clone(), &q(TOP_K)).unwrap());
        assert!(old_top[0].contains("999"), "{executor}{old_top:?}");
        assert_eq!(
            rows(e.query_on(old, &q(RANGE)).unwrap()).len(),
            1,
            "{executor}"
        );
    }
}
