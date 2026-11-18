//! Facade-X database sources on a server (#216): the administrator's connections and shared
//! tables, read live by queries.

#![cfg(feature = "postgres")]

use std::time::Duration;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::NamedNode;
use tripletrail_server::database_sources::{DatabaseSource, DatabaseSources};
use tripletrail_server::{Options, Server};

fn serve(sources: DatabaseSources) -> String {
    let engine = Engine::new().unwrap();
    engine
        .load(
            RdfFormat::NTriples,
            "<http://ex/s1> <http://ex/code> \"OSL\" .\n".as_bytes(),
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine)],
        Options {
            threads: 4,
            database_sources: sources,
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

/// Status and body of a query, the body as TSV.
fn query(base: &str, q: &str) -> (u16, String) {
    let q = format!("PREFIX xyz: <http://sparql.xyz/facade-x/data/>\n{q}");
    let r = ureq::get(&format!("{base}/kg/query"))
        .query("query", &q)
        .set("Accept", "text/tab-separated-values")
        .call();
    let r = match r {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{e}"),
    };
    (r.status(), r.into_string().unwrap())
}

#[test]
fn a_server_without_database_sources_reads_none() {
    let base = serve(DatabaseSources::default());
    let (status, body) = query(
        &base,
        "SELECT ?c WHERE { SERVICE <x-sparql-anything:table=station,database=rail> { ?r xyz:code ?c } }",
    );
    assert_ne!(status, 200, "{body}");
}

#[test]
#[ignore = "needs TRIPLETRAIL_TEST_POSTGRES, a PostgreSQL database it may write a table to"]
fn queries_read_the_shared_tables() {
    let url = std::env::var("TRIPLETRAIL_TEST_POSTGRES").expect("set TRIPLETRAIL_TEST_POSTGRES");
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute(
            "DROP TABLE IF EXISTS fx_srv_station; \
             CREATE TABLE fx_srv_station (code text, km numeric); \
             INSERT INTO fx_srv_station VALUES ('OSL', 0), ('LLS', 21.5); \
",
        )
        .unwrap();
    let mut connections = std::collections::HashMap::new();
    connections.insert("rail".to_owned(), DatabaseSource { url: url.clone() });
    let base = serve(DatabaseSources {
        connections,
        timeout: Duration::from_secs(1),
        max_rows: 1,
        max_bytes: 10_000,
        sessions: 3,
        graphs: vec![(
            NamedNode::new("http://ex/stations").unwrap(),
            NamedNode::new("x-sparql-anything:table=fx_srv_station,database=rail").unwrap(),
        )],
        ..DatabaseSources::default()
    });
    let service = |options: &str, pattern: &str| {
        format!("SELECT ?c WHERE {{ SERVICE <x-sparql-anything:{options}> {{ {pattern} }} }}")
    };
    let sql = |sql: &str| {
        format!(
            "SELECT ?c WHERE {{ SERVICE <x-sparql-anything:database=rail> {{ \
             <http://sparql.xyz/facade-x/engine/properties> \
             <http://sparql.xyz/facade-x/engine/sql> \"{sql}\" . ?r xyz:code ?c }} }}"
        )
    };

    // A shared table, joined to local data (a bind join: one row read, within the cap).
    let (status, body) = query(
        &base,
        "SELECT ?c WHERE { ?s <http://ex/code> ?c \
         SERVICE <x-sparql-anything:table=fx_srv_station,database=rail> { ?r xyz:code ?c } }",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.lines().nth(1), Some("\"OSL\""), "{body}");
    // The whole table is two rows: over the server's cap.
    let (status, body) = query(
        &base,
        &service("table=fx_srv_station,database=rail", "?r xyz:code ?c"),
    );
    assert_ne!(status, 200);
    assert!(body.contains("more than 1 rows"), "{body}");

    // A named virtual graph reads its SERVICE; another graph name reads the store.
    let (status, body) = query(
        &base,
        "SELECT ?c WHERE { GRAPH <http://ex/stations> { ?r xyz:code ?c FILTER(?c = \"LLS\") } }",
    );
    assert_eq!(body.lines().nth(1), Some("\"LLS\""), "{status} {body}");
    let (status, body) = query(
        &base,
        "SELECT ?c WHERE { GRAPH <http://ex/other> { ?r xyz:code ?c } }",
    );
    assert_eq!((status, body.lines().count()), (200, 1), "{body}");

    // With a LIMIT, only that many rows are read: within the cap.
    let (status, body) = query(
        &base,
        &format!(
            "{} LIMIT 1",
            service("table=fx_srv_station,database=rail", "?r xyz:code ?c")
        ),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.lines().count(), 2, "{body}");
    // So does a LIMIT inside the SERVICE.
    let (status, body) = query(
        &base,
        &service(
            "table=fx_srv_station,database=rail",
            "SELECT ?c WHERE { ?r xyz:code ?c } LIMIT 1",
        ),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.lines().count(), 2, "{body}");

    // A connection must be named and be the server's; files are not read.
    for (options, error) in [
        ("table=fx_srv_station", "database=<name>"),
        (
            "table=fx_srv_station,database=nope",
            "not one of the server's",
        ),
        ("location=/etc/hostname", "outside"),
    ] {
        let (status, body) = query(&base, &service(options, "?r ?p ?c"));
        assert_ne!(status, 200, "{options}");
        assert!(body.contains(error), "{options}: {body}");
    }
    // sql= queries, read-only and within the timeout.
    let (status, body) = query(
        &base,
        "SELECT ?c WHERE { SERVICE <x-sparql-anything:database=rail> { \
         <http://sparql.xyz/facade-x/engine/properties> <http://sparql.xyz/facade-x/engine/sql> \
         \"SELECT code FROM fx_srv_station WHERE km > 1\" . ?r xyz:code ?c } }",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.lines().nth(1), Some("\"LLS\""), "{body}");
    let (status, body) = query(
        &base,
        "SELECT ?c WHERE { SERVICE <x-sparql-anything:database=rail> { \
         <http://sparql.xyz/facade-x/engine/properties> <http://sparql.xyz/facade-x/engine/sql> \
         \"SELECT 'x' AS code FROM pg_sleep(3)\" . ?r xyz:code ?c } }",
    );
    assert_ne!(status, 200);
    assert!(body.contains("statement timeout"), "{body}");
    let (status, body) = query(
        &base,
        "SELECT ?c WHERE { SERVICE <x-sparql-anything:database=rail> { \
         <http://sparql.xyz/facade-x/engine/properties> <http://sparql.xyz/facade-x/engine/sql> \
         \"WITH w AS (INSERT INTO fx_srv_station VALUES ('BAD', 1) RETURNING code) SELECT code FROM w\" . ?r xyz:code ?c } }",
    );
    assert_ne!(status, 200, "{body}");
    // SQL that switches the timeout off is still cancelled at it.
    let started = std::time::Instant::now();
    let (status, body) = query(
        &base,
        "SELECT ?c WHERE { SERVICE <x-sparql-anything:database=rail> { \
         <http://sparql.xyz/facade-x/engine/properties> <http://sparql.xyz/facade-x/engine/sql> \
         \"SELECT set_config('statement_timeout', '0', true) || pg_sleep(4)::text AS code\" . \
         ?r xyz:code ?c } }",
    );
    assert_ne!(status, 200, "{body}");
    assert!(body.contains("statement timeout"), "{body}");
    assert!(started.elapsed() < Duration::from_secs(3), "{body}");
    // The cancel does not reach the calls after it: the next one, and one that waited
    // behind it for the session, both run.
    let (status, body) = query(&base, &sql("SELECT pg_sleep(0.5)::text || 'OSL' AS code"));
    assert_eq!(body.lines().nth(1), Some("\"OSL\""), "{status} {body}");
    let slow = {
        let (base, q) = (base.clone(), sql("SELECT pg_sleep(3)::text AS code"));
        std::thread::spawn(move || query(&base, &q))
    };
    std::thread::sleep(Duration::from_millis(300));
    let (status, body) = query(&base, &sql("SELECT pg_sleep(0.5)::text || 'LLS' AS code"));
    assert_eq!(body.lines().nth(1), Some("\"LLS\""), "{status} {body}");
    assert!(slow.join().unwrap().1.contains("statement timeout"));
    // Calls at once run on sessions of their own, up to the server's limit.
    let started = std::time::Instant::now();
    let calls: Vec<_> = (0..3)
        .map(|_| {
            let (base, q) = (
                base.clone(),
                sql("SELECT pg_sleep(0.6)::text || 'OSL' AS code"),
            );
            std::thread::spawn(move || query(&base, &q))
        })
        .collect();
    for call in calls {
        let (status, body) = call.join().unwrap();
        assert_eq!(body.lines().nth(1), Some("\"OSL\""), "{status} {body}");
    }
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    // Bytes are capped as they arrive.
    let (status, body) = query(&base, &sql("SELECT repeat('x', 20000) AS code"));
    assert_ne!(status, 200, "{body}");
    assert!(body.contains("more than 10000 bytes"), "{body}");
    // The session is reset after a call: a session advisory lock does not outlive it.
    let (status, body) = query(
        &base,
        &sql("SELECT pg_try_advisory_lock(681)::text AS code"),
    );
    assert_eq!(body.lines().nth(1), Some("\"true\""), "{status} {body}");
    let free: bool = client
        .query_one("SELECT pg_try_advisory_lock(681)", &[])
        .unwrap()
        .get(0);
    assert!(free, "the server's session still holds the lock");
    // A connection the database closed (here: its backend terminated) is opened again, and
    // the call retried.
    client
        .batch_execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = current_database() AND pid <> pg_backend_pid() \
             AND backend_type = 'client backend'",
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let (status, body) = query(&base, &sql("SELECT 'OSL' AS code"));
    assert_eq!(body.lines().nth(1), Some("\"OSL\""), "{status} {body}");
    // With a cache, the same statement gives the rows read then, until the TTL is up.
    let mut cached = DatabaseSources {
        cache_ttl: Duration::from_secs(3600),
        ..DatabaseSources::default()
    };
    cached
        .connections
        .insert("rail".to_owned(), DatabaseSource { url: url.clone() });
    let cached = serve(cached);
    let far = sql("SELECT code FROM fx_srv_station WHERE km > 1");
    assert_eq!(query(&cached, &far).1.lines().count(), 2);
    client
        .batch_execute("INSERT INTO fx_srv_station VALUES ('NEW', 9)")
        .unwrap();
    assert_eq!(query(&cached, &far).1.lines().count(), 2);
    let new = sql("SELECT code FROM fx_srv_station WHERE code = 'NEW'");
    assert_eq!(query(&base, &new).1.lines().nth(1), Some("\"NEW\""));
    client
        .batch_execute("DELETE FROM fx_srv_station WHERE code = 'NEW'")
        .unwrap();
    let count: i64 = client
        .query_one("SELECT count(*) FROM fx_srv_station", &[])
        .unwrap()
        .get(0);
    assert_eq!(count, 2);
}
