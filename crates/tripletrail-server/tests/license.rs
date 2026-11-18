//! The edition and, in the enterprise build, the licence endpoint (#734).
//! `cargo test -p tripletrail-server --features enterprise --test license` runs the enterprise
//! half.

use serde_json::Value;
use tripletrail::Engine;
use tripletrail_server::{Options, Server};

fn start(options: Options) -> String {
    let server = Server::bind(
        "127.0.0.1:0",
        vec![("ds".to_owned(), Engine::new().unwrap())],
        options,
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

fn get(url: &str) -> (u16, Value) {
    match ureq::get(url).call() {
        Ok(r) => (
            r.status(),
            serde_json::from_str(&r.into_string().unwrap()).unwrap_or(Value::Null),
        ),
        Err(ureq::Error::Status(code, _)) => (code, Value::Null),
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn server_names_its_edition() {
    let base = start(Options::default());
    let (status, json) = get(&format!("{base}/$/server"));
    assert_eq!(status, 200);
    assert_eq!(json["edition"], tripletrail_server::EDITION);
    #[cfg(not(feature = "enterprise"))]
    {
        assert_eq!(json["edition"], "open");
        // The open build has no licence endpoint.
        assert_eq!(get(&format!("{base}/$/license")).0, 404);
    }
}

#[cfg(feature = "enterprise")]
#[test]
fn license_endpoint() {
    use tripletrail_enterprise::license::{Date, License, LicenseError};
    let base = start(Options::default());
    let (status, json) = get(&format!("{base}/$/license"));
    assert_eq!(status, 200);
    assert_eq!(json["status"], "missing");
    assert_eq!(json["active"], false);

    let license = License {
        id: "2026-001".into(),
        kid: "test".into(),
        customer: "Example Rail AS".into(),
        issued: Date::today(),
        expires: Date::today().plus_days(10),
        features: ["sso".to_owned()].into(),
        max_nodes: None,
        max_cores: None,
        note: None,
    };
    let base = start(Options {
        license: Ok(license.clone()),
        ..Options::default()
    });
    let (_, json) = get(&format!("{base}/$/license"));
    assert_eq!(json["status"], "expiring");
    assert_eq!(json["daysLeft"], 10);
    assert_eq!(json["license"]["customer"], "Example Rail AS");

    let base = start(Options {
        license: Err(LicenseError::BadSignature),
        ..Options::default()
    });
    let (_, json) = get(&format!("{base}/$/license"));
    assert_eq!(json["status"], "invalid");
    assert!(json["reason"].as_str().unwrap().contains("signature"));
}
