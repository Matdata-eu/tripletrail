//! A slow sub-select inside the `path:`, `win:` and `st:` SERVICEs stops with the query's
//! cancellation token (a timeout), as the same sub-select does on its own.

use std::time::{Duration, Instant};
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::sparql::{CancellationToken, QueryResults};

/// Four copies of 300 rows joined, then a FILTER no row passes: billions of rows, none out.
const SLOW: &str = "?s <urn:n> ?a . ?b <urn:n> ?c . ?d <urn:n> ?e . ?o <urn:n> ?f \
                    FILTER(?a + ?c + ?e + ?f = -1)";

fn engine() -> Engine {
    let e = Engine::new_tripletrail();
    let nt: String = (0..300)
        .map(|i| {
            format!("<urn:x{i}> <urn:n> \"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n")
        })
        .collect();
    e.load(RdfFormat::NTriples, nt.as_bytes()).unwrap();
    e
}

/// Runs `q` with a token cancelled after 300 ms; it must fail within a few seconds.
fn stops(q: &str) {
    let token = CancellationToken::new();
    let e = engine().with_cancellation_token(token.clone());
    let cancel = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        token.cancel();
    });
    let started = Instant::now();
    let result = match e.query(q) {
        Err(err) => Err(err.to_string()),
        Ok(QueryResults::Solutions(mut s)) => s
            .find_map(Result::err)
            .map_or(Ok(()), |e| Err(e.to_string())),
        Ok(_) => Ok(()),
    };
    cancel.join().unwrap();
    assert!(result.is_err(), "{q}: not cancelled");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{q}: took {:?}",
        started.elapsed()
    );
}

#[test]
fn path_edge_sub_select() {
    stops(&format!(
        "PREFIX path: <http://w3id.org/tripletrail/path#> SELECT * {{ SERVICE <http://w3id.org/tripletrail/path> {{
           {{ SELECT ?s ?o {{ {SLOW} }} }} [] path:source <urn:x1> ; path:end ?n }} }}"
    ));
}

#[test]
fn window_input() {
    stops(&format!(
        "PREFIX win: <http://w3id.org/tripletrail/window#> SELECT * {{ SERVICE <http://w3id.org/tripletrail/window> {{
           {{ SELECT ?s ?a {{ {SLOW} }} }} [] win:rowNumber ?r }} }}"
    ));
}

#[test]
fn st_input() {
    stops(&format!(
        "PREFIX st: <http://w3id.org/tripletrail/st#> SELECT * {{ SERVICE <http://w3id.org/tripletrail/st> {{
           [] st:function st:ST_Dump ; st:geom ?part .
           {{ SELECT ?s (STR(?a) AS ?geom) {{ {SLOW} }} }} }} }}"
    ));
}
