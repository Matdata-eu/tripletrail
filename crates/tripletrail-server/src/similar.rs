//! `GET /{ds}/search/similar`: retrieval for GraphRAG (#219). The resources whose vector of a
//! property is nearest a query vector, or nearest the vector of a given resource, each with
//! its score and its concise bounded description, as JSON with the descriptions in JSON-LD:
//! what an application hands a language model as context.

use crate::{Entry, HttpError, Params, Reply, View, json_reply};
use serde_json::{Value, json};
use std::time::Duration;
use tripletrail::oxigraph::io::{JsonLdProfileSet, RdfFormat};
use tripletrail::oxigraph::model::{NamedNode, Term};

/// How many resources a request may ask for.
const MAX_K: usize = 100;
const DEFAULT_K: usize = 10;
/// The most (resource, vector) pairs read to find `k` resources with several vectors each.
const MAX_PAIRS: usize = 6_400;
const NEAREST: &str = "http://w3id.org/tripletrail/fn#nearest";

/// `property=IRI&vector=[…]` or `property=IRI&iri=IRI`, with `k` (10, at most 100) and
/// `describe=false` to leave the descriptions out.
pub(crate) fn similar(
    entry: &Entry,
    params: &Params,
    timeout: Option<Duration>,
    view: &View,
) -> Result<Reply, HttpError> {
    let iri_param = |name: &str| -> Result<Option<NamedNode>, HttpError> {
        params
            .get(name)
            .map(|v| {
                NamedNode::new(v)
                    .map_err(|e| HttpError::bad_request(format!("Invalid IRI {v} for {name}: {e}")))
            })
            .transpose()
    };
    let property = iri_param("property")?
        .ok_or_else(|| HttpError::bad_request("Missing the property parameter"))?;
    let k = match params.get("k") {
        Some(k) => k
            .parse::<usize>()
            .ok()
            .filter(|k| (1..=MAX_K).contains(k))
            .ok_or_else(|| HttpError::bad_request(format!("k is a number from 1 to {MAX_K}")))?,
        None => DEFAULT_K,
    };
    let describe = match params.get("describe") {
        None | Some("true") => true,
        Some("false") => false,
        Some(other) => {
            return Err(HttpError::bad_request(format!(
                "describe is true or false, not {other}"
            )));
        }
    };
    // `{K}` is how many pairs `tt:nearest` keeps: a resource with several vectors is in
    // several, so it grows until there are `k` resources (or no more pairs).
    let template = match (params.get("vector"), iri_param("iri")?) {
        (Some(vector), None) => {
            if tripletrail::vector::parse_text(vector).is_none_or(|v| v.is_empty()) {
                return Err(HttpError::bad_request(
                    "vector is a JSON array of numbers, such as [0.1, -0.2]",
                ));
            }
            let literal = tripletrail::oxigraph::model::Literal::new_simple_literal(vector);
            format!(
                "SELECT ?s ?score WHERE {{ ?s <{NEAREST}> ({property} {literal} {{K}} ?score) }}"
            )
        }
        // The resource's vector, in any graph the caller reads, as `tt:nearest` searches
        // them all.
        (None, Some(iri)) => format!(
            "SELECT ?s ?score WHERE {{ \
             {{ {iri} {property} ?q }} UNION {{ GRAPH ?g {{ {iri} {property} ?q }} }} \
             ?s <{NEAREST}> ({property} ?q {{K}} ?score) }}"
        ),
        _ => {
            return Err(HttpError::bad_request(
                "Give either vector (a JSON array of numbers) or iri (a resource with a vector)",
            ));
        }
    };
    // Each resource once, with its best score.
    let mut best: std::collections::HashMap<NamedNode, f64> = std::collections::HashMap::new();
    let mut pairs = k;
    loop {
        let query = template.replace("{K}", &pairs.to_string());
        let rows = crate::ogc::run(entry, &query, timeout, view)?;
        let found = rows.len();
        let before = best.len();
        for row in rows {
            if let [Some(Term::NamedNode(s)), Some(Term::Literal(score))] = row.as_slice()
                && let Ok(score) = score.value().parse::<f64>()
            {
                let b = best.entry(s.clone()).or_insert(score);
                *b = b.max(score);
            }
        }
        if best.len() >= k || found < pairs || best.len() == before || pairs >= MAX_PAIRS {
            break;
        }
        pairs = (pairs * 4).min(MAX_PAIRS);
    }
    let mut hits: Vec<(NamedNode, f64)> = best.into_iter().collect();
    hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    hits.truncate(k);
    let rows_view = match &view.rows {
        Some(rules) => {
            let (engine, _deadline) = crate::with_deadline(&entry.engine, timeout);
            Some(engine.row_view(rules)?)
        }
        None => None,
    };
    const JSON_LD: RdfFormat = RdfFormat::JsonLd {
        profile: JsonLdProfileSet::empty(),
    };
    let mut results = Vec::with_capacity(hits.len());
    for (iri, score) in hits {
        let mut result = json!({ "iri": iri.as_str(), "score": score });
        if describe {
            let (triples, truncated) = crate::linked_data::describe(
                &entry.engine,
                &iri,
                view.graphs.as_ref(),
                rows_view.as_ref(),
            )?;
            let bytes = crate::service_description::serialize(&triples, JSON_LD)?;
            let doc: Value = serde_json::from_slice(&bytes)
                .map_err(|e| HttpError(500, format!("JSON-LD of {iri}: {e}")))?;
            result["description"] = doc;
            if truncated {
                result["truncated"] = Value::Bool(true);
            }
        }
        results.push(result);
    }
    Ok(json_reply(
        200,
        &json!({ "property": property.as_str(), "k": k, "results": results }),
    ))
}
