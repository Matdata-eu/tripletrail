//! The result cache (#126): repeated SELECT and ASK queries are answered from it until a
//! write, and answers are those of an evaluation.

use tripletrail::oxigraph::model::{GraphName, Literal, NamedNode, Quad};
use tripletrail::oxigraph::sparql::{QueryResults, Variable};
use tripletrail::{CacheStatus, Engine, QueryOptions};

const EX: &str = "http://example.com/";

fn engine(n: usize) -> Engine {
    let e = Engine::new_tripletrail();
    e.set_result_cache(1 << 20);
    e.load_quads((0..n).map(|i| {
        Quad::new(
            NamedNode::new_unchecked(format!("{EX}s{i}")),
            NamedNode::new_unchecked(format!("{EX}p")),
            Literal::from(i as i64),
            GraphName::DefaultGraph,
        )
    }))
    .unwrap();
    e
}

fn rows(e: &Engine, q: &str) -> (Vec<String>, CacheStatus) {
    let (results, status) = e
        .query_with_cache_status(q, QueryOptions::default())
        .unwrap();
    let QueryResults::Solutions(s) = results else {
        panic!("a SELECT")
    };
    let mut out: Vec<String> = s
        .map(|r| {
            r.unwrap()
                .iter()
                .map(|(v, t)| format!("{v}={t}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out.sort();
    (out, status)
}

const SUM: &str = "SELECT (SUM(?o) AS ?n) WHERE { ?s <http://example.com/p> ?o }";

#[test]
fn repeated_queries_are_answered_from_the_cache_until_a_write() {
    let e = engine(100);
    let (first, status) = rows(&e, SUM);
    assert_eq!(status, CacheStatus::Miss);
    let (again, status) = rows(&e, SUM);
    assert_eq!(status, CacheStatus::Hit);
    assert_eq!(first, again);
    // The same algebra written differently is the same entry.
    let (_, status) = rows(
        &e,
        "SELECT (SUM(?o) AS ?n)\n WHERE {?s <http://example.com/p> ?o}",
    );
    assert_eq!(status, CacheStatus::Hit);
    let stats = e.result_cache_stats();
    assert_eq!((stats.hits, stats.entries), (2, 1));

    e.update(r#"INSERT DATA { <http://example.com/x> <http://example.com/p> 1000 }"#)
        .unwrap();
    let (after, status) = rows(&e, SUM);
    assert_eq!(status, CacheStatus::Miss);
    assert_ne!(after, first);
    assert_eq!(rows(&e, SUM).1, CacheStatus::Hit);

    e.load_quads([Quad::new(
        NamedNode::new_unchecked(format!("{EX}y")),
        NamedNode::new_unchecked(format!("{EX}p")),
        Literal::from(1i64),
        GraphName::DefaultGraph,
    )])
    .unwrap();
    assert_eq!(rows(&e, SUM).1, CacheStatus::Miss);
}

#[test]
fn ask_options_and_settings_are_part_of_the_key() {
    let e = engine(10);
    let ask = "ASK { ?s <http://example.com/p> 3 }";
    let (r, status) = e
        .query_with_cache_status(ask, QueryOptions::default())
        .unwrap();
    assert!(matches!(r, QueryResults::Boolean(true)) && status == CacheStatus::Miss);
    let (r, status) = e
        .query_with_cache_status(ask, QueryOptions::default())
        .unwrap();
    assert!(matches!(r, QueryResults::Boolean(true)) && status == CacheStatus::Hit);

    let q = "SELECT ?s ?o WHERE { ?s <http://example.com/p> ?o }";
    let bound = |v: i64| QueryOptions {
        substitutions: vec![(Variable::new_unchecked("o"), Literal::from(v).into())],
        ..Default::default()
    };
    let count = |o: QueryOptions| {
        let (r, s) = e.query_with_cache_status(q, o).unwrap();
        let QueryResults::Solutions(r) = r else {
            panic!()
        };
        (r.count(), s)
    };
    assert_eq!(count(bound(1)), (1, CacheStatus::Miss));
    assert_eq!(count(bound(2)), (1, CacheStatus::Miss));
    assert_eq!(count(bound(1)), (1, CacheStatus::Hit));
    assert_eq!(count(QueryOptions::default()), (10, CacheStatus::Miss));

    e.set_spatial_rewrite(false);
    assert_eq!(count(QueryOptions::default()), (10, CacheStatus::Miss));
    e.set_spatial_rewrite(true);
    assert_eq!(count(QueryOptions::default()), (10, CacheStatus::Hit));
}

#[test]
fn what_is_not_cached() {
    let e = engine(10);
    for q in [
        "SELECT (RAND() AS ?r) WHERE {}",
        "SELECT (NOW() AS ?r) WHERE {}",
        "SELECT (STRUUID() AS ?r) WHERE {}",
        "SELECT * WHERE { SERVICE <http://example.com/sparql> { ?s ?p ?o } }",
        "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }",
    ] {
        for _ in 0..2 {
            let (_, status) = e
                .query_with_cache_status(q, QueryOptions::default())
                .unwrap_or_else(|_| (QueryResults::Boolean(false), CacheStatus::Bypass));
            assert_eq!(status, CacheStatus::Bypass, "{q}");
        }
    }
    // Off by default, and on Oxigraph's store.
    let off = Engine::new_tripletrail();
    assert_eq!(rows(&off, SUM).1, CacheStatus::Bypass);
    let ox = Engine::new_oxigraph().unwrap();
    ox.set_result_cache(1 << 20);
    assert_eq!(rows(&ox, SUM).1, CacheStatus::Bypass);
}

#[test]
fn results_larger_than_an_entry_or_not_read_to_the_end_are_not_kept() {
    let e = engine(2000);
    // A quarter of 16 KiB is far less than 2000 rows.
    e.set_result_cache(16 << 10);
    let all = "SELECT ?s ?o WHERE { ?s <http://example.com/p> ?o }";
    assert_eq!(rows(&e, all).0.len(), 2000);
    assert_eq!(rows(&e, all).1, CacheStatus::Miss);

    e.set_result_cache(64 << 20);
    // Streamed by the evaluator (`MD5` is not computed on ids): the id executor (#11) has the
    // whole answer at once, which the cache keeps however much of it is read.
    let all = "SELECT ?s ?o WHERE { ?s <http://example.com/p> ?o BIND(MD5(STR(?o)) AS ?t) }";
    let (r, _) = e
        .query_with_cache_status(all, QueryOptions::default())
        .unwrap();
    let QueryResults::Solutions(mut r) = r else {
        panic!()
    };
    r.next().unwrap().unwrap();
    drop(r);
    assert_eq!(rows(&e, all).1, CacheStatus::Miss);
    assert_eq!(rows(&e, all).1, CacheStatus::Hit);

    // The budget holds: the least recently used entries go first.
    e.set_result_cache(1 << 20);
    for i in 0..200 {
        rows(
            &e,
            &format!("SELECT ?s WHERE {{ ?s <http://example.com/p> {i} }}"),
        );
    }
    let stats = e.result_cache_stats();
    assert!(stats.bytes <= stats.budget, "{stats:?}");
    assert_eq!(
        rows(&e, "SELECT ?s WHERE { ?s <http://example.com/p> 199 }").1,
        CacheStatus::Hit
    );
}
