//! Remote locations: `location=https://…`, read over HTTP.
//!
//! Off unless [`RemoteLocations::allow`] lists URL prefixes, as `SERVICE <https://…>` is: a
//! query on a server must not reach hosts inside its network. A redirect is followed only to
//! a URL on the allowlist. One read waits at most [`RemoteLocations::timeout`] and reads at
//! most [`RemoteLocations::max_bytes`].
//!
//! SPARQL Anything's options: `http.method` (GET or POST), `http.payload` (the body of a
//! POST), `http.header.<name>`, `http.query.<name>` (added to the URL's query),
//! `http.auth.user` and `http.auth.password` (HTTP Basic), and `http.redirect` (default
//! true). Headers and credentials are sent only to the first URL's scheme, host and port,
//! not after a redirect to another; the query parameters are not added to a redirect's URL.

use crate::{FxError, Options};
use oxiri::Iri;
use std::time::Duration;

/// Which URLs a location may name, and how they are read.
#[derive(Debug, Clone)]
pub struct RemoteLocations {
    /// URL prefixes that may be read (`https://data.example.org/`): same scheme, host and
    /// port, and a path prefix on a `/` boundary. Empty: none.
    pub allow: Vec<String>,
    /// The longest one read may take, connecting and redirects included.
    pub timeout: Duration,
    /// The largest body read; a larger one is an error.
    pub max_bytes: usize,
}

impl Default for RemoteLocations {
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            timeout: Duration::from_secs(30),
            max_bytes: 256 << 20,
        }
    }
}

/// Redirects followed at most.
#[cfg(feature = "remote")]
const MAX_REDIRECTS: usize = 5;

/// Whether a location is a URL to read over HTTP.
pub(crate) fn is_remote(location: &str) -> bool {
    let lower = location.get(..8).unwrap_or(location).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Whether `url` is on `allow`: an entry with the same scheme, host and port whose path is a
/// prefix of the URL's on a segment boundary. URLs with user info, and paths with `.` or `..`
/// segments, are never allowed.
pub fn allows(allow: &[String], url: &str) -> bool {
    let Some(url) = Endpoint::parse(url) else {
        return false;
    };
    allow
        .iter()
        .filter_map(|entry| Endpoint::parse(entry))
        .any(|entry| entry.covers(&url))
}

/// The parts of an http(s) IRI the allowlist compares.
#[derive(Debug, PartialEq)]
struct Endpoint {
    scheme: String,
    host: String,
    port: u16,
    /// The path, without its query and fragment.
    path: String,
}

impl Endpoint {
    /// `None` for anything but an http(s) IRI with a host and no user info.
    fn parse(iri: &str) -> Option<Self> {
        let iri = Iri::parse(iri).ok()?;
        let scheme = iri.scheme().to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => return None,
        };
        let authority = iri.authority()?;
        if authority.contains('@') {
            return None;
        }
        // `[v6]:port`, or `host:port`.
        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            let (host, after) = rest.split_once(']')?;
            (format!("[{host}]"), after.strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host.to_owned(), Some(port)),
                None => (authority.to_owned(), None),
            }
        };
        let port = match port {
            None | Some("") => default_port,
            Some(p) => p.parse().ok()?,
        };
        if host.is_empty() {
            return None;
        }
        // `.` and `..` segments (also percent-encoded) would let a client's normalisation
        // move the call out of an allowed path.
        let path = iri.path();
        if path.split('/').any(|segment| {
            let segment = segment.to_ascii_lowercase().replace("%2e", ".");
            segment == "." || segment == ".."
        }) {
            return None;
        }
        Some(Self {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
            path: iri.path().to_owned(),
        })
    }

    /// Whether this allowlist entry covers `url`.
    fn covers(&self, url: &Self) -> bool {
        if (&self.scheme, &self.host, self.port) != (&url.scheme, &url.host, url.port) {
            return false;
        }
        let prefix = self.path.as_str();
        let path = url.path.as_str();
        prefix.is_empty()
            || prefix == "/"
            || path == prefix
            || (path.starts_with(prefix)
                && (prefix.ends_with('/') || path[prefix.len()..].starts_with('/')))
    }
}

/// A body read from a URL, and its media type.
pub(crate) struct Fetched {
    pub bytes: Vec<u8>,
    pub media_type: Option<String>,
}

/// `url` with the `http.query.<name>` options added to its query, percent-encoded.
#[cfg(feature = "remote")]
fn with_query(url: &str, options: &Options) -> String {
    let mut params: Vec<(&str, &str)> = options
        .0
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("http.query.")?, v.as_str())))
        .collect();
    if params.is_empty() {
        return url.to_owned();
    }
    params.sort();
    let (base, fragment) = match url.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (url, None),
    };
    let mut out = base.to_owned();
    for (i, (k, v)) in params.iter().enumerate() {
        out.push(if i == 0 && !base.contains('?') {
            '?'
        } else {
            '&'
        });
        out.push_str(&encode(k));
        out.push('=');
        out.push_str(&encode(v));
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

/// Percent-encodes all but unreserved characters.
#[cfg(feature = "remote")]
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Base64 of `bytes`, for HTTP Basic.
#[cfg(feature = "remote")]
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Sends `request` with `timeout` for the whole of it.
#[cfg(feature = "remote")]
fn run<S: ureq::AsSendBody>(
    agent: &ureq::Agent,
    request: ureq::http::Request<S>,
    timeout: std::time::Duration,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let request = agent
        .configure_request(request)
        .timeout_global(Some(timeout))
        .build();
    agent.run(request)
}

/// Reads `location` over HTTP, under `config` and the `http.*` options.
#[cfg(feature = "remote")]
pub(crate) fn fetch(
    config: &RemoteLocations,
    location: &str,
    options: &Options,
) -> Result<Fetched, FxError> {
    use std::io::Read;
    let denied = |url: &str| {
        FxError::Denied(if config.allow.is_empty() {
            format!("{url}: reading URLs is off (allow URL prefixes, as with lift --allow-url)")
        } else {
            format!("{url} is not on the list of URLs sources may be read from")
        })
    };
    let fail = |url: &str, e: &dyn std::fmt::Display| FxError::Read {
        path: url.to_owned(),
        source: std::io::Error::other(e.to_string()),
    };
    let mut method = options
        .get("http.method")
        .unwrap_or("GET")
        .to_ascii_uppercase();
    if method != "GET" && method != "POST" {
        return Err(FxError::Unsupported(format!(
            "http.method {method} (GET or POST)"
        )));
    }
    let mut payload = options.get("http.payload").map(str::to_owned);
    if payload.is_some() && method == "GET" {
        return Err(FxError::Option(
            "http.payload is the body of a POST: set http.method=POST".into(),
        ));
    }
    if options.0.keys().any(|k| {
        k.strip_prefix("http.header.")
            .is_some_and(|h| h.eq_ignore_ascii_case("host"))
    }) {
        return Err(FxError::Option(
            "http.header.Host: the host is the URL's".into(),
        ));
    }
    let follow = options.flag("http.redirect", true)?;
    let mut url = with_query(location, options);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(config.timeout))
        // ureq 2's connect timeout, kept with ureq 3 (which has none by default).
        .timeout_connect(Some(std::time::Duration::from_secs(30)))
        // Redirects are followed here, each checked against the allowlist.
        .max_redirects(0)
        .http_status_as_error(false)
        // No proxy from the environment, as before ureq 3.
        .proxy(None)
        .user_agent(concat!("tripletrail/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let started = std::time::Instant::now();
    // Headers and credentials go only to the first URL's origin, as curl and browsers do: a
    // redirect to another host, even an allowed one, must not carry them.
    let origin = Endpoint::parse(&url).map(|e| (e.scheme, e.host, e.port));
    for hop in 0..=MAX_REDIRECTS {
        if !allows(&config.allow, &url) {
            return Err(denied(&url));
        }
        let left = config
            .timeout
            .checked_sub(started.elapsed())
            .ok_or_else(|| fail(&url, &"timed out"))?;
        let mut request = ureq::http::Request::builder()
            .method(method.as_str())
            .uri(url.as_str());
        let same_origin =
            origin.is_some() && Endpoint::parse(&url).map(|e| (e.scheme, e.host, e.port)) == origin;
        for (name, value) in options.0.iter().filter(|_| same_origin) {
            if let Some(header) = name.strip_prefix("http.header.") {
                request = request.header(header, value.as_str());
            }
        }
        if same_origin && let Some(user) = options.get("http.auth.user") {
            let password = options.get("http.auth.password").unwrap_or_default();
            request = request.header(
                "Authorization",
                format!("Basic {}", base64(format!("{user}:{password}").as_bytes())),
            );
        }
        let response = match &payload {
            Some(body) => request
                .body(body.clone())
                .map_err(|e| fail(&url, &e))
                .and_then(|r| run(&agent, r, left).map_err(|e| fail(&url, &e))),
            None => request
                .body(())
                .map_err(|e| fail(&url, &e))
                .and_then(|r| run(&agent, r, left).map_err(|e| fail(&url, &e))),
        }?;
        let status = response.status().as_u16();
        if status >= 400 {
            return Err(fail(&url, &format!("HTTP status {status}")));
        }
        if (300..400).contains(&status) {
            let Some(next) = response
                .headers()
                .get("Location")
                .and_then(|v| v.to_str().ok())
            else {
                return Err(fail(
                    &url,
                    &format!("HTTP status {status} without a Location"),
                ));
            };
            if !follow {
                return Err(fail(
                    &url,
                    &format!("HTTP status {status}: a redirect, and http.redirect is false"),
                ));
            }
            if hop == MAX_REDIRECTS {
                return Err(fail(&url, &format!("more than {MAX_REDIRECTS} redirects")));
            }
            let base = Iri::parse(url.clone()).map_err(|e| fail(&url, &e))?;
            url = base.resolve(next).map_err(|e| fail(&url, &e))?.into_inner();
            // As browsers do: 307 and 308 repeat the request, the others become a GET.
            if status != 307 && status != 308 {
                method = "GET".to_owned();
                payload = None;
            }
            continue;
        }
        let media_type = response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .map(|m| {
                m.split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
            })
            .filter(|m| !m.is_empty());
        let mut bytes = Vec::new();
        let limit = config.max_bytes;
        response
            .into_body()
            .into_reader()
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| fail(&url, &e))?;
        if bytes.len() > limit {
            return Err(fail(
                &url,
                &format!("the body is larger than {limit} bytes"),
            ));
        }
        return Ok(Fetched { bytes, media_type });
    }
    unreachable!("the loop returns by its last hop")
}

/// The body at `url`, read with no `http.*` options (for RML sources).
pub fn read(config: &RemoteLocations, url: &str) -> Result<Vec<u8>, FxError> {
    fetch(config, url, &Options::default()).map(|f| f.bytes)
}

/// Whether `location` is an `http://` or `https://` URL, which [`read`] reads.
pub fn is_url(location: &str) -> bool {
    is_remote(location)
}

/// Without the `remote` feature, URLs cannot be read.
#[cfg(not(feature = "remote"))]
pub(crate) fn fetch(
    _config: &RemoteLocations,
    location: &str,
    _options: &Options,
) -> Result<Fetched, FxError> {
    Err(FxError::Unsupported(format!(
        "{location}: this build reads no URLs (tripletrail-fx without the remote feature)"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(entry: &str, url: &str) -> bool {
        allows(&[entry.to_owned()], url)
    }

    #[test]
    fn allowlist() {
        assert!(allowed(
            "https://example.org",
            "https://example.org/data.csv"
        ));
        assert!(allowed(
            "https://example.org/data/",
            "https://example.org/data/a.csv"
        ));
        assert!(allowed(
            "https://example.org/data",
            "https://example.org/data/a.csv"
        ));
        assert!(!allowed(
            "https://example.org/data",
            "https://example.org/database.csv"
        ));
        assert!(!allowed("https://example.org", "http://example.org/a.csv"));
        assert!(!allowed(
            "https://example.org",
            "https://example.org:8443/a.csv"
        ));
        assert!(!allowed(
            "https://example.org",
            "https://user@example.org/a.csv"
        ));
        assert!(!allowed(
            "https://example.org/data/",
            "https://example.org/data/../x"
        ));
        assert!(!allowed(
            "https://example.org/data/",
            "https://example.org/data/%2E%2E/x"
        ));
        assert!(!allows(&[], "https://example.org/a.csv"));
    }

    #[cfg(feature = "remote")]
    #[test]
    fn query_and_basic() {
        let mut options = Options::default();
        options.0.insert("http.query.q".into(), "a b&c".into());
        options.0.insert("http.query.n".into(), "1".into());
        assert_eq!(
            with_query("https://x.org/a?z=0#f", &options),
            "https://x.org/a?z=0&n=1&q=a%20b%26c#f"
        );
        assert_eq!(
            base64(b"Aladdin:open sesame"),
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        assert_eq!(base64(b"ab"), "YWI=");
    }
}
