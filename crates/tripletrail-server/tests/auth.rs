//! Caller identity (#199): API keys, trusted proxy headers and anonymous access, with the
//! `auth` configuration of sparql-guard.

use std::path::Path;
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail_server::auth::{sha256, to_hex};
use tripletrail_server::{Options, Server, WorkspaceOptions};

fn hash(key: &str) -> String {
    to_hex(&sha256(key.as_bytes()))
}

/// `proxies` is the JSON list of trusted proxies, or "" to leave `trustedProxies` out.
fn config(anonymous: bool, proxies: &str) -> String {
    let proxies = if proxies.is_empty() {
        String::new()
    } else {
        format!(r#", "trustedProxies": [{proxies}]"#)
    };
    format!(
        r#"{{"upstream": "http://ignored", "auth": {{
            "apiKeys": [
                {{"sha256": "{}", "agent": "https://example.org/people/reader"}},
                {{"sha256": "{}", "agent": "https://example.org/people/writer", "groups": ["tripletrail:writer"]}},
                {{"sha256": "{}", "agent": "https://example.org/people/rinf", "datasets": ["rinf"]}},
                {{"sha256": "{}", "agent": "https://example.org/people/old", "expires": "2020-01-01"}}
            ],
            "trustedHeaders": {{"agent": "x-forwarded-user", "groups": "x-forwarded-groups",
                               "agentPrefix": "https://example.org/people/"{proxies}}},
            "anonymous": {anonymous}
        }}}}"#,
        hash("reader-key"),
        hash("writer-key"),
        hash("rinf-key"),
        hash("old-key"),
    )
}

fn temp_file(name: &str, text: &str) -> tripletrail_testdir::TestPath {
    let path = tripletrail_testdir::tempdir(&format!("auth-{name}")).into_path_of("guard.json");
    std::fs::write(&path, text).unwrap();
    path
}

fn start(path: &Path) -> String {
    let engine = || {
        let e = Engine::new_tripletrail();
        e.load(
            RdfFormat::NTriples,
            &b"<urn:s> <urn:p> \"o\" .\n<https://data.example.org/op/1> <urn:p> \"o\" .\n"[..],
        )
        .unwrap();
        e
    };
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), engine()), ("rinf".to_owned(), engine())],
        Options {
            threads: 2,
            update_token: Some("update-token".to_owned()),
            admin_token: Some("admin-token".to_owned()),
            auth_config: Some(path.to_owned()),
            base_iris: vec![("https://data.example.org/".to_owned(), "kg".to_owned())],
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

/// The status of a request, with headers set.
fn status(request: ureq::Request, body: Option<&str>) -> (u16, Option<String>) {
    let result = match body {
        Some(b) => request
            .set("Content-Type", "application/sparql-update")
            .send_string(b),
        None => request.call(),
    };
    match result {
        Ok(r) => (r.status(), None),
        Err(ureq::Error::Status(code, r)) => {
            (code, r.header("WWW-Authenticate").map(str::to_owned))
        }
        Err(e) => panic!("{e}"),
    }
}

fn query(base: &str, ds: &str) -> ureq::Request {
    ureq::get(&format!("{base}/{ds}/query")).query("query", "ASK {}")
}

fn update(base: &str) -> ureq::Request {
    ureq::post(&format!("{base}/kg/update"))
}

const INSERT: &str = "INSERT DATA { <urn:a> <urn:b> <urn:c> }";

#[test]
fn keys_decide_who_reads_writes_and_administers() {
    let path = temp_file("keys", &config(false, r#""127.0.0.1""#));
    let base = start(&path);

    // Health checks stay open; everything else needs an identity.
    assert_eq!(status(ureq::get(&format!("{base}/$/ping")), None).0, 200);
    // Ready may be 503 while the spatial index builds, but never asks for credentials.
    assert_ne!(status(ureq::get(&format!("{base}/$/ready")), None).0, 401);
    let (code, challenge) = status(query(&base, "kg"), None);
    assert_eq!(code, 401);
    assert_eq!(challenge.as_deref(), Some("Bearer realm=\"tripletrail\""));
    assert_eq!(
        status(ureq::get(&format!("{base}/$/datasets")), None).0,
        401
    );

    // Keys in either header.
    let reader = |r: ureq::Request| r.set("Authorization", "Bearer reader-key");
    assert_eq!(status(reader(query(&base, "kg")), None).0, 200);
    assert_eq!(
        status(query(&base, "kg").set("X-API-Key", "reader-key"), None).0,
        200
    );
    // A reader may not write (403: known, not allowed); a writer and the old tokens may.
    assert_eq!(status(reader(update(&base)), Some(INSERT)).0, 403);
    let writer = update(&base).set("Authorization", "Bearer writer-key");
    assert_eq!(status(writer, Some(INSERT)).0, 200);
    let token = update(&base).set("Authorization", "Bearer update-token");
    assert_eq!(status(token, Some(INSERT)).0, 200);
    // Graph Store writes check the same.
    let put = ureq::put(&format!("{base}/kg/data?default"))
        .set("Authorization", "Bearer reader-key")
        .set("Content-Type", "application/n-triples");
    assert_eq!(
        put.send_string("<urn:x> <urn:y> <urn:z> .\n")
            .err()
            .map(|e| match e {
                ureq::Error::Status(c, _) => c,
                e => panic!("{e}"),
            }),
        Some(403)
    );

    // Admin changes need tripletrail:admin: the writer is refused, the admin token passes
    // (and the request then fails on its own: there is no dataset "nope").
    let delete = |key: &str| {
        ureq::delete(&format!("{base}/$/datasets/nope"))
            .set("Authorization", &format!("Bearer {key}"))
    };
    assert_eq!(status(delete("writer-key"), None).0, 403);
    assert_eq!(status(delete("admin-token"), None).0, 404);
    assert_eq!(
        status(reader(ureq::get(&format!("{base}/$/datasets"))), None).0,
        200
    );

    // A key limited to some datasets.
    let rinf = |ds: &str| query(&base, ds).set("Authorization", "Bearer rinf-key");
    assert_eq!(status(rinf("rinf"), None).0, 200);
    assert_eq!(status(rinf("kg"), None).0, 403);
    // ... nor through kg's Linked Data IRIs, its VoID or the admin API.
    let resource = |key: &str| {
        ureq::get(&format!("{base}/op/1"))
            .set("X-Forwarded-Host", "data.example.org")
            .set("X-Forwarded-Proto", "https")
            .set("Accept", "text/turtle")
            .set("X-API-Key", key)
    };
    assert_eq!(status(resource("reader-key"), None).0, 200);
    assert_eq!(status(resource("rinf-key"), None).0, 403);
    let void = ureq::get(&format!("{base}/.well-known/void"))
        .set("Accept", "application/n-triples")
        .set("X-API-Key", "rinf-key")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(void.contains("/rinf/") && !void.contains("/kg/"), "{void}");
    // The DCAT catalog lists only rinf too.
    let catalog = ureq::get(&format!("{base}/.well-known/dcat"))
        .set("Accept", "application/n-triples")
        .set("X-API-Key", "rinf-key")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        catalog.contains("/rinf/sparql") && !catalog.contains("/kg/"),
        "{catalog}"
    );
    let datasets = ureq::get(&format!("{base}/$/datasets")).set("X-API-Key", "rinf-key");
    assert_eq!(status(datasets, None).0, 403);

    // Expired and unknown keys are 401, never anonymous.
    assert_eq!(
        status(query(&base, "kg").set("X-API-Key", "old-key"), None).0,
        401
    );
    assert_eq!(
        status(query(&base, "kg").set("X-API-Key", "typo"), None).0,
        401
    );

    // Trusted headers from a trusted address; plain names get the agent prefix.
    let user = query(&base, "kg").set("X-Forwarded-User", "jan");
    assert_eq!(status(user, None).0, 200);
    let user = update(&base)
        .set("X-Forwarded-User", "jan")
        .set("X-Forwarded-Groups", "staff, tripletrail:writer");
    assert_eq!(status(user, Some(INSERT)).0, 200);
    let user = update(&base).set("X-Forwarded-User", "jan");
    assert_eq!(status(user, Some(INSERT)).0, 403);

    // Too many unknown keys from one address: 429 for a while, even for a good key.
    for _ in 0..12 {
        let _ = status(query(&base, "kg").set("X-API-Key", "guess"), None);
    }
    assert_eq!(status(reader(query(&base, "kg")), None).0, 429);
}

/// `Authorization: Basic` for a user name and password.
fn basic(user: &str, password: &str) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = format!("{user}:{password}").into_bytes();
    let mut out = String::from("Basic ");
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                A[((n >> (18 - 6 * i)) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

#[test]
fn users_sign_in_with_basic_as_on_fuseki() {
    // The hashes were made with Python's hashlib: PBKDF2-HMAC-SHA256 of "ann-pass" (1000
    // rounds) and Shiro's iterated SHA-256 of "bob pass:2" with salt "pepper" (500 rounds).
    let path = temp_file(
        "basic",
        r#"{"auth": {
            "apiKeys": [{"sha256": "HASH", "agent": "https://example.org/people/reader"}],
            "users": [
                {"name": "ann", "password": "$pbkdf2-sha256$1000$bWF0Z3JhcGgtdGVzdC0wMQ$M07duW59Qqi6AOSwVVdusumZxtaFzyCjlgYz1r8x5ps",
                 "groups": ["tripletrail:writer"]},
                {"name": "bob", "password": "$shiro1$SHA-256$500$cGVwcGVy$/9CvrWdc97zKy8hJjnHJQKlc/miRRXB1yEASJoji0iA=",
                 "agent": "https://example.org/people/bob", "datasets": ["rinf"]},
                {"name": "old", "password": "$pbkdf2-sha256$1000$bWF0Z3JhcGgtdGVzdC0wMQ$M07duW59Qqi6AOSwVVdusumZxtaFzyCjlgYz1r8x5ps",
                 "expires": "2020-01-01"}
            ]
        }}"#
        .replace("HASH", &hash("reader-key"))
        .as_str(),
    );
    let base = start(&path);
    let as_user = |r: ureq::Request, user: &str, password: &str| {
        r.set("Authorization", &basic(user, password))
    };

    // Ann writes; Bob (a password with a space and a colon) reads only his dataset.
    assert_eq!(
        status(as_user(query(&base, "kg"), "ann", "ann-pass"), None).0,
        200
    );
    assert_eq!(
        status(as_user(update(&base), "ann", "ann-pass"), Some(INSERT)).0,
        200
    );
    // Asked again, from memory of the checked password: still the same answer.
    assert_eq!(
        status(as_user(query(&base, "kg"), "ann", "ann-pass"), None).0,
        200
    );
    assert_eq!(
        status(as_user(query(&base, "rinf"), "bob", "bob pass:2"), None).0,
        200
    );
    assert_eq!(
        status(as_user(query(&base, "kg"), "bob", "bob pass:2"), None).0,
        403
    );
    assert_eq!(
        status(as_user(update(&base), "bob", "bob pass:2"), Some(INSERT)).0,
        403
    );
    // A wrong password, an unknown user, an expired user and unreadable credentials are
    // 401, never anonymous; the answer asks for Basic as well as a key.
    for (user, password) in [
        ("ann", "ann-pas"),
        ("ann", "Ann-pass"),
        ("eve", "ann-pass"),
        ("old", "ann-pass"),
    ] {
        let (code, _) = status(as_user(query(&base, "kg"), user, password), None);
        assert_eq!(code, 401, "{user}:{password}");
    }
    let garbled = query(&base, "kg").set("Authorization", "Basic !!!");
    assert_eq!(status(garbled, None).0, 401);
    let challenges = match query(&base, "kg").call() {
        Err(ureq::Error::Status(401, r)) => r
            .all("WWW-Authenticate")
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert!(
        challenges.iter().any(|c| c.starts_with("Basic realm=")),
        "{challenges:?}"
    );
    assert!(
        challenges.iter().any(|c| c.starts_with("Bearer")),
        "{challenges:?}"
    );

    // API keys keep working next to users.
    let reader = query(&base, "kg").set("Authorization", "Bearer reader-key");
    assert_eq!(status(reader, None).0, 200);

    // The access pages list the users, never their hashes.
    let access: serde_json::Value = ureq::get(&format!("{base}/$/access"))
        .set("Authorization", "Bearer admin-token")
        .call()
        .unwrap()
        .into_string()
        .unwrap()
        .parse()
        .unwrap();
    let text = access.to_string();
    let users = access["users"].as_array().expect("users");
    assert_eq!(users[0]["name"], "ann", "{text}");
    // A user without an agent gets one from the name, as trusted headers do.
    assert_eq!(users[0]["agent"], "urn:sparql-guard:user:ann", "{text}");
    assert_eq!(
        users[1]["agent"], "https://example.org/people/bob",
        "{text}"
    );
    assert_eq!(users[2]["expired"], true, "{text}");
    assert!(
        !text.contains("shiro1") && !text.contains("pbkdf2"),
        "{text}"
    );

    // Guessing passwords: 429 for a while, even for the right one.
    for _ in 0..12 {
        let _ = status(as_user(query(&base, "kg"), "ann", "guess"), None);
    }
    assert_eq!(
        status(as_user(query(&base, "kg"), "ann", "ann-pass"), None).0,
        429
    );
}

#[test]
fn without_users_a_basic_header_is_ignored_as_before() {
    // A proxy that signs people in and forwards its own Basic header (nginx auth_basic,
    // oauth2-proxy --pass-basic-auth), or a client that always sends one: not for TripleTrail.
    let path = temp_file("no-users", &config(true, r#""127.0.0.1""#));
    let base = start(&path);
    let forwarded = query(&base, "kg")
        .set("Authorization", &basic("jan", "proxy-password"))
        .set("X-Forwarded-User", "jan");
    assert_eq!(status(forwarded, None).0, 200);
    let anonymous = query(&base, "kg").set("Authorization", &basic("x", "y"));
    assert_eq!(status(anonymous, None).0, 200);
    // And the 401 asks for a key only.
    let path = temp_file("no-users-closed", &config(false, r#""127.0.0.1""#));
    let base = start(&path);
    match query(&base, "kg").call() {
        Err(ureq::Error::Status(401, r)) => {
            let challenges: Vec<&str> = r.all("WWW-Authenticate");
            assert!(
                challenges.iter().all(|c| !c.starts_with("Basic")),
                "{challenges:?}"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_user_whose_password_is_not_a_hash_stops_the_server_from_starting() {
    let path = temp_file(
        "plain",
        r#"{"auth": {"users": [{"name": "admin", "password": "admin123"}]}}"#,
    );
    let error = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), Engine::new_tripletrail())],
        Options {
            auth_config: Some(path.to_path_buf()),
            ..Options::default()
        },
    )
    .err()
    .expect("refused");
    let error = error.to_string();
    assert!(error.contains("auth.users[0].password"), "{error}");
    assert!(error.contains("hash-password"), "{error}");
}

#[test]
fn headers_from_untrusted_addresses_are_ignored_and_the_file_reloads() {
    let path = temp_file("reload", &config(false, r#""10.0.0.0/8""#));
    let base = start(&path);
    let user = || query(&base, "kg").set("X-Forwarded-User", "mallory");
    assert_eq!(status(user(), None).0, 401);
    let spoof = ureq::delete(&format!("{base}/$/datasets/nope"))
        .set("X-Forwarded-User", "mallory")
        .set("X-Forwarded-Groups", "tripletrail:admin");
    assert_eq!(status(spoof, None).0, 401);
    assert_eq!(status(query(&base, "kg"), None).0, 401);

    // Turn anonymous access on: picked up without a restart.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&path, config(true, r#""10.0.0.0/8""#)).unwrap();
    let mut code = 0;
    for _ in 0..50 {
        code = status(query(&base, "kg"), None).0;
        if code == 200 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(code, 200);
    // Anonymous callers may read but not write: 401, as they have no identity.
    assert_eq!(status(update(&base), Some(INSERT)).0, 401);

    // A broken file keeps the previous configuration.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&path, "{ not json").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(status(query(&base, "kg"), None).0, 200);
}

#[test]
fn headers_without_listed_proxies_come_from_loopback_and_never_grant_server_groups() {
    let path = temp_file("loopback", &config(false, ""));
    let base = start(&path);
    // From 127.0.0.1 the user is believed ...
    let user = query(&base, "kg").set("X-Forwarded-User", "jan");
    assert_eq!(status(user, None).0, 200);
    // ... but not its claim to the server's groups.
    let spoof = |r: ureq::Request| {
        r.set("X-Forwarded-User", "mallory").set(
            "X-Forwarded-Groups",
            "tripletrail:admin, tripletrail:writer",
        )
    };
    assert_eq!(
        status(
            spoof(ureq::delete(&format!("{base}/$/datasets/nope"))),
            None
        )
        .0,
        403
    );
    assert_eq!(status(spoof(update(&base)), Some(INSERT)).0, 403);
}

#[test]
fn unknown_keys_are_counted_per_client_behind_a_listed_proxy() {
    let path = temp_file("forwarded", &config(false, r#""127.0.0.1""#));
    let base = start(&path);
    for _ in 0..12 {
        let guess = query(&base, "kg")
            .set("X-Forwarded-For", "198.51.100.7")
            .set("X-API-Key", "guess");
        let _ = status(guess, None);
    }
    let blocked = query(&base, "kg")
        .set("X-Forwarded-For", "198.51.100.7")
        .set("X-API-Key", "reader-key");
    assert_eq!(status(blocked, None).0, 429);
    // Another client behind the same proxy is not locked out.
    let other = query(&base, "kg")
        .set("X-Forwarded-For", "198.51.100.8")
        .set("X-API-Key", "reader-key");
    assert_eq!(status(other, None).0, 200);
}

/// Key rotation (enterprise #9): a key replaced in the file works without a restart, and the
/// old one is a 401 from then on, never anonymous, even with anonymous access turned on.
#[test]
fn a_rotated_key_works_at_once_and_the_old_one_is_never_anonymous() {
    let path = temp_file("rotate", &config(false, ""));
    let base = start(&path);
    let with = |key: &str| query(&base, "kg").set("X-API-Key", key);
    assert_eq!(status(with("reader-key"), None).0, 200);
    assert_eq!(status(with("new-reader-key"), None).0, 401);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let rotated = config(true, "").replace(&hash("reader-key"), &hash("new-reader-key"));
    std::fs::write(&path, rotated).unwrap();
    let mut code = 0;
    for _ in 0..50 {
        code = status(with("new-reader-key"), None).0;
        if code == 200 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(code, 200);
    // Anonymous access is on now, yet the old key is refused rather than read as no key.
    assert_eq!(status(query(&base, "kg"), None).0, 200);
    let (code, challenge) = status(with("reader-key"), None);
    assert_eq!(code, 401);
    assert!(challenge.is_some());
    // The other keys are unchanged.
    let writer = update(&base).set("X-API-Key", "writer-key");
    assert_eq!(status(writer, Some(INSERT)).0, 200);
}

#[test]
fn a_broken_configuration_stops_the_server_from_starting() {
    let path = temp_file("broken", r#"{"auth": {"apiKeys": [{"sha256": "abc"}]}}"#);
    let result = Server::bind(
        "127.0.0.1:0",
        Vec::<(String, Engine)>::new(),
        Options {
            auth_config: Some(path.to_path_buf()),
            ..Options::default()
        },
    );
    let error = result.err().expect("refused").to_string();
    assert!(error.contains("auth.apiKeys[0].sha256"), "{error}");
}

const SG: &str =
    "@prefix sg: <https://matdata.eu/ns/sparql-guard#> .\n@prefix ex: <http://example.org/> .\n";

/// RINF open to anyone, the KG ("kg") to signed-in callers with the asset team writing, and
/// "other" without policies.
fn policy_server(dir: &Path) -> String {
    std::fs::write(
        dir.join("rinf.ttl"),
        format!("{SG} ex:RinfPublic a sg:Policy ; sg:agent sg:Anyone ; sg:readAllGraphs true ."),
    )
    .unwrap();
    std::fs::write(
        dir.join("kg.ttl"),
        format!(
            "{SG} ex:AssetReaders a sg:Policy ; sg:agent sg:AuthenticatedAgent ; sg:readAllGraphs true .
             ex:AssetWriters a sg:Policy ; sg:group \"asset-team\" ; sg:writeAllGraphs true ."
        ),
    )
    .unwrap();
    let key = |k: &str, groups: &str| {
        format!(
            r#"{{"sha256": "{}", "agent": "https://example.org/people/{k}", "groups": [{groups}]}}"#,
            hash(k)
        )
    };
    let config = format!(
        r#"{{"auth": {{"anonymous": true, "apiKeys": [{}, {}, {}, {}]}},
            "datasets": {{
                "rinf": {{"policies": ["rinf.ttl"]}},
                "kg": {{"policies": "kg.ttl", "admins": ["kg-admins"]}}
            }}}}"#,
        key("reader", ""),
        key("assets", r#""asset-team""#),
        key("kgadmin", r#""kg-admins""#),
        key("monitor", r#""tripletrail:monitor""#),
    );
    let path = dir.join("guard.json");
    std::fs::write(&path, config).unwrap();

    let engine = || {
        let e = Engine::new_tripletrail();
        e.load(RdfFormat::NTriples, &b"<urn:s> <urn:p> \"o\" .\n"[..])
            .unwrap();
        e
    };
    let server = Server::bind(
        "127.0.0.1:0",
        [
            ("kg".to_owned(), engine()),
            ("rinf".to_owned(), engine()),
            ("other".to_owned(), engine()),
        ],
        Options {
            threads: 2,
            auth_config: Some(path.to_path_buf()),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    base
}

#[test]
fn policies_open_rinf_to_anyone_and_assets_to_keys() {
    let dir = tripletrail_testdir::tempdir("auth-policies");
    let base = policy_server(&dir);
    let with = |r: ureq::Request, key: &str| r.set("X-API-Key", key);
    let insert = |ds: &str| ureq::post(&format!("{base}/{ds}/update"));

    // Anonymous: RINF yes, the KG no (401: signing in may help), "other" has no policies.
    assert_eq!(status(query(&base, "rinf"), None).0, 200);
    assert_eq!(status(query(&base, "kg"), None).0, 401);
    assert_eq!(status(with(query(&base, "other"), "reader"), None).0, 403);
    // Signed in: the KG readable, writable only by the asset team, which cannot write RINF.
    assert_eq!(status(with(query(&base, "kg"), "reader"), None).0, 200);
    assert_eq!(status(with(insert("kg"), "reader"), Some(INSERT)).0, 403);
    assert_eq!(status(with(insert("kg"), "assets"), Some(INSERT)).0, 200);
    assert_eq!(status(with(insert("rinf"), "assets"), Some(INSERT)).0, 403);
    let put = with(ureq::put(&format!("{base}/rinf/data?default")), "assets")
        .set("Content-Type", "application/n-triples");
    assert!(matches!(
        put.send_string("<urn:x> <urn:y> <urn:z> .\n"),
        Err(ureq::Error::Status(403, _))
    ));

    // Listings show what the caller may read.
    let datasets = |key: Option<&str>| {
        let mut r = ureq::get(&format!("{base}/$/datasets"));
        if let Some(k) = key {
            r = r.set("X-API-Key", k);
        }
        r.call().unwrap().into_string().unwrap()
    };
    let anonymous = datasets(None);
    assert!(
        anonymous.contains("\"/rinf\"") && !anonymous.contains("\"/kg\""),
        "{anonymous}"
    );
    assert!(datasets(Some("reader")).contains("\"/kg\""));
    assert!(!datasets(Some("reader")).contains("\"/other\""));
    assert_eq!(
        status(ureq::get(&format!("{base}/$/datasets/kg")), None).0,
        404
    );

    // Server-wide pages need tripletrail:monitor (or admin) once policies exist.
    let metrics = |key: &str| with(ureq::get(&format!("{base}/$/metrics")), key);
    assert_eq!(status(metrics("reader"), None).0, 403);
    assert_eq!(status(metrics("monitor"), None).0, 200);

    // A dataset's admins may detach it, not another one.
    let detach = |ds: &str| with(ureq::delete(&format!("{base}/$/datasets/{ds}")), "kgadmin");
    assert_eq!(status(detach("rinf"), None).0, 403);
    assert_eq!(status(detach("kg"), None).0, 200);
}

#[test]
fn policy_files_reload_and_unenforceable_rules_are_refused() {
    let dir = tripletrail_testdir::tempdir("auth-policy-reload");
    let base = policy_server(&dir);
    assert_eq!(status(query(&base, "kg"), None).0, 401);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(
        dir.join("kg.ttl"),
        format!("{SG} ex:Open a sg:Policy ; sg:agent sg:Anyone ; sg:readAllGraphs true ."),
    )
    .unwrap();
    let mut code = 0;
    for _ in 0..50 {
        code = status(query(&base, "kg"), None).0;
        if code == 200 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(code, 200);

    // Row rules naming a shape that is not there: the file fails to load, and the old
    // policies stay.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(
        dir.join("kg.ttl"),
        format!("{SG} ex:Rows a sg:Policy ; sg:agent sg:Anyone ; sg:visibleIf ex:Shape ."),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(status(query(&base, "kg"), None).0, 200);
    // ... and a server does not start with them.
    let result = Server::bind(
        "127.0.0.1:0",
        Vec::<(String, Engine)>::new(),
        Options {
            auth_config: Some(dir.join("guard.json")),
            ..Options::default()
        },
    );
    let error = result.err().expect("refused").to_string();
    // The open build cannot enforce row rules at all, and says so (#818): fail closed.
    if cfg!(feature = "enterprise") {
        assert!(error.contains("no shape"), "{error}");
    } else {
        assert!(error.contains("need TripleTrail Enterprise"), "{error}");
    }
}

/// Contractors may read the release graph of "assets" (#201), the team every graph.
#[test]
fn a_graph_grant_shows_only_its_graphs() {
    let dir = tripletrail_testdir::tempdir("auth-graph-grants");
    std::fs::write(
        dir.join("assets.ttl"),
        format!(
            "{SG} ex:Contractors a sg:Policy ; sg:group \"contractor-x\" ; sg:readGraph <urn:release> .
             ex:Team a sg:Policy ; sg:group \"team\" ; sg:readAllGraphs true ."
        ),
    )
    .unwrap();
    let key = |k: &str, group: &str| {
        format!(
            r#"{{"sha256": "{}", "agent": "https://example.org/people/{k}", "groups": ["{group}"]}}"#,
            hash(k)
        )
    };
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"apiKeys": [{}, {}]}}, "datasets": {{"assets": {{"policies": ["assets.ttl"]}}}}}}"#,
            key("contractor", "contractor-x"),
            key("team", "team"),
        ),
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            &b"<urn:a> <urn:in> \"default\" .
               <urn:release> { <urn:a> <urn:in> \"release\" }
               <urn:wip> { <urn:a> <urn:in> \"wip\" . <urn:w> <urn:only> \"wip\" }"[..],
        )
        .unwrap();
    let writer = engine.clone();
    let server = Server::bind(
        "127.0.0.1:0",
        [("assets".to_owned(), engine)],
        Options {
            threads: 2,
            auth_config: Some(dir.join("guard.json")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();

    let get = |path: &str, key: &str| -> Result<String, u16> {
        match ureq::get(&format!("{base}{path}"))
            .set("X-API-Key", key)
            .set(
                "Accept",
                "application/sparql-results+json, application/n-quads, application/n-triples",
            )
            .call()
        {
            Ok(r) => Ok(r.into_string().unwrap()),
            Err(ureq::Error::Status(code, _)) => Err(code),
            Err(e) => panic!("{e}"),
        }
    };
    let select = |query: &str, key: &str| {
        let path = format!(
            "/assets/query?query={}",
            form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        );
        let json: serde_json::Value = serde_json::from_str(&get(&path, key).unwrap()).unwrap();
        let mut values: Vec<String> = json["results"]["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["v"]["value"].as_str().unwrap().to_owned())
            .collect();
        values.sort();
        values
    };
    let all = "SELECT ?v { { ?s <urn:in> ?v } UNION { GRAPH ?g { ?s <urn:in> ?v } } }";
    assert_eq!(select(all, "team"), ["default", "release", "wip"]);
    // The contractor's default graph is the release graph, and it is its only named graph.
    assert_eq!(select(all, "contractor"), ["release", "release"]);
    assert_eq!(
        select("SELECT DISTINCT ?v { GRAPH ?v { } }", "contractor"),
        ["urn:release"]
    );
    assert!(select("SELECT ?v FROM <urn:wip> { ?s <urn:in> ?v }", "contractor").is_empty());
    let both = "SELECT ?v FROM <urn:wip> FROM <urn:release> { ?s <urn:in> ?v }";
    assert_eq!(select(both, "contractor"), ["release"]);
    // The protocol's dataset narrows too.
    let path = format!(
        "/assets/query?default-graph-uri=urn:wip&query={}",
        form_urlencoded::byte_serialize(b"SELECT ?v { ?s <urn:in> ?v }").collect::<String>()
    );
    assert!(!get(&path, "contractor").unwrap().contains("wip"));

    // The Graph Store Protocol: other named graphs do not exist, the default graph is empty.
    assert!(
        get("/assets/data?graph=urn:release", "contractor")
            .unwrap()
            .contains("release")
    );
    assert_eq!(get("/assets/data?graph=urn:wip", "contractor"), Err(404));
    assert_eq!(
        get("/assets/data?graph=urn:nothing", "contractor"),
        Err(404)
    );
    assert!(
        !get("/assets/data?default", "contractor")
            .unwrap()
            .contains("default")
    );
    let dump = get("/assets/data", "contractor").unwrap();
    assert!(dump.contains("release") && !dump.contains("wip") && !dump.contains("default"));

    // Describe keeps to the grant (#203): a resource only in other graphs is not found, and
    // callers with other grants get other validators.
    let doc = get("/assets/describe?iri=urn:a", "contractor").unwrap();
    assert!(doc.contains("release") && !doc.contains("wip") && !doc.contains("default"));
    assert!(
        get("/assets/describe?iri=urn:w", "team")
            .unwrap()
            .contains("wip")
    );
    assert_eq!(get("/assets/describe?iri=urn:w", "contractor"), Err(404));
    let etag = |key: &str| {
        ureq::get(&format!("{base}/assets/describe?iri=urn:a"))
            .set("X-API-Key", key)
            .call()
            .unwrap()
            .header("ETag")
            .unwrap()
            .to_owned()
    };
    assert_ne!(etag("contractor"), etag("team"));

    // The service description lists only the granted graphs, and no size for the default
    // graph (the union of those).
    for path in ["/assets/sparql", "/assets/query"] {
        let sd = get(path, "contractor").unwrap();
        assert!(
            sd.contains("urn:release") && !sd.contains("urn:wip"),
            "{sd}"
        );
        assert!(get(path, "team").unwrap().contains("urn:wip"));
    }

    // Services that would read past the grant are refused; writes need a write grant.
    // Explain runs on the readable graphs, and VoID describes only them (#203).
    let explain = |key: &str| {
        let path = format!(
            "/assets/explain?query={}",
            form_urlencoded::byte_serialize(b"SELECT * { GRAPH ?g { ?s ?p ?o } }")
                .collect::<String>()
        );
        let json: serde_json::Value = serde_json::from_str(&get(&path, key).unwrap()).unwrap();
        json["rows"].as_u64().unwrap()
    };
    assert_eq!(explain("contractor"), 1);
    assert_eq!(explain("team"), 3);
    let plan = get("/assets/explain?query=ASK%7B%7D", "contractor").unwrap();
    assert!(
        !plan.contains("plan") && !plan.contains("spatial"),
        "{plan}"
    );
    let void = get("/assets/void", "contractor").unwrap();
    assert!(void.contains("urn:release"), "{void}");
    assert!(
        !void.contains("urn:wip") && !void.contains("classes"),
        "{void}"
    );
    assert!(get("/assets/void", "team").unwrap().contains("urn:wip"));
    let service = "SELECT * { SERVICE <http://w3id.org/tripletrail/st> { ?s ?p ?o } }";
    let path = format!(
        "/assets/query?query={}",
        form_urlencoded::byte_serialize(service.as_bytes()).collect::<String>()
    );
    assert_eq!(get(&path, "contractor"), Err(400));
    let insert = ureq::post(&format!("{base}/assets/update")).set("X-API-Key", "contractor");
    assert_eq!(status(insert, Some(INSERT)).0, 403);

    // The RDF4J API reads the granted graphs only, and writes nothing (#203).
    let repo = "/repositories/assets";
    let statements = get(&format!("{repo}/statements"), "contractor").unwrap();
    assert!(
        statements.contains("release") && !statements.contains("wip"),
        "{statements}"
    );
    assert!(!statements.contains("default"), "{statements}");
    assert!(
        !get(
            &format!("{repo}/statements?context=%3Curn:wip%3E"),
            "contractor"
        )
        .unwrap()
        .contains("wip")
    );
    let contexts = get(&format!("{repo}/contexts"), "contractor").unwrap();
    assert!(contexts.contains("urn:release") && !contexts.contains("urn:wip"));
    assert!(
        get(&format!("{repo}/contexts"), "team")
            .unwrap()
            .contains("urn:wip")
    );
    assert_eq!(get(&format!("{repo}/size"), "contractor").unwrap(), "1");
    assert_eq!(
        get(&format!("{repo}/size?context=%3Curn:wip%3E"), "contractor").unwrap(),
        "0"
    );
    assert_eq!(get(&format!("{repo}/size"), "team").unwrap(), "4");
    let rows = get(
        &format!(
            "{repo}?query={}",
            form_urlencoded::byte_serialize(b"SELECT ?o { ?s ?p ?o }").collect::<String>()
        ),
        "contractor",
    )
    .unwrap();
    assert!(rows.contains("release") && !rows.contains("wip"), "{rows}");
    let post = ureq::post(&format!("{base}{repo}/statements"))
        .set("X-API-Key", "contractor")
        .set("Content-Type", "application/sparql-update");
    assert_eq!(status(post, Some(INSERT)).0, 403);
    let begin = ureq::post(&format!("{base}{repo}/transactions")).set("X-API-Key", "contractor");
    assert_eq!(status(begin, None).0, 403);
    // The listings name the dataset, read-only and without its counts.
    let repositories = get("/repositories", "contractor").unwrap();
    assert!(repositories.contains("assets"), "{repositories}");
    let listed = get("/$/datasets", "contractor").unwrap();
    assert!(listed.contains("/assets"), "{listed}");
    let one = get("/$/datasets/assets", "contractor").unwrap();
    assert!(
        !one.contains("triples") && !one.contains("location"),
        "{one}"
    );
    assert!(
        get("/$/datasets/assets", "team")
            .unwrap()
            .contains("triples")
    );
    let void = get("/.well-known/void", "contractor").unwrap();
    assert!(
        void.contains("urn:release") && !void.contains("urn:wip"),
        "{void}"
    );

    // A write the contractor may not read leaves its tag as it was (the store's write
    // generation would tell it that something changed); one to its own graph does not.
    let before = etag("contractor");
    writer
        .update("INSERT DATA { GRAPH <urn:wip> { <urn:a> <urn:in> \"later\" } }")
        .unwrap();
    assert_eq!(etag("contractor"), before);
    writer
        .update("INSERT DATA { GRAPH <urn:release> { <urn:a> <urn:in> \"later\" } }")
        .unwrap();
    assert_ne!(etag("contractor"), before);
    // Nor does the result cache tell it (`X-Cache`).
    let cached = |key: &str| {
        ureq::get(&format!(
            "{base}/assets/query?query=ASK%7B%3Fs%20%3Fp%20%3Fo%7D"
        ))
        .set("X-API-Key", key)
        .call()
        .unwrap()
        .header("X-Cache")
        .map(str::to_owned)
    };
    assert_eq!(cached("contractor"), None);
    assert!(cached("team").is_some());
}

/// Editors may write the work-in-progress graph of "assets" and read it with the release graph
/// (#201); the team reads everything and writes only the release graph.
#[test]
fn a_graph_write_grant_writes_only_its_graphs() {
    let dir = tripletrail_testdir::tempdir("auth-graph-writes");
    std::fs::write(
        dir.join("assets.ttl"),
        format!(
            "{SG} ex:Editors a sg:Policy ; sg:group \"editors\" ;
               sg:readGraph <urn:release> ; sg:writeGraph <urn:wip> .
             ex:Team a sg:Policy ; sg:group \"team\" ; sg:readAllGraphs true ;
               sg:writeGraph <urn:release> ."
        ),
    )
    .unwrap();
    let key = |k: &str, group: &str| {
        format!(
            r#"{{"sha256": "{}", "agent": "https://example.org/people/{k}", "groups": ["{group}"]}}"#,
            hash(k)
        )
    };
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"apiKeys": [{}, {}]}}, "datasets": {{"assets": {{"policies": ["assets.ttl"]}}}}}}"#,
            key("editor", "editors"),
            key("team", "team"),
        ),
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            &b"<urn:a> <urn:in> \"default\" .
               <urn:release> { <urn:a> <urn:in> \"release\" }
               <urn:wip> { <urn:a> <urn:in> \"wip\" }"[..],
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("assets".to_owned(), engine)],
        Options {
            threads: 2,
            auth_config: Some(dir.join("guard.json")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();

    let update = |key: &str, text: &str| {
        let r = ureq::post(&format!("{base}/assets/update")).set("X-API-Key", key);
        status(r, Some(text)).0
    };
    let put = |key: &str, target: &str| match ureq::put(&format!("{base}/assets/data{target}"))
        .set("X-API-Key", key)
        .set("Content-Type", "application/n-triples")
        .send_string("<urn:b> <urn:in> \"put\" .\n")
    {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    };
    let ask = |query: &str| {
        let r = ureq::get(&format!("{base}/assets/query"))
            .set("X-API-Key", "team")
            .set("Accept", "application/sparql-results+json")
            .query("query", query)
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        r.contains("true")
    };

    // The editor writes the wip graph, and may copy from the release graph into it.
    assert_eq!(
        update(
            "editor",
            "INSERT DATA { GRAPH <urn:wip> { <urn:e> <urn:in> \"edited\" } }"
        ),
        200
    );
    assert_eq!(
        update(
            "editor",
            "INSERT { GRAPH <urn:wip> { ?s <urn:from> ?v } } WHERE { GRAPH <urn:release> { ?s <urn:in> ?v } }"
        ),
        200
    );
    assert!(ask(
        "ASK { GRAPH <urn:wip> { <urn:a> <urn:from> \"release\" } }"
    ));
    // ... but not the release graph, the default graph or every graph; nothing is applied.
    for refused in [
        "INSERT DATA { GRAPH <urn:release> { <urn:x> <urn:y> 1 } }",
        "INSERT DATA { <urn:x> <urn:y> 1 }",
        "INSERT DATA { GRAPH <urn:wip> { <urn:x> <urn:y> 1 } } ; CLEAR ALL",
        "DELETE WHERE { GRAPH ?g { ?s ?p ?o } }",
    ] {
        assert_eq!(update("editor", refused), 403, "{refused}");
    }
    assert!(!ask("ASK { <urn:x> <urn:y> 1 }"));
    assert!(!ask("ASK { GRAPH ?g { <urn:x> <urn:y> 1 } }"));
    // The Graph Store Protocol: only a writable graph, named.
    assert_eq!(put("editor", "?graph=urn:wip"), 204);
    assert_eq!(put("editor", "?graph=urn:release"), 403);
    assert_eq!(put("editor", "?default"), 403);
    assert_eq!(put("editor", ""), 403);
    // A quad body sent to a writable graph may not name another graph.
    let send = |post: bool, media: &str, body: &str| {
        let url = format!("{base}/assets/data?graph=urn:wip");
        let r = if post {
            ureq::post(&url)
        } else {
            ureq::put(&url)
        };
        match r
            .set("X-API-Key", "editor")
            .set("Content-Type", media)
            .send_string(body)
        {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(code, _)) => code,
            Err(e) => panic!("{e}"),
        }
    };
    for (post, media, body) in [
        (
            true,
            "application/trig",
            "<urn:release> { <urn:q> <urn:trig> 1 }",
        ),
        (
            false,
            "application/trig",
            "<urn:q> <urn:trig> 2 . <urn:release> { <urn:q> <urn:trig> 2 }",
        ),
        (
            true,
            "application/n-quads",
            "<urn:q> <urn:nq> <urn:o> <urn:release> .\n",
        ),
    ] {
        assert_eq!(send(post, media, body), 403, "{body}");
    }
    assert!(!ask("ASK { GRAPH ?g { <urn:q> ?p ?o } }"));
    assert!(ask("ASK { GRAPH <urn:wip> { <urn:b> <urn:in> \"put\" } }"));
    // ... though a quad body that names only the target graph is fine.
    assert_eq!(
        send(
            true,
            "application/n-quads",
            "<urn:q> <urn:nq> <urn:o> <urn:wip> .\n"
        ),
        204
    );

    // Graph management (enterprise #9): the editor clears, drops and fills only the writable
    // graph. An operation that writes or empties any other graph is refused whole.
    for refused in [
        "CLEAR GRAPH <urn:release>",
        "DROP GRAPH <urn:release>",
        "DROP SILENT GRAPH <urn:release>",
        "CLEAR DEFAULT",
        "DROP NAMED",
        "CLEAR ALL",
        "CREATE GRAPH <urn:other>",
        "MOVE GRAPH <urn:release> TO GRAPH <urn:wip>",
        "MOVE GRAPH <urn:wip> TO GRAPH <urn:release>",
        "COPY GRAPH <urn:wip> TO DEFAULT",
        "COPY GRAPH <urn:wip> TO GRAPH <urn:other>",
        "ADD GRAPH <urn:wip> TO GRAPH <urn:release>",
        "LOAD <http://127.0.0.1:9/data.ttl> INTO GRAPH <urn:release>",
        "CLEAR GRAPH <urn:wip> ; DROP GRAPH <urn:release>",
    ] {
        assert_eq!(update("editor", refused), 403, "{refused}");
    }
    let delete =
        |key: &str, target: &str| match ureq::delete(&format!("{base}/assets/data{target}"))
            .set("X-API-Key", key)
            .call()
        {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(code, _)) => code,
            Err(e) => panic!("{e}"),
        };
    assert_eq!(delete("editor", "?graph=urn:release"), 403);
    assert_eq!(delete("editor", "?default"), 403);
    assert!(ask(
        "ASK { GRAPH <urn:release> { <urn:a> <urn:in> \"release\" } }"
    ));
    assert!(ask("ASK { <urn:a> <urn:in> \"default\" }"));
    assert!(ask("ASK { GRAPH <urn:wip> { <urn:b> <urn:in> \"put\" } }"));
    assert!(!ask("ASK { GRAPH <urn:other> { ?s ?p ?o } }"));
    // Copying a readable graph into the writable one replaces what it held.
    assert_eq!(
        update("editor", "COPY GRAPH <urn:release> TO GRAPH <urn:wip>"),
        200
    );
    assert!(ask(
        "ASK { GRAPH <urn:wip> { <urn:a> <urn:in> \"release\" } }"
    ));
    assert!(!ask("ASK { GRAPH <urn:wip> { <urn:b> <urn:in> \"put\" } }"));
    assert_eq!(update("editor", "CLEAR GRAPH <urn:wip>"), 200);
    assert!(!ask("ASK { GRAPH <urn:wip> { ?s ?p ?o } }"));
    assert_eq!(update("editor", "DROP SILENT GRAPH <urn:wip>"), 200);
    assert_eq!(put("editor", "?graph=urn:wip"), 201);
    assert_eq!(delete("editor", "?graph=urn:wip"), 204);
    assert!(ask(
        "ASK { GRAPH <urn:release> { <urn:a> <urn:in> \"release\" } }"
    ));

    // The team reads every graph, and writes only the release graph.
    assert_eq!(
        update(
            "team",
            "INSERT DATA { GRAPH <urn:release> { <urn:t> <urn:in> 1 } }"
        ),
        200
    );
    assert_eq!(
        update(
            "team",
            "INSERT DATA { GRAPH <urn:wip> { <urn:t> <urn:in> 1 } }"
        ),
        403
    );
    assert_eq!(put("team", "?graph=urn:release"), 204);
    assert_eq!(put("team", "?graph=urn:wip"), 403);
}

/// A policy swapped while updates run (enterprise #9): each update is judged under one
/// version of the policy, so it is applied whole or refused whole, never in part. Each update
/// writes two graphs; one version of the policy lets the editor write both, the other only
/// the first.
#[test]
fn updates_during_a_policy_swap_are_applied_whole_or_not_at_all() {
    let dir = tripletrail_testdir::tempdir("auth-policy-swap");
    let policy = |both: bool| {
        let second = if both {
            " ; sg:writeGraph <urn:wip2>"
        } else {
            ""
        };
        format!(
            "{SG} ex:Editors a sg:Policy ; sg:group \"editors\" ; sg:writeGraph <urn:wip>{second} .
             ex:Team a sg:Policy ; sg:group \"team\" ; sg:readAllGraphs true ."
        )
    };
    std::fs::write(dir.join("kg.ttl"), policy(true)).unwrap();
    let key = |k: &str, group: &str| {
        format!(
            r#"{{"sha256": "{}", "agent": "https://example.org/people/{k}", "groups": ["{group}"]}}"#,
            hash(k)
        )
    };
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"apiKeys": [{}, {}]}}, "datasets": {{"kg": {{"policies": ["kg.ttl"]}}}}}}"#,
            key("editor", "editors"),
            key("team", "team"),
        ),
    )
    .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), Engine::new_tripletrail())],
        Options {
            threads: 4,
            auth_config: Some(dir.join("guard.json")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writers: Vec<_> = (0..2)
        .map(|w| {
            let (base, stop) = (base.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut answers = Vec::new();
                let mut i = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let n = w * 1_000_000 + i;
                    let text = format!(
                        "INSERT DATA {{ GRAPH <urn:wip> {{ <urn:n> <urn:v> {n} }} }} ; \
                         INSERT DATA {{ GRAPH <urn:wip2> {{ <urn:n> <urn:v> {n} }} }}"
                    );
                    let r = ureq::post(&format!("{base}/kg/update")).set("X-API-Key", "editor");
                    answers.push((n, status(r, Some(&text)).0));
                    i += 1;
                }
                answers
            })
        })
        .collect();
    // Four swaps, each picked up after the file's time changes.
    for round in 0..4 {
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(dir.join("kg.ttl"), policy(round % 2 == 1)).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(1100));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let answers: Vec<(usize, u16)> = writers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();

    let values = |graph: &str| -> std::collections::HashSet<usize> {
        let body = ureq::get(&format!("{base}/kg/query"))
            .set("X-API-Key", "team")
            .set("Accept", "text/csv")
            .query(
                "query",
                &format!("SELECT ?v {{ GRAPH <{graph}> {{ <urn:n> <urn:v> ?v }} }}"),
            )
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        body.lines()
            .skip(1)
            .map(|l| l.trim().parse().unwrap())
            .collect()
    };
    let (wip, wip2) = (values("urn:wip"), values("urn:wip2"));
    let applied: std::collections::HashSet<usize> = answers
        .iter()
        .filter(|(_, code)| *code == 200)
        .map(|(n, _)| *n)
        .collect();
    let refused = answers.iter().filter(|(_, code)| *code == 403).count();
    assert_eq!(applied.len() + refused, answers.len(), "{answers:?}");
    // Both versions of the policy were in force while the updates ran.
    assert!(
        !applied.is_empty() && refused > 0,
        "{} applied, {refused} refused",
        applied.len()
    );
    assert_eq!(wip, applied);
    assert_eq!(wip2, applied);
}

/// OGC API Features and tiles keep to a caller's graphs (#203): a class only in other graphs
/// is no collection for it, and features, extents and tiles come from its graphs only.
#[test]
fn ogc_api_and_tiles_keep_to_a_graph_grant() {
    let dir = tripletrail_testdir::tempdir("auth-ogc-grants");
    std::fs::write(
        dir.join("assets.ttl"),
        format!(
            "{SG} ex:Contractors a sg:Policy ; sg:group \"contractor-x\" ; sg:readGraph <urn:release> .
             ex:Team a sg:Policy ; sg:group \"team\" ; sg:readAllGraphs true ."
        ),
    )
    .unwrap();
    let key = |k: &str, group: &str| {
        format!(
            r#"{{"sha256": "{}", "agent": "https://example.org/people/{k}", "groups": ["{group}"]}}"#,
            hash(k)
        )
    };
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"apiKeys": [{}, {}]}}, "datasets": {{"assets": {{"policies": ["assets.ttl"]}}}}}}"#,
            key("contractor", "contractor-x"),
            key("team", "team"),
        ),
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::TriG,
            &br#"@prefix ex: <http://example.com/> .
               @prefix geo: <http://www.opengis.net/ont/geosparql#> .
               <urn:release> { ex:s1 a ex:Station ; geo:hasGeometry [ geo:asWKT "POINT(4.35 50.85)"^^geo:wktLiteral ] }
               <urn:wip> { ex:s2 a ex:Station ; ex:secretCode "X" ; geo:hasGeometry [ geo:asWKT "POINT(-70 -30)"^^geo:wktLiteral ] .
                           ex:l1 a ex:Line ; geo:hasGeometry [ geo:asWKT "LINESTRING(4.3 50.8, 4.4 50.9)"^^geo:wktLiteral ] }"#[..],
        )
        .unwrap();
    // The team reads every graph as the default graph.
    engine.set_union_default_graph(true);
    let server = Server::bind(
        "127.0.0.1:0",
        [("assets".to_owned(), engine)],
        Options {
            threads: 2,
            auth_config: Some(dir.join("guard.json")),
            // In the test's folder: a server with no location would make its workspaces in
            // the system's temporary folder, removed only when the server stops.
            workspaces: WorkspaceOptions {
                dir: Some(dir.join("workspaces")),
                ..WorkspaceOptions::default()
            },
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let get = |path: &str, key: &str| -> Result<Vec<u8>, u16> {
        match ureq::get(&format!("{base}{path}"))
            .set("X-API-Key", key)
            .set("Accept", "application/json")
            .call()
        {
            Ok(r) => {
                let mut body = Vec::new();
                std::io::Read::read_to_end(&mut r.into_reader(), &mut body).unwrap();
                Ok(body)
            }
            Err(ureq::Error::Status(code, _)) => Err(code),
            Err(e) => panic!("{e}"),
        }
    };
    let text = |path: &str, key: &str| String::from_utf8(get(path, key).unwrap()).unwrap();
    let holds = |body: &[u8], s: &str| body.windows(s.len()).any(|w| w == s.as_bytes());

    // The team sees both classes; the contractor only the stations, and only its own.
    let all = text("/assets/ogc/collections", "team");
    assert!(all.contains("Station") && all.contains("Line"), "{all}");
    let some = text("/assets/ogc/collections", "contractor");
    assert!(some.contains("Station") && !some.contains("Line"), "{some}");
    assert!(!text("/assets/ogc/api", "contractor").contains("Line"));
    assert_eq!(get("/assets/ogc/collections/Line", "contractor"), Err(404));
    assert_eq!(
        get("/assets/ogc/collections/Line/items", "contractor"),
        Err(404)
    );
    let items = text("/assets/ogc/collections/Station/items", "contractor");
    assert!(items.contains("example.com/s1") && !items.contains("example.com/s2"));
    assert!(text("/assets/ogc/collections/Station/items", "team").contains("example.com/s2"));
    let s2 = "/assets/ogc/collections/Station/items/http%3A%2F%2Fexample.com%2Fs2";
    assert_eq!(get(s2, "contractor"), Err(404));
    assert!(get(s2, "team").is_ok());

    // A property only in the hidden graph is no queryable, filter or API parameter of the
    // contractor's: its shapes come from its own graph (#203).
    let queryables = "/assets/ogc/collections/Station/queryables";
    assert!(text(queryables, "team").contains("secretCode"));
    assert!(!text(queryables, "contractor").contains("secretCode"));
    assert!(!text("/assets/ogc/api", "contractor").contains("secretCode"));
    assert!(!text("/assets/ogc/collections/Station", "contractor").contains("secretCode"));
    assert_eq!(
        get(
            "/assets/ogc/collections/Station/items?secretCode=X",
            "contractor"
        ),
        get(
            "/assets/ogc/collections/Station/items?unheardOf=X",
            "contractor"
        ),
    );

    // A workspace sample keeps to the grant; an overlay, which would share every graph, is
    // refused (#203).
    let post = |path: &str, body: &str| -> (u16, String) {
        match ureq::post(&format!("{base}{path}"))
            .set("X-API-Key", "contractor")
            .set("Content-Type", "application/json")
            .send_string(body)
        {
            Ok(r) => (200, r.into_string().unwrap()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
            Err(e) => panic!("{e}"),
        }
    };
    let (code, made) = post("/$/workspaces", "{}");
    assert_eq!(code, 200, "{made}");
    let id = serde_json::from_str::<serde_json::Value>(&made).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, sampled) = post(
        &format!("/$/workspaces/{id}/sample"),
        r#"{"dataset": "assets", "classes": ["http://example.com/Station"]}"#,
    );
    assert_eq!(code, 200, "{sampled}");
    let sampled: serde_json::Value = serde_json::from_str(&sampled).unwrap();
    assert_eq!(sampled["instances"], 1, "{sampled}");
    let file = text(
        &format!("/$/workspaces/{id}/files/data/sample-assets.nt"),
        "contractor",
    );
    assert!(
        file.contains("example.com/s1") && !file.contains("example.com/s2"),
        "{file}"
    );
    assert!(
        !file.contains("secretCode") && !file.contains("-70"),
        "{file}"
    );
    let (code, refused) = post(
        &format!("/$/workspaces/{id}/overlay"),
        r#"{"dataset": "assets"}"#,
    );
    assert_eq!(code, 403);
    assert!(refused.contains("sample"), "{refused}");

    // Tiles: the contractor's world tile has its station only, and is not the team's.
    let world = "/assets/tiles/all/0/0/0.mvt";
    let team = get(world, "team").unwrap();
    assert!(holds(&team, "http://example.com/s2") && holds(&team, "Line"));
    let mine = get(world, "contractor").unwrap();
    assert!(holds(&mine, "http://example.com/s1"));
    assert!(!holds(&mine, "http://example.com/s2") && !holds(&mine, "Line"));
    // Asked again (from the cache), the same.
    assert_eq!(get(world, "contractor").unwrap(), mine);
    assert_eq!(get("/assets/tiles/Line/0/0/0.mvt", "contractor"), Err(404));
}

/// Row rules (#202): contractors see only the projects of their own area, without phone
/// numbers; reads only; the team (no row rules) and admins see everything.
// Row-level policies are an enterprise feature (#818).
#[cfg(feature = "enterprise")]
#[test]
fn row_rules_hide_rows_from_queries() {
    let dir = tripletrail_testdir::tempdir("auth-row-rules");
    std::fs::write(
        dir.join("assets.ttl"),
        format!(
            "{SG}@prefix sh: <http://www.w3.org/ns/shacl#> .
             ex:Contractors a sg:Policy ; sg:group \"contractor\" ; sg:readAllGraphs true ;
               sg:visibleIf ex:OwnArea ; sg:hideProperties ex:NoPhones .
             ex:Team a sg:Policy ; sg:group \"team\" ; sg:readAllGraphs true .
             ex:OwnArea a sh:NodeShape ; sh:targetClass ex:Project ;
               sh:property [ sh:path ex:area ; sh:hasValue sg:requestingAgent ] .
             ex:NoPhones a sh:NodeShape ; sh:property [ sh:path ex:phone ] ."
        ),
    )
    .unwrap();
    let key = |k: &str, group: &str| {
        format!(
            r#"{{"sha256": "{}", "agent": "http://example.org/{k}", "groups": ["{group}"]}}"#,
            hash(k)
        )
    };
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"apiKeys": [{}, {}, {}, {}]}}, "datasets": {{"assets": {{"policies": ["assets.ttl"], "admins": ["admins"]}}}}}}"#,
            key("north", "contractor"),
            key("team", "team"),
            key("boss", "admins"),
            key("root", "tripletrail:admin"),
        ),
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            &b"@prefix ex: <http://example.org/> .
               ex:p1 a ex:Project ; ex:area ex:north ; ex:name \"p1\" ; ex:phone \"1\" .
               ex:p2 a ex:Project ; ex:area ex:south ; ex:name \"p2\" .
               ex:t1 ex:of ex:p2 ; ex:name \"t1\" ."[..],
        )
        .unwrap();
    let writer = engine.clone();
    let server = Server::bind(
        "127.0.0.1:0",
        [("assets".to_owned(), engine)],
        Options {
            threads: 2,
            auth_config: Some(dir.join("guard.json")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();

    let get = |path: &str, key: &str| -> Result<String, u16> {
        match ureq::get(&format!("{base}{path}"))
            .set("X-API-Key", key)
            .set("Accept", "application/sparql-results+json")
            .call()
        {
            Ok(r) => Ok(r.into_string().unwrap()),
            Err(ureq::Error::Status(code, _)) => Err(code),
            Err(e) => panic!("{e}"),
        }
    };
    let encode = |q: &str| form_urlencoded::byte_serialize(q.as_bytes()).collect::<String>();
    let select = |query: &str, key: &str| {
        let json: serde_json::Value = serde_json::from_str(
            &get(&format!("/assets/query?query={}", encode(query)), key).unwrap(),
        )
        .unwrap();
        let mut values: Vec<String> = json["results"]["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                b["v"]["value"]
                    .as_str()
                    .unwrap()
                    .replace("http://example.org/", "")
            })
            .collect();
        values.sort();
        values
    };
    let names = "SELECT ?v { ?s <http://example.org/name> ?v }";
    let phones = "SELECT ?v { ?s <http://example.org/phone> ?v }";
    let links = "SELECT ?v { ?v <http://example.org/of> ?o }";
    assert_eq!(select(names, "north"), ["p1", "t1"]);
    assert!(select(phones, "north").is_empty());
    assert!(select(links, "north").is_empty());
    let count = "SELECT (COUNT(*) AS ?v) { ?s ?p ?o }";
    assert_eq!(select(count, "north"), ["4"]);
    for key in ["team", "boss"] {
        assert_eq!(select(names, key), ["p1", "p2", "t1"]);
        assert_eq!(select(phones, key), ["1"]);
        assert_eq!(select(count, key), ["9"]);
    }

    // Descriptions keep to what the rules show: a hidden resource is not found, a visible
    // one comes without its hidden properties and links (#202).
    let triples = |iri: &str, key: &str, accept: &str| -> Result<String, u16> {
        match ureq::get(&format!("{base}/assets/describe"))
            .query("iri", &format!("http://example.org/{iri}"))
            .set("X-API-Key", key)
            .set("Accept", accept)
            .call()
        {
            Ok(r) => Ok(r.into_string().unwrap()),
            Err(ureq::Error::Status(code, _)) => Err(code),
            Err(e) => panic!("{e}"),
        }
    };
    let nt = "application/n-triples";
    let p1 = triples("p1", "north", nt).unwrap();
    assert!(p1.contains("\"p1\"") && !p1.contains("phone"), "{p1}");
    assert_eq!(triples("p2", "north", nt), Err(404));
    let t1 = triples("t1", "north", nt).unwrap();
    assert!(!t1.contains("example.org/of"), "{t1}");
    assert!(triples("p2", "team", nt).is_ok());
    assert!(triples("p1", "team", nt).unwrap().contains("phone"));
    // The Graph Store Protocol reads the same view.
    let dump = |key: &str, query: &str, accept: &str| -> String {
        ureq::get(&format!("{base}/assets/data{query}"))
            .set("X-API-Key", key)
            .set("Accept", accept)
            .call()
            .unwrap()
            .into_string()
            .unwrap()
    };
    let lines = |text: String| text.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(lines(dump("north", "", "application/n-quads")), 4);
    assert_eq!(lines(dump("north", "?default", nt)), 4);
    assert_eq!(lines(dump("team", "", "application/n-quads")), 9);
    // So does the RDF4J API: statements, size, contexts and queries, and its writes are
    // refused.
    let rdf4j = |key: &str, path: &str, accept: &str| -> Result<String, u16> {
        match ureq::get(&format!("{base}/repositories/assets{path}"))
            .set("X-API-Key", key)
            .set("Accept", accept)
            .call()
        {
            Ok(r) => Ok(r.into_string().unwrap()),
            Err(ureq::Error::Status(code, _)) => Err(code),
            Err(e) => panic!("{e}"),
        }
    };
    let nq = "application/n-quads";
    assert_eq!(lines(rdf4j("north", "/statements", nq).unwrap()), 4);
    assert_eq!(lines(rdf4j("team", "/statements", nq).unwrap()), 9);
    let mine = rdf4j("north", "/statements?subj=%3Chttp://example.org/p1%3E", nq).unwrap();
    assert!(mine.contains("\"p1\"") && !mine.contains("phone"), "{mine}");
    assert_eq!(
        lines(rdf4j("north", "/statements?subj=%3Chttp://example.org/p2%3E", nq).unwrap()),
        0
    );
    assert_eq!(rdf4j("north", "/size", "text/plain").unwrap(), "4");
    assert_eq!(rdf4j("team", "/size", "text/plain").unwrap(), "9");
    let json = "application/sparql-results+json";
    let rdf4j_names = rdf4j("north", &format!("?query={}", encode(names)), json).unwrap();
    assert!(
        rdf4j_names.contains("\"p1\"") && !rdf4j_names.contains("\"p2\""),
        "{rdf4j_names}"
    );
    let removed = ureq::delete(&format!("{base}/repositories/assets/statements"))
        .set("X-API-Key", "north")
        .call();
    assert!(matches!(removed, Err(ureq::Error::Status(403, _))));
    assert_eq!(lines(rdf4j("team", "/statements", nq).unwrap()), 9);
    // A named graph with nothing visible in it is no context of the caller's, and counts 0.
    let quads = "PREFIX ex: <http://example.org/> INSERT DATA { \
                 GRAPH <urn:g:south> { ex:p2 ex:name \"p2b\" } \
                 GRAPH <urn:g:north> { ex:p1 ex:name \"p1b\" } }";
    writer.update(quads).unwrap();
    let contexts = |key: &str| rdf4j(key, "/contexts", json).unwrap();
    assert!(contexts("north").contains("urn:g:north"));
    assert!(!contexts("north").contains("urn:g:south"));
    assert!(contexts("team").contains("urn:g:south"));
    let size = |key: &str, query: &str| rdf4j(key, &format!("/size{query}"), "text/plain").unwrap();
    assert_eq!(size("north", "?context=%3Curn:g:south%3E"), "0");
    assert_eq!(size("team", "?context=%3Curn:g:south%3E"), "1");
    assert_eq!(
        size(
            "team",
            "?context=%3Curn:g:south%3E&context=%3Curn:g:south%3E"
        ),
        "1"
    );
    assert_eq!(
        size(
            "north",
            "?context=%3Curn:g:north%3E&context=%3Curn:g:south%3E"
        ),
        "1"
    );
    assert_eq!(size("north", ""), "5");
    writer
        .update(&quads.replace("INSERT DATA", "DELETE DATA"))
        .unwrap();
    // So does the describe page and its view model, shapes included.
    let html = "text/html";
    let page = triples("p1", "north", html).unwrap();
    assert!(page.contains("p1") && !page.contains("phone"), "{page}");
    assert_eq!(triples("p2", "north", html), Err(404));
    assert!(
        !triples("t1", "north", html)
            .unwrap()
            .contains("example.org/p2")
    );
    assert!(triples("p1", "team", html).unwrap().contains("phone"));
    let model = ureq::get(&format!("{base}/assets/describe"))
        .query("iri", "http://example.org/p1")
        .query("format", "view")
        .set("X-API-Key", "north")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(model.contains("p1") && !model.contains("phone"), "{model}");
    // The service description is refused under row rules.
    assert_eq!(get("/assets/query", "north"), Err(403));
    // Explain gives the row count alone, of the query as the caller would run it.
    let explained = |key: &str| get(&format!("/assets/explain?query={}", encode(names)), key);
    assert_eq!(
        explained("north").unwrap(),
        r#"{"rows":2,"restricted":true}"#
    );
    assert!(!explained("team").unwrap().contains("restricted"));
    let post = |key: &str| {
        ureq::post(&format!("{base}/assets/update"))
            .set("X-API-Key", key)
            .set("Content-Type", "application/sparql-update")
            .send_string("INSERT DATA { <urn:a> <urn:b> <urn:c> }")
            .map_or_else(
                |e| match e {
                    ureq::Error::Status(code, _) => code,
                    e => panic!("{e}"),
                },
                |r| r.status(),
            )
    };
    // The contractors may read, not write.
    assert_eq!(post("north"), 403);

    // The access explain page tells a server admin what a caller sees and why.
    let explain = |key: &str, query: &str| -> Result<serde_json::Value, u16> {
        match ureq::get(&format!("{base}/$/access/datasets/assets/explain?{query}"))
            .set("X-API-Key", key)
            .call()
        {
            Ok(r) => Ok(serde_json::from_str(&r.into_string().unwrap()).unwrap()),
            Err(ureq::Error::Status(code, _)) => Err(code),
            Err(e) => panic!("{e}"),
        }
    };
    let as_north = "agent=http://example.org/north&groups=contractor";
    assert!(matches!(explain("north", as_north), Err(403)));
    assert!(matches!(explain("boss", as_north), Err(403)));
    let north = explain(
        "root",
        &format!("{as_north}&resource=http://example.org/p2"),
    )
    .unwrap();
    assert_eq!(north["read"], true);
    assert_eq!(
        north["policies"],
        serde_json::json!(["<http://example.org/Contractors>"])
    );
    assert_eq!(north["rowRules"].as_array().unwrap().len(), 2, "{north}");
    assert_eq!(north["resource"]["visible"], false, "{north}");
    assert_eq!(
        north["resource"]["hiddenBy"],
        serde_json::json!(["<http://example.org/OwnArea>"])
    );
    let p1 = explain(
        "root",
        &format!("{as_north}&resource=http://example.org/p1"),
    )
    .unwrap();
    assert_eq!(p1["resource"]["visible"], true, "{p1}");
    assert_eq!(
        p1["resource"]["hiddenProperties"],
        serde_json::json!(["http://example.org/phone"])
    );
    let team = explain(
        "root",
        "agent=http://example.org/team&groups=team&resource=http://example.org/p2",
    )
    .unwrap();
    assert_eq!(team["rowRules"], serde_json::json!([]));
    assert_eq!(team["resource"]["visible"], true, "{team}");
    // And for a query, which of its patterns the rules restrict.
    let query = form_urlencoded::byte_serialize(
        b"PREFIX ex: <http://example.org/> SELECT * { ?p a ex:Project ; ex:phone ?f OPTIONAL { ?t ex:of ?p } }",
    )
    .collect::<String>();
    let patterns = explain("root", &format!("{as_north}&query={query}")).unwrap();
    let patterns = patterns["query"].as_array().unwrap().clone();
    let by = |p: &str| {
        patterns
            .iter()
            .find(|x| x["pattern"].as_str().unwrap().contains(p))
            .unwrap_or_else(|| panic!("{p}: {patterns:?}"))
            .clone()
    };
    assert_eq!(by("rdf-syntax-ns#type")["matches"], 2);
    assert_eq!(by("rdf-syntax-ns#type")["visible"], 1);
    assert_eq!(by("phone")["visible"], 0);
    assert_eq!(by("phone")["restricted"], true);
    assert_eq!(by("example.org/of")["matches"], 1);
    assert_eq!(by("example.org/of")["visible"], 0);
    let nobody = explain("root", "resource=http://example.org/p1").unwrap();
    assert_eq!(nobody["read"], false);
    assert_eq!(nobody["resource"]["visible"], false);
    assert_eq!(nobody["resource"]["exists"], true);
    assert_eq!(nobody["resource"]["readable"], false);
    assert_eq!(north["resource"]["exists"], true);
    assert_eq!(north["resource"]["readable"], true);
    let unknown = explain("root", &format!("{as_north}&resource=urn:nothing")).unwrap();
    assert_eq!(unknown["resource"]["exists"], false);

    // A write changes what the rules hide.
    writer
        .update("PREFIX ex: <http://example.org/> DELETE DATA { ex:p2 ex:area ex:south } ; INSERT DATA { ex:p2 ex:area ex:north }")
        .unwrap();
    assert_eq!(select(names, "north"), ["p1", "p2", "t1"]);

    // Edited policies apply as soon as they are reloaded, with no write in between: a filter
    // compiled from the policies before is not used for them.
    for (round, area) in ["south", "north", "south", "north"].iter().enumerate() {
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(
            dir.join("assets.ttl"),
            format!(
                "{SG}@prefix sh: <http://www.w3.org/ns/shacl#> .
                 ex:Contractors a sg:Policy ; sg:group \"contractor\" ; sg:readAllGraphs true ;
                   sg:visibleIf ex:Area .
                 ex:Area a sh:NodeShape ; sh:targetClass ex:Project ;
                   sh:property [ sh:path ex:area ; sh:hasValue ex:{area} ] ."
            ),
        )
        .unwrap();
        let expected: &[&str] = if *area == "south" {
            &["t1"]
        } else {
            &["p1", "p2", "t1"]
        };
        let mut seen = Vec::new();
        for _ in 0..50 {
            seen = select(names, "north");
            if seen == expected {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(seen, expected, "round {round}");
    }
}

/// OGC API Features and tiles under row rules (#202): a contractor gets the same answers as
/// from a server that holds only what the rules let it see, and the team sees everything.
// Row-level policies are an enterprise feature (#818).
#[cfg(feature = "enterprise")]
#[test]
fn ogc_api_and_tiles_keep_to_row_rules() {
    let dir = tripletrail_testdir::tempdir("auth-ogc-rows");
    std::fs::write(
        dir.join("assets.ttl"),
        format!(
            "{SG}@prefix sh: <http://www.w3.org/ns/shacl#> .
             ex:Contractors a sg:Policy ; sg:group \"contractor\" ; sg:readAllGraphs true ;
               sg:visibleIf ex:OwnArea ; sg:hideProperties ex:NoCodes .
             ex:Team a sg:Policy ; sg:group \"team\" ; sg:readAllGraphs true .
             ex:OwnArea a sh:NodeShape ; sh:targetClass ex:Station, ex:Line ;
               sh:property [ sh:path ex:area ; sh:hasValue sg:requestingAgent ] .
             ex:NoCodes a sh:NodeShape ; sh:property [ sh:path ex:code ] ."
        ),
    )
    .unwrap();
    let key = |k: &str, group: &str| {
        format!(
            r#"{{"sha256": "{}", "agent": "http://example.org/{k}", "groups": ["{group}"]}}"#,
            hash(k)
        )
    };
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"apiKeys": [{}, {}]}}, "datasets": {{"assets": {{"policies": ["assets.ttl"]}}}}}}"#,
            key("north", "contractor"),
            key("team", "team"),
        ),
    )
    .unwrap();
    let prefixes = "@prefix ex: <http://example.org/> .
        @prefix geo: <http://www.opengis.net/ont/geosparql#> .\n";
    let visible = "ex:s1 a ex:Station ; ex:area ex:north ; ex:name \"s1\" ; geo:hasGeometry ex:g1 .
        ex:g1 geo:asWKT \"POINT(4.35 50.85)\"^^geo:wktLiteral .
        ex:g2 geo:asWKT \"POINT(-70 -30)\"^^geo:wktLiteral .
        ex:g3 geo:asWKT \"LINESTRING(4.3 50.8, 4.4 50.9)\"^^geo:wktLiteral .\n";
    let hidden = "ex:s1 ex:code \"A\" .
        ex:s2 a ex:Station ; ex:area ex:south ; ex:name \"s2\" ; ex:code \"X\" ; geo:hasGeometry ex:g2 .
        ex:l1 a ex:Line ; ex:area ex:south ; geo:hasGeometry ex:g3 .\n";
    let serve = |ttl: String, auth: bool| {
        let engine = Engine::new_tripletrail();
        engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
        let server = Server::bind(
            "127.0.0.1:0",
            [("assets".to_owned(), engine)],
            Options {
                threads: 2,
                auth_config: auth.then(|| dir.join("guard.json")),
                ..Options::default()
            },
        )
        .unwrap();
        let base = format!("http://{}", server.address());
        server.spawn();
        base
    };
    let full = serve(format!("{prefixes}{visible}{hidden}"), true);
    let only = serve(format!("{prefixes}{visible}"), false);
    let get = |base: &str, path: &str, key: &str| -> (u16, Vec<u8>) {
        match ureq::get(&format!("{base}{path}"))
            .set("X-API-Key", key)
            .set("Accept", "application/json")
            .call()
        {
            Ok(r) => {
                let status = r.status();
                let mut body = Vec::new();
                std::io::Read::read_to_end(&mut r.into_reader(), &mut body).unwrap();
                (status, body)
            }
            Err(ureq::Error::Status(code, _)) => (code, Vec::new()),
            Err(e) => panic!("{e}"),
        }
    };
    let holds = |body: &[u8], s: &str| body.windows(s.len()).any(|w| w == s.as_bytes());

    // The team sees both classes and the codes; the contractor its own station only.
    let (_, all) = get(&full, "/assets/ogc/collections", "team");
    assert!(holds(&all, "Station") && holds(&all, "Line"));
    assert!(holds(
        &get(&full, "/assets/ogc/collections/Station/queryables", "team").1,
        "code"
    ));
    assert!(holds(
        &get(&full, "/assets/tiles/all/0/0/0.mvt", "team").1,
        "http://example.org/l1"
    ));
    let paths = [
        "/assets/ogc",
        "/assets/ogc/api",
        "/assets/ogc/collections",
        "/assets/ogc/collections/Station",
        "/assets/ogc/collections/Line",
        "/assets/ogc/collections/Station/queryables",
        "/assets/ogc/collections/Station/items?limit=100",
        "/assets/ogc/collections/Station/items?bbox=-80,-40,10,60",
        "/assets/ogc/collections/Station/items?code=X",
        "/assets/ogc/collections/Station/items?name=s2",
        "/assets/ogc/collections/Station/items?filter=name%3D's2'",
        "/assets/ogc/collections/Station/items/http%3A%2F%2Fexample.org%2Fs1",
        "/assets/ogc/collections/Station/items/http%3A%2F%2Fexample.org%2Fs2",
        "/assets/ogc/collections/Line/items",
        "/assets/tiles/all/0/0/0.mvt",
        "/assets/tiles/Station/0/0/0.mvt",
        "/assets/tiles/Line/0/0/0.mvt",
        "/assets/tiles/all/5/16/10.mvt",
    ];
    // The port, and the item lists' `timeStamp`, which may fall in different seconds.
    let normal = |base: &str, body: &[u8]| {
        let text = String::from_utf8_lossy(body).replace(host(base), "HOST");
        match text.find("\"timeStamp\":\"") {
            Some(at) => {
                let end = at + text[at..].find("Z\"").map_or(0, |e| e + 2);
                format!("{}\"timeStamp\":\"T\"{}", &text[..at], &text[end..])
            }
            None => text,
        }
    };
    // Twice: the second time from the caches, with the extents filled.
    for round in 0..2 {
        for path in paths {
            let (a, b) = (get(&full, path, "north"), get(&only, path, "north"));
            assert_eq!(a.0, b.0, "{path} (round {round})");
            if round == 1 {
                assert_eq!(normal(&full, &a.1), normal(&only, &b.1), "{path}");
            }
        }
    }
    let (_, mine) = get(
        &full,
        "/assets/ogc/collections/Station/items?limit=100",
        "north",
    );
    assert!(holds(&mine, "http://example.org/s1") && !holds(&mine, "\"X\""));
    // Writes are not taken.
    let post = ureq::post(&format!("{full}/assets/ogc/collections"))
        .set("X-API-Key", "north")
        .call();
    assert!(matches!(post, Err(ureq::Error::Status(405 | 403, _))));
}

/// The describe page, its view model and explain under row rules (#202): the same answers as
/// from a server holding only the rows the caller may see.
// Row-level policies are an enterprise feature (#818).
#[cfg(feature = "enterprise")]
#[test]
fn describe_pages_and_explain_keep_to_row_rules() {
    let dir = tripletrail_testdir::tempdir("auth-describe-rows");
    std::fs::write(
        dir.join("assets.ttl"),
        format!(
            "{SG}@prefix sh: <http://www.w3.org/ns/shacl#> .
             ex:Contractors a sg:Policy ; sg:group \"contractor\" ; sg:readAllGraphs true ;
               sg:visibleIf ex:OwnArea ; sg:hideProperties ex:NoPhones .
             ex:OwnArea a sh:NodeShape ; sh:targetClass ex:Project ;
               sh:property [ sh:path ex:area ; sh:hasValue sg:requestingAgent ] .
             ex:NoPhones a sh:NodeShape ; sh:property [ sh:path ex:phone ] ."
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"apiKeys": [{{"sha256": "{}", "agent": "http://example.org/north", "groups": ["contractor"]}}]}}, "datasets": {{"assets": {{"policies": ["assets.ttl"]}}}}}}"#,
            hash("north")
        ),
    )
    .unwrap();
    let prefixes = "@prefix ex: <http://example.org/> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n";
    let visible = "ex:p1 a ex:Project ; ex:area ex:north ; rdfs:label \"P one\" ; ex:lead ex:ann .
        ex:ann rdfs:label \"Ann\" ; ex:knows ex:bob .
        ex:t1 rdfs:label \"t1\" .
        ex:Project rdfs:label \"Project\" .\n";
    let hidden = "ex:p1 ex:phone \"123\" .
        ex:ann ex:phone \"456\" .
        ex:p2 a ex:Project ; ex:area ex:south ; rdfs:label \"P two\" ; ex:lead ex:ann ; ex:budget 9 .
        ex:t1 ex:of ex:p2 .\n";
    let serve = |ttl: String, auth: bool| {
        let engine = Engine::new_tripletrail();
        engine.load(RdfFormat::Turtle, ttl.as_bytes()).unwrap();
        let server = Server::bind(
            "127.0.0.1:0",
            [("assets".to_owned(), engine)],
            Options {
                threads: 2,
                auth_config: auth.then(|| dir.join("guard.json")),
                ..Options::default()
            },
        )
        .unwrap();
        let base = format!("http://{}", server.address());
        server.spawn();
        base
    };
    let full = serve(format!("{prefixes}{visible}{hidden}"), true);
    let only = serve(format!("{prefixes}{visible}"), false);
    let get = |base: &str, path: &str, accept: &str| -> (u16, String) {
        match ureq::get(&format!("{base}{path}"))
            .set("X-API-Key", "north")
            .set("Accept", accept)
            .call()
        {
            Ok(r) => (r.status(), r.into_string().unwrap()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
            Err(e) => panic!("{e}"),
        }
    };
    let encode = |q: &str| form_urlencoded::byte_serialize(q.as_bytes()).collect::<String>();
    let mut paths = Vec::new();
    for iri in ["p1", "p2", "ann", "bob", "t1", "Project", "north", "south"] {
        let iri = encode(&format!("http://example.org/{iri}"));
        paths.push((format!("/assets/describe?iri={iri}"), "text/html"));
        paths.push((format!("/assets/describe?iri={iri}&format=view"), "*/*"));
    }
    for q in [
        "SELECT * { ?s ?p ?o }",
        "SELECT ?s { ?s a <http://example.org/Project> }",
        "ASK { ?s <http://example.org/phone> ?o }",
        "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p <http://example.org/ann> }",
    ] {
        paths.push((format!("/assets/explain?query={}", encode(q)), "*/*"));
    }
    // The resources nearest a vector, also with the parameters as a form (the Search page's
    // way): only the rows the caller may see. On servers of their own, with the same dataset
    // name and more data, started and asked before the pages below are compared: nothing of
    // theirs reaches the other servers' pages (#50; their data model's order once differed,
    // fixed by #58).
    let vectors = "ex:p1 ex:emb \"[1, 0]\" . ex:p2 ex:emb \"[1, 0.1]\" .\n";
    let full_v = serve(format!("{prefixes}{visible}{hidden}{vectors}"), true);
    let only_v = serve(
        format!("{prefixes}{visible}ex:p1 ex:emb \"[1, 0]\" .\n"),
        false,
    );
    let emb = encode("http://example.org/emb");
    for form in [
        format!("property={emb}&vector=%5B1,0.1%5D"),
        format!("property={emb}&iri={}", encode("http://example.org/p2")),
    ] {
        let post = |base: &str| match ureq::post(&format!("{base}/assets/search/similar"))
            .set("X-API-Key", "north")
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(&form)
        {
            Ok(r) => (r.status(), r.into_string().unwrap()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
            Err(e) => panic!("{e}"),
        };
        let (a, b) = (post(&full_v), post(&only_v));
        assert_eq!((a.0, b.0), (200, 200), "{form}: {}", a.1);
        assert_eq!(
            a.1.replace(host(&full_v), "HOST"),
            b.1.replace(host(&only_v), "HOST"),
            "{form}"
        );
        let get = |base: &str| get(base, &format!("/assets/search/similar?{form}"), "*/*");
        assert_eq!(get(&full_v), a, "{form}");
    }
    for base in [&full, &only] {
        let (_, page) = get(base, &paths[0].0, "text/html");
        assert!(!page.contains("http://example.org/emb"), "{page}");
    }
    for (path, accept) in &paths {
        let (a, b) = (get(&full, path, accept), get(&only, path, accept));
        assert_eq!(a.0, b.0, "{path}");
        if path.starts_with("/assets/explain") {
            // The restricted explain gives the row count alone; the other one, the plan too.
            let rows =
                |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap()["rows"].clone();
            assert!(a.1.contains("\"restricted\":true"), "{}", a.1);
            assert_eq!(rows(&a.1), rows(&b.1), "{path}");
            continue;
        }
        assert_eq!(
            a.1.replace(host(&full), "HOST"),
            b.1.replace(host(&only), "HOST"),
            "{path}"
        );
    }
    let (_, page) = get(&full, &paths[0].0, "text/html");
    assert!(page.contains("P one") && !page.contains("123"), "{page}");
}

/// Compiling row rules for a description or a Graph Store read keeps to the query timeout,
/// as for a query (#202).
// Row-level policies are an enterprise feature (#818).
#[cfg(feature = "enterprise")]
#[test]
fn row_rules_compile_within_the_query_timeout() {
    let dir = tripletrail_testdir::tempdir("auth-row-rules-timeout");
    std::fs::write(
        dir.join("assets.ttl"),
        format!(
            "{SG}@prefix sh: <http://www.w3.org/ns/shacl#> .
             ex:Contractors a sg:Policy ; sg:group \"contractor\" ; sg:readAllGraphs true ;
               sg:visibleIf ex:Named .
             ex:Named a sh:NodeShape ; sh:targetClass ex:Project ;
               sh:property [ sh:path ( ex:part ex:name ) ; sh:minCount 1 ; sh:pattern \"^p\" ] ."
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"apiKeys": [{{"sha256": "{}", "agent": "http://example.org/north", "groups": ["contractor"]}}]}}, "datasets": {{"assets": {{"policies": ["assets.ttl"]}}}}}}"#,
            hash("north")
        ),
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    let mut data = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..40_000 {
        data.push_str(&format!(
            "ex:p{i} a ex:Project ; ex:part ex:q{i} . ex:q{i} ex:name \"p{i}\" .\n"
        ));
    }
    engine.load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let start = |timeout: Option<std::time::Duration>| {
        let server = Server::bind(
            "127.0.0.1:0",
            [("assets".to_owned(), engine.clone())],
            Options {
                threads: 2,
                auth_config: Some(dir.join("guard.json")),
                timeout,
                ..Options::default()
            },
        )
        .unwrap();
        let base = format!("http://{}", server.address());
        server.spawn();
        base
    };
    let status = |base: &str, path: &str| match ureq::get(&format!("{base}{path}"))
        .set("X-API-Key", "north")
        .set("Accept", "application/n-triples")
        .call()
    {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    };
    let describe = "/assets/describe?iri=http%3A%2F%2Fexample.org%2Fp1";
    let open = start(None);
    assert_eq!(status(&open, describe), 200);
    // A timeout far shorter than the compile: neither read waits for it to finish.
    engine
        .update("INSERT DATA { <urn:x> <urn:y> <urn:z> }")
        .unwrap();
    let short = start(Some(std::time::Duration::from_micros(1)));
    assert_ne!(status(&short, describe), 200);
    assert_ne!(status(&short, "/assets/data?default"), 200);
}

/// Writes under row rules (#202): a caller who may write and has row rules writes only into
/// what it sees, and the WHERE of an update matches only that.
// Row-level policies are an enterprise feature (#818).
#[cfg(feature = "enterprise")]
#[test]
fn row_rules_bound_updates() {
    let dir = tripletrail_testdir::tempdir("auth-row-writes");
    std::fs::write(
        dir.join("assets.ttl"),
        format!(
            "{SG}@prefix sh: <http://www.w3.org/ns/shacl#> .
             ex:Contractors a sg:Policy ; sg:group \"contractor\" ; sg:readAllGraphs true ;
               sg:writeAllGraphs true ; sg:visibleIf ex:OwnArea .
             ex:OwnArea a sh:NodeShape ; sh:targetClass ex:Project ;
               sh:property [ sh:path ex:area ; sh:hasValue sg:requestingAgent ] ."
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"apiKeys": [{{"sha256": "{}", "agent": "http://example.org/north", "groups": ["contractor"]}}]}}, "datasets": {{"assets": {{"policies": ["assets.ttl"]}}}}}}"#,
            hash("north")
        ),
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            &b"@prefix ex: <http://example.org/> .
               ex:p1 a ex:Project ; ex:area ex:north ; ex:name \"p1\" .
               ex:p2 a ex:Project ; ex:area ex:south ; ex:name \"p2\" ."[..],
        )
        .unwrap();
    let reader = engine.clone();
    let server = Server::bind(
        "127.0.0.1:0",
        [("assets".to_owned(), engine)],
        Options {
            threads: 2,
            auth_config: Some(dir.join("guard.json")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let post = |update: &str| {
        ureq::post(&format!("{base}/assets/update"))
            .set("X-API-Key", "north")
            .set("Content-Type", "application/sparql-update")
            .send_string(&format!("PREFIX ex: <http://example.org/> {update}"))
            .map_or_else(
                |e| match e {
                    ureq::Error::Status(code, _) => code,
                    e => panic!("{e}"),
                },
                |r| r.status(),
            )
    };
    let names = || {
        let tripletrail::oxigraph::sparql::QueryResults::Solutions(s) = reader
            .query("SELECT ?v { ?s <http://example.org/name> ?v }")
            .unwrap()
        else {
            panic!("not solutions")
        };
        let mut v: Vec<String> = s
            .map(|r| r.unwrap().get("v").unwrap().to_string())
            .collect();
        v.sort();
        v
    };
    assert_eq!(post("INSERT DATA { ex:p2 ex:name \"x\" }"), 403);
    assert_eq!(post("DELETE DATA { ex:p1 ex:area ex:north }"), 403);
    assert_eq!(names(), ["\"p1\"", "\"p2\""]);
    assert!((200..300).contains(&post("DELETE { ?s ex:name ?o } WHERE { ?s ex:name ?o }")));
    assert_eq!(names(), ["\"p2\""]);
    assert!((200..300).contains(&post("INSERT DATA { ex:p1 ex:name \"one\" }")));
    assert_eq!(names(), ["\"one\"", "\"p2\""]);

    // The Graph Store Protocol writes the same way: one graph, only what the caller sees.
    let gsp = |method: &str, query: &str, body: &str| {
        ureq::request(method, &format!("{base}/assets/data{query}"))
            .set("X-API-Key", "north")
            .set("Content-Type", "text/turtle")
            .send_string(&format!("@prefix ex: <http://example.org/> . {body}"))
            .map_or_else(
                |e| match e {
                    ureq::Error::Status(code, _) => code,
                    e => panic!("{e}"),
                },
                |r| r.status(),
            )
    };
    let count = || {
        let tripletrail::oxigraph::sparql::QueryResults::Solutions(mut s) = reader
            .query("SELECT (COUNT(*) AS ?n) { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }")
            .unwrap()
        else {
            panic!("not solutions")
        };
        s.next().unwrap().unwrap().get("n").unwrap().to_string()
    };
    let start = count();
    for (method, query, body) in [
        ("POST", "?default", "ex:p2 ex:name \"x\" ."),
        ("POST", "?default", "ex:p2 ex:name \"p2\" ."),
        ("PUT", "?default", "ex:p1 a ex:Project ."),
        ("POST", "", "ex:p1 ex:name \"x\" ."),
    ] {
        assert_eq!(gsp(method, query, body), 403, "{method} {query} {body}");
        assert_eq!(count(), start, "{body}");
    }
    assert_eq!(gsp("DELETE", "?graph=urn:g:none", ""), 404);
    assert_eq!(gsp("POST", "?default", "ex:p1 ex:name \"two\" ."), 204);
    assert_eq!(names(), ["\"one\"", "\"p2\"", "\"two\""]);
    assert_eq!(
        gsp(
            "PUT",
            "?default",
            "ex:p1 a ex:Project ; ex:area ex:north ; ex:name \"new\" ."
        ),
        204
    );
    assert_eq!(names(), ["\"new\"", "\"p2\""]);
    assert_eq!(
        gsp("POST", "?graph=urn:g:north", "ex:p1 ex:name \"g\" ."),
        201
    );
    assert_eq!(gsp("DELETE", "?graph=urn:g:north", ""), 204);
    assert_eq!(gsp("DELETE", "?default", ""), 204);
    assert_eq!(names(), ["\"p2\""]);
    assert_eq!(count(), "\"3\"^^<http://www.w3.org/2001/XMLSchema#integer>");

    // So does the RDF4J API: statements, updates and transactions.
    let rdf4j = |method: &str, path: &str, media: &str, body: &str| {
        let url = if path.starts_with("http") {
            path.to_owned()
        } else {
            format!("{base}/repositories/assets{path}")
        };
        ureq::request(method, &url)
            .set("X-API-Key", "north")
            .set("Content-Type", media)
            .send_string(body)
            .map_or_else(
                |e| match e {
                    ureq::Error::Status(code, r) => (code, r.into_string().unwrap_or_default()),
                    e => panic!("{e}"),
                },
                |r| {
                    let location = r.header("Location").unwrap_or_default().to_owned();
                    (r.status(), location + &r.into_string().unwrap())
                },
            )
            .clone()
    };
    let ttl = "text/turtle";
    let prefix = "@prefix ex: <http://example.org/> . ";
    let code = |r: (u16, String)| r.0;
    assert_eq!(
        code(rdf4j(
            "POST",
            "/statements",
            ttl,
            &format!("{prefix} ex:p1 a ex:Project ; ex:area ex:north ; ex:name \"r\" .")
        )),
        204
    );
    assert_eq!(names(), ["\"p2\"", "\"r\""]);
    let before = count();
    assert_eq!(
        code(rdf4j(
            "POST",
            "/statements",
            ttl,
            &format!("{prefix} ex:p2 ex:name \"p2\" .")
        )),
        403
    );
    assert_eq!(
        code(rdf4j(
            "POST",
            "/statements",
            ttl,
            &format!("{prefix} ex:p2 ex:name \"x\" .")
        )),
        403
    );
    assert_eq!(count(), before);
    assert_eq!(
        code(rdf4j(
            "POST",
            "/statements",
            "application/sparql-update",
            "PREFIX ex: <http://example.org/> DELETE WHERE { ?s ex:name ?o }"
        )),
        204
    );
    assert_eq!(names(), ["\"p2\""]);
    assert_eq!(
        code(rdf4j(
            "PUT",
            "/statements",
            ttl,
            &format!("{prefix} ex:p1 a ex:Project ; ex:area ex:north ; ex:name \"put\" .")
        )),
        204
    );
    assert_eq!(names(), ["\"p2\"", "\"put\""]);
    assert_eq!(
        code(rdf4j(
            "DELETE",
            "/statements?pred=%3Chttp://example.org/name%3E",
            ttl,
            ""
        )),
        204
    );
    assert_eq!(names(), ["\"p2\""]);
    let (status, location) = rdf4j(
        "POST",
        "/transactions",
        "application/x-www-form-urlencoded",
        "",
    );
    assert_eq!(status, 201);
    assert_eq!(
        code(rdf4j(
            "PUT",
            &format!("{location}?action=ADD"),
            ttl,
            &format!("{prefix} ex:p2 ex:name \"y\" .")
        )),
        200
    );
    assert_eq!(
        rdf4j("PUT", &format!("{location}?action=SIZE"), ttl, ""),
        (200, "2".to_owned())
    );
    assert_eq!(
        code(rdf4j("PUT", &format!("{location}?action=COMMIT"), ttl, "")),
        403
    );
    assert_eq!(names(), ["\"p2\""]);
}

/// The `host:port` of a server's base URL, for comparing the answers of two servers: only
/// that is replaced, as the port's digits may also be part of a number or a binary tile.
#[cfg(feature = "enterprise")]
fn host(base: &str) -> &str {
    base.trim_start_matches("http://")
}

/// A shape pack from git names files and URLs of the server, as a git binding does: only
/// server admins install one; a dataset's admins install a pack's files (#148).
#[cfg(feature = "git")]
#[test]
fn only_server_admins_install_a_shape_pack_from_git() {
    let dir = tripletrail_testdir::tempdir("auth-pack-git");
    let key = |k: &str, groups: &str| {
        format!(
            r#"{{"sha256": "{}", "agent": "https://example.org/people/{k}", "groups": [{groups}]}}"#,
            hash(k)
        )
    };
    let config = format!(
        r#"{{"auth": {{"anonymous": false, "apiKeys": [{}, {}]}},
            "datasets": {{"kg": {{"admins": ["kg-admins"]}}}}}}"#,
        key("kgadmin", r#""kg-admins""#),
        key("root", r#""tripletrail:admin""#),
    );
    let path = dir.join("guard.json");
    std::fs::write(&path, config).unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), Engine::new_tripletrail())],
        Options {
            threads: 2,
            auth_config: Some(path.to_path_buf()),
            location: Some(dir.join("data")),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let put = |key: &str, body: &serde_json::Value| -> (u16, String) {
        match ureq::put(&format!("{base}/$/datasets/kg/shapes/pack"))
            .set("X-API-Key", key)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string())
        {
            Ok(r) => (r.status(), r.into_string().unwrap()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
            Err(e) => panic!("{e}"),
        }
    };
    // A token file of the server sent to a host of the caller's choosing.
    let git = serde_json::json!({ "git": {
        "url": "https://git.example.invalid/x.git", "tag": "v1", "user": "u",
        "tokenFile": path.to_str().unwrap(),
    }});
    let (code, text) = put("kgadmin", &git);
    assert_eq!(code, 403, "{text}");
    assert!(!dir.join("data/.shapes/kg/.pack-git").exists());
    // Nor a local repository, or a fetch that probes the server's files.
    let local =
        serde_json::json!({ "git": { "url": format!("file://{}", dir.display()), "tag": "v1" }});
    assert_eq!(put("kgadmin", &local).0, 403);
    // The files form stays with the dataset's admins.
    let files = serde_json::json!({
        "name": "rail", "version": "1.0",
        "files": { "Station.ttl": "@prefix sh: <http://www.w3.org/ns/shacl#> .\n\
            <urn:pack:Station> a sh:NodeShape ; sh:targetClass <urn:Station> ." },
    });
    let (code, text) = put("kgadmin", &files);
    assert_eq!(code, 200, "{text}");
    // A server admin passes the check: here the fetch itself fails.
    let (code, text) = put("root", &local);
    assert_eq!(code, 502, "{text}");
}

/// Row-level policies with a lapsed (or narrower) licence (#818): they stay enforced, and only
/// changing them is refused, with `/$/license` saying why. A licence that covers
/// `row-policies` lets them be changed again.
#[cfg(feature = "enterprise")]
#[test]
fn a_lapsed_licence_keeps_row_rules_enforced_and_turns_off_changing_them() {
    use tripletrail_enterprise::license::{Date, License};
    let dir = tripletrail_testdir::tempdir("auth-row-licence");
    std::fs::write(
        dir.join("kg.ttl"),
        format!(
            "{SG}@prefix sh: <http://www.w3.org/ns/shacl#> .
             ex:Public a sg:Policy ; sg:agent sg:Anyone ; sg:readAllGraphs true ;
               sg:visibleIf ex:PublicDoc .
             ex:PublicDoc a sh:NodeShape ; sh:targetClass ex:Doc ;
               sh:property [ sh:path ex:public ; sh:hasValue true ] ."
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("plain.ttl"),
        format!("{SG} ex:Open a sg:Policy ; sg:agent sg:Anyone ; sg:readAllGraphs true ."),
    )
    .unwrap();
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"anonymous": true, "apiKeys": [
                {{"sha256": "{}", "agent": "https://example.org/people/root", "groups": ["tripletrail:admin"]}}]}},
              "datasets": {{"kg": {{"policies": "kg.ttl"}}, "plain": {{"policies": "plain.ttl"}}}}}}"#,
            hash("root")
        ),
    )
    .unwrap();
    let licence = |expires: Date, features: &[&str]| License {
        id: "2026-042".into(),
        kid: "test".into(),
        customer: "Example Rail AS".into(),
        issued: Date::today().plus_days(-400),
        expires,
        features: features.iter().map(|f| (*f).to_owned()).collect(),
        max_nodes: None,
        max_cores: None,
        note: None,
    };
    let start = |license: License| {
        let engine = |trig: &str| {
            let e = Engine::new_tripletrail();
            e.load(RdfFormat::Turtle, trig.as_bytes()).unwrap();
            e
        };
        let docs = "@prefix ex: <http://example.org/> .
            ex:a a ex:Doc ; ex:public true . ex:b a ex:Doc ; ex:public false .";
        let server = Server::bind(
            "127.0.0.1:0",
            [
                ("kg".to_owned(), engine(docs)),
                ("plain".to_owned(), engine(docs)),
            ],
            Options {
                threads: 2,
                auth_config: Some(dir.join("guard.json")),
                license: Ok(license),
                ..Options::default()
            },
        )
        .unwrap();
        let base = format!("http://{}", server.address());
        server.spawn();
        base
    };
    let docs = |base: &str| -> String {
        let r = ureq::get(&format!("{base}/kg/query"))
            .query(
                "query",
                "SELECT (COUNT(*) AS ?n) { ?d a <http://example.org/Doc> }",
            )
            .set("Accept", "text/csv")
            .call()
            .unwrap();
        r.into_string().unwrap().lines().nth(1).unwrap().to_owned()
    };
    let write = |base: &str, dataset: &str| -> (u16, String) {
        let body = serde_json::json!({
            "id": "http://example.org/Readers",
            "agents": ["https://example.org/people/reader"],
            "readAllGraphs": true,
        });
        match ureq::put(&format!("{base}/$/access/datasets/{dataset}/policies"))
            .set("X-API-Key", "root")
            .send_string(&body.to_string())
        {
            Ok(r) => (r.status(), r.into_string().unwrap()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
            Err(e) => panic!("{e}"),
        }
    };
    let license = |base: &str| -> serde_json::Value {
        let r = ureq::get(&format!("{base}/$/license"))
            .set("X-API-Key", "root")
            .call()
            .unwrap();
        serde_json::from_str(&r.into_string().unwrap()).unwrap()
    };

    // Lapsed (expired past the grace period): still enforced, not changeable.
    let lapsed = start(licence(Date::today().plus_days(-90), &["row-policies"]));
    assert_eq!(docs(&lapsed), "1");
    let (code, text) = write(&lapsed, "kg");
    assert_eq!(code, 403, "{text}");
    assert!(text.contains("row-policies"), "{text}");
    assert_eq!(docs(&lapsed), "1");
    // A file without row rules is changed as before.
    assert_eq!(write(&lapsed, "plain").0, 200);
    // A restore to a version whose configuration no longer names the file with row rules
    // takes them out of force: refused too, and nothing changes.
    let config = std::fs::read_to_string(dir.join("guard.json")).unwrap();
    let without_rows = config.replace(r#""kg": {"policies": "kg.ttl"}, "#, "");
    assert_ne!(config, without_rows);
    let version = serde_json::json!({
        "version": 999, "time": "2026-01-01T00:00:00Z", "by": null, "change": "old",
        "files": [{"path": dir.join("guard.json").display().to_string(), "text": without_rows}],
    });
    std::fs::write(dir.join(".access-history/v999.json"), version.to_string()).unwrap();
    let restored = ureq::post(&format!("{lapsed}/$/access/history/999/restore"))
        .set("X-API-Key", "root")
        .call();
    assert!(
        matches!(restored, Err(ureq::Error::Status(403, _))),
        "{restored:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("guard.json")).unwrap(),
        config
    );
    assert_eq!(docs(&lapsed), "1");
    std::fs::remove_file(dir.join(".access-history/v999.json")).unwrap();
    let rows = &license(&lapsed)["rowPolicies"];
    assert_eq!(
        (&rows["inUse"], &rows["enforced"], &rows["editable"]),
        (
            &serde_json::json!(true),
            &serde_json::json!(true),
            &serde_json::json!(false)
        ),
        "{rows}"
    );
    assert!(
        rows["reason"].as_str().unwrap().contains("row-policies"),
        "{rows}"
    );

    // Valid, but for other features only: the same.
    let narrow = start(licence(Date::today().plus_days(100), &["sso"]));
    assert_eq!(docs(&narrow), "1");
    assert_eq!(write(&narrow, "kg").0, 403);

    // Valid and covering row-policies: changed.
    let covered = start(licence(Date::today().plus_days(100), &["row-policies"]));
    let (code, text) = write(&covered, "kg");
    assert_eq!(code, 200, "{text}");
    assert_eq!(license(&covered)["rowPolicies"]["editable"], true);
    assert_eq!(docs(&covered), "1");
}

/// Single sign-on (#198, TripleTrail Enterprise): a JWT from a trusted issuer, checked against
/// its JWKS (served here on a local port), gives the agent and groups its claims map to.
/// Tokens from another issuer, for another audience, expired or with a bad signature are
/// 401s, and without a licence that covers `sso` every token is.
#[cfg(feature = "enterprise")]
#[test]
fn oidc_tokens_sign_callers_in() {
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    use std::io::{Read, Write};
    use tripletrail_enterprise::license::{Date, License, base64url_encode};

    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let pair =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    let point = pair.public_key().as_ref();
    let jwks = serde_json::json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "kid": "k1",
        "x": base64url_encode(&point[1..33]), "y": base64url_encode(&point[33..65]),
    }]})
    .to_string();
    // The issuer's JWKS endpoint.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{jwks}",
                jwks.len()
            );
        }
    });
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let token = |claims: serde_json::Value| {
        let signed = format!(
            "{}.{}",
            base64url_encode(br#"{"alg":"ES256","typ":"JWT","kid":"k1"}"#),
            base64url_encode(claims.to_string().as_bytes())
        );
        let sig = pair.sign(&rng, signed.as_bytes()).unwrap();
        format!("{signed}.{}", base64url_encode(sig.as_ref()))
    };
    let other = {
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap()
    };
    let signed_by = |key: &EcdsaKeyPair, kid: &str, claims: serde_json::Value| {
        let signed = format!(
            "{}.{}",
            base64url_encode(format!(r#"{{"alg":"ES256","typ":"JWT","kid":"{kid}"}}"#).as_bytes()),
            base64url_encode(claims.to_string().as_bytes())
        );
        let sig = key.sign(&rng, signed.as_bytes()).unwrap();
        format!("{signed}.{}", base64url_encode(sig.as_ref()))
    };
    let unsigned = |alg: &str, claims: serde_json::Value, sig: &str| {
        format!(
            "{}.{}.{sig}",
            base64url_encode(format!(r#"{{"alg":"{alg}","typ":"JWT","kid":"k1"}}"#).as_bytes()),
            base64url_encode(claims.to_string().as_bytes())
        )
    };
    let claims = |iss: &str, aud: &str, exp: u64| {
        serde_json::json!({
            "iss": iss, "aud": aud, "exp": exp, "sub": "s-1",
            "preferred_username": "dana", "roles": ["editors"],
        })
    };
    let path = temp_file(
        "oidc",
        &format!(
            r#"{{"auth": {{"oidc": {{"issuers": [{{
                "issuer": "{issuer}", "audience": "tripletrail", "jwksUri": "{issuer}/keys",
                "agent": "https://example.org/people/{{preferred_username}}",
                "groups": "roles", "groupNames": {{"editors": "tripletrail:writer"}}
            }}]}}}}}}"#
        ),
    );
    let serve = |license: bool| {
        let mut options = Options {
            threads: 2,
            auth_config: Some(path.to_path_buf()),
            admin_token: Some("admin-token".to_owned()),
            ..Options::default()
        };
        if license {
            options.license = Ok(License {
                id: "2026-test".into(),
                kid: "test".into(),
                customer: "Example Rail AS".into(),
                issued: Date::today(),
                expires: Date::today().plus_days(100),
                features: ["sso".to_owned()].into(),
                max_nodes: None,
                max_cores: None,
                note: None,
            });
        }
        let server = Server::bind(
            "127.0.0.1:0",
            [("kg".to_owned(), Engine::new_tripletrail())],
            options,
        )
        .unwrap();
        let base = format!("http://{}", server.address());
        server.spawn();
        base
    };
    let base = serve(true);
    let bearer = |r: ureq::Request, t: &str| r.set("Authorization", &format!("Bearer {t}"));
    let good = token(claims(&issuer, "tripletrail", now + 600));
    assert_eq!(status(bearer(query(&base, "kg"), &good), None).0, 200);
    // `editors` maps to tripletrail:writer: the update goes through.
    assert_eq!(status(bearer(update(&base), &good), Some(INSERT)).0, 200);
    for bad in [
        token(claims("https://evil.example.org", "tripletrail", now + 600)),
        token(claims(&issuer, "other", now + 600)),
        token(claims(&issuer, "tripletrail", now - 3600)),
        format!("{}x", &good[..good.len() - 4]),
    ] {
        let (code, challenge) = status(bearer(query(&base, "kg"), &bad), None);
        assert_eq!(code, 401, "{bad}");
        assert!(challenge.unwrap().starts_with("Bearer"));
    }
    // Enterprise #9: unsigned, signed with another algorithm, by another key or with a key
    // the issuer does not list, not valid yet, or without an expiry. On a second server, as
    // ten failed sign-ins from one address are answered 429 for a while.
    let second = serve(true);
    for bad in [
        unsigned("none", claims(&issuer, "tripletrail", now + 600), ""),
        unsigned("none", claims(&issuer, "tripletrail", now + 600), "e30"),
        unsigned(
            "HS256",
            claims(&issuer, "tripletrail", now + 600),
            "c2lnbmF0dXJl",
        ),
        signed_by(&other, "k1", claims(&issuer, "tripletrail", now + 600)),
        signed_by(&pair, "k2", claims(&issuer, "tripletrail", now + 600)),
        token({
            let mut c = claims(&issuer, "tripletrail", now + 600);
            c["nbf"] = (now + 3600).into();
            c
        }),
        token({
            let mut c = claims(&issuer, "tripletrail", now + 600);
            c.as_object_mut().unwrap().remove("exp");
            c
        }),
    ] {
        let (code, challenge) = status(bearer(query(&second, "kg"), &bad), None);
        assert_eq!(code, 401, "{bad}");
        assert!(challenge.unwrap().starts_with("Bearer"));
    }
    // ... while a good token still signs in there.
    assert_eq!(status(bearer(query(&second, "kg"), &good), None).0, 200);
    // The access explain page says whom a token signs in, and which claim gave each group.
    let explain = |t: &str| {
        ureq::get(&format!("{base}/$/access/datasets/kg/explain"))
            .set("Authorization", "Bearer admin-token")
            .set("X-TripleTrail-Explain-Token", t)
            .call()
            .map_or_else(
                |e| match e {
                    ureq::Error::Status(code, r) => (code, r.into_string().unwrap()),
                    e => panic!("{e}"),
                },
                |r| (r.status(), r.into_string().unwrap()),
            )
    };
    let (code, body) = explain(&good);
    assert_eq!(code, 200, "{body}");
    let answer: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["agent"], "https://example.org/people/dana");
    assert_eq!(answer["groups"], serde_json::json!(["tripletrail:writer"]));
    assert_eq!(answer["write"], true);
    assert_eq!(answer["token"]["issuer"], issuer.as_str());
    assert_eq!(
        answer["token"]["groupClaims"],
        serde_json::json!([{ "group": "tripletrail:writer", "claim": "roles" }])
    );
    let (code, body) = explain(&token(claims(&issuer, "other", now + 600)));
    assert_eq!(code, 422);
    assert!(body.contains("audience"), "{body}");
    // Without a licence that covers sso, the same good token is refused.
    let unlicensed = serve(false);
    assert_eq!(status(bearer(query(&unlicensed, "kg"), &good), None).0, 401);
}

/// Browser sign-in (#198): the authorization code flow with PKCE against a provider served
/// here on a local port. The session cookie signs the browser in; changes with it need the
/// server's own origin; signing out ends it; a sign-in's state works once; the code is
/// exchanged with the PKCE verifier and the client secret; the ID token must carry the
/// sign-in's nonce; without a licence nobody signs in.
#[cfg(feature = "enterprise")]
#[test]
fn browsers_sign_in_with_the_identity_provider() {
    use ring::digest::{SHA256, digest};
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};
    use tripletrail_enterprise::license::{Date, License, base64url_encode};

    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let pair = Arc::new(
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap(),
    );
    let point = pair.public_key().as_ref();
    let jwks = serde_json::json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "kid": "k1",
        "x": base64url_encode(&point[1..33]), "y": base64url_encode(&point[33..65]),
    }]})
    .to_string();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    // What the provider expects at its token endpoint: the code, the PKCE challenge it was
    // given with it, and the nonce to put in the ID token.
    #[derive(Default)]
    struct Expect {
        code: String,
        challenge: String,
        nonce: String,
    }
    let expect: Arc<Mutex<Expect>> = Arc::default();
    {
        let (issuer, expect, pair) = (issuer.clone(), Arc::clone(&expect), Arc::clone(&pair));
        std::thread::spawn(move || {
            let rng = SystemRandom::new();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let target = line.split(' ').nth(1).unwrap_or_default().to_owned();
                let mut length = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let (status, answer) = match target.as_str() {
                    "/.well-known/openid-configuration" => (
                        200,
                        serde_json::json!({
                            "issuer": issuer,
                            "authorization_endpoint": format!("{issuer}/auth"),
                            "token_endpoint": format!("{issuer}/token"),
                            "jwks_uri": format!("{issuer}/keys"),
                        })
                        .to_string(),
                    ),
                    "/keys" => (200, jwks.clone()),
                    "/token" => {
                        let form: HashMap<String, String> =
                            form_urlencoded::parse(&body).into_owned().collect();
                        let e = expect.lock().unwrap();
                        let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                        let challenge =
                            base64url_encode(digest(&SHA256, verifier.as_bytes()).as_ref());
                        if form.get("grant_type").map(String::as_str) != Some("authorization_code")
                            || form.get("code") != Some(&e.code)
                            || challenge != e.challenge
                            || form.get("client_id").map(String::as_str) != Some("tripletrail-ui")
                            || form.get("client_secret").map(String::as_str) != Some("s3cret")
                            || form.get("redirect_uri").map(String::as_str)
                                != Some("http://localhost:7878/$/sso/callback")
                        {
                            (400, r#"{"error":"invalid_grant"}"#.to_owned())
                        } else {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_secs();
                            let claims = serde_json::json!({
                                "iss": issuer, "aud": "tripletrail-ui", "exp": now + 300,
                                "iat": now, "sub": "s-1", "nonce": e.nonce,
                                "preferred_username": "dana", "roles": ["editors"],
                            });
                            let signed = format!(
                                "{}.{}",
                                base64url_encode(br#"{"alg":"ES256","typ":"JWT","kid":"k1"}"#),
                                base64url_encode(claims.to_string().as_bytes())
                            );
                            let sig = pair.sign(&rng, signed.as_bytes()).unwrap();
                            let id_token = format!("{signed}.{}", base64url_encode(sig.as_ref()));
                            (
                                200,
                                serde_json::json!({ "id_token": id_token, "token_type": "Bearer" })
                                    .to_string(),
                            )
                        }
                    }
                    _ => (404, "{}".to_owned()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                );
            }
        });
    }
    let path = temp_file(
        "sso",
        &format!(
            r#"{{"auth": {{"oidc": {{"issuers": [{{
                "issuer": "{issuer}", "audience": "tripletrail",
                "agent": "https://example.org/people/{{preferred_username}}",
                "groups": "roles", "groupNames": {{"editors": "tripletrail:writer"}},
                "browser": {{"name": "Example Rail", "clientId": "tripletrail-ui",
                    "clientSecret": "s3cret",
                    "redirectUri": "http://localhost:7878/$/sso/callback"}}
            }}]}}}}}}"#
        ),
    );
    let serve = |license: bool| {
        let mut options = Options {
            threads: 2,
            auth_config: Some(path.to_path_buf()),
            ..Options::default()
        };
        if license {
            options.license = Ok(License {
                id: "2026-test".into(),
                kid: "test".into(),
                customer: "Example Rail AS".into(),
                issued: Date::today(),
                expires: Date::today().plus_days(100),
                features: ["sso".to_owned()].into(),
                max_nodes: None,
                max_cores: None,
                note: None,
            });
        }
        let server = Server::bind(
            "127.0.0.1:0",
            [("kg".to_owned(), Engine::new_tripletrail())],
            options,
        )
        .unwrap();
        let base = format!("http://{}", server.address());
        server.spawn();
        base
    };
    let base = serve(true);
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    // Any answer, with its status and headers.
    let call = |r: ureq::Request| match r.call() {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{e}"),
    };
    let send = |r: ureq::Request, body: &str| match r
        .set("Content-Type", "application/sparql-update")
        .send_string(body)
    {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r.status(),
        Err(e) => panic!("{e}"),
    };
    let params = |url: &str| -> HashMap<String, String> {
        form_urlencoded::parse(url.split_once('?').unwrap().1.as_bytes())
            .into_owned()
            .collect()
    };

    // What the sign-in page offers, before anyone signs in.
    let overview: serde_json::Value = call(agent.get(&format!("{base}/$/sso")))
        .into_string()
        .map(|t| serde_json::from_str(&t).unwrap())
        .unwrap();
    assert_eq!(overview["signIn"][0]["name"], "Example Rail");
    assert_eq!(overview["signIn"][0]["login"], "/$/sso/login?issuer=0");
    assert!(overview["signedIn"].is_null());
    // A browser opening the UI without credentials is sent to sign in; a client is a 401.
    let r = call(
        agent
            .get(&format!("{base}/$/ui/"))
            .set("Accept", "text/html"),
    );
    assert_eq!(r.status(), 302);
    assert_eq!(
        r.header("Location"),
        Some("/$/sso/login?return=%2F%24%2Fui%2F")
    );
    assert_eq!(
        call(agent.get(&format!("{base}/kg/query?query=ASK%7B%7D"))).status(),
        401
    );

    // Off to the provider, and back.
    let sign_in = |return_to: &str| {
        let r = call(
            agent
                .get(&format!("{base}/$/sso/login"))
                .query("return", return_to),
        );
        assert_eq!(r.status(), 302);
        let location = r.header("Location").unwrap().to_owned();
        assert!(
            location.starts_with(&format!("{issuer}/auth?")),
            "{location}"
        );
        let p = params(&location);
        assert_eq!(p["response_type"], "code");
        assert_eq!(p["client_id"], "tripletrail-ui");
        assert_eq!(p["redirect_uri"], "http://localhost:7878/$/sso/callback");
        assert_eq!(p["code_challenge_method"], "S256");
        assert!(p["scope"].split(' ').any(|s| s == "openid"));
        // The cookie that ties the sign-in to this browser.
        let flow = r.header("Set-Cookie").unwrap();
        assert!(flow.starts_with("tripletrail_sso_flow="), "{flow}");
        for attribute in ["HttpOnly", "SameSite=Lax", "Path=/$/sso", "Max-Age=600"] {
            assert!(flow.contains(attribute), "{flow}");
        }
        let mut p = p;
        p.insert("flow".into(), flow.split(';').next().unwrap().to_owned());
        p
    };
    let first = sign_in("/$/ui/#/datasets");
    *expect.lock().unwrap() = Expect {
        code: "c0de".into(),
        challenge: first["code_challenge"].clone(),
        nonce: first["nonce"].clone(),
    };
    let callback_with = |p: &HashMap<String, String>, code: &str, flow: Option<&str>| {
        let r = agent
            .get(&format!("{base}/$/sso/callback"))
            .query("code", code)
            .query("state", &p["state"]);
        call(match flow {
            Some(f) => r.set("Cookie", f),
            None => r,
        })
    };
    let callback =
        |p: &HashMap<String, String>, code: &str| callback_with(p, code, Some(&p["flow"]));
    let r = callback(&first, "c0de");
    assert_eq!(r.status(), 302);
    assert_eq!(r.header("Location"), Some("/$/ui/#/datasets"));
    let set_cookie = r.header("Set-Cookie").unwrap().to_owned();
    assert!(
        set_cookie.starts_with("tripletrail_session="),
        "{set_cookie}"
    );
    for attribute in ["HttpOnly", "SameSite=Lax", "Path=/", "Max-Age=28800"] {
        assert!(set_cookie.contains(attribute), "{set_cookie}");
    }
    let session = set_cookie.split(';').next().unwrap().to_owned();
    // The flow cookie is cleared.
    assert!(
        r.all("Set-Cookie")
            .iter()
            .any(|c| c.starts_with("tripletrail_sso_flow=;") && c.contains("Max-Age=0"))
    );
    // A state is used once.
    assert_eq!(callback(&first, "c0de").status(), 401);
    // Login CSRF: someone else's sign-in (state and code) sent to this browser, which does not
    // have that sign-in's flow cookie, is refused, with no cookie or another flow's.
    for flow in [None, Some(first["flow"].as_str())] {
        let other = sign_in("/$/ui/");
        *expect.lock().unwrap() = Expect {
            code: "c0de".into(),
            challenge: other["code_challenge"].clone(),
            nonce: other["nonce"].clone(),
        };
        let r = callback_with(&other, "c0de", flow);
        assert_eq!(r.status(), 401);
        assert!(
            r.into_string()
                .unwrap()
                .contains("not started in this browser")
        );
    }

    // Signed in: Dana reads, and is listed as signed in.
    let with = |r: ureq::Request| r.set("Cookie", &format!("theme=dark; {session}"));
    assert_eq!(
        call(with(agent.get(&format!("{base}/kg/query?query=ASK%7B%7D")))).status(),
        200
    );
    let overview: serde_json::Value = call(with(agent.get(&format!("{base}/$/sso"))))
        .into_string()
        .map(|t| serde_json::from_str(&t).unwrap())
        .unwrap();
    assert_eq!(
        overview["signedIn"]["agent"],
        "https://example.org/people/dana"
    );
    assert_eq!(overview["signedIn"]["groups"][0], "tripletrail:writer");
    assert_eq!(overview["signedIn"]["groupClaims"][0]["claim"], "roles");
    // Changes need the server's own origin: another site's page cannot send them (CSRF).
    let update = || with(agent.post(&format!("{base}/kg/update")));
    assert_eq!(send(update(), INSERT), 403);
    assert_eq!(
        send(update().set("Origin", "http://evil.example"), INSERT),
        403
    );
    assert_eq!(
        send(update().set("Sec-Fetch-Site", "cross-site"), INSERT),
        403
    );
    assert_eq!(
        send(update().set("Origin", "http://localhost:7878"), INSERT),
        200
    );
    assert_eq!(
        send(update().set("Sec-Fetch-Site", "same-origin"), INSERT),
        200
    );
    // A made-up session is a 401.
    let made_up = agent
        .get(&format!("{base}/kg/query?query=ASK%7B%7D"))
        .set("Cookie", "tripletrail_session=AAAA");
    assert_eq!(call(made_up).status(), 401);

    // A sign-in that came back to the wrong flow sends the wrong PKCE verifier, which the
    // provider refuses; a provider's ID token for another nonce is refused here.
    let second = sign_in("//evil.example/x");
    assert_eq!(callback(&second, "c0de").status(), 401);
    let third = sign_in("https://evil.example/x");
    *expect.lock().unwrap() = Expect {
        code: "c0de".into(),
        challenge: third["code_challenge"].clone(),
        nonce: "another".into(),
    };
    let r = callback(&third, "c0de");
    assert_eq!(r.status(), 401);
    assert!(r.into_string().unwrap().contains("nonce"));
    // Returning anywhere but this server is not possible: an open redirect.
    let fourth = sign_in("//evil.example/x");
    *expect.lock().unwrap() = Expect {
        code: "c0de".into(),
        challenge: fourth["code_challenge"].clone(),
        nonce: fourth["nonce"].clone(),
    };
    let r = callback(&fourth, "c0de");
    assert_eq!(r.header("Location"), Some("/$/ui/"));
    // Browsers drop tabs and line breaks from a URL and read \\ as /: none get through.
    for evil in [
        "/\t/evil.example/x",
        "/\t\t/evil.example/x",
        "/\n/evil.example/x",
        "/\\evil.example/x",
    ] {
        let p = sign_in(evil);
        *expect.lock().unwrap() = Expect {
            code: "c0de".into(),
            challenge: p["code_challenge"].clone(),
            nonce: p["nonce"].clone(),
        };
        assert_eq!(
            callback(&p, "c0de").header("Location"),
            Some("/$/ui/"),
            "{evil:?}"
        );
    }

    // A change to the configuration that keeps the issuers keeps the sessions.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replacen(r#"{"auth": {"#, r#"{"auth": {"anonymous": false, "#, 1),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(
        call(with(agent.get(&format!("{base}/kg/query?query=ASK%7B%7D")))).status(),
        200
    );
    std::fs::write(&path, text).unwrap();

    // Signing out needs the own origin too, and ends the session.
    let logout = || with(agent.post(&format!("{base}/$/sso/logout")));
    assert_eq!(
        call(logout().set("Origin", "http://evil.example")).status(),
        403
    );
    let r = call(logout().set("Origin", "http://localhost:7878"));
    assert_eq!(r.status(), 204);
    assert!(r.header("Set-Cookie").unwrap().contains("Max-Age=0"));
    assert_eq!(
        call(with(agent.get(&format!("{base}/kg/query?query=ASK%7B%7D")))).status(),
        401
    );

    // Without a licence that covers sso nobody signs in.
    let unlicensed = serve(false);
    assert_eq!(
        call(agent.get(&format!("{unlicensed}/$/sso/login"))).status(),
        403
    );
}

/// The audit log (#207): every change and admin call, and every refused attempt at one, is a
/// line with who sent it and the outcome, in a hash chain that goes on over restarts; reads
/// are not logged, and an update's text is not kept.
/// `tripletrail audit report` exits 1 when the chain is not whole, for scripts that look at the
/// exit code only (#207).
#[cfg(feature = "enterprise")]
#[test]
fn an_audit_report_of_a_broken_chain_exits_1() {
    let dir = tripletrail_testdir::tempdir("audit-cli");
    let path = dir.join("audit.jsonl");
    let log = tripletrail_enterprise::audit::Log::open(&path).unwrap();
    let mut line = serde_json::Map::new();
    line.insert("event".into(), "update".into());
    line.insert("agent".into(), "urn:ann".into());
    log.append(line).unwrap();
    drop(log);
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_tripletrail"))
            .args(["audit", "report"])
            .args(args)
            .arg(&path)
            .output()
            .unwrap()
    };
    let whole = run(&[]);
    assert!(whole.status.success(), "{whole:?}");
    assert!(String::from_utf8_lossy(&whole.stdout).contains("urn:ann"));
    let offset = run(&["--since", "2026-11-15T10:00:00+01:00"]);
    assert!(!offset.status.success());
    assert!(
        String::from_utf8_lossy(&offset.stderr).contains("UTC"),
        "{offset:?}"
    );
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("urn:ann", "urn:eve");
    std::fs::write(&path, text).unwrap();
    let broken = run(&["--json"]);
    assert_eq!(broken.status.code(), Some(1), "{broken:?}");
    let json: serde_json::Value = serde_json::from_slice(&broken.stdout).unwrap();
    assert_eq!(json["chainWhole"], false);
}

#[cfg(feature = "enterprise")]
#[test]
fn audit_lines_also_go_to_syslog() {
    let dir = tripletrail_testdir::tempdir("audit-syslog");
    let collector = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    collector
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("kg".to_owned(), Engine::new_tripletrail())],
        Options {
            threads: 2,
            update_token: Some("update-token".to_owned()),
            audit_log: Some(dir.join("audit.jsonl")),
            audit_syslog: Some(format!("udp://{}", collector.local_addr().unwrap())),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    assert_eq!(status(update(&base), Some(INSERT)).0, 401);
    let mut buf = [0; 8192];
    let mut got = Vec::new();
    for _ in 0..2 {
        let n = collector.recv(&mut buf).unwrap();
        got.push(String::from_utf8(buf[..n].to_vec()).unwrap());
    }
    assert!(got[0].starts_with("<109>1 "), "{}", got[0]);
    assert!(
        got[0].contains(" tripletrail ") && got[0].contains(" start - {"),
        "{}",
        got[0]
    );
    // A refused change is a warning; the message is the line in the file.
    assert!(got[1].starts_with("<108>1 "), "{}", got[1]);
    let file = std::fs::read_to_string(dir.join("audit.jsonl")).unwrap();
    let last = file.lines().last().unwrap();
    assert!(
        got[1].ends_with(&format!(" update - {last}")),
        "{} / {last}",
        got[1]
    );
    let metrics = ureq::get(&format!("{base}/$/metrics"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        metrics.contains("tripletrail_audit_syslog_dropped_total 0"),
        "{metrics}"
    );
}

#[cfg(feature = "enterprise")]
#[test]
fn changes_are_written_to_the_audit_log() {
    let dir = tripletrail_testdir::tempdir("audit");
    let log = dir.join("audit.jsonl");
    let path = temp_file("audit", &config(false, r#""127.0.0.1""#));
    let serve = |reads: bool| {
        let server = Server::bind(
            "127.0.0.1:0",
            [("kg".to_owned(), Engine::new_tripletrail())],
            Options {
                threads: 2,
                update_token: Some("update-token".to_owned()),
                admin_token: Some("admin-token".to_owned()),
                auth_config: Some(path.to_path_buf()),
                audit_log: Some(log.clone()),
                audit_reads: reads,
                backup_dir: Some(dir.to_path_buf()),
                workspaces: WorkspaceOptions {
                    dir: Some(dir.join("workspaces")),
                    ..WorkspaceOptions::default()
                },
                ..Options::default()
            },
        )
        .unwrap();
        let base = format!("http://{}", server.address());
        server.spawn();
        base
    };
    let base = serve(false);
    let with = |r: ureq::Request, key: &str| r.set("Authorization", &format!("Bearer {key}"));
    assert_eq!(
        status(with(query(&base, "kg"), "update-token"), None).0,
        200
    );
    assert_eq!(
        status(with(update(&base), "update-token"), Some(INSERT)).0,
        200
    );
    assert_eq!(status(update(&base), Some(INSERT)).0, 401);
    let put = ureq::put(&format!("{base}/kg/data?graph=urn:g"))
        .set("Authorization", "Bearer update-token")
        .set("Content-Type", "application/n-triples")
        .send_string("<urn:a> <urn:b> <urn:c> .\n")
        .unwrap();
    assert!(put.status() < 300);
    let admin = ureq::post(&format!("{base}/$/datasets?dbName=more&dbType=mem"))
        .set("Authorization", "Bearer admin-token")
        .call()
        .unwrap();
    assert!(admin.status() < 300);
    // Routes that read a dataset but change something else are changes too: a workspace's
    // sample, and RDF4J's namespaces cleared.
    let post = |path: &str, body: &str| {
        ureq::post(&format!("{base}{path}"))
            .set("Authorization", "Bearer admin-token")
            .set("Content-Type", "application/json")
            .send_string(body)
            .unwrap_or_else(|e| panic!("{}", e.into_response().unwrap().into_string().unwrap()))
            .into_string()
            .unwrap()
    };
    let made: serde_json::Value = serde_json::from_str(&post("/$/workspaces", "{}")).unwrap();
    let id = made["id"].as_str().unwrap();
    post(
        &format!("/$/workspaces/{id}/sample"),
        r#"{"dataset": "kg", "classes": ["urn:C"]}"#,
    );
    let cleared = ureq::delete(&format!("{base}/rdf4j-server/repositories/kg/namespaces"))
        .set("Authorization", "Bearer update-token")
        .call()
        .unwrap();
    assert!(cleared.status() < 300);
    // The same insert again changes nothing.
    assert_eq!(
        status(with(update(&base), "update-token"), Some(INSERT)).0,
        200
    );
    // A large upload is a bulk load, and so is a dataset made from a file: what they wrote is
    // not counted, which the line says.
    let big: String = (0..100_001)
        .map(|i| format!("<urn:s{i}> <urn:p> <urn:o> .\n"))
        .collect();
    let put = ureq::put(&format!("{base}/kg/data?graph=urn:big"))
        .set("Authorization", "Bearer update-token")
        .set("Content-Type", "application/n-triples")
        .send_string(&big)
        .unwrap();
    assert!(put.status() < 300);
    std::fs::write(dir.join("seed.nt"), "<urn:a> <urn:b> <urn:c> .\n").unwrap();
    let loaded = ureq::post(&format!(
        "{base}/$/datasets?dbName=seeded&dbType=mem&from=seed.nt"
    ))
    .set("Authorization", "Bearer admin-token")
    .call()
    .unwrap();
    assert!(loaded.status() < 300);

    let lines = || -> Vec<serde_json::Value> {
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let got = lines();
    let events: Vec<&str> = got.iter().map(|l| l["event"].as_str().unwrap()).collect();
    assert_eq!(
        events,
        // A change refused before its kind is known is a `change`.
        [
            "start", "update", "change", "upload", "admin", "admin", "admin", "update", "update",
            "upload", "admin"
        ],
        "{got:?}"
    );
    let (done, refused, upload, created) = (&got[1], &got[2], &got[3], &got[4]);
    assert_eq!(got[6]["path"], format!("/$/workspaces/{id}/sample"));
    assert_eq!(got[6]["dataset"], "kg");
    assert_eq!(got[7]["path"], "/rdf4j-server/repositories/kg/namespaces");
    assert_eq!(got[7]["method"], "DELETE");
    assert!(got[8].get("graphs").is_none(), "{}", got[8]);
    assert_eq!(got[8]["outcome"], "done");
    assert!(got[8].get("uncounted").is_none(), "{}", got[8]);
    for counted in &got[1..9] {
        assert!(counted.get("uncounted").is_none(), "{counted}");
    }
    for bulk in [&got[9], &got[10]] {
        assert_eq!(bulk["uncounted"], true, "{bulk}");
        assert!(bulk.get("graphs").is_none(), "{bulk}");
    }
    assert_eq!(got[10]["path"], "/$/datasets");
    assert_eq!(done["outcome"], "done");
    assert_eq!(done["dataset"], "kg");
    assert_eq!(done["agent"], "urn:tripletrail:admin");
    assert_eq!(done["auth"], "apiKey");
    assert_eq!(done["path"], "/kg/update");
    assert_eq!(done["client"], "127.0.0.1");
    assert_eq!(done["bytes"], INSERT.len());
    assert!(done["textHash"].is_string());
    assert!(!got[1].to_string().contains("INSERT"));
    assert_eq!(refused["outcome"], "refused");
    assert_eq!(refused["status"], 401);
    assert!(refused["agent"].is_null());
    assert_eq!(upload["graph"], "urn:g");
    // What each change did to each graph.
    assert_eq!(
        upload["graphs"],
        serde_json::json!([{"graph": "urn:g", "added": 1, "removed": 0}])
    );
    assert_eq!(
        (&upload["added"], &upload["removed"]),
        (&1.into(), &0.into())
    );
    assert_eq!(
        done["graphs"],
        serde_json::json!([{"graph": null, "added": 1, "removed": 0}])
    );
    assert_eq!(upload["method"], "PUT");
    assert_eq!(created["path"], "/$/datasets");
    assert_eq!(created["seq"], 5);

    // A restart goes on with the chain, which `verify` finds whole. With reads logged, a
    // query has its line too.
    let base = serve(true);
    assert_eq!(
        status(with(query(&base, "kg"), "update-token"), None).0,
        200
    );
    assert_eq!(
        status(with(update(&base), "update-token"), Some(INSERT)).0,
        200
    );
    let got = lines();
    assert_eq!(got.len(), 14);
    assert_eq!(got[13]["seq"], 14);
    let read = &got[12];
    assert_eq!(read["event"], "query", "{read}");
    assert_eq!(read["path"], "/kg/query");
    assert_eq!(read["agent"], "urn:tripletrail:admin");
    assert!(read["textHash"].is_string());
    assert!(!read.to_string().contains("ASK"));

    let report = tripletrail_enterprise::audit::verify(&log).unwrap();
    assert_eq!(report.problems, vec![]);
    assert_eq!(report.starts, 2);
    assert_eq!(report.last_hash, got[13]["hash"].as_str().unwrap());
}

/// The row rule builder's API (enterprise#8): the classes and properties of the data, a
/// preview of a rule before it is saved (the file stays as it was), then the shape and the
/// rule saved, enforced, listed with the policy, and kept by a client that leaves them out.
#[cfg(feature = "enterprise")]
#[test]
fn row_rules_are_built_previewed_and_saved_through_the_access_api() {
    use serde_json::{Value, json};
    use tripletrail_enterprise::license::{Date, License};
    let dir = tripletrail_testdir::tempdir("auth-row-builder");
    std::fs::write(
        dir.join("assets.ttl"),
        format!(
            "{SG}ex:Contractors a sg:Policy ; sg:group \"contractor\" ; sg:readAllGraphs true .
             ex:Team a sg:Policy ; sg:group \"team\" ; sg:readAllGraphs true ."
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("guard.json"),
        format!(
            r#"{{"auth": {{"apiKeys": [
                {{"sha256": "{}", "agent": "http://example.org/north", "groups": ["contractor"]}},
                {{"sha256": "{}", "agent": "http://example.org/root", "groups": ["tripletrail:admin"]}}]}},
              "datasets": {{"assets": {{"policies": ["assets.ttl"]}}}}}}"#,
            hash("north"),
            hash("root"),
        ),
    )
    .unwrap();
    let engine = Engine::new_tripletrail();
    engine
        .load(
            RdfFormat::Turtle,
            &b"@prefix ex: <http://example.org/> .
               ex:p1 a ex:Project ; ex:area ex:north ; ex:name \"p1\" .
               ex:p2 a ex:Project ; ex:area ex:south ; ex:name \"p2\" .
               ex:p3 a ex:Project ; ex:area ex:south ; ex:name \"p3\" .
               ex:t1 a ex:Task ; ex:of ex:p2 ."[..],
        )
        .unwrap();
    let server = Server::bind(
        "127.0.0.1:0",
        [("assets".to_owned(), engine)],
        Options {
            threads: 2,
            auth_config: Some(dir.join("guard.json")),
            license: Ok(License {
                id: "2026-043".into(),
                kid: "test".into(),
                customer: "Example Rail AS".into(),
                issued: Date::today().plus_days(-10),
                expires: Date::today().plus_days(300),
                features: ["row-policies".to_owned()].into(),
                max_nodes: None,
                max_cores: None,
                note: None,
            }),
            ..Options::default()
        },
    )
    .unwrap();
    let base = format!("http://{}", server.address());
    server.spawn();
    let call = |method: &str, path: &str, body: Option<Value>| -> (u16, Value) {
        let request = ureq::request(method, &format!("{base}/$/access/datasets/assets{path}"))
            .set("X-API-Key", "root");
        let response = match body {
            Some(b) => request.send_string(&b.to_string()),
            None => request.call(),
        };
        match response {
            Ok(r) => {
                let status = r.status();
                let text = r.into_string().unwrap();
                (status, serde_json::from_str(&text).unwrap_or(Value::Null))
            }
            Err(ureq::Error::Status(code, r)) => (code, json!(r.into_string().unwrap_or_default())),
            Err(e) => panic!("{e}"),
        }
    };
    let projects = |key: &str| -> usize {
        let r = ureq::get(&format!("{base}/assets/query"))
            .query("query", "SELECT ?p { ?p a <http://example.org/Project> }")
            .set("X-API-Key", key)
            .set("Accept", "text/csv")
            .call()
            .unwrap();
        r.into_string().unwrap().lines().count() - 1
    };
    let file = || std::fs::read_to_string(dir.join("assets.ttl")).unwrap();

    // What the builder offers: the classes, and the properties of one.
    let (code, offered) = call("GET", "/shapes?class=http://example.org/Project", None);
    assert_eq!(code, 200, "{offered}");
    assert_eq!(offered["shapes"], json!([]));
    assert_eq!(
        offered["classes"][0],
        json!({ "class": "http://example.org/Project", "instances": 3 })
    );
    let paths: Vec<&str> = offered["properties"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"http://example.org/area"), "{offered}");

    // A rule previewed: the contractors would see their own area only.
    let shape = json!({
        "id": "http://example.org/OwnArea",
        "turtle": "<http://example.org/OwnArea> a sh:NodeShape ; sh:targetClass <http://example.org/Project> ;
                     sh:property [ sh:path <http://example.org/area> ; sh:hasValue sg:requestingAgent ] .",
    });
    let policy = json!({
        "id": "http://example.org/Contractors",
        "groups": ["contractor"],
        "readAllGraphs": true,
        "rowRules": [{ "rule": "sg:visibleIf", "shape": "http://example.org/OwnArea" }],
    });
    let before = file();
    let (code, preview) = call(
        "POST",
        "/preview",
        Some(
            json!({ "agent": "http://example.org/north", "groups": ["contractor"],
                     "shapes": [shape], "policy": policy }),
        ),
    );
    assert_eq!(code, 200, "{preview}");
    assert_eq!(file(), before);
    assert_eq!(preview["rowRulesNow"], json!([]));
    assert_eq!(preview["rowRulesAfter"][0]["rule"], "sg:visibleIf");
    let project = preview["classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["class"] == "http://example.org/Project")
        .unwrap()
        .clone();
    assert_eq!(
        (
            &project["instances"],
            &project["visibleNow"],
            &project["visibleAfter"]
        ),
        (&json!(3), &json!(3), &json!(1)),
        "{project}"
    );
    assert_eq!(project["sampled"], 3);
    assert_eq!(project["visible"], json!(["http://example.org/p1"]));
    assert_eq!(project["hidden"].as_array().unwrap().len(), 2);
    // The contractors read but do not write.
    assert_eq!(project["sampledEditable"], 0);
    // A team member is not under the rule.
    let (_, team) = call(
        "POST",
        "/preview",
        Some(
            json!({ "agent": "http://example.org/t", "groups": "team", "shapes": [shape], "policy": policy }),
        ),
    );
    assert_eq!(team["rowRulesAfter"], json!([]));
    // A shape that does not parse is a 422, and nothing is written.
    let broken = json!({ "id": "http://example.org/OwnArea", "turtle": "<http://example.org/OwnArea> sh:targetClass" });
    assert_eq!(
        call("POST", "/preview", Some(json!({ "shapes": [broken] }))).0,
        422
    );
    assert_eq!(call("PUT", "/shapes", Some(broken)).0, 400);
    assert_eq!(projects("north"), 3);

    // Saved: the shape, then the rule. The contractors now see their own area only.
    let (code, saved) = call("PUT", "/shapes", Some(shape.clone()));
    assert_eq!(code, 200, "{saved}");
    let (code, saved) = call("PUT", "/policies", Some(policy.clone()));
    assert_eq!(code, 200, "{saved}");
    assert_eq!(projects("north"), 1);
    let text = file();
    assert!(text.contains("sg:visibleIf ex:OwnArea"), "{text}");
    assert!(text.contains("sh:hasValue sg:requestingAgent"), "{text}");
    // Listed with the policy that uses it, and in the overview.
    let (_, listed) = call("GET", "/shapes", None);
    assert_eq!(listed["shapes"][0]["id"], "http://example.org/OwnArea");
    assert_eq!(
        listed["shapes"][0]["usedBy"],
        json!([{ "policy": "http://example.org/Contractors", "rule": "sg:visibleIf" }])
    );
    let overview: Value = serde_json::from_str(
        &ureq::get(&format!("{base}/$/access"))
            .set("X-API-Key", "root")
            .call()
            .unwrap()
            .into_string()
            .unwrap(),
    )
    .unwrap();
    let contractors = overview["datasets"][0]["policies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "http://example.org/Contractors")
        .unwrap()
        .clone();
    assert_eq!(
        contractors["rowRules"][0]["shape"],
        "http://example.org/OwnArea"
    );
    // A client that does not send row rules keeps them.
    let mut plain = policy.clone();
    plain.as_object_mut().unwrap().remove("rowRules");
    assert_eq!(call("PUT", "/policies", Some(plain)).0, 200);
    assert_eq!(projects("north"), 1);
    // The preview now compares with the rule in force.
    let (_, now) = call(
        "POST",
        "/preview",
        Some(json!({ "agent": "http://example.org/north", "groups": ["contractor"] })),
    );
    assert_eq!(now["rowRulesNow"].as_array().unwrap().len(), 1, "{now}");
    // A shape a rule uses cannot be removed; the rule first, then the shape.
    let (code, _) = call("DELETE", "/shapes?id=http://example.org/OwnArea", None);
    assert_eq!(code, 422);
    let mut none = policy.clone();
    none["rowRules"] = json!([]);
    assert_eq!(call("PUT", "/policies", Some(none)).0, 200);
    assert_eq!(
        call("DELETE", "/shapes?id=http://example.org/OwnArea", None).0,
        204
    );
    assert_eq!(projects("north"), 3);
    assert!(!file().contains("OwnArea"), "{}", file());
}
