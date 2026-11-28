//! Requests a browser sends for a page of another site: changes are refused whatever the
//! credentials, and a server without authentication answers only its own host names
//! (against DNS rebinding).

use tripletrail::Engine;
use tripletrail_server::{Options, Server};

fn start(options: Options) -> String {
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), Engine::new_tripletrail())],
        Options {
            threads: 2,
            ..options
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

fn code(result: Result<ureq::Response, ureq::Error>) -> u16 {
    match result {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    }
}

/// A form `POST` of an update to `/kg/update`, with the token and these headers.
fn form_update(base: &str, headers: &[(&str, &str)]) -> u16 {
    let mut request =
        ureq::post(&format!("{base}/kg/update")).set("Authorization", "Bearer update-token");
    for (name, value) in headers {
        request = request.set(name, value);
    }
    code(request.send_form(&[("update", "INSERT DATA { <urn:a> <urn:b> <urn:c> }")]))
}

#[test]
fn a_page_of_another_site_cannot_change_data_with_the_users_credentials() {
    let base = start(Options {
        update_token: Some("update-token".to_owned()),
        cors_origins: vec!["https://app.example.org".to_owned()],
        ..Options::default()
    });
    let own = base.clone();
    // Clients that are not browsers, and the server's own pages, change data as before.
    assert_eq!(form_update(&base, &[]), 200);
    assert_eq!(form_update(&base, &[("Origin", own.as_str())]), 200);
    assert_eq!(
        form_update(&base, &[("Sec-Fetch-Site", "same-origin")]),
        200
    );
    // Behind a proxy that changes the Host, the browser's word that it is the same origin.
    assert_eq!(
        form_update(
            &base,
            &[
                ("Origin", "https://kg.example.org"),
                ("Sec-Fetch-Site", "same-origin")
            ]
        ),
        200
    );
    assert_eq!(
        form_update(
            &base,
            &[
                ("Origin", "https://kg.example.org"),
                ("X-Forwarded-Host", "kg.example.org")
            ]
        ),
        200
    );
    // An origin listed with --cors may.
    assert_eq!(
        form_update(&base, &[("Origin", "https://app.example.org")]),
        200
    );
    // Another site may not, credentials or not.
    assert_eq!(
        form_update(&base, &[("Origin", "https://evil.example")]),
        403
    );
    assert_eq!(form_update(&base, &[("Origin", "null")]), 403);
    assert_eq!(form_update(&base, &[("Sec-Fetch-Site", "cross-site")]), 403);
    assert_eq!(
        form_update(
            &base,
            &[
                ("Origin", "https://evil.example"),
                ("Sec-Fetch-Site", "cross-site")
            ]
        ),
        403
    );
    // Graph Store and admin changes are refused the same way; reads are not changes.
    let upload = ureq::post(&format!("{base}/kg/data"))
        .set("Authorization", "Bearer update-token")
        .set("Origin", "https://evil.example")
        .set("Content-Type", "text/plain")
        .send_string("<urn:a> <urn:b> <urn:c> .");
    assert_eq!(code(upload), 403);
    let admin = ureq::post(&format!("{base}/$/datasets"))
        .set("Origin", "https://evil.example")
        .send_form(&[("name", "other"), ("type", "mem")]);
    assert_eq!(code(admin), 403);
    let read = ureq::get(&format!("{base}/kg/query"))
        .query("query", "ASK {}")
        .set("Origin", "https://evil.example")
        .call();
    assert_eq!(code(read), 200);
}

#[test]
fn with_cors_for_every_origin_only_forms_are_refused() {
    let base = start(Options {
        cors: true,
        ..Options::default()
    });
    // A script's request with a SPARQL body is asked about first and sent without
    // credentials, so it is allowed; a form is sent with them, unasked.
    let script = ureq::post(&format!("{base}/kg/update"))
        .set("Origin", "https://app.example.org")
        .set("Content-Type", "application/sparql-update")
        .send_string("INSERT DATA { <urn:a> <urn:b> <urn:c> }");
    assert_eq!(code(script), 200);
    let form = ureq::post(&format!("{base}/kg/update"))
        .set("Origin", "https://app.example.org")
        .send_form(&[("update", "INSERT DATA { <urn:a> <urn:b> <urn:c> }")]);
    assert_eq!(code(form), 403);
}

#[test]
fn names_answered_for_are_not_sites_that_may_change_data() {
    for allowed in ["*", ".example.org"] {
        let base = start(Options {
            update_token: Some("update-token".to_owned()),
            allowed_hosts: vec![allowed.to_owned()],
            ..Options::default()
        });
        assert_eq!(
            form_update(&base, &[("Origin", "https://evil.example")]),
            403
        );
        assert_eq!(
            form_update(&base, &[("Origin", "https://app.example.org")]),
            403
        );
    }
    let base = start(Options {
        update_token: Some("update-token".to_owned()),
        allowed_hosts: vec!["kg.example.org".to_owned()],
        ..Options::default()
    });
    assert_eq!(
        form_update(&base, &[("Origin", "https://kg.example.org")]),
        200
    );
}

fn ask(base: &str, host: &str) -> u16 {
    code(
        ureq::get(&format!("{base}/kg/query"))
            .query("query", "ASK {}")
            .set("Host", host)
            .call(),
    )
}

#[test]
fn without_authentication_only_the_servers_own_names_are_answered() {
    let base = start(Options::default());
    let port = base.rsplit(':').next().unwrap();
    // Listening on loopback: IP addresses and localhost, not a name another site may own.
    assert_eq!(ask(&base, &format!("127.0.0.1:{port}")), 200);
    assert_eq!(ask(&base, &format!("localhost:{port}")), 200);
    assert_eq!(ask(&base, &format!("app.localhost:{port}")), 200);
    assert_eq!(ask(&base, &format!("[::1]:{port}")), 200);
    assert_eq!(ask(&base, &format!("attacker.example:{port}")), 403);
    assert_eq!(ask(&base, "attacker.example"), 403);

    let named = start(Options {
        allowed_hosts: vec!["kg.example.org".to_owned(), ".internal.example".to_owned()],
        ..Options::default()
    });
    assert_eq!(ask(&named, "kg.example.org"), 200);
    assert_eq!(ask(&named, "KG.example.org:443"), 200);
    assert_eq!(ask(&named, "rail.internal.example"), 200);
    assert_eq!(ask(&named, "kg.example.org.attacker.example"), 403);

    let any = start(Options {
        allowed_hosts: vec!["*".to_owned()],
        ..Options::default()
    });
    assert_eq!(ask(&any, "anything.example"), 200);
}

#[test]
fn with_authentication_any_name_is_answered() {
    let dir = tripletrail_testdir::tempdir("browser-auth");
    let path = dir.path().join("guard.json");
    std::fs::write(&path, r#"{"auth": {"apiKeys": [], "anonymous": true}}"#).unwrap();
    let base = start(Options {
        auth_config: Some(path),
        ..Options::default()
    });
    assert_eq!(ask(&base, "kg.example.org"), 200);
}
