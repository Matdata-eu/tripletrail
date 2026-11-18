//! The access admin API (#205), under `/$/access`, for server admins (`tripletrail:admin`):
//!
//! - `GET /$/access`: who may read and write each dataset, from which policy files, the API
//!   keys (without their hashes) and the groups in use;
//! - `GET /$/access/datasets/{name}/graphs`: the dataset's named graphs with their triple
//!   counts and labels (`dct:title`, `rdfs:label`), for the graph access matrix;
//! - `PUT /$/access/datasets/{name}/policies` (a policy as JSON) writes one `sg:Policy` into the
//!   dataset's Turtle policy file, and `DELETE …/policies?id=IRI` removes one. The rest of the
//!   file, comments and other policies included, stays as it was ([`crate::policy_edit`]);
//! - `POST /$/access/keys` creates an API key (shown once; only its hash is stored) and
//!   `DELETE /$/access/keys/{id}` revokes one;
//! - `PUT /$/access/anonymous` (`{"anonymous": true}`) turns anonymous access on or off;
//! - `GET /$/access/history` lists the versions of the access files, one per change made
//!   here, `GET /$/access/history/{n}` shows one with the version before it, and
//!   `POST /$/access/history/{n}/restore` puts it back ([`crate::access_history`]);
//! - with `auth.review` (#207, TripleTrail Enterprise), those changes (all but a key that is
//!   not an admin's, and revoking a key) wait for a second admin: `GET /$/access/proposals`
//!   lists them, `POST /$/access/proposals/{n}/approve` makes one and `…/reject` drops it
//!   ([`crate::access_review`]).

use crate::access_history;
use crate::access_review;
use crate::admin::Registry;
use crate::auth::Caller;
use crate::policy_edit::{self, PolicySpec};
use crate::{HttpError, Params, Reply, State, json_reply};
use serde_json::{Value, json};
use std::sync::Arc;
use tiny_http::{Method, Request};
use tripletrail::geo::sync::RwLockUnpoisoned;
use tripletrail::oxigraph::sparql::QueryResults;

pub(crate) fn route(
    state: &Arc<State>,
    request: &mut Request,
    rest: &[&str],
    params: &Params,
    caller: Option<&Caller>,
    viewing_as: bool,
) -> Result<Reply, HttpError> {
    let auth = state.auth.as_ref().ok_or_else(|| {
        HttpError(
            404,
            "Access control is not configured: start the server with --auth-config".to_owned(),
        )
    })?;
    if caller.is_some_and(|c| !c.is_admin()) {
        return Err(crate::auth::refuse(
            caller.expect("checked"),
            "The access pages need the group tripletrail:admin",
        ));
    }
    let method = request.method().clone();
    let changes = matches!(method, Method::Put | Method::Post | Method::Delete);
    // Viewing as someone else is for looking (#207): a change made, proposed or decided
    // under another agent's name would forge who made it, and let one admin approve alone.
    if changes && viewing_as {
        return Err(HttpError(
            403,
            "Access changes are made as yourself: leave X-TripleTrail-View-As out".to_owned(),
        ));
    }
    let by = caller.and_then(|c| c.agent.clone());
    let by = by.as_deref();
    if changes {
        // Before the first change, the files as they were found.
        access_history::start(auth)?;
    }
    route_one(state, request, rest, params, auth, &method, by)
}

/// A change of the access files made through these pages: applied at once, or, with
/// `auth.review` (#207), proposed and applied when a second admin approves it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Change {
    /// A policy (as JSON, see [`PolicySpec`]) written into a dataset's policy file.
    Policy { dataset: String, policy: Value },
    /// A policy removed, by its IRI.
    RemovePolicy { dataset: String, id: String },
    /// Anonymous access turned on or off.
    Anonymous(bool),
    /// A version of the access history put back.
    Restore(u64),
    /// An API key (its spec: agent, groups...) created.
    Key(Value),
}

impl Change {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Change::Policy { dataset, policy } => {
                json!({"kind": "policy", "dataset": dataset, "policy": policy})
            }
            Change::RemovePolicy { dataset, id } => {
                json!({"kind": "removePolicy", "dataset": dataset, "id": id})
            }
            Change::Anonymous(on) => json!({"kind": "anonymous", "anonymous": on}),
            Change::Restore(v) => json!({"kind": "restore", "version": v}),
            Change::Key(spec) => json!({"kind": "key", "key": spec}),
        }
    }

    pub(crate) fn from_json(v: &Value) -> Option<Change> {
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        Some(match v.get("kind")?.as_str()? {
            "policy" => Change::Policy {
                dataset: text("dataset")?,
                policy: v.get("policy")?.clone(),
            },
            "removePolicy" => Change::RemovePolicy {
                dataset: text("dataset")?,
                id: text("id")?,
            },
            "anonymous" => Change::Anonymous(v.get("anonymous")?.as_bool()?),
            "restore" => Change::Restore(v.get("version")?.as_u64()?),
            "key" => Change::Key(v.get("key")?.clone()),
            _ => return None,
        })
    }

    /// What the change does, for the history and the proposals.
    pub(crate) fn describe(&self) -> String {
        match self {
            Change::Policy { dataset, policy } => format!(
                "Policy {} of {dataset} written",
                policy["id"].as_str().unwrap_or("?")
            ),
            Change::RemovePolicy { dataset, id } => format!("Policy {id} of {dataset} removed"),
            Change::Anonymous(on) => {
                format!("Anonymous access {}", if *on { "on" } else { "off" })
            }
            Change::Restore(v) => format!("Restored version {v}"),
            Change::Key(spec) => format!(
                "API key for {} created",
                spec["agent"].as_str().unwrap_or("?")
            ),
        }
    }

    /// Whether `auth.review` holds the change for a second admin: every change but a key
    /// that does not make its holder an admin (such a key cannot approve anything).
    fn reviewed(&self) -> bool {
        match self {
            Change::Key(spec) => spec["groups"]
                .as_array()
                .is_some_and(|g| g.iter().any(|g| g.as_str() == Some(crate::auth::ADMIN))),
            _ => true,
        }
    }
}

/// Makes `change`, by `by`. `approved`: the proposal it comes from (its number and who
/// proposed it), which the access history says too.
pub(crate) fn apply(
    state: &Arc<State>,
    auth: &crate::auth::Auth,
    change: &Change,
    by: Option<&str>,
    approved: Option<(u64, Option<&str>)>,
) -> Result<(u16, Value), HttpError> {
    let note = |text: String| match approved {
        None => text,
        Some((n, proposer)) => format!(
            "{text} (proposal {n} by {}, approved)",
            proposer.unwrap_or("anonymous")
        ),
    };
    let record = |text: &str| access_history::record(auth, by, &note(text.to_owned()));
    let history = change.describe();
    let history = history.as_str();
    known_dataset(state, auth, change)?;
    match change {
        Change::Policy { dataset, policy } => {
            let spec = PolicySpec::from_json(policy).map_err(HttpError::bad_request)?;
            auth.edit_policy(dataset, &spec.id.clone(), |text| {
                let after = policy_edit::write(text, &spec)?;
                crate::row_policies::check_change(&state.options, text, &after)?;
                Ok(after)
            })
            .map_err(refused)?;
            record(history)?;
            Ok((200, json!({ "policy": spec.to_json() })))
        }
        Change::RemovePolicy { dataset, id } => {
            auth.edit_policy(dataset, id, |text| {
                let after = policy_edit::remove(text, id)?;
                crate::row_policies::check_change(&state.options, text, &after)?;
                Ok(after)
            })
            .map_err(refused)?;
            record(history)?;
            Ok((204, Value::Null))
        }
        Change::Key(spec) => {
            let (key, id) = auth.create_key(spec)?;
            let label = spec["label"].as_str().unwrap_or("");
            record(&format!("API key {id} {label} created").replace("  ", " "))?;
            Ok((201, json!({ "id": id, "key": key })))
        }
        Change::Anonymous(on) => {
            auth.set_anonymous(*on)?;
            record(history)?;
            Ok((200, json!({ "anonymous": on })))
        }
        Change::Restore(v) => {
            access_history::restore(auth, *v, by, &state.options, &note(history.to_owned()))?;
            Ok((200, json!({ "restored": v })))
        }
    }
}

/// A change names a dataset that is attached or configured: a typo must not create a
/// policy file (and with it, turn policies on and close every other dataset). Checked when
/// a change is proposed and again when it is made.
fn known_dataset(
    state: &Arc<State>,
    auth: &crate::auth::Auth,
    change: &Change,
) -> Result<(), HttpError> {
    if let Change::Policy { dataset, .. } | Change::RemovePolicy { dataset, .. } = change
        && !datasets(&state.registry.read_unpoisoned())
            .iter()
            .any(|d| d == dataset)
        && !auth.configures(dataset)
    {
        return Err(HttpError(404, format!("No dataset {dataset}")));
    }
    Ok(())
}

fn body_json(request: &mut Request, max: usize) -> Result<Value, HttpError> {
    let body = crate::read_body(request, Some(max))?;
    serde_json::from_str(&body)
        .map_err(|e| HttpError::bad_request(format!("The body is not JSON: {e}")))
}

fn route_one(
    state: &Arc<State>,
    request: &mut Request,
    rest: &[&str],
    params: &Params,
    auth: &crate::auth::Auth,
    method: &Method,
    by: Option<&str>,
) -> Result<Reply, HttpError> {
    let method = method.clone();
    let change = match rest {
        [] if matches!(method, Method::Get | Method::Head) => {
            let names = datasets(&state.registry.read_unpoisoned());
            return Ok(json_reply(200, &auth.overview(&names)));
        }
        ["datasets", name, "graphs"] if matches!(method, Method::Get | Method::Head) => {
            return graphs(state, name);
        }
        ["datasets", name, "explain"] if matches!(method, Method::Get | Method::Head) => {
            let token = crate::request_header(request, EXPLAIN_TOKEN).map(str::to_owned);
            return explain(state, params, auth, name, token.as_deref());
        }
        ["datasets", name, "policies"] if method == Method::Put => {
            let policy = body_json(request, 1 << 20)?;
            // Checked now, so that a proposal holds a policy that can be written.
            PolicySpec::from_json(&policy).map_err(HttpError::bad_request)?;
            Change::Policy {
                dataset: (*name).to_owned(),
                policy,
            }
        }
        ["datasets", name, "policies"] if method == Method::Delete => Change::RemovePolicy {
            dataset: (*name).to_owned(),
            id: params
                .get("id")
                .ok_or_else(|| HttpError::bad_request("id (the policy's IRI) is missing"))?
                .to_owned(),
        },
        ["keys"] if method == Method::Post => Change::Key(body_json(request, 1 << 16)?),
        ["keys", id] if method == Method::Delete => {
            // Revoking takes access away: never held for review.
            auth.revoke_key(id)?;
            access_history::record(auth, by, &format!("API key {id} revoked"))?;
            return Ok(Reply::bytes(204, "text/plain", Vec::new()));
        }
        ["anonymous"] if method == Method::Put => {
            let value = body_json(request, 1 << 10)?;
            Change::Anonymous(
                value["anonymous"]
                    .as_bool()
                    .ok_or_else(|| HttpError::bad_request("anonymous must be true or false"))?,
            )
        }
        ["history"] if matches!(method, Method::Get | Method::Head) => {
            return Ok(json_reply(200, &access_history::list(auth)));
        }
        ["history", n] if matches!(method, Method::Get | Method::Head) => {
            return Ok(json_reply(200, &access_history::show(auth, version(n)?)?));
        }
        ["history", n, "restore"] if method == Method::Post => {
            let v = version(n)?;
            // A version that is not there is not proposed.
            access_history::show(auth, v)?;
            Change::Restore(v)
        }
        ["proposals"] if matches!(method, Method::Get | Method::Head) => {
            return Ok(json_reply(200, &access_review::list(auth)));
        }
        ["proposals", n, decision @ ("approve" | "reject")] if method == Method::Post => {
            let n = n
                .parse()
                .map_err(|_| HttpError(404, format!("No proposal {n}")))?;
            let approve = *decision == "approve";
            if approve && auth.review() {
                builtin_refused(by)?;
            }
            return access_review::decide(state, auth, n, approve, by);
        }
        _ => return Err(HttpError(404, "No such access page".to_owned())),
    };
    if auth.review() && change.reviewed() {
        builtin_refused(by)?;
        known_dataset(state, auth, &change)?;
        return access_review::propose(auth, &change, by);
    }
    Ok(match apply(state, auth, &change, by, None)? {
        (204, _) => Reply::bytes(204, "text/plain", Vec::new()),
        (status, body) => json_reply(status, &body),
    })
}

/// With review, the built-in `--admin-token` neither proposes nor approves: it is a shared
/// secret without a name, so it cannot stand for one of two people. It can still reject.
fn builtin_refused(by: Option<&str>) -> Result<(), HttpError> {
    if by == Some(crate::auth::ADMIN_AGENT) {
        return Err(HttpError(
            403,
            "With review on, the built-in admin token cannot propose or approve access \
             changes: use a key of a named admin"
                .to_owned(),
        ));
    }
    Ok(())
}

/// A refused policy edit: 403 when row-level policies may not be changed (#818), else 422.
fn refused(e: String) -> HttpError {
    let code = if e.starts_with(crate::row_policies::REFUSED) {
        403
    } else {
        422
    };
    HttpError(code, e)
}

fn version(n: &str) -> Result<u64, HttpError> {
    n.parse()
        .map_err(|_| HttpError(404, format!("No version {n} in the access history")))
}

/// The attached datasets and aliases.
fn datasets(registry: &Registry) -> Vec<String> {
    registry
        .datasets
        .keys()
        .chain(registry.aliases.keys())
        .cloned()
        .collect()
}

/// The header with which the explain page is given a single sign-on token (#198): never a
/// query parameter, which would leave the token in logs.
const EXPLAIN_TOKEN: &str = "X-TripleTrail-Explain-Token";

/// `GET /$/access/datasets/{ds}/explain?agent=&groups=&resource=` (#202): what a caller
/// (`agent` an IRI or `anonymous`, the default; `groups` separated by commas) may do with a
/// dataset, the policies that give it, the row rules in force, and with `resource`, whether
/// that IRI is visible to the caller, which `sg:visibleIf` shapes hide it, which of its
/// properties `sg:hideProperties` hides, and which `sg:editableIf` shapes keep it from being
/// changed. With [`EXPLAIN_TOKEN`], the caller is the bearer of that single sign-on token
/// (#198), and `token.groupClaims` says which claim gave each group.
fn explain(
    state: &Arc<State>,
    params: &Params,
    auth: &crate::auth::Auth,
    name: &str,
    token: Option<&str>,
) -> Result<Reply, HttpError> {
    let entry = state
        .registry
        .read_unpoisoned()
        .resolve(name)
        .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?;
    // Built as "view as" builds it, from an admin, so the parameters are checked the same way.
    let admin = Caller {
        agent: None,
        groups: vec![crate::auth::ADMIN.to_owned()],
        method: crate::auth::Method::Anonymous,
        datasets: None,
    };
    // With a token, the caller is whom a single sign-on token says (#198), and the answer
    // says which claim gave each group.
    #[allow(unused_mut)]
    let mut token_json = serde_json::Value::Null;
    let caller = match token {
        #[cfg(feature = "enterprise")]
        Some(token) => {
            let id = auth.explain_token(token)?;
            token_json = json!({
                "issuer": id.issuer,
                "groupClaims": id.group_claims.iter()
                    .map(|(group, claim)| json!({ "group": group, "claim": claim }))
                    .collect::<Vec<_>>(),
            });
            Caller {
                agent: Some(id.agent),
                groups: id.groups,
                method: crate::auth::Method::Oidc,
                datasets: None,
            }
        }
        #[cfg(not(feature = "enterprise"))]
        Some(_) => {
            return Err(HttpError(
                400,
                format!("{EXPLAIN_TOKEN} (single sign-on) is a TripleTrail Enterprise feature"),
            ));
        }
        None => crate::auth::view_as(admin, &|header| match header {
            crate::auth::VIEW_AS => Some(params.get("agent").unwrap_or("anonymous").to_owned()),
            crate::auth::VIEW_AS_GROUPS => params.get("groups").map(str::to_owned),
            _ => None,
        })?,
    };
    let names = [name, entry.name.as_str()];
    let access = auth.access(&caller, &names);
    let graphs = |g: tripletrail::ReadableGraphs| -> Vec<String> {
        g.default_graph
            .then(|| "default".to_owned())
            .into_iter()
            .chain(g.named.iter().map(|n| n.as_str().to_owned()))
            .collect()
    };
    let rules = auth.row_rules(&caller, &names);
    let rule_json = |r: &tripletrail::RowRule| match r {
        tripletrail::RowRule::VisibleIf { shape, references } => {
            json!({ "rule": "sg:visibleIf", "shape": shape.to_string(), "hideReferences": references })
        }
        tripletrail::RowRule::HideProperties { shape } => {
            json!({ "rule": "sg:hideProperties", "shape": shape.to_string() })
        }
        tripletrail::RowRule::EditableIf { shape } => {
            json!({ "rule": "sg:editableIf", "shape": shape.to_string() })
        }
    };
    let mut out = json!({
        "dataset": entry.name,
        "agent": caller.agent,
        "groups": caller.groups,
        "read": access.read,
        "write": access.write,
        "admin": access.admin,
        "readableGraphs": (access.partial).then(|| graphs(auth.readable_graphs(&caller, &names))),
        "writableGraphs": (access.partial_write).then(|| graphs(auth.writable_graphs(&caller, &names))),
        "policies": auth.applying_policies(&caller, &names),
        "rowRules": rules.as_ref().map(|(_, r)| r.iter().map(rule_json).collect::<Vec<_>>()).unwrap_or_default(),
    });
    if !token_json.is_null() {
        out["token"] = token_json;
    }
    let may_read = access.read || access.partial;
    let readable = access
        .partial
        .then(|| auth.readable_graphs(&caller, &names));
    let row_rules = rules.as_ref().map(|(shapes, rules)| tripletrail::RowRules {
        shapes: Arc::clone(shapes),
        rules: rules.clone(),
        agent: caller.agent.clone(),
        graphs: readable.as_ref().map(|r| {
            r.default_graph
                .then_some(tripletrail::oxigraph::model::GraphName::DefaultGraph)
                .into_iter()
                .chain(
                    r.named
                        .iter()
                        .cloned()
                        .map(tripletrail::oxigraph::model::GraphName::NamedNode),
                )
                .collect()
        }),
    });
    // Each triple pattern of a query, with how many matches it has and how many the caller
    // sees.
    if let Some(query) = params.get("query") {
        let count = |pattern: &str, options: Option<tripletrail::QueryOptions>| {
            let text = format!("SELECT (COUNT(*) AS ?n) WHERE {{ {pattern} }}");
            let results = match options {
                Some(options) => entry.engine.query_with_options(&text, options),
                None => entry.engine.query(&text),
            }
            .map_err(|e| HttpError(500, e.to_string()))?;
            let QueryResults::Solutions(mut rows) = results else {
                return Ok(0);
            };
            Ok::<u64, HttpError>(
                rows.next()
                    .transpose()
                    .map_err(|e| HttpError(500, e.to_string()))?
                    .and_then(|r| match r.get("n") {
                        Some(tripletrail::oxigraph::model::Term::Literal(l)) => {
                            l.value().parse().ok()
                        }
                        _ => None,
                    })
                    .unwrap_or(0),
            )
        };
        let mut patterns = Vec::new();
        for pattern in tripletrail::Engine::query_patterns(query)? {
            let all = count(&pattern, None)?;
            let visible = if may_read {
                count(
                    &pattern,
                    Some(tripletrail::QueryOptions {
                        readable: readable.clone(),
                        row_rules: row_rules.clone(),
                        ..Default::default()
                    }),
                )?
            } else {
                0
            };
            patterns.push(json!({
                "pattern": pattern,
                "matches": all,
                "visible": visible,
                "restricted": visible < all,
            }));
        }
        out["query"] = json!(patterns);
    }
    if let Some(resource) = params.get("resource") {
        let iri = tripletrail::oxigraph::model::NamedNode::new(resource)
            .map_err(|e| HttpError::bad_request(format!("resource is not an IRI: {e}")))?;
        let explained = match (&row_rules, may_read) {
            (Some(row_rules), true) => entry.engine.explain_resource(row_rules, iri.as_ref())?,
            _ => tripletrail::ResourceExplained::default(),
        };
        let terms = |t: &[tripletrail::oxigraph::model::Term]| -> Vec<String> {
            t.iter().map(ToString::to_string).collect()
        };
        // Whether the resource is the subject of any quad, and of one in a graph the caller
        // may read: "visible" is about the rules, and says nothing of either.
        let mut exists = false;
        let mut readable_quads = false;
        for quad in
            entry
                .engine
                .store()
                .quads_for_pattern(Some(iri.as_ref().into()), None, None, None)
        {
            let quad = quad.map_err(|e| HttpError(500, e.to_string()))?;
            exists = true;
            if access.read
                || readable
                    .as_ref()
                    .is_some_and(|r| r.contains(quad.graph_name.as_ref()))
            {
                readable_quads = true;
                break;
            }
        }
        out["resource"] = json!({
            "iri": iri.as_str(),
            "exists": exists,
            "readable": may_read && readable_quads,
            "visible": may_read && explained.hidden_by.is_empty(),
            "hiddenBy": terms(&explained.hidden_by),
            "hiddenProperties": explained.hidden_properties.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            // In the graphs the caller may write, when only some.
            "editable": (access.write || access.partial_write) && explained.hidden_by.is_empty() && explained.not_editable_by.is_empty(),
            "notEditableBy": terms(&explained.not_editable_by),
        });
    }
    Ok(json_reply(200, &out))
}

/// The named graphs of a dataset with their triple counts and labels.
fn graphs(state: &Arc<State>, name: &str) -> Result<Reply, HttpError> {
    let entry = state
        .registry
        .read_unpoisoned()
        .resolve(name)
        .ok_or_else(|| HttpError(404, format!("No dataset {name}")))?;
    let rows = |query: &str| -> Result<Vec<Vec<Option<String>>>, HttpError> {
        let QueryResults::Solutions(solutions) = entry
            .engine
            .query(query)
            .map_err(|e| HttpError(500, e.to_string()))?
        else {
            return Ok(Vec::new());
        };
        let variables: Vec<_> = solutions.variables().to_vec();
        solutions
            .map(|row| {
                let row = row.map_err(|e| HttpError(500, e.to_string()))?;
                Ok(variables
                    .iter()
                    .map(|v| {
                        row.get(v).map(|t| match t {
                            tripletrail::oxigraph::model::Term::Literal(l) => l.value().to_owned(),
                            tripletrail::oxigraph::model::Term::NamedNode(n) => {
                                n.as_str().to_owned()
                            }
                            other => other.to_string(),
                        })
                    })
                    .collect())
            })
            .collect()
    };
    let counts = rows("SELECT ?g (COUNT(*) AS ?n) { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g")?;
    let labels = rows(
        "SELECT ?g (SAMPLE(?l) AS ?label) { GRAPH ?any { ?g <http://purl.org/dc/terms/title>|<http://www.w3.org/2000/01/rdf-schema#label> ?l } } GROUP BY ?g",
    )?;
    let label = |g: &str| {
        labels
            .iter()
            .find(|r| r[0].as_deref() == Some(g))
            .and_then(|r| r[1].clone())
    };
    let mut graphs: Vec<Value> = counts
        .iter()
        .filter_map(|r| {
            let g = r[0].as_deref()?;
            Some(json!({
                "graph": g,
                "triples": r[1].as_deref().and_then(|n| n.parse::<u64>().ok()),
                "label": label(g),
            }))
        })
        .collect();
    graphs.sort_by(|a, b| a["graph"].as_str().cmp(&b["graph"].as_str()));
    Ok(json_reply(
        200,
        &json!({ "dataset": name, "defaultGraph": crate::policy::DEFAULT_GRAPH, "graphs": graphs }),
    ))
}
