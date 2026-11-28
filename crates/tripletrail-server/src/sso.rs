//! Browser sign-in with single sign-on (#198, TripleTrail Enterprise): `/$/sso`.
//!
//! - `GET /$/sso`: the issuers people can sign in with, and who is signed in (none without
//!   browser sign-in).
//! - `GET /$/sso/login?issuer=N&return=PATH`: off to the provider.
//! - `GET /$/sso/callback`: back from it; sets the session cookie and returns to `PATH`.
//! - `POST /$/sso/logout`: ends the session.
//!
//! These answer without credentials, as `/$/ping` does: they are how a browser gets them.

use crate::{HttpError, Params, Reply, State, json_reply, request_header};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tiny_http::{Method, Request};
use tripletrail_enterprise::sso::{COOKIE, FLOW_COOKIE, SignIn};

pub(crate) fn route(
    state: &Arc<State>,
    request: &Request,
    rest: &[&str],
    params: &Params,
) -> Result<Reply, HttpError> {
    let sign_in = state.auth.as_ref().and_then(|a| a.browser_sign_in());
    let method = request.method();
    let get = matches!(method, Method::Get | Method::Head);
    // Without browser sign-in there is nothing to choose: the web UI asks on every page, so
    // the answer is an empty list, not a 404 its console would show (#369).
    if sign_in.is_none() && rest.is_empty() && get {
        return Ok(json_reply(
            200,
            &serde_json::json!({ "signIn": [], "signedIn": null }),
        ));
    }
    let sign_in = sign_in.ok_or_else(|| {
        HttpError(
            404,
            "Browser sign-in is not configured: give an auth.oidc issuer a browser client"
                .to_owned(),
        )
    })?;
    match rest {
        [] if get => Ok(overview(&sign_in, request)),
        ["login"] if get => {
            licensed(state)?;
            let issuer = match params.get("issuer") {
                None => None,
                Some(i) => Some(
                    i.parse()
                        .map_err(|_| HttpError(400, "issuer must be a number".to_owned()))?,
                ),
            };
            let (location, flow) = sign_in
                .start(
                    issuer,
                    params.get("return").unwrap_or("/$/ui/"),
                    crate::client_address(state, request),
                )
                .map_err(|e| HttpError(502, e))?;
            let mut reply = redirect(&location);
            // Ties the sign-in to this browser: the callback needs it back (login CSRF).
            reply
                .headers
                .push(("Set-Cookie", flow_cookie(&flow, 600, sign_in.secure())));
            reply.headers.push(("Cache-Control", "no-store".to_owned()));
            Ok(reply)
        }
        ["callback"] if get => {
            licensed(state)?;
            if let Some(error) = params.get("error") {
                let why = params.get("error_description").unwrap_or(error);
                return Err(HttpError(
                    401,
                    format!("The provider did not sign you in: {why}"),
                ));
            }
            let (Some(code), Some(st)) = (params.get("code"), params.get("state")) else {
                return Err(HttpError(400, "code and state are missing".to_owned()));
            };
            let header = |name: &str| header_value(request, name);
            let flow = cookie_value(&header, FLOW_COOKIE);
            let done = sign_in
                .finish(st, code, flow.as_deref(), SystemTime::now())
                .map_err(|e| HttpError(401, e))?;
            let mut reply = redirect(&done.return_to);
            reply.headers.push((
                "Set-Cookie",
                cookie(&done.cookie, done.max_age.as_secs(), done.secure),
            ));
            reply
                .headers
                .push(("Set-Cookie", flow_cookie("", 0, done.secure)));
            reply.headers.push(("Cache-Control", "no-store".to_owned()));
            Ok(reply)
        }
        ["logout"] if *method == Method::Post => {
            if !same_origin(&sign_in, request) {
                return Err(HttpError(
                    403,
                    "Signing out must come from this server's own pages".to_owned(),
                ));
            }
            let header = |name: &str| header_value(request, name);
            if let Some(id) = crate::auth::session_cookie(&header) {
                sign_in.sign_out(&id);
            }
            let mut reply = Reply::empty(204);
            reply.content_type = None;
            reply
                .headers
                .push(("Set-Cookie", cookie("", 0, sign_in.secure())));
            Ok(reply)
        }
        _ => Err(HttpError(404, "Not found".to_owned())),
    }
}

/// Single sign-on needs a licence that covers it.
fn licensed(state: &State) -> Result<(), HttpError> {
    if state
        .options
        .license_status()
        .allows(tripletrail_enterprise::features::SSO)
    {
        Ok(())
    } else {
        Err(HttpError(
            403,
            "Single sign-on needs a TripleTrail Enterprise licence that covers sso".to_owned(),
        ))
    }
}

fn overview(sign_in: &SignIn, request: &Request) -> Reply {
    let header = |name: &str| header_value(request, name);
    let session =
        crate::auth::session_cookie(&header).and_then(|id| sign_in.session(&id, SystemTime::now()));
    json_reply(
        200,
        &serde_json::json!({
            "signIn": sign_in.choices().iter().map(|c| serde_json::json!({
                "name": c.name,
                "issuer": c.issuer,
                "login": format!("/$/sso/login?issuer={}", c.index),
            })).collect::<Vec<_>>(),
            "signedIn": session.map(|s| serde_json::json!({
                "agent": s.identity.agent,
                "groups": s.identity.groups,
                "groupClaims": s.identity.group_claims.iter()
                    .map(|(group, claim)| serde_json::json!({ "group": group, "claim": claim }))
                    .collect::<Vec<_>>(),
                "issuer": s.identity.issuer,
                "ends": s.ends.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            })),
        }),
    )
}

fn redirect(location: &str) -> Reply {
    let mut reply = Reply::empty(302);
    reply.content_type = None;
    reply.headers.push(("Location", location.to_owned()));
    reply
}

fn cookie(value: &str, max_age: u64, secure: bool) -> String {
    format!(
        "{COOKIE}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )
}

/// The cookie of a sign-in under way: only sent back to `/$/sso`.
fn flow_cookie(value: &str, max_age: u64, secure: bool) -> String {
    format!(
        "{FLOW_COOKIE}={value}; Path=/$/sso; Max-Age={max_age}; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )
}

fn cookie_value(header: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    header("Cookie")?.split(';').find_map(|c| {
        let (n, v) = c.trim().split_once('=')?;
        (n == name && !v.is_empty()).then(|| v.trim_matches('"').to_owned())
    })
}

fn header_value(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_owned())
}

/// Whether a request comes from this server's own pages: its `Origin` is the server's public
/// origin, or, without one, the browser says it is same-origin (`Sec-Fetch-Site`). A browser
/// sends one of the two with every change; other sites cannot forge them. This is what
/// keeps another site from making a signed-in browser change things here (CSRF).
pub(crate) fn same_origin(sign_in: &SignIn, request: &Request) -> bool {
    match request_header(request, "Origin") {
        Some(origin) => sign_in.is_own_origin(origin),
        None => request_header(request, "Sec-Fetch-Site") == Some("same-origin"),
    }
}
