//! Writes to the REST API (#143): records created, replaced, changed and deleted as JSON,
//! checked against the dataset's own shapes before they are written.
//!
//! - `POST /{ds}/api/v1/{dataset}`: a new record, its IRI in `id` (201, `Location`).
//! - `PUT /{ds}/api/v1/{dataset}/{id}`: the record's writable keys replaced by the body's;
//!   a key the body leaves out loses its values. Creates the record when it is missing.
//! - `PATCH /{ds}/api/v1/{dataset}/{id}`: only the keys in the body (JSON Merge Patch:
//!   `null` removes a key's values).
//! - `DELETE /{ds}/api/v1/{dataset}/{id}`: the record's type and writable keys (204).
//!
//! `If-Match` with the `ETag` a read gave makes `PUT`, `PATCH` and `DELETE` refuse (412) a
//! record that changed since.
//!
//! A key is writable when its path is one predicate or the inverse of one. The values are
//! read as the record gives them: references as `{"id": …}` or the IRI, language maps, and
//! JSON numbers, booleans and strings as literals of the key's `sh:datatype`; an embedded
//! object without `id` is a new blank node with its keys' values, and the blank nodes it
//! replaces go with the blank nodes below them that nothing else links to. Before writing,
//! the record as it would be is checked against its node shape in the dataset's own shapes
//! (the generated shapes describe the data as it is, so they constrain nothing): a record
//! that does not conform is refused with 422 and the results.
//!
//! The record's triples are in the default graph; when the default graph is the union of all
//! graphs, a key's values are removed from every graph that holds them, and new values go to
//! the graph of the record's type triple (`graph`, or the default graph, for a new record).
//! The write is one transaction (on other stores than tripletrail, one SPARQL update), under
//! the caller's write rights. REST writes to a dataset
//! take turns, from reading the record to writing it; a SPARQL update at the same time is not
//! held back.

use super::{ApiSet, Field, Sets, find, records, sets};
use crate::View;
use crate::admin::Entry;
use crate::ogc::percent_decode;
use crate::{HttpError, Params, Reply, State};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tripletrail::oxigraph::model::vocab::{rdf, xsd};
use tripletrail::oxigraph::model::{
    BlankNode, Graph, GraphName, GraphNameRef, Literal, NamedNode, NamedOrBlankNode,
    NamedOrBlankNodeRef, Quad, Term, TermRef, Triple,
};
use tripletrail::shacl::{DataGraph, Edited, Path, Shapes, StoreGraph, Triple3, Union, Validator};
use tripletrail::shapes::Reader;

const SH: &str = "http://www.w3.org/ns/shacl#";
const PROBLEM: &str = "application/problem+json";
/// The prefix of the generated shapes' IRIs (see `tripletrail::shapes::generated_shape`).
const GENERATED: &str = "urn:tripletrail:shape:";

/// What a write does.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Method {
    Create,
    Replace,
    Change,
    Delete,
}

/// What a write needs besides the dataset: the body and the caller's rights.
pub(crate) struct Request<'a> {
    pub(crate) method: Method,
    pub(crate) body: &'a [u8],
    pub(crate) options: &'a tripletrail::UpdateOptions,
    /// `If-Match`: the `ETag`s the record must still have (`*`: it must exist).
    pub(crate) if_match: Option<&'a str>,
}

/// Answers a write to `/{ds}/api/{rest}` (see the module documentation).
#[allow(clippy::too_many_arguments)]
pub(crate) fn write(
    state: &State,
    entry: &Entry,
    rest: &[&str],
    params: &Params,
    base: &str,
    timeout: Option<Duration>,
    view: &View,
    request: &Request<'_>,
) -> Result<Reply, HttpError> {
    let allowed: &[&str] = if request.method == Method::Create {
        &["graph", "timeout", "dry"]
    } else {
        &["timeout", "dry"]
    };
    if let Some((name, _)) = params
        .0
        .iter()
        .find(|(n, _)| !allowed.contains(&n.as_str()))
    {
        return Err(HttpError::bad_request(format!(
            "Unknown parameter {name}: this resource takes {}",
            allowed.join(", ")
        )));
    }
    // `dry=true` (#82): every check of the write, and nothing written.
    let dry = match params.get("dry") {
        None | Some("false") => false,
        Some("true") => true,
        Some(other) => {
            return Err(HttpError::bad_request(format!(
                "dry is true or false, not {other}"
            )));
        }
    };
    let sets = sets(state, entry, view)?;
    let (set, id) = match (request.method, rest) {
        (Method::Create, ["v1", slug]) => (find(&sets, slug)?, None),
        (Method::Create, ["v1", _, _]) => {
            return Err(HttpError(
                405,
                "POST makes a new record in a dataset (…/v1/{dataset}); PUT or PATCH change \
                 this one"
                    .to_owned(),
            ));
        }
        (_, ["v1", slug, id]) => {
            let set = find(&sets, slug)?;
            let iri = percent_decode(id)
                .and_then(|s| NamedNode::new(s).ok())
                .ok_or_else(|| HttpError(404, format!("No record {id}")))?;
            (set, Some(iri))
        }
        (_, ["v1", _]) => {
            return Err(HttpError(
                405,
                "PUT, PATCH and DELETE change one record (…/v1/{dataset}/{id}); POST makes one"
                    .to_owned(),
            ));
        }
        _ => return Err(HttpError(405, "Only records can be written".to_owned())),
    };
    if view.rows.is_some() {
        return Err(HttpError(
            403,
            "Row-level rules limit what you see of this dataset: write it with SPARQL updates"
                .to_owned(),
        ));
    }
    let body = if request.method == Method::Delete {
        Map::new()
    } else {
        match serde_json::from_slice(request.body) {
            Ok(Value::Object(map)) => map,
            Ok(_) => return Err(HttpError::bad_request("The body must be a JSON object")),
            Err(e) => return Err(HttpError::bad_request(format!("Unreadable JSON: {e}"))),
        }
    };
    let mut problems = Vec::new();
    let named = match (body.get("id"), &id) {
        (None, Some(id)) => id.clone(),
        (Some(Value::String(given)), Some(id)) if given == id.as_str() => id.clone(),
        (Some(given), Some(id)) => {
            return Ok(problem(
                422,
                "The id in the body is not the record's",
                vec![
                    json!({ "key": "id", "value": given, "message": format!("This record is {id}") }),
                ],
            ));
        }
        (Some(Value::String(given)), None) => match NamedNode::new(given.as_str()) {
            Ok(n) => n,
            Err(e) => {
                return Ok(problem(
                    422,
                    "The id is not an absolute IRI",
                    vec![json!({ "key": "id", "value": given, "message": e.to_string() })],
                ));
            }
        },
        (Some(given), None) => {
            return Ok(problem(
                422,
                "The id is not an absolute IRI",
                vec![json!({ "key": "id", "value": given, "message": "Give the IRI as a string" })],
            ));
        }
        // A new record without id (#82): its IRI from the dataset's `tts:iriTemplate`, else
        // `urn:uuid:…`.
        (None, None) => match new_iri(set.iri_template.as_deref(), &body) {
            Ok(n) => n,
            Err(message) => {
                return Ok(problem(
                    422,
                    "The record's IRI cannot be made",
                    vec![json!({ "key": "id", "message": message })],
                ));
            }
        },
    };
    if let Some(given) = body.get("type") {
        let class = set.class.as_str();
        let ok = match given {
            Value::String(t) => t == class,
            Value::Array(ts) => ts.iter().any(|t| t.as_str() == Some(class)),
            _ => false,
        };
        if !ok {
            problems.push(json!({ "key": "type", "value": given, "message": format!("Records of {} are of type {class}", set.slug) }));
        }
    }
    // The keys the write changes, with their new values.
    let mut changed: Vec<(&Field, Vec<Term>)> = Vec::new();
    // The triples of new embedded objects.
    let mut embedded: Vec<Triple> = Vec::new();
    for (key, value) in &body {
        if matches!(key.as_str(), "id" | "type" | "@context") {
            continue;
        }
        if key == "truncated" {
            problems.push(json!({ "key": key, "message": "The record was cut short when it was read: change its keys with PATCH" }));
            continue;
        }
        let Some(field) = set.fields.iter().find(|f| &f.key == key) else {
            problems.push(json!({ "key": key, "value": value, "message": format!("{} has no key {key}", set.slug) }));
            continue;
        };
        if field.predicate.is_none() {
            problems.push(json!({ "key": key, "value": value, "message": "The path of this key is not one property: it is read-only" }));
            continue;
        }
        let inverse = field
            .predicate
            .as_ref()
            .is_some_and(|(_, inverse)| *inverse);
        match terms(field, value, &mut embedded) {
            Ok(values) if inverse && values.iter().any(|v| node(v).is_none()) => {
                problems.push(json!({ "key": key, "value": value, "message": "Expects IRIs: the record is their object" }));
            }
            Ok(values) => changed.push((field, values)),
            Err(message) => {
                problems.push(json!({ "key": key, "value": value, "message": message }));
            }
        }
    }
    if !problems.is_empty() {
        return Ok(problem(422, "The record cannot be written", problems));
    }
    // A key PUT leaves out loses its values; DELETE takes them all.
    if matches!(request.method, Method::Replace | Method::Delete) {
        for field in &set.fields {
            if field.predicate.is_some() && !changed.iter().any(|(f, _)| f.key == field.key) {
                changed.push((field, Vec::new()));
            }
        }
    }
    // Reading the record, checking it and writing it are one step among the REST writes to
    // this dataset: two of them must not both check against the record as it was before the
    // other.
    let lock = writer(entry.id);
    let writing = lock.lock().unwrap_or_else(|e| e.into_inner());
    let exists = {
        let is = format!(
            "SELECT ?f WHERE {{ VALUES ?f {{ {named} }} ?f a <{}> }}",
            set.class.as_str()
        );
        !crate::ogc::run(entry, &is, timeout, view)?.is_empty()
    };
    if let Some(wanted) = request.if_match {
        let current = if exists {
            records(
                entry,
                &sets,
                set,
                std::slice::from_ref(&named),
                timeout,
                view,
            )?
            .pop()
            .map(|r| super::record_tag(&r))
        } else {
            None
        };
        let matches = wanted.split(',').map(str::trim).any(|w| match &current {
            Some(tag) => {
                let w = w.strip_prefix("W/").unwrap_or(w);
                w == "*" || w == tag || w == super::plain_tag(tag)
            }
            None => false,
        });
        if !matches {
            return Err(HttpError(
                412,
                match current {
                    Some(tag) => {
                        format!("The record changed since it was read: its ETag is now {tag}")
                    }
                    None => format!("No record {named} in {}", set.slug),
                },
            ));
        }
    }
    match (request.method, exists) {
        (Method::Create, true) => {
            return Err(HttpError(
                409,
                format!("{named} is already a record of {}", set.slug),
            ));
        }
        (Method::Change | Method::Delete, false) => {
            return Err(HttpError(404, format!("No record {named} in {}", set.slug)));
        }
        _ => {}
    }
    let union = entry.engine.union_default_graph();
    let store = entry.engine.store();
    let subject: NamedOrBlankNodeRef<'_> = named.as_ref().into();
    let readable = |g: GraphNameRef<'_>| -> bool {
        (union || g.is_default_graph()) && view.graphs.as_ref().is_none_or(|r| r.contains(g))
    };
    let type_quads = |object: TermRef<'_>| -> Result<Vec<GraphName>, HttpError> {
        let mut graphs = Vec::new();
        for q in store.quads_for_pattern(Some(subject), Some(rdf::TYPE), Some(object), None) {
            let q = q.map_err(|e| HttpError::from(tripletrail::Error::from(e)))?;
            if readable(q.graph_name.as_ref()) {
                graphs.push(q.graph_name);
            }
        }
        graphs.sort_by_key(|g| (!g.is_default_graph(), g.to_string()));
        Ok(graphs)
    };
    let class_term: Term = set.class.clone().into();
    // Where new values go: the graph of the record's type triple.
    let target = match (exists, params.get("graph")) {
        (true, _) => type_quads(class_term.as_ref())?
            .into_iter()
            .next()
            .unwrap_or(GraphName::DefaultGraph),
        (false, None) => GraphName::DefaultGraph,
        (false, Some(_)) if !union => {
            return Err(HttpError::bad_request(
                "graph is for a dataset whose default graph is the union of its graphs",
            ));
        }
        (false, Some(g)) => GraphName::NamedNode(
            NamedNode::new(g)
                .map_err(|e| HttpError::bad_request(format!("graph is not an IRI: {e}")))?,
        ),
    };
    // The triples the write takes away (by graph) and adds.
    let mut removed: Vec<(GraphName, Triple)> = Vec::new();
    let mut added: Vec<Triple> = std::mem::take(&mut embedded);
    // The embedded blank nodes whose links the write takes away.
    let mut blanks: Vec<BlankNode> = Vec::new();
    let subject_term: Term = named.clone().into();
    for (field, values) in &changed {
        let (predicate, inverse) = field.predicate.as_ref().expect("writable");
        let quads = if *inverse {
            store.quads_for_pattern(
                None,
                Some(predicate.as_ref()),
                Some(subject_term.as_ref()),
                None,
            )
        } else {
            store.quads_for_pattern(Some(subject), Some(predicate.as_ref()), None, None)
        };
        for q in quads {
            let q = q.map_err(|e| HttpError::from(tripletrail::Error::from(e)))?;
            if readable(q.graph_name.as_ref()) {
                if let (Term::BlankNode(b), false, Some(_)) = (&q.object, inverse, &field.embed) {
                    blanks.push(b.clone());
                }
                removed.push((q.graph_name, Triple::new(q.subject, q.predicate, q.object)));
            }
        }
        for v in values {
            added.push(if *inverse {
                Triple::new(node(v).expect("checked"), predicate.clone(), named.clone())
            } else {
                Triple::new(named.clone(), predicate.clone(), v.clone())
            });
        }
    }
    // An embedded blank node goes with its link when nothing else links to it: its triples,
    // and those of the blank nodes below it that go too.
    let writable = |g: GraphNameRef<'_>| {
        request
            .options
            .writable
            .as_ref()
            .is_none_or(|w| w.contains(g))
    };
    let dropped = if blanks.is_empty() {
        Vec::new()
    } else {
        orphans(store, &named, &removed, blanks, &|g| {
            readable(g) && writable(g)
        })?
    };
    match request.method {
        Method::Delete => {
            for g in type_quads(class_term.as_ref())? {
                removed.push((g, Triple::new(named.clone(), rdf::TYPE, set.class.clone())));
            }
        }
        _ if !exists => added.push(Triple::new(named.clone(), rdf::TYPE, set.class.clone())),
        _ => {}
    }
    if request.method != Method::Delete
        && let Some(refused) = check(
            state,
            entry,
            set,
            view,
            &subject_term,
            &[removed.as_slice(), dropped.as_slice()].concat(),
            &added,
            union,
        )?
    {
        return Ok(refused);
    }
    if dry {
        drop(writing);
        for (g, _) in removed.iter().chain(&dropped) {
            if !writable(g.as_ref()) {
                return Err(HttpError(
                    403,
                    format!("The write changes {g}, which you may not write"),
                ));
            }
        }
        if !writable(target.as_ref()) {
            return Err(HttpError(
                403,
                format!("The write changes {target}, which you may not write"),
            ));
        }
        let mut answer = json!({ "dry": true, "id": named.as_str(), "created": !exists });
        if request.method == Method::Delete {
            // The links to the record that a delete leaves behind.
            let count = format!(
                "SELECT (COUNT(*) AS ?n) WHERE {{ ?s ?p {named} FILTER(?p != <{}>) }}",
                rdf::TYPE.as_str()
            );
            let n = crate::ogc::run(entry, &count, timeout, view)?
                .first()
                .and_then(|row| row.first().cloned().flatten())
                .and_then(|t| match t {
                    Term::Literal(l) => l.value().parse::<u64>().ok(),
                    _ => None,
                })
                .unwrap_or(0);
            answer["referencedBy"] = n.into();
        }
        return Ok(Reply::bytes(
            200,
            "application/json",
            answer.to_string().into_bytes(),
        ));
    }
    if matches!(store, tripletrail::store::Dataset::TripleTrail(_)) {
        // One transaction of exactly these quads: a blank node can't be named in SPARQL.
        for (g, _) in removed.iter().chain(&dropped) {
            if !writable(g.as_ref()) {
                return Err(HttpError(
                    403,
                    format!("The write changes {g}, which you may not write"),
                ));
            }
        }
        if !writable(target.as_ref()) {
            return Err(HttpError(
                403,
                format!("The write changes {target}, which you may not write"),
            ));
        }
        let mut changes: Vec<(bool, Quad)> = removed
            .iter()
            .chain(&dropped)
            .map(|(g, t)| (false, t.clone().in_graph(g.clone())))
            .collect();
        changes.extend(
            added
                .iter()
                .map(|t| (true, t.clone().in_graph(target.clone()))),
        );
        if !changes.is_empty() {
            entry.engine.apply_logged(&changes)?;
        }
    } else if let Some(update) = update_text(&removed, &added, &target) {
        // Other stores: by SPARQL, which removes an embedded blank node's link only.
        let (engine, _deadline) = crate::with_deadline(&entry.engine, timeout);
        engine.update_with_options(&update, request.options)?;
    }
    drop(writing);
    if request.method == Method::Delete {
        return Ok(Reply::bytes(204, "text/plain", Vec::new()));
    }
    // The record as it is now, under the new data's shapes.
    let sets: Sets = super::sets(state, entry, view)?;
    let set = find(&sets, &set.slug)?;
    let mut record = records(
        entry,
        &sets,
        set,
        std::slice::from_ref(&named),
        timeout,
        view,
    )?
    .pop()
    .expect("one");
    let tag = super::record_tag(&record);
    if let Value::Object(map) = &mut record {
        let mut out = Map::new();
        out.insert("@context".into(), super::context(&sets, set));
        out.append(map);
        record = Value::Object(out);
    }
    let created = !exists;
    let mut reply = Reply::bytes(
        if created { 201 } else { 200 },
        super::JSON_LD,
        record.to_string().into_bytes(),
    );
    reply.headers.push(("ETag", tag));
    if created {
        let id = percent_encode(named.as_str());
        reply
            .headers
            .push(("Location", format!("{base}/v1/{}/{id}", set.slug)));
    }
    Ok(reply)
}

/// A new record's IRI (#82): `template` with `{uuid}` a random UUID and `{key}` the body's
/// value of `key`, percent-encoded; without a template, `urn:uuid:` and a random UUID.
fn new_iri(template: Option<&str>, body: &Map<String, Value>) -> Result<NamedNode, String> {
    let uuid = {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).map_err(|e| format!("No random numbers: {e}"))?;
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &h[0..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..32]
        )
    };
    let Some(template) = template else {
        return NamedNode::new(format!("urn:uuid:{uuid}")).map_err(|e| e.to_string());
    };
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find('}')
            .ok_or_else(|| format!("tts:iriTemplate {template} has an unclosed {{"))?;
        let name = &rest[start + 1..start + end];
        if name == "uuid" {
            out.push_str(&uuid);
        } else {
            let value = match body.get(name) {
                Some(Value::String(v)) if !v.is_empty() => v.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => return Err(format!("The IRI is made from {name}: give it a value")),
            };
            out.push_str(&percent_encode(&value));
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    NamedNode::new(out.as_str()).map_err(|e| format!("The IRI {out} is not valid: {e}"))
}

/// The lock the REST writes to a dataset take (by [`Entry::id`]).
fn writer(dataset: u64) -> Arc<Mutex<()>> {
    static WRITERS: Mutex<Option<HashMap<u64, Arc<Mutex<()>>>>> = Mutex::new(None);
    let mut writers = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(
        writers
            .get_or_insert_with(HashMap::new)
            .entry(dataset)
            .or_default(),
    )
}

/// The most blank nodes one write looks at below the embedded blank nodes it unlinks.
const MAX_BELOW: usize = 100_000;

/// The quads of the blank nodes that `links` (the record's triples the write removes) leave
/// unlinked: of `blanks`, and of the blank nodes below them, those that nothing else links
/// to, in any graph, once those links and the quads of the other unlinked blank nodes are
/// gone (so a cycle among them goes too). A blank node with a quad in a graph where `may`
/// says no stays, with the blank nodes it links to.
fn orphans(
    store: &tripletrail::store::Dataset,
    record: &NamedNode,
    links: &[(GraphName, Triple)],
    blanks: Vec<BlankNode>,
    may: &dyn Fn(GraphNameRef<'_>) -> bool,
) -> Result<Vec<(GraphName, Triple)>, HttpError> {
    use std::collections::{HashMap, HashSet};
    let failed = |e| HttpError::from(tripletrail::Error::from(e));
    let links: HashSet<(&GraphName, &Triple)> = links.iter().map(|(g, t)| (g, t)).collect();
    // The blank nodes below, with their quads out and in.
    let mut out: HashMap<BlankNode, Vec<Quad>> = HashMap::new();
    let mut into: HashMap<BlankNode, Vec<Quad>> = HashMap::new();
    let mut next = blanks;
    while let Some(b) = next.pop() {
        if out.contains_key(&b) {
            continue;
        }
        if out.len() >= MAX_BELOW {
            return Err(HttpError(
                413,
                format!(
                    "The embedded objects this write unlinks reach more than {MAX_BELOW} blank nodes: change them with SPARQL"
                ),
            ));
        }
        let mut quads = Vec::new();
        for q in store.quads_for_pattern(Some(b.as_ref().into()), None, None, None) {
            let q = q.map_err(failed)?;
            if let Term::BlankNode(c) = &q.object {
                next.push(c.clone());
            }
            quads.push(q);
        }
        let mut incoming = Vec::new();
        for q in store.quads_for_pattern(None, None, Some(b.as_ref().into()), None) {
            incoming.push(q.map_err(failed)?);
        }
        out.insert(b.clone(), quads);
        into.insert(b, incoming);
    }
    // Those that go: take away, until none is left to take, each that something staying
    // links to, or that has a quad in a graph the caller may not change.
    let mut going: HashSet<BlankNode> = out.keys().cloned().collect();
    loop {
        let stays: Vec<BlankNode> = going
            .iter()
            .filter(|b| {
                out[*b].iter().any(|q| !may(q.graph_name.as_ref()))
                    || into[*b].iter().any(|q| {
                        let from_going = matches!(&q.subject, NamedOrBlankNode::BlankNode(s) if going.contains(s));
                        let unlinked = matches!(&q.subject, NamedOrBlankNode::NamedNode(s) if s == record)
                            && links.contains(&(
                                &q.graph_name,
                                &Triple::new(q.subject.clone(), q.predicate.clone(), q.object.clone()),
                            ));
                        !from_going && !unlinked
                    })
            })
            .cloned()
            .collect();
        if stays.is_empty() {
            break;
        }
        for b in stays {
            going.remove(&b);
        }
    }
    let mut dropped: Vec<(GraphName, Triple)> = going
        .iter()
        .flat_map(|b| &out[b])
        .map(|q| {
            (
                q.graph_name.clone(),
                Triple::new(q.subject.clone(), q.predicate.clone(), q.object.clone()),
            )
        })
        .collect();
    dropped.sort_by_cached_key(|(g, t)| (g.to_string(), t.to_string()));
    Ok(dropped)
}

/// An IRI or blank node as a subject.
fn node(t: &Term) -> Option<NamedOrBlankNode> {
    match t {
        Term::NamedNode(n) => Some(n.clone().into()),
        Term::BlankNode(b) => Some(b.clone().into()),
        _ => None,
    }
}

/// `application/problem+json` (RFC 9457), with what is wrong per key.
fn problem(status: u16, title: &str, errors: Vec<Value>) -> Reply {
    Reply::bytes(
        status,
        PROBLEM,
        json!({ "title": title, "status": status, "errors": errors })
            .to_string()
            .into_bytes(),
    )
}

/// Every character but the unreserved ones percent-encoded, slashes too.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A key's values from its JSON, as a record gives them.
fn terms(field: &Field, value: &Value, extra: &mut Vec<Triple>) -> Result<Vec<Term>, String> {
    if field.lang() {
        return match value {
            Value::Null => Ok(Vec::new()),
            Value::Object(map) => {
                let mut out = Vec::new();
                for (lang, texts) in map {
                    let texts: Vec<&Value> = match texts {
                        Value::Array(a) => a.iter().collect(),
                        one => vec![one],
                    };
                    for text in texts {
                        let Some(text) = text.as_str() else {
                            return Err(format!("The text for {lang} must be a string"));
                        };
                        out.push(if lang == "@none" {
                            Literal::new_simple_literal(text).into()
                        } else {
                            Literal::new_language_tagged_literal(text, lang)
                                .map_err(|e| format!("{lang} is not a language tag: {e}"))?
                                .into()
                        });
                    }
                }
                Ok(out)
            }
            _ => Err("Expects text by language tag: {\"en\": \"…\"}".to_owned()),
        };
    }
    let values: Vec<&Value> = match value {
        Value::Null => return Ok(Vec::new()),
        Value::Array(values) => values.iter().collect(),
        one => vec![one],
    };
    if field.single && values.len() > 1 {
        return Err("Takes one value".to_owned());
    }
    values.into_iter().map(|v| term(field, v, extra)).collect()
}

/// One value of a key.
fn term(field: &Field, value: &Value, extra: &mut Vec<Triple>) -> Result<Term, String> {
    if let (Some(keys), Value::Object(map)) = (&field.embed, value)
        && !map.contains_key("@value")
    {
        return match map.get("id") {
            // A resource that is no dataset's record: only the link is written.
            Some(Value::String(iri)) => NamedNode::new(iri.as_str())
                .map(Into::into)
                .map_err(|e| format!("{iri} is not an absolute IRI: {e}")),
            Some(_) => Err("id must be an IRI".to_owned()),
            None => blank(field, keys, map, extra).map(Into::into),
        };
    }
    if field.iri {
        let iri = match value {
            Value::String(s) => s.as_str(),
            Value::Object(map) => map
                .get("id")
                .and_then(Value::as_str)
                .ok_or("A reference needs its IRI as id")?,
            _ => return Err("Expects an IRI".to_owned()),
        };
        return NamedNode::new(iri)
            .map(Into::into)
            .map_err(|e| format!("{iri} is not an absolute IRI: {e}"));
    }
    let lexical = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Object(map) if map.contains_key("@value") => {
            // A JSON-LD value object.
            let text = match &map["@value"] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            if let Some(lang) = map.get("@language").and_then(Value::as_str) {
                return Literal::new_language_tagged_literal(text, lang)
                    .map(Into::into)
                    .map_err(|e| format!("{lang} is not a language tag: {e}"));
            }
            if let Some(t) = map.get("@type").and_then(Value::as_str) {
                let t = NamedNode::new(t).map_err(|e| format!("@type is not an IRI: {e}"))?;
                return checked(Literal::new_typed_literal(text, t));
            }
            text
        }
        _ => return Err("Expects a string, a number or a boolean".to_owned()),
    };
    let datatype = match (&field.datatype, value) {
        (Some(d), _) => d.clone(),
        (None, Value::Number(n)) if n.is_i64() || n.is_u64() => xsd::INTEGER.into_owned(),
        (None, Value::Number(_)) => xsd::DOUBLE.into_owned(),
        (None, Value::Bool(_)) => xsd::BOOLEAN.into_owned(),
        (None, _) => xsd::STRING.into_owned(),
    };
    if datatype == xsd::STRING && !value.is_string() {
        return Err("Expects a string".to_owned());
    }
    checked(Literal::new_typed_literal(lexical, datatype))
}

/// A new blank node for an embedded object, with the triples of its keys in `extra`. It is
/// of the key's `sh:class` when the object gives no `type`.
fn blank(
    field: &Field,
    keys: &[Field],
    map: &Map<String, Value>,
    extra: &mut Vec<Triple>,
) -> Result<BlankNode, String> {
    let b = BlankNode::default();
    let types: Vec<&Value> = match map.get("type") {
        None => Vec::new(),
        Some(Value::Array(types)) => types.iter().collect(),
        Some(one) => vec![one],
    };
    for t in &types {
        let t = t.as_str().ok_or("type must be an IRI")?;
        let t = NamedNode::new(t).map_err(|e| format!("type {t} is not an absolute IRI: {e}"))?;
        extra.push(Triple::new(b.clone(), rdf::TYPE, t));
    }
    if types.is_empty()
        && let Some(class) = &field.class
    {
        extra.push(Triple::new(b.clone(), rdf::TYPE, class.clone()));
    }
    for (key, value) in map {
        if matches!(key.as_str(), "type" | "@context") {
            continue;
        }
        let Some(inner) = keys.iter().find(|k| &k.key == key) else {
            return Err(format!(
                "An embedded object of {} has no key {key}",
                field.key
            ));
        };
        let Some((predicate, inverse)) = &inner.predicate else {
            return Err(format!(
                "{key}: the path of this key is not one property: it is read-only"
            ));
        };
        let values = terms(inner, value, extra).map_err(|e| format!("{key}: {e}"))?;
        for v in values {
            extra.push(if *inverse {
                let Some(s) = node(&v) else {
                    return Err(format!("{key}: expects IRIs: the object is their object"));
                };
                Triple::new(s, predicate.clone(), b.clone())
            } else {
                Triple::new(b.clone(), predicate.clone(), v)
            });
        }
    }
    Ok(b)
}

/// A literal, unless its text is not one of its datatype.
fn checked(literal: Literal) -> Result<Term, String> {
    if tripletrail::shacl::has_datatype(&literal, literal.datatype().as_str()) {
        Ok(literal.into())
    } else {
        Err(format!(
            "{} is not a valid {}",
            literal.value(),
            literal.datatype().as_str()
        ))
    }
}

/// The refusal (422) when the record as it would be does not conform to its node
/// shape in the dataset's own shapes.
#[allow(clippy::too_many_arguments)]
fn check(
    state: &State,
    entry: &Entry,
    set: &ApiSet,
    view: &View,
    focus: &Term,
    removed: &[(GraphName, Triple)],
    added: &[Triple],
    union: bool,
) -> Result<Option<Reply>, HttpError> {
    let projected = crate::shapes::projected(state, entry, Reader::RestApi, view)?;
    let shapes = own(&projected);
    let Ok(compiled) = Shapes::compile_with(&shapes, std::slice::from_ref(&set.node)) else {
        return Ok(None);
    };
    let Some(shape) = compiled.shape(&set.node) else {
        return Ok(None);
    };
    let store = entry.engine.store();
    // The graphs the record is read from.
    let graphs: Vec<StoreGraph> = if !union {
        vec![StoreGraph::new(store, GraphNameRef::DefaultGraph)]
    } else if let (None, tripletrail::store::Dataset::TripleTrail(s)) = (&view.graphs, store) {
        // All graphs, read as one.
        vec![StoreGraph::of_snapshot(&s.snapshot(), None)]
    } else {
        let mut names = vec![GraphName::DefaultGraph];
        if let Ok(named) = store.named_graphs() {
            names.extend(named.into_iter().map(GraphName::from));
        }
        names
            .iter()
            .filter(|g| view.graphs.as_ref().is_none_or(|r| r.contains(g.as_ref())))
            .map(|g| StoreGraph::new(store, g.as_ref()))
            .collect()
    };
    let readers: Vec<&dyn DataGraph> = graphs.iter().map(|g| g as _).collect();
    let base = Union(readers);
    let mut graph = Graph::new();
    for t in added {
        graph.insert(t);
    }
    let removed: Vec<Triple3> = removed
        .iter()
        .map(|(_, t)| {
            (
                t.subject.clone().into(),
                t.predicate.clone(),
                t.object.clone(),
            )
        })
        .collect();
    let edited = Edited::new(&base, removed, graph);
    let validator = Validator::new(&compiled, &edited);
    let mut errors = Vec::new();
    explain(
        &validator,
        &compiled,
        &shapes,
        focus,
        shape,
        &set.fields,
        "",
        0,
        &mut errors,
    );
    if let Some(e) = validator.take_error() {
        return Err(HttpError(500, format!("Checking the record failed: {e}")));
    }
    Ok((!errors.is_empty())
        .then(|| problem(422, "The record does not conform to its shape", errors)))
}

/// An embedded object that fails its `sh:node`: the object, its key's keys, and the node
/// shapes.
type Inside<'f> = (Term, &'f [Field], Vec<Term>);

/// The violations of `focus` against `shape` as errors, with their key (in `fields`, after
/// `prefix`). An embedded object that fails its `sh:node` is explained key by key.
#[allow(clippy::too_many_arguments)]
fn explain(
    validator: &Validator<'_>,
    compiled: &Shapes,
    shapes: &Graph,
    focus: &Term,
    shape: tripletrail::shacl::ShapeId,
    fields: &[Field],
    prefix: &str,
    depth: usize,
    errors: &mut Vec<Value>,
) {
    let violation = Term::from(NamedNode::new_unchecked(format!("{SH}Violation")));
    let node = NamedNode::new_unchecked(format!("{SH}node"));
    let mut found: Vec<(Map<String, Value>, Option<Inside<'_>>)> = Vec::new();
    validator.report_focus(focus, shape, &mut |r| {
        if *r.severity != violation {
            return;
        }
        let field = r.result_path.as_deref().and_then(|p| {
            let (predicate, inverse) = match p {
                Path::Predicate(n) => (n, false),
                Path::Inverse(inner) => match inner.as_ref() {
                    Path::Predicate(n) => (n, true),
                    _ => return None,
                },
                _ => return None,
            };
            fields
                .iter()
                .find(|f| f.predicate.as_ref() == Some(&(predicate.clone(), inverse)))
        });
        let component = r.source_constraint_component.to_string();
        let constraint = component
            .trim_matches(['<', '>'])
            .strip_prefix(SH)
            .and_then(|c| c.strip_suffix("ConstraintComponent"))
            .map(|c| {
                let mut chars = c.chars();
                let first = chars.next().map(|f| f.to_ascii_lowercase());
                format!(
                    "sh:{}{}",
                    first.into_iter().collect::<String>(),
                    chars.as_str()
                )
            })
            .unwrap_or(component);
        let mut e = Map::new();
        if let Some(field) = field {
            e.insert("key".into(), format!("{prefix}{}", field.key).into());
        }
        // A new blank node has no name worth giving.
        if let Some(v) = r.value.as_ref().filter(|v| !v.is_blank_node()) {
            e.insert(
                "value".into(),
                crate::ogc::value(v).unwrap_or_else(|| v.to_string().into()),
            );
        }
        let message = r.messages.iter().find_map(|m| match m {
            Term::Literal(l) => Some(l.value().to_owned()),
            _ => None,
        });
        let nested = match (&r.value, field.and_then(|f| f.embed.as_deref())) {
            (Some(v), Some(keys)) if constraint == "sh:node" && message.is_none() => {
                let targets: Vec<Term> = match subject(r.source_shape) {
                    Some(s) => shapes
                        .objects_for_subject_predicate(s, &node)
                        .map(TermRef::into_owned)
                        .collect(),
                    None => Vec::new(),
                };
                Some((v.clone(), keys.as_slice(), targets))
            }
            _ => None,
        };
        e.insert("constraint".into(), constraint.into());
        let message = message.unwrap_or_else(|| default_message(&e));
        e.insert("message".into(), message.into());
        e.insert("shape".into(), r.source_shape.to_string().into());
        found.push((e, nested));
    });
    for (e, nested) in found {
        let before = errors.len();
        if let (Some((value, keys, targets)), true) = (nested, depth < 8) {
            let key = e.get("key").and_then(Value::as_str).unwrap_or_default();
            let inner = format!("{key}.");
            for t in targets {
                if let Some(s) = compiled.shape(&t) {
                    explain(
                        validator,
                        compiled,
                        shapes,
                        &value,
                        s,
                        keys,
                        &inner,
                        depth + 1,
                        errors,
                    );
                }
            }
        }
        // Explained by the object's own errors, or else by this one.
        if errors.len() == before {
            errors.push(Value::Object(e));
        }
    }
}

/// A shape node as a subject.
fn subject(t: &Term) -> Option<NamedOrBlankNodeRef<'_>> {
    match t {
        Term::NamedNode(n) => Some(n.into()),
        Term::BlankNode(b) => Some(b.into()),
        _ => None,
    }
}

fn default_message(e: &Map<String, Value>) -> String {
    let constraint = e.get("constraint").and_then(Value::as_str).unwrap_or("");
    format!("Fails {constraint}")
}

/// The dataset's own shapes of a projected shapes graph: the generated property shapes and
/// the generated node shapes' own constraints left out.
fn own(projected: &Graph) -> Graph {
    let property = NamedNode::new_unchecked(format!("{SH}property"));
    let generated =
        |t: TermRef<'_>| matches!(t, TermRef::NamedNode(n) if n.as_str().starts_with(GENERATED));
    let mut out = Graph::new();
    for t in projected {
        if t.predicate == property.as_ref() && generated(t.object) {
            continue;
        }
        out.insert(t);
    }
    out
}

/// One SPARQL update that removes `removed` (from their graphs) and inserts `added` into
/// `target`; `None` when there is nothing to do.
fn update_text(
    removed: &[(GraphName, Triple)],
    added: &[Triple],
    target: &GraphName,
) -> Option<String> {
    let mut ops: Vec<String> = Vec::new();
    // Blank nodes cannot be in DELETE DATA: a triple with one is matched by a variable,
    // checked to be that blank node's triple by its other terms.
    let mut plain: Vec<(GraphName, String)> = Vec::new();
    for (i, (g, t)) in removed.iter().enumerate() {
        let s = match &t.subject {
            NamedOrBlankNode::BlankNode(_) => format!("?s{i}"),
            s => s.to_string(),
        };
        let o = match &t.object {
            Term::BlankNode(_) => format!("?o{i}"),
            o => o.to_string(),
        };
        let triple = format!("{s} {} {o} .", t.predicate);
        if s.starts_with('?') || o.starts_with('?') {
            let filter = match (&t.subject, &t.object) {
                (NamedOrBlankNode::BlankNode(_), _) => format!("FILTER(isBlank(?s{i}))"),
                _ => format!("FILTER(isBlank(?o{i}))"),
            };
            ops.push(match g {
                // Not DELETE WHERE: it takes no FILTER.
                GraphName::DefaultGraph => {
                    format!("DELETE {{ {triple} }} WHERE {{ {triple} {filter} }}")
                }
                g => format!("DELETE {{ GRAPH {g} {{ {triple} }} }} WHERE {{ GRAPH {g} {{ {triple} {filter} }} }}"),
            });
        } else {
            plain.push((g.clone(), triple));
        }
    }
    let block = |triples: &[(GraphName, String)]| -> String {
        let mut out = String::new();
        let mut graphs: Vec<&GraphName> = triples.iter().map(|(g, _)| g).collect();
        graphs.sort_by_key(|g| g.to_string());
        graphs.dedup();
        for g in graphs {
            let inner: Vec<&str> = triples
                .iter()
                .filter(|(h, _)| h == g)
                .map(|(_, t)| t.as_str())
                .collect();
            match g {
                GraphName::DefaultGraph => out.push_str(&inner.join(" ")),
                g => out.push_str(&format!(" GRAPH {g} {{ {} }}", inner.join(" "))),
            }
            out.push(' ');
        }
        out
    };
    if !plain.is_empty() {
        ops.push(format!("DELETE DATA {{ {}}}", block(&plain)));
    }
    if !added.is_empty() {
        let inserted: Vec<(GraphName, String)> = added
            .iter()
            .map(|t| {
                (
                    target.clone(),
                    format!("{} {} {} .", t.subject, t.predicate, t.object),
                )
            })
            .collect();
        ops.push(format!("INSERT DATA {{ {}}}", block(&inserted)));
    }
    (!ops.is_empty()).then(|| ops.join(" ;\n"))
}
