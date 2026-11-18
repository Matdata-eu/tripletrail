//! Remote locations (`location=http://…`), against a small HTTP server on 127.0.0.1.
#![cfg(feature = "remote")]

use oxigraph::model::Dataset;
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::Duration;
use tripletrail_fx::remote::RemoteLocations;
use tripletrail_fx::{FxConfig, FxService};

const PREFIXES: &str = "PREFIX xyz: <http://sparql.xyz/facade-x/data/>
PREFIX fx: <http://sparql.xyz/facade-x/ns/>
PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
";

/// Serves requests on a free port until the test ends, and gives its address.
///
/// `/data.json`: a JSON object. `/table`: CSV with no extension, served as text/csv.
/// `/moved`: a redirect to `/data.json`. `/away`: a redirect to `localhost`, off the
/// allowlist. `/loop`: a redirect to itself. `/big`: 4,096 bytes. `/missing`: 404.
/// `/echo`: the request's method, query, `X-Test` and `Authorization` headers and body, as
/// JSON.
fn server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_owned();
            let target = parts.next().unwrap_or_default().to_owned();
            let (mut length, mut test, mut auth) = (0usize, String::new(), String::new());
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                let (name, value) = header.split_once(':').unwrap();
                match name.to_ascii_lowercase().as_str() {
                    "content-length" => length = value.trim().parse().unwrap(),
                    "x-test" => test = value.trim().to_owned(),
                    "authorization" => auth = value.trim().to_owned(),
                    _ => {}
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let (path, query) = target.split_once('?').unwrap_or((&target, ""));
            let reply = |status: &str, headers: &str, body: &str| {
                format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            let response = match path {
                "/data.json" => reply(
                    "200 OK",
                    "Content-Type: application/json\r\n",
                    r#"{"name":"remote"}"#,
                ),
                "/table" => reply(
                    "200 OK",
                    "Content-Type: text/csv; charset=utf-8\r\n",
                    "a,b\n1,2\n",
                ),
                "/moved" => reply("302 Found", "Location: /data.json\r\n", ""),
                "/away" => reply(
                    "302 Found",
                    &format!("Location: http://localhost:{port}/data.json\r\n"),
                    "",
                ),
                "/loop" => reply("302 Found", "Location: /loop\r\n", ""),
                // `/to/<port>`: a redirect to `/echo` on another port of 127.0.0.1.
                p if p.starts_with("/to/") => reply(
                    "307 Temporary Redirect",
                    &format!("Location: http://127.0.0.1:{}/echo\r\n", &p[4..]),
                    "",
                ),
                "/big" => reply("200 OK", "Content-Type: text/plain\r\n", &"x".repeat(4096)),
                "/echo" => reply(
                    "200 OK",
                    "Content-Type: application/json\r\n",
                    &format!(
                        r#"{{"method":"{method}","query":"{query}","test":"{test}","auth":"{auth}","body":"{}"}}"#,
                        String::from_utf8_lossy(&body)
                    ),
                ),
                _ => reply("404 Not Found", "", ""),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    address
}

fn service(allow: &[&str], max_bytes: usize) -> FxService {
    FxService::new(
        FxConfig {
            fixed_limits: false,
            databases: None,
            remote: RemoteLocations {
                allow: allow.iter().map(|s| (*s).to_owned()).collect(),
                timeout: Duration::from_secs(10),
                max_bytes,
            },
            ..FxConfig::default()
        },
        SparqlEvaluator::new,
    )
}

/// The values of `?v`, or the error.
fn values(fx: &FxService, query: &str) -> Result<Vec<String>, String> {
    let empty = Box::leak(Box::new(Dataset::new()));
    let results = fx
        .register(SparqlEvaluator::new())
        .parse_query(&format!("{PREFIXES}{query}"))
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .map_err(|e| e.to_string())?;
    let QueryResults::Solutions(solutions) = results else {
        panic!("not a SELECT")
    };
    let mut out = Vec::new();
    for s in solutions {
        let s = s.map_err(|e| e.to_string())?;
        out.extend(s.get("v").map(ToString::to_string));
    }
    Ok(out)
}

#[test]
fn urls_are_read_by_extension_or_media_type() {
    let base = server();
    let fx = service(&[&base], 1 << 20);
    let name = format!(
        "SELECT ?v WHERE {{ SERVICE <x-sparql-anything:location={base}/data.json> {{ ?s xyz:name ?v }} }}"
    );
    assert_eq!(values(&fx, &name).unwrap(), [r#""remote""#]);
    // No extension: the response's media type says CSV.
    let table = format!(
        "SELECT ?v WHERE {{ SERVICE <x-sparql-anything:location={base}/table,csv.headers=true> {{ ?s xyz:b ?v }} }}"
    );
    assert_eq!(values(&fx, &table).unwrap(), [r#""2""#]);
    // A redirect within the allowlist is followed.
    let moved = name.replace("/data.json", "/moved");
    assert_eq!(values(&fx, &moved).unwrap(), [r#""remote""#]);
}

#[test]
fn urls_are_read_only_from_the_allowlist() {
    let base = server();
    let query = |path: &str| {
        format!(
            "SELECT ?v WHERE {{ SERVICE <x-sparql-anything:location={base}{path},media-type=application/json> {{ ?s xyz:name ?v }} }}"
        )
    };
    // Off by default.
    let e = values(&service(&[], 1 << 20), &query("/data.json")).unwrap_err();
    assert!(e.contains("reading URLs is off"), "{e}");
    // Another path than the one allowed.
    let fx = service(&[&format!("{base}/data")], 1 << 20);
    let e = values(&fx, &query("/moved")).unwrap_err();
    assert!(e.contains("not on the list"), "{e}");
    // A redirect off the allowlist (to localhost) is refused.
    let fx = service(&[&base], 1 << 20);
    let e = values(&fx, &query("/away")).unwrap_err();
    assert!(
        e.contains("not on the list") && e.contains("localhost"),
        "{e}"
    );
    // ... and so are endless redirects, too large bodies and errors.
    let e = values(&fx, &query("/loop")).unwrap_err();
    assert!(e.contains("redirects"), "{e}");
    let e = values(&service(&[&base], 1000), &query("/big")).unwrap_err();
    assert!(e.contains("larger than 1000 bytes"), "{e}");
    let e = values(&fx, &query("/missing")).unwrap_err();
    assert!(e.contains("404"), "{e}");
    let e = values(&fx, &query("/moved,http.redirect=false")).unwrap_err();
    assert!(e.contains("http.redirect"), "{e}");
}

#[test]
fn http_options_shape_the_request() {
    let base = server();
    let fx = service(&[&base], 1 << 20);
    let query = |field: &str| {
        format!(
            r#"SELECT ?v WHERE {{ SERVICE <x-sparql-anything:location={base}/echo,http.method=POST,http.payload=hello,http.header.X-Test=yes> {{
                fxe:properties <http://sparql.xyz/facade-x/engine/http.query.q> "a b" ;
                    <http://sparql.xyz/facade-x/engine/http.auth.user> "Aladdin" ;
                    <http://sparql.xyz/facade-x/engine/http.auth.password> "open sesame" .
                ?s xyz:{field} ?v }} }}"#
        )
    };
    assert_eq!(values(&fx, &query("method")).unwrap(), [r#""POST""#]);
    assert_eq!(values(&fx, &query("body")).unwrap(), [r#""hello""#]);
    assert_eq!(values(&fx, &query("test")).unwrap(), [r#""yes""#]);
    assert_eq!(values(&fx, &query("query")).unwrap(), [r#""q=a%20b""#]);
    assert_eq!(
        values(&fx, &query("auth")).unwrap(),
        [r#""Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==""#]
    );
}

/// Headers and credentials are meant for the URL's own host: a redirect to another origin,
/// even an allowed one, arrives without them.
#[test]
fn credentials_stay_with_their_origin() {
    let (first, second) = (server(), server());
    let port = second.rsplit(':').next().unwrap();
    let fx = service(&[&first, &second], 1 << 20);
    let query = |path: &str, field: &str| {
        format!(
            r#"SELECT ?v WHERE {{ SERVICE <x-sparql-anything:location={first}{path},media-type=application/json,http.header.X-Test=yes> {{
                <http://sparql.xyz/facade-x/engine/properties> <http://sparql.xyz/facade-x/engine/http.auth.user> "Aladdin" .
                ?s xyz:{field} ?v }} }}"#
        )
    };
    // Same origin: sent.
    assert_eq!(values(&fx, &query("/echo", "test")).unwrap(), [r#""yes""#]);
    assert_ne!(values(&fx, &query("/echo", "auth")).unwrap(), [r#""""#]);
    // Another port: dropped.
    let away = format!("/to/{port}");
    assert_eq!(values(&fx, &query(&away, "test")).unwrap(), [r#""""#]);
    assert_eq!(values(&fx, &query(&away, "auth")).unwrap(), [r#""""#]);
    // A Host header, and a body on a GET, are refused.
    let host = query("/echo", "test").replace("X-Test=yes", "Host=other");
    assert!(values(&fx, &host).unwrap_err().contains("http.header.Host"));
    let get = query("/echo", "test").replace("X-Test=yes", "X-Test=yes,http.payload=x");
    assert!(values(&fx, &get).unwrap_err().contains("http.method=POST"));
}
