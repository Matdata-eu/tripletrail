//! Requests that a web browser sends on behalf of another site.
//!
//! Two checks keep a page of another site from using a visitor's browser against the server:
//!
//! - [`cross_site_change`]: a change (anything but `GET`, `HEAD` and `OPTIONS`) sent by a page
//!   of another site is refused, whatever the caller signs in with. Browsers attach saved
//!   credentials (a password for HTTP Basic, a proxy's cookie) to a plain HTML form posted
//!   from anywhere, so without it a page the user opens could change the data in their name.
//!   Clients that are not browsers send neither `Origin` nor `Sec-Fetch-Site` and pass.
//! - [`check_host`]: on a server without authentication, a request is answered only for a
//!   host name it is meant to answer to. Otherwise a page could point its own name at the
//!   server's address (DNS rebinding) and read the answers as a page of the same origin.

use crate::{HttpError, State, request_header};
use tiny_http::{Method, Request};

/// Refuses a change that a page of another site sends from a browser.
///
/// Allowed: a browser that says the request comes from the server's own pages
/// (`Sec-Fetch-Site: same-origin`) or from the user (`none`), an `Origin` that is the
/// server's own (its `Host` or `X-Forwarded-Host`, or an exact `--allowed-host` name: not `*`
/// or `.domain`), an origin listed
/// with `--cors`, and with `--cors` for every origin, a body a form cannot send. Without
/// `Origin` and `Sec-Fetch-Site`, the request is not from a browser page and passes.
pub(crate) fn cross_site_change(
    state: &State,
    request: &Request,
    method: &Method,
) -> Result<(), HttpError> {
    if matches!(method, Method::Get | Method::Head | Method::Options) {
        return Ok(());
    }
    let fetch_site = request_header(request, "Sec-Fetch-Site").map(str::trim);
    if matches!(fetch_site, Some("same-origin" | "none")) {
        return Ok(());
    }
    let refuse = |from: &str| {
        Err(HttpError(
            403,
            format!(
                "A change sent by a web page of another site ({from}) is refused: list its \
                 origin with --cors to let its pages change data here"
            ),
        ))
    };
    let Some(origin) = request_header(request, "Origin").map(str::trim) else {
        return match fetch_site {
            Some(site @ ("cross-site" | "same-site")) => refuse(site),
            _ => Ok(()),
        };
    };
    if own_origin(state, request, origin) || state.options.cors_origins.iter().any(|o| o == origin)
    {
        return Ok(());
    }
    // With `--cors` for every origin, a cross-origin script may change data, but only with
    // a request the browser asks about first and then sends without credentials. A form (or
    // a "simple" fetch) is sent with them, unasked: those stay refused.
    if state.options.cors && !form_body(request) {
        return Ok(());
    }
    refuse(origin)
}

/// Whether `origin` (`scheme://host[:port]`) is this server as the browser reached it.
fn own_origin(state: &State, request: &Request, origin: &str) -> bool {
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    let wanted = normal_authority(&scheme, authority);
    let hosts = [
        request_header(request, "Host"),
        request_header(request, "X-Forwarded-Host").and_then(|h| h.split(',').next()),
    ];
    hosts
        .into_iter()
        .flatten()
        .any(|h| normal_authority(&scheme, h.trim()) == wanted)
        || state.options.allowed_hosts.iter().any(|allowed| {
            // Only an exact name: `*` and `.domain` say which names to answer for, not which
            // sites' pages may change data. Any scheme and port of that name count.
            let allowed = allowed.trim().trim_end_matches('.');
            !allowed.is_empty()
                && allowed != "*"
                && !allowed.starts_with('.')
                && host_of(authority).eq_ignore_ascii_case(allowed)
        })
        || sign_in_origin(state, origin)
}

/// Whether `origin` is the public origin of the enterprise browser sign-in (#198).
#[cfg(feature = "enterprise")]
fn sign_in_origin(state: &State, origin: &str) -> bool {
    state
        .auth
        .as_ref()
        .and_then(crate::auth::Auth::browser_sign_in)
        .is_some_and(|s| s.is_own_origin(origin))
}

#[cfg(not(feature = "enterprise"))]
fn sign_in_origin(_: &State, _: &str) -> bool {
    false
}

/// `host[:port]` in lower case, without the scheme's default port.
fn normal_authority(scheme: &str, authority: &str) -> String {
    let authority = authority.trim_end_matches('/').to_ascii_lowercase();
    let default = match scheme {
        "https" => ":443",
        "http" => ":80",
        _ => return authority,
    };
    authority
        .strip_suffix(default)
        .map_or(authority.clone(), str::to_owned)
}

/// Whether the body is one an HTML form (or a fetch without a preflight) can send.
fn form_body(request: &Request) -> bool {
    match crate::content_type(request) {
        None => true,
        Some(t) => matches!(
            t.as_str(),
            "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
        ),
    }
}

/// Refuses a request for a host name this server does not answer to, on a server without
/// authentication (with it, a page of another name has no credentials to use).
///
/// Always answered: no `Host` (HTTP/1.0), an IP address, `localhost` and names under
/// `.localhost` (which browsers keep on this machine), and the `--allowed-host` names (`*`
/// for any). Other names are answered when the server listens beyond loopback and no
/// `--allowed-host` is given, as before (it warns at start); otherwise refused.
pub(crate) fn check_host(state: &State, request: &Request) -> Result<(), HttpError> {
    if state.auth.is_some() {
        return Ok(());
    }
    let Some(host) = request_header(request, "Host").map(str::trim) else {
        return Ok(());
    };
    let name = host_of(host);
    if name.is_empty() || is_ip(name) || name == "localhost" || name.ends_with(".localhost") {
        return Ok(());
    }
    let allowed = &state.options.allowed_hosts;
    if allowed.iter().any(|a| host_matches(a, name)) {
        return Ok(());
    }
    if allowed.is_empty() && !state.loopback {
        return Ok(());
    }
    Err(HttpError(
        403,
        format!(
            "This server does not answer for the host name {name}: start it with \
             --allowed-host {name} if that is its name (a check against DNS rebinding, on \
             servers without authentication)"
        ),
    ))
}

/// The host of `host[:port]` (`[::1]:7878` → `[::1]`), in lower case, without a final dot.
fn host_of(authority: &str) -> &str {
    let host = if authority.starts_with('[') {
        authority
            .find(']')
            .map_or(authority, |end| &authority[..=end])
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    host.strip_suffix('.').unwrap_or(host)
}

fn is_ip(host: &str) -> bool {
    host.starts_with('[') || host.parse::<std::net::Ipv4Addr>().is_ok()
}

/// Whether `allowed` (`kg.example.org`, `.example.org` for it and every name under it, or
/// `*`) covers `host`.
pub(crate) fn host_matches(allowed: &str, host: &str) -> bool {
    let allowed = allowed.trim().trim_end_matches('.');
    if allowed == "*" {
        return true;
    }
    match allowed.strip_prefix('.') {
        Some(domain) => {
            host.eq_ignore_ascii_case(domain)
                || (host.len() > domain.len() + 1
                    && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
                    && host[host.len() - domain.len()..].eq_ignore_ascii_case(domain))
        }
        None => host.eq_ignore_ascii_case(allowed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert_eq!(host_of("kg.example.org:7878"), "kg.example.org");
        assert_eq!(host_of("[::1]:7878"), "[::1]");
        assert_eq!(host_of("kg.example.org."), "kg.example.org");
        assert!(is_ip("127.0.0.1") && is_ip("[::1]") && !is_ip("evil.example"));
        assert!(host_matches("kg.example.org", "KG.example.org"));
        assert!(host_matches(".example.org", "kg.example.org"));
        assert!(host_matches(".example.org", "example.org"));
        assert!(!host_matches(".example.org", "badexample.org"));
        assert!(!host_matches("example.org", "example.org.evil.com"));
        assert!(host_matches("*", "anything"));
    }

    #[test]
    fn authorities() {
        assert_eq!(
            normal_authority("https", "KG.example.org:443"),
            "kg.example.org"
        );
        assert_eq!(
            normal_authority("http", "kg.example.org:80"),
            "kg.example.org"
        );
        assert_eq!(
            normal_authority("http", "kg.example.org:7878"),
            "kg.example.org:7878"
        );
    }
}
