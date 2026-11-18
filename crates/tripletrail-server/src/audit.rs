//! What goes into the audit log (#207, TripleTrail Enterprise): every request that changes
//! something (an update, a Graph Store Protocol write, a load, a change under `/$/`: datasets,
//! access policies, keys, shapes, workspaces) and every refused attempt at one, with who sent
//! it, its outcome and its size. Queries and other reads of a dataset are logged only with
//! `--audit-reads`.
//!
//! The log itself (the file, its hash chain and `tripletrail audit verify`) is
//! `tripletrail_enterprise::audit`. It is written whatever the licence says, like row-level
//! policies are enforced: a lapsed licence never turns off the record of who changed what.

use crate::Context;
use crate::admin::Operation;
use crate::auth::Method;
use serde_json::{Map, Value, json};
use std::net::IpAddr;
use tripletrail::oxigraph::model::GraphName;

/// The graphs a line names at most; `moreGraphs` counts the rest (`added` and `removed` are
/// over all of them).
const MAX_GRAPHS: usize = 100;
use std::time::Duration;

#[allow(clippy::too_many_arguments)]
pub(crate) fn record(
    log: &tripletrail_enterprise::audit::Log,
    reads: bool,
    method: &str,
    url: &str,
    status: u16,
    elapsed: Duration,
    context: &Context,
    client: Option<IpAddr>,
    bytes: Option<u64>,
) {
    let safe = matches!(method, "GET" | "HEAD" | "OPTIONS");
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let event = match context.operation {
        Some(Operation::Update) => "update",
        Some(Operation::Upload) => "upload",
        Some(Operation::Query) if !context.changes && reads => "query",
        Some(Operation::Read) if !context.changes && reads => "read",
        Some(Operation::Query | Operation::Read) if !context.changes => return,
        // With reads logged, a read refused before its kind is known (no credentials).
        None if safe && reads && matches!(status, 401 | 403) => "read",
        None if safe => return,
        _ if path.starts_with("/$/") => "admin",
        _ => "change",
    };
    let mut line = Map::new();
    line.insert("event".into(), event.into());
    line.insert("method".into(), method.into());
    line.insert("path".into(), path.into());
    // The graph a Graph Store Protocol request names; the rest of the query string can hold
    // an update's text, which is not kept (its hash is).
    if let Some((_, graph)) =
        form_urlencoded::parse(query.as_bytes()).find(|(k, _)| k == "graph" || k == "default")
    {
        line.insert("graph".into(), graph.into_owned().into());
    }
    if let Some(entry) = &context.dataset {
        line.insert("dataset".into(), entry.name.clone().into());
    }
    line.insert("status".into(), status.into());
    line.insert(
        "outcome".into(),
        match status {
            0..=399 => "done",
            401 | 403 | 429 => "refused",
            _ => "failed",
        }
        .into(),
    );
    // With view-as, the admin who sent the request, and whom they viewed as.
    match context.viewer.as_ref().or(context.caller.as_ref()) {
        Some(c) => {
            line.insert("agent".into(), json!(c.agent));
            line.insert("groups".into(), json!(c.groups));
            line.insert("auth".into(), auth_name(c.method).into());
            if context.viewer.is_some()
                && let Some(v) = &context.caller
            {
                line.insert(
                    "viewAs".into(),
                    json!({"agent": v.agent, "groups": v.groups}),
                );
            }
        }
        // Before the caller was known (no credentials, a refused key) or without
        // authentication configured.
        None => {
            line.insert("agent".into(), Value::Null);
        }
    }
    if let Some(ip) = client {
        line.insert("client".into(), ip.to_string().into());
    }
    if let Some(n) = bytes {
        line.insert("bytes".into(), n.into());
    }
    if let Some(graphs) = &context.graph_changes
        && !graphs.is_empty()
    {
        line.insert(
            "graphs".into(),
            graphs
                .iter()
                .take(MAX_GRAPHS)
                .map(|g| {
                    json!({
                        "graph": match &g.graph {
                            GraphName::DefaultGraph => Value::Null,
                            GraphName::NamedNode(n) => n.as_str().into(),
                            GraphName::BlankNode(b) => b.to_string().into(),
                        },
                        "added": g.added,
                        "removed": g.removed,
                    })
                })
                .collect::<Vec<_>>()
                .into(),
        );
        if graphs.len() > MAX_GRAPHS {
            line.insert("moreGraphs".into(), (graphs.len() - MAX_GRAPHS).into());
        }
        let (added, removed) = graphs
            .iter()
            .fold((0, 0), |(a, r), g| (a + g.added, r + g.removed));
        line.insert("added".into(), added.into());
        line.insert("removed".into(), removed.into());
    }
    if context.graph_changes.is_none() {
        // A bulk load, or a write to a store that does not say what changed: what it did is
        // not known, which is not the same as nothing.
        line.insert("uncounted".into(), true.into());
    }
    if let Some(h) = context.text_hash {
        line.insert("textHash".into(), format!("{h:016x}").into());
    }
    line.insert(
        "ms".into(),
        json!((elapsed.as_secs_f64() * 1e5).round() / 100.0),
    );
    if let Err(e) = log.append(line) {
        // The change is done; the log could not say so. Loud, on every one.
        eprintln!("tripletrail: AUDIT LOG NOT WRITTEN: {e}");
    }
}

fn auth_name(method: Method) -> &'static str {
    match method {
        Method::ApiKey => "apiKey",
        Method::Password => "password",
        Method::TrustedHeaders => "trustedHeaders",
        Method::Oidc => "oidc",
        Method::Session => "session",
        Method::Anonymous => "anonymous",
    }
}
