//! A filter on ids must not hold the dictionary lock while it tests rows (#654): with a write
//! waiting for that lock, a second read of it from the same thread (another kernel of the
//! filter decoding a term) waited for the writer, and the writer for the query, for ever.

use oxigraph::io::RdfFormat;
use oxigraph::sparql::QueryResults;
use std::fmt::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tripletrail::Engine;

#[test]
fn lang_filters_run_while_writes_wait() {
    let mut d = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..20_000 {
        writeln!(d, "ex:s{i} ex:label \"l{i}\"@en ; ex:alt \"a{i}\"@en-GB .").unwrap();
    }
    let engine = Arc::new(Engine::new_tripletrail());
    engine.load(RdfFormat::Turtle, d.as_bytes()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicUsize::new(0));
    let (e, s) = (engine.clone(), stop.clone());
    let writer = std::thread::spawn(move || {
        let mut i = 0;
        while !s.load(Ordering::Relaxed) {
            e.update(&format!(
                "INSERT DATA {{ <http://example.org/n{i}> <http://example.org/new> \"v{i}\" }}"
            ))
            .unwrap();
            i += 1;
        }
    });
    let queries = 60;
    let (e, n) = (engine.clone(), done.clone());
    // `LANG = "en"` reads tags from the dictionary keys, `langMatches` decodes the terms.
    let query = "PREFIX ex: <http://example.org/> SELECT (COUNT(*) AS ?n) WHERE { \
        ?s ex:label ?l ; ex:alt ?m FILTER(LANG(?l) = \"en\" && langMatches(LANG(?m), \"en\")) }";
    let reader = std::thread::spawn(move || {
        for _ in 0..queries {
            let QueryResults::Solutions(mut rows) = e.query(query).unwrap() else {
                panic!("not solutions")
            };
            let count = rows.next().unwrap().unwrap().get("n").unwrap().to_string();
            assert!(count.contains("20000"), "{count}");
            n.fetch_add(1, Ordering::Relaxed);
        }
    });
    // A deadlock leaves the threads blocked: fail instead of hanging the test run.
    let mut last = (0, Instant::now());
    while done.load(Ordering::Relaxed) < queries {
        std::thread::sleep(Duration::from_millis(100));
        let n = done.load(Ordering::Relaxed);
        if n != last.0 {
            last = (n, Instant::now());
        }
        assert!(
            last.1.elapsed() < Duration::from_secs(20),
            "stuck after {n} queries"
        );
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    reader.join().unwrap();
}
