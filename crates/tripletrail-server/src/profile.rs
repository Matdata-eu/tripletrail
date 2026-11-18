//! `GET /$/datasets/{ds}/profile` (#137): what a dataset contains, as JSON, for generated
//! shapes and the readers that work from them. See [`tripletrail::Engine::dataset_profile`].

use crate::admin::Entry;
use crate::{HttpError, Params, State};
use serde_json::{Value, json};
use tripletrail::oxigraph::model::Term;
use tripletrail::{DatasetProfile, ProfileOptions};

/// The options from `sample`, `path_sample` and `classes`, within bounds.
pub(crate) fn options(params: &Params) -> Result<ProfileOptions, HttpError> {
    let mut options = ProfileOptions::default();
    let number = |name: &str, max: usize| -> Result<Option<usize>, HttpError> {
        params
            .get(name)
            .map(|v| match v.parse::<usize>() {
                Ok(n) if (1..=max).contains(&n) => Ok(n),
                _ => Err(HttpError::bad_request(format!(
                    "{name} must be a number from 1 to {max}"
                ))),
            })
            .transpose()
    };
    if let Some(n) = number("sample", 10_000)? {
        options.sample = n;
    }
    if let Some(n) = number("path_sample", 1_000)? {
        options.path_sample = n;
    }
    if let Some(n) = number("classes", 10_000)? {
        options.max_classes = n;
    }
    options.path_sample = options.path_sample.min(options.sample);
    Ok(options)
}

/// The profile, under the dataset's query timeout (`timeout=` asks for a shorter one), so a
/// large sample on a large dataset ends as a timed-out query does.
pub(crate) fn reply(state: &State, entry: &Entry, params: &Params) -> Result<Value, HttpError> {
    Ok(to_json(
        &entry.name,
        &*compute(state, entry, params, &crate::View::default())?,
    ))
}

/// The profile the parameters ask for, under the dataset's query timeout: of the `view` graphs
/// only when given (#203).
pub(crate) fn compute(
    state: &State,
    entry: &Entry,
    params: &Params,
    view: &crate::View,
) -> Result<std::sync::Arc<DatasetProfile>, HttpError> {
    let options = options(params)?;
    let asked = params
        .get("timeout")
        .map(crate::parse_timeout)
        .transpose()?;
    let timeout = crate::limits::Limits::of(state, &entry.name).timeout(asked);
    let (engine, _deadline) = crate::with_deadline(&entry.engine, timeout);
    Ok(engine.dataset_profile_of(options, view.graphs.as_ref(), view.rows.as_ref())?)
}

/// A term as in SPARQL's JSON results.
pub(crate) fn term(t: &Term) -> Value {
    match t {
        Term::NamedNode(n) => json!({"type": "uri", "value": n.as_str()}),
        Term::BlankNode(b) => json!({"type": "bnode", "value": b.as_str()}),
        Term::Literal(l) => match l.language() {
            Some(lang) => json!({"type": "literal", "value": l.value(), "xml:lang": lang}),
            None => {
                json!({"type": "literal", "value": l.value(), "datatype": l.datatype().as_str()})
            }
        },
        #[allow(unreachable_patterns)]
        _ => json!({"type": "triple", "value": t.to_string()}),
    }
}

fn counted<K>(items: &[(K, u64)], key: &str, show: impl Fn(&K) -> String) -> Value {
    Value::Array(
        items
            .iter()
            .map(|(k, n)| json!({ key: show(k), "count": n }))
            .collect(),
    )
}

pub(crate) fn to_json(name: &str, profile: &DatasetProfile) -> Value {
    let iri = |n: &tripletrail::oxigraph::model::NamedNode| n.as_str().to_owned();
    let classes: Vec<Value> = profile
        .classes
        .iter()
        .map(|c| {
            let properties: Vec<Value> = c
                .properties
                .iter()
                .map(|p| {
                    let mut v = json!({
                        "property": p.property.as_str(),
                        "instances": p.instances,
                        "values": p.values,
                        "minCount": p.min_count,
                        "maxCount": p.max_count,
                        "iris": p.iris,
                        "blankNodes": p.blank_nodes,
                        "literals": p.literals,
                        "datatypes": counted(&p.datatypes, "datatype", iri),
                        "languages": counted(&p.languages, "language", String::clone),
                        "classes": counted(&p.classes, "class", iri),
                    });
                    if let Some(label) = &p.label {
                        v["label"] = json!(label);
                    }
                    if let Some(values) = &p.distinct_values {
                        v["distinctValues"] = Value::Array(values.iter().map(term).collect());
                    }
                    v
                })
                .collect();
            let geometries: Vec<Value> = c
                .geometries
                .iter()
                .map(|g| {
                    json!({
                        "path": g.path.iter().map(iri).collect::<Vec<_>>(),
                        "datatype": g.datatype.as_str(),
                        "instances": g.instances,
                        "types": counted(&g.types, "type", String::clone),
                    })
                })
                .collect();
            let mut v = json!({
                "class": c.class.as_str(),
                "instances": c.instances,
                "sampled": c.sampled,
                "superclasses": c.superclasses.iter().map(iri).collect::<Vec<_>>(),
                "labelProperties": c.label_properties.iter().map(iri).collect::<Vec<_>>(),
                "properties": properties,
                "geometries": geometries,
            });
            if let Some(label) = &c.label {
                v["label"] = json!(label);
            }
            v
        })
        .collect();
    json!({
        "dataset": name,
        "generation": profile.generation,
        "classesTotal": profile.classes_total,
        "options": {
            "sample": profile.options.sample,
            "pathSample": profile.options.path_sample,
            "classes": profile.options.max_classes,
        },
        "classes": classes,
    })
}
