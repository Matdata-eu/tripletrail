//! Native TLS (#221): HTTPS on the server's address, its URLs in https, plain HTTP refused,
//! and a new certificate taken without a restart.

#![cfg(feature = "tls")]

use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tripletrail::Engine;
use tripletrail_server::{Options, Server};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/tls-fixtures")
        .join(name)
}

/// A client that trusts only the test CA `ca`.
fn agent(ca: &str) -> ureq::Agent {
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(fixture(ca)).unwrap() {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    ureq::AgentBuilder::new()
        .tls_config(Arc::new(config))
        .timeout(Duration::from_secs(10))
        .build()
}

#[test]
fn https() {
    let dir = tripletrail_testdir::tempdir("tls");
    let (cert, key) = (dir.join("server.pem"), dir.join("server.key"));
    std::fs::copy(fixture("server1.pem"), &cert).unwrap();
    std::fs::copy(fixture("server1.key"), &key).unwrap();
    let engine = Engine::new().unwrap();
    engine
        .load(
            tripletrail::oxigraph::io::RdfFormat::NTriples,
            "<http://example.com/a> <http://example.com/p> \"x\" .\n".as_bytes(),
        )
        .unwrap();
    engine.warm_up();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine)],
        Options {
            threads: 4,
            tls_cert: Some(cert.clone()),
            tls_key: Some(key.clone()),
            ..Options::default()
        },
    )
    .unwrap();
    let port = server.address().port();
    let base = format!("https://localhost:{port}");
    server.spawn();
    let client = agent("ca1.pem");

    // Queries, several on one kept-alive connection, and an upload with a body.
    for _ in 0..3 {
        let body = client
            .get(&format!("{base}/kg/query"))
            .query("query", "SELECT ?o { ?s ?p ?o }")
            .set("Accept", "text/tab-separated-values")
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        assert_eq!(body, "?o\n\"x\"\n");
    }
    let big = (0..5000)
        .map(|i| format!("<http://example.com/s{i}> <http://example.com/p> \"{i}\" .\n"))
        .collect::<String>();
    let r = client
        .post(&format!("{base}/kg/data?default"))
        .set("Content-Type", "application/n-triples")
        .send_string(&big)
        .unwrap();
    assert_eq!(r.status(), 204);
    let count = client
        .get(&format!("{base}/kg/query"))
        .query("query", "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }")
        .set("Accept", "text/tab-separated-values")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(count.contains("5001"), "{count}");

    // A few MB each way: the relay passes them in pieces that fit rustls's buffers.
    let big = (0..60_000)
        .map(|i| format!("<http://example.com/big{i}> <http://example.com/p> \"{i:0>40}\" .\n"))
        .collect::<String>();
    assert!(big.len() > 4 << 20);
    let r = client
        .post(&format!("{base}/kg/data?graph=http://example.com/big"))
        .set("Content-Type", "application/n-triples")
        .send_string(&big)
        .unwrap();
    assert_eq!(r.status(), 201, "a new graph");
    let back = client
        .get(&format!("{base}/kg/data?graph=http://example.com/big"))
        .set("Accept", "application/n-triples")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert_eq!(back.lines().count(), 60_000);
    assert!(back.len() > 4 << 20);

    // `tripletrail health` checks it over HTTPS.
    let health = |url: &str, tls: bool| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_tripletrail"));
        command.args(["health", "--url", url]);
        if tls {
            command.env("TRIPLETRAIL_TLS_CERT", &cert);
        } else {
            command.env_remove("TRIPLETRAIL_TLS_CERT");
        }
        command.output().unwrap().status.success()
    };
    // Ready once the spatial index has caught up with the upload.
    let healthy = |url: &str, tls: bool| {
        (0..100).any(|_| {
            health(url, tls) || {
                std::thread::sleep(Duration::from_millis(100));
                false
            }
        })
    };
    assert!(healthy(&format!("https://127.0.0.1:{port}"), false));
    assert!(healthy(&format!("http://127.0.0.1:{port}"), true));
    assert!(!health(&format!("http://127.0.0.1:{port}"), false));

    // The server's own URLs are https.
    let void = client
        .get(&format!("{base}/kg/void"))
        .set("Accept", "application/n-triples")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(void.contains(&format!("<{base}/kg/sparql>")), "{void}");

    // Plain HTTP on the TLS port gets no answer.
    assert!(
        ureq::get(&format!("http://127.0.0.1:{port}/$/ping"))
            .timeout(Duration::from_secs(5))
            .call()
            .is_err()
    );
    // A client that does not trust the certificate cannot connect.
    assert!(
        agent("ca2.pem")
            .get(&format!("{base}/$/ping"))
            .call()
            .is_err()
    );

    // A new certificate is taken without a restart, by the next connection.
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::copy(fixture("server2.key"), &key).unwrap();
    std::fs::copy(fixture("server2.pem"), &cert).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let r = agent("ca2.pem")
        .get(&format!("{base}/$/ping"))
        .call()
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[test]
fn cert_and_key_go_together() {
    let r = Server::bind(
        "127.0.0.1:0",
        Vec::<(String, Engine)>::new(),
        Options {
            tls_cert: Some(fixture("server1.pem")),
            ..Options::default()
        },
    );
    assert!(r.is_err());
    let r = Server::bind(
        "127.0.0.1:0",
        Vec::<(String, Engine)>::new(),
        Options {
            tls_cert: Some(fixture("server1.pem")),
            tls_key: Some(fixture("server2.key")),
            ..Options::default()
        },
    );
    let e = r.err().expect("a key of another certificate");
    assert!(e.to_string().contains("do not match"), "{e}");
}
