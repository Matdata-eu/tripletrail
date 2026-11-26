//! `--time-offset` (#300): each dataset's temporal functions read dates without a timezone at
//! its offset, `*` giving every other dataset's.

use tripletrail::Engine;
use tripletrail_server::{Options, Server};

#[test]
fn each_dataset_reads_dates_at_its_offset() {
    let server = Server::bind(
        "127.0.0.1:0",
        vec![
            ("rinf".to_owned(), Engine::new_tripletrail()),
            ("other".to_owned(), Engine::new_tripletrail()),
        ],
        Options {
            time_offsets: vec![("*".to_owned(), -300), ("rinf".to_owned(), 60)],
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let start = |ds: &str| -> String {
        let q = "PREFIX tt: <http://w3id.org/tripletrail/fn#> \
                 PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> \
                 SELECT ?v WHERE { BIND(STR(tt:periodStart(\"2026-11-26\"^^xsd:date)) AS ?v) }";
        ureq::post(&format!("{base}/{ds}/query"))
            .set("Content-Type", "application/sparql-query")
            .set("Accept", "text/csv")
            .send_string(q)
            .unwrap()
            .into_string()
            .unwrap()
    };
    assert_eq!(start("rinf").trim(), "v\r\n2026-11-25T23:00:00Z");
    assert_eq!(start("other").trim(), "v\r\n2026-11-26T05:00:00Z");
}
