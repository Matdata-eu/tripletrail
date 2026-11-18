//! SHACL validation and SHACL-AF inference over HTTP (#401), for a workspace's scratch store
//! (`/$/workspaces/{id}/validate`, `…/infer`) and, reading only, for a dataset with a shapes
//! graph (`/$/datasets/{name}/validate`, `…/infer`).
//!
//! Results are JSON by default, or RDF by `Accept` (or `format=`): the W3C validation report,
//! or the inferred triples. With `provenance=true` (workspaces only), each inferred triple
//! comes with where it came from, in the one shape the workbench's "why?" panel reads:
//! `{"kind": "rule", "rule", "shape", "focus", "group", "pass", "bindings"}`. Reasoning (#187)
//! and pipelines (#404) add the kinds `entailed` and `step` later; `asserted` is a triple of
//! the data.

use crate::profile::term;
use crate::{HttpError, Params, Reply, json_reply};
use serde_json::{Map, Value, json};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tiny_http::Request;
use tripletrail::Engine;
use tripletrail::oxigraph::io::{RdfFormat, RdfSerializer};
use tripletrail::oxigraph::model::{GraphName, GraphNameRef, NamedNode, Term, Triple};
use tripletrail::oxigraph::sparql::QueryResults;
use tripletrail::shacl::{Origin, RuleRun};

const SH: &str = "http://www.w3.org/ns/shacl#";
/// Results listed in a JSON answer at most, unless `limit=` asks for another number.
const RESULTS: usize = 10_000;

/// The RDF format the request asks for, or `None` for JSON.
fn rdf_wanted(request: &Request, params: &Params) -> Result<Option<RdfFormat>, HttpError> {
    if let Some(f) = params.get("format") {
        if f.eq_ignore_ascii_case("json") {
            return Ok(None);
        }
        return crate::rdf_format(request, params, RdfFormat::Turtle, false).map(Some);
    }
    match crate::accepted(request).first().map(String::as_str) {
        None | Some("*/*" | "application/json" | "application/*") => Ok(None),
        Some(t) => Ok(RdfFormat::from_media_type(t).filter(|f| !f.supports_datasets())),
    }
}

fn truthy(params: &Params, name: &str) -> bool {
    params
        .get(name)
        .is_some_and(|v| matches!(v, "" | "true" | "1" | "yes"))
}

fn number(params: &Params, name: &str, max: usize) -> Result<Option<usize>, HttpError> {
    params
        .get(name)
        .map(|v| match v.parse::<usize>() {
            Ok(n) if (1..=max).contains(&n) => Ok(n),
            _ => Err(HttpError::bad_request(format!(
                "{name} must be a number from 1 to {max}"
            ))),
        })
        .transpose()
}

pub(crate) fn rdf_reply(format: RdfFormat, triples: &[Triple]) -> Result<Reply, HttpError> {
    let mut serializer = RdfSerializer::from_format(format)
        .with_prefix("sh", SH)
        .map_err(|e| HttpError(500, e.to_string()))?
        .for_writer(Vec::new());
    for t in triples {
        serializer.serialize_triple(t)?;
    }
    let bytes = serializer.finish()?;
    Ok(Reply::bytes(200, format.media_type(), bytes))
}

/// A shapes graph that does not compile is a `422` saying why (the shape lint of #401: compile
/// errors such as pre-binding refusals, with the shape); a cancelled run is the timeout's `503`.
pub(crate) fn shacl_error(e: tripletrail::Error) -> HttpError {
    shacl_error_as(e, "The shapes do not compile")
}

pub(crate) fn shacl_error_as(e: tripletrail::Error, what: &str) -> HttpError {
    match &e {
        tripletrail::Error::Shacl(_) if !e.to_string().contains("cancelled") => {
            HttpError(422, format!("{what}: {e}"))
        }
        _ => e.into(),
    }
}

/// What to validate: the data graph and the shapes graph of an engine.
pub(crate) struct Target<'a> {
    pub engine: &'a Engine,
    pub shapes: NamedNode,
    pub data: GraphName,
    pub timeout: Option<Duration>,
}

/// `…/validate`: the validation results as JSON (with how many nodes each shape was checked
/// against, and coverage), or the W3C report as RDF.
pub(crate) fn validate(
    request: &Request,
    params: &Params,
    target: &Target<'_>,
) -> Result<Reply, HttpError> {
    let started = Instant::now();
    let (engine, _deadline) = crate::with_deadline(target.engine, target.timeout);
    let data = target.data.as_ref();
    if let Some(format) = rdf_wanted(request, params)? {
        let mut triples = Vec::new();
        engine
            .validate_report(&target.shapes, data, true, &mut |t| triples.push(t))
            .map_err(shacl_error)?;
        return rdf_reply(format, &triples);
    }
    let limit = number(params, "limit", 1_000_000)?.unwrap_or(RESULTS);
    let mut results = Vec::new();
    let mut total = 0usize;
    let counts = engine
        .validate_counted(&target.shapes, data, true, &mut |r| {
            total += 1;
            if results.len() < limit {
                results.push(result_json(&r));
            }
        })
        .map_err(shacl_error)?;
    let shapes: Vec<Value> = counts
        .iter()
        .map(|c| {
            json!({
                "shape": term(&c.shape),
                "property": c.property,
                "focusNodes": c.focus_nodes,
                "valueNodes": c.value_nodes,
            })
        })
        .collect();
    let empty: Vec<Value> = counts
        .iter()
        .filter(|c| !c.property && c.focus_nodes == 0)
        .map(|c| term(&c.shape))
        .collect();
    Ok(json_reply(
        200,
        &json!({
            "conforms": total == 0,
            "resultCount": total,
            "results": results,
            "truncated": total > results.len(),
            "shapes": shapes,
            "coverage": {
                "untargetedClasses": untargeted(&engine, target)?,
                "emptyShapes": empty,
            },
            "ms": started.elapsed().as_millis() as u64,
        }),
    ))
}

/// The classes with instances in the data that no shape of the shapes graph targets (by
/// `sh:targetClass` or as an implicit class target), directly or through a superclass.
fn untargeted(engine: &Engine, target: &Target<'_>) -> Result<Value, HttpError> {
    let data = |pattern: &str| match &target.data {
        GraphName::DefaultGraph => pattern.to_owned(),
        GraphName::NamedNode(n) => format!("GRAPH <{}> {{ {pattern} }}", n.as_str()),
        GraphName::BlankNode(_) => pattern.to_owned(),
    };
    let query = format!(
        "PREFIX sh: <{SH}>
         PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
         SELECT ?class (COUNT(DISTINCT ?s) AS ?n) WHERE {{
           {}
           FILTER NOT EXISTS {{
             {}
             GRAPH <{}> {{
               {{ ?shape sh:targetClass ?t }} UNION {{ ?t a sh:NodeShape , rdfs:Class }}
               UNION {{ ?t a sh:NodeShape , <http://www.w3.org/2002/07/owl#Class> }}
             }}
           }}
         }} GROUP BY ?class ORDER BY DESC(?n) LIMIT 1000",
        data("?s a ?class"),
        data("?class rdfs:subClassOf* ?t"),
        target.shapes.as_str()
    );
    let mut out = Vec::new();
    if let QueryResults::Solutions(solutions) = engine.query(&query)? {
        for s in solutions {
            let s = s.map_err(|e| HttpError(500, e.to_string()))?;
            let n = match s.get("n") {
                Some(Term::Literal(l)) => l.value().parse::<u64>().unwrap_or(0),
                _ => 0,
            };
            if let Some(class) = s.get("class") {
                out.push(json!({ "class": term(class), "instances": n }));
            }
        }
    }
    Ok(Value::Array(out))
}

/// `…/infer`: the triples the SHACL-AF rules infer (`max_iterations` passes at most,
/// `single_pass=true` for one, `include_existing=true` for those already in the data too),
/// with what each rule did, and with `provenance=true` where each triple came from.
pub(crate) fn infer(
    request: &Request,
    params: &Params,
    target: &Target<'_>,
    provenance_allowed: bool,
) -> Result<Reply, HttpError> {
    let started = Instant::now();
    let provenance = truthy(params, "provenance");
    if provenance && !provenance_allowed {
        return Err(HttpError::bad_request(
            "provenance=true is for workspaces (/$/workspaces/{id}/infer)",
        ));
    }
    let mut options = tripletrail::InferOptions {
        include_existing: truthy(params, "include_existing"),
        ..Default::default()
    };
    if let Some(n) = number(params, "max_iterations", 10_000)? {
        options.max_passes = n;
    }
    if truthy(params, "single_pass") {
        options.max_passes = 1;
    }
    // The plan only when asked: its edges grow as the square of the rules (#742).
    let plan_sink = std::sync::Arc::new(tripletrail::shacl::PlanSink::default());
    if truthy(params, "plan") {
        options.plan = Some(std::sync::Arc::clone(&plan_sink));
    }
    let (engine, _deadline) = crate::with_deadline(target.engine, target.timeout);
    let runs = Mutex::new(Vec::new());
    let ran = Mutex::new(Vec::<Term>::new());
    let observer = |r: &RuleRun<'_>| {
        if r.done {
            if provenance {
                ran.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(r.rule.clone());
            }
            runs.lock().unwrap_or_else(|e| e.into_inner()).push(json!({
                "rule": r.rule.to_string(),
                "shape": term(r.shape),
                "group": r.group,
                "pass": r.pass,
                "focusNodes": r.focus_nodes,
                "constructed": r.constructed,
                "new": r.new,
                "ms": r.elapsed.as_secs_f64() * 1000.0,
            }));
        }
    };
    let data = target.data.as_ref();
    let (inferred, origins) = if provenance {
        engine.infer_traced(&target.shapes, data, &options, &observer)
    } else {
        engine
            .infer_with(&target.shapes, data, None, &options, &observer)
            .map(|i| (i, Vec::new()))
    }
    .map_err(shacl_error)?;
    if let Some(format) = rdf_wanted(request, params)? {
        return rdf_reply(format, inferred.as_slice());
    }
    let limit = number(params, "limit", 10_000_000)?.unwrap_or(RESULTS);
    let triples: Vec<Value> = inferred
        .iter()
        .take(limit)
        .map(|t| {
            json!({
                "subject": term(&t.subject.clone().into()),
                "predicate": term(&t.predicate.clone().into()),
                "object": term(&t.object),
            })
        })
        .collect();
    let mut answer = json!({
        "count": inferred.len(),
        "triples": triples,
        "truncated": inferred.len() > limit,
        "runs": runs.into_inner().unwrap_or_else(|e| e.into_inner()),
        "ms": started.elapsed().as_millis() as u64,
    });
    if let Some(plan) = plan_sink.lock().unwrap_or_else(|e| e.into_inner()).take() {
        answer["plan"] = plan_json(&plan);
    }
    if provenance {
        answer["provenance"] = Value::Array(origins.iter().take(limit).map(origin).collect());
        let ran = ran.into_inner().unwrap_or_else(|e| e.into_inner());
        answer["rules"] = rules(
            &engine,
            &target.shapes,
            origins.iter().map(|o| &o.rule).chain(&ran),
        );
    }
    Ok(json_reply(200, &answer))
}

/// The plan of a run (its strata, #408 reuses the shape): the rules, the groups in the order
/// they run (`group` of a run is the position here, from 1), and the dependencies between
/// the rules, each with the predicates it goes through and whether it is positive (a
/// negated one orders the groups but does not make a group recursive).
pub(crate) fn plan_json(plan: &tripletrail::shacl::Plan) -> Value {
    json!({
        "rules": plan.rules.iter().map(|r| json!({
            // As `rule` of the runs, the key of `rules`.
            "rule": r.rule.to_string(),
            "shape": term(&r.shape),
            "order": r.order,
            "reads": r.reads,
            "writes": r.writes,
        })).collect::<Vec<_>>(),
        "groups": plan.groups.iter().enumerate().map(|(i, g)| json!({
            "group": i + 1,
            "rules": g.rules,
            "recursive": g.recursive,
        })).collect::<Vec<_>>(),
        "edges": plan.edges.iter().map(|e| json!({
            "from": e.from,
            "to": e.to,
            "through": e.through,
            "positive": e.positive,
        })).collect::<Vec<_>>(),
        "truncated": plan.truncated,
    })
}

/// One triple's provenance, in the workbench's one format.
pub(crate) fn origin(o: &Origin) -> Value {
    let bindings: Map<String, Value> = o
        .solution
        .as_ref()
        .map(|s| {
            s.iter()
                .map(|(v, t)| (v.as_str().to_owned(), term(t)))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "kind": "rule",
        "rule": o.rule.to_string(),
        "shape": term(&o.shape),
        "focus": o.focus.as_ref().map(term),
        "group": o.group,
        "pass": o.pass,
        "bindings": bindings,
    })
}

/// What the rules named in `origins` are, by the `rule` key provenance uses (a rule is often a
/// blank node, which a query cannot name): its type, `rdfs:label`, and its `sh:construct` or its
/// subject, predicate and object, and `sh:order`. Every rule that ran is described, also those
/// that inferred nothing.
fn rules<'a>(engine: &Engine, shapes: &NamedNode, ran: impl Iterator<Item = &'a Term>) -> Value {
    let mut out = Map::new();
    let graph = GraphNameRef::NamedNode(shapes.as_ref());
    for rule_term in ran {
        let key = rule_term.to_string();
        if out.contains_key(&key) {
            continue;
        }
        let subject = match rule_term {
            Term::NamedNode(n) => n.as_ref().into(),
            Term::BlankNode(b) => b.as_ref().into(),
            _ => continue,
        };
        let mut rule = Map::new();
        for quad in engine
            .store()
            .quads_for_pattern(Some(subject), None, None, Some(graph))
            .flatten()
        {
            let name = match quad.predicate.as_str().strip_prefix(SH) {
                Some(local) => local.to_owned(),
                None if quad.predicate.as_str()
                    == "http://www.w3.org/1999/02/22-rdf-syntax-ns#type" =>
                {
                    "type".to_owned()
                }
                None if quad.predicate.as_str() == "http://www.w3.org/2000/01/rdf-schema#label" => {
                    "label".to_owned()
                }
                None => continue,
            };
            let value = match &quad.object {
                Term::Literal(l) => Value::String(l.value().to_owned()),
                other => Value::String(other.to_string()),
            };
            rule.insert(name, value);
        }
        out.insert(key, Value::Object(rule));
    }
    Value::Object(out)
}

/// The graph a workspace's entailment is written to, in a fork of its store.
const ENTAILED_GRAPH: &str = "urn:tripletrail:workspace:entailed";

/// The most triples one derivation search goes through (#407); it also reads at most 64 index
/// entries per triple of this, about a second's work.
const WHY_BUDGET: usize = 20_000;

/// How long `…/reason/why` may take when the server has no timeout.
const WHY_TIMEOUT: Duration = Duration::from_secs(10);

/// The data of a workspace's store entailed under a regime, in a fork of the store without
/// the shapes and rules graphs: the snapshot, the graph the entailment is in, and its
/// triples.
struct Entailment {
    snapshot: tripletrail::store::Snapshot,
    graph: Option<tripletrail::store::TermId>,
    triples: Vec<[tripletrail::store::TermId; 3]>,
}

fn regime(params: &Params) -> Result<tripletrail::store::entail::Regime, HttpError> {
    use tripletrail::store::entail::Regime;
    match params.get("regime").unwrap_or("rdfs") {
        "rdfs" => Ok(Regime::Rdfs),
        "owl-rl" => Ok(Regime::OwlRl),
        other => Err(HttpError::bad_request(format!(
            "regime must be rdfs or owl-rl, not {other}"
        ))),
    }
}

fn entailment(
    engine: &Engine,
    skip: &[&str],
    regime: tripletrail::store::entail::Regime,
    cancelled: &dyn Fn() -> bool,
) -> Result<Entailment, HttpError> {
    use tripletrail::store::Dataset;
    let Dataset::TripleTrail(store) = engine.store() else {
        return Err(HttpError::bad_request(
            "Reasoning needs the tripletrail store",
        ));
    };
    let fork = store.fork();
    for graph in skip {
        fork.clear_graph(tripletrail::oxigraph::model::NamedNodeRef::new_unchecked(
            graph,
        ))
        .map_err(|e| HttpError::internal_error(e.to_string()))?;
    }
    let into = tripletrail::oxigraph::model::NamedNodeRef::new_unchecked(ENTAILED_GRAPH);
    let done = fork
        .transaction(|t| {
            Ok::<_, tripletrail::store::StoreError>(t.entail_until(into, regime, cancelled))
        })
        .map_err(|e| HttpError::internal_error(e.to_string()))?;
    if done.is_none() {
        return Err(HttpError(
            503,
            "The reasoning took longer than the timeout".to_owned(),
        ));
    }
    let snapshot = fork.snapshot();
    let graph = snapshot.term_id(into.into());
    let triples = match graph {
        Some(g) => snapshot
            .quad_ids_for_pattern(
                None,
                None,
                None,
                tripletrail::store::GraphSelector::Graph(g),
            )
            .map(|q| [q[0], q[1], q[2]])
            .collect(),
        None => Vec::new(),
    };
    Ok(Entailment {
        snapshot,
        graph,
        triples,
    })
}

/// `…/reason` (#407): what the regime entails from a workspace's data (its shapes and rules
/// left out): how many triples, by predicate and by class, the triples, and under OWL 2 RL
/// what the data contradicts. RDF by `Accept`: the entailed triples.
pub(crate) fn reason(
    request: &Request,
    params: &Params,
    engine: &Engine,
    skip: &[&str],
    timeout: Option<Duration>,
) -> Result<Reply, HttpError> {
    use tripletrail::store::entail::Regime;
    let started = Instant::now();
    let regime = regime(params)?;
    let Entailment {
        snapshot,
        graph,
        triples,
    } = {
        let (limited, _deadline) = crate::with_deadline(engine, timeout);
        entailment(engine, skip, regime, &|| limited.is_cancelled())?
    };
    let decode = |id| {
        snapshot
            .term(id)
            .map_err(|e| HttpError::internal_error(e.to_string()))
    };
    let triple = |t: &[tripletrail::store::TermId; 3]| -> Result<Triple, HttpError> {
        let (Term::NamedNode(p), s) = (decode(t[1])?, decode(t[0])?) else {
            return Err(HttpError::internal_error("a predicate that is no IRI"));
        };
        let s: tripletrail::oxigraph::model::NamedOrBlankNode = match s {
            Term::NamedNode(n) => n.into(),
            Term::BlankNode(b) => b.into(),
            other => return Err(HttpError::internal_error(format!("subject {other}"))),
        };
        Ok(Triple::new(s, p, decode(t[2])?))
    };
    if let Some(format) = rdf_wanted(request, params)? {
        let all = triples.iter().map(triple).collect::<Result<Vec<_>, _>>()?;
        return rdf_reply(format, &all);
    }
    let rdf_type = snapshot.term_id(tripletrail::oxigraph::model::vocab::rdf::TYPE.into());
    let mut predicates: std::collections::HashMap<_, usize> = std::collections::HashMap::new();
    let mut classes: std::collections::HashMap<_, usize> = std::collections::HashMap::new();
    for t in &triples {
        *predicates.entry(t[1]).or_default() += 1;
        if Some(t[1]) == rdf_type {
            *classes.entry(t[2]).or_default() += 1;
        }
    }
    let counted = |counts: std::collections::HashMap<_, usize>, key: &str| {
        let mut counts: Vec<_> = counts.into_iter().collect();
        counts.sort_by_key(|&(id, n)| (std::cmp::Reverse(n), id));
        counts
            .into_iter()
            .map(|(id, n)| Ok(json!({ key: term(&decode(id)?), "count": n })))
            .collect::<Result<Vec<Value>, HttpError>>()
    };
    let limit = number(params, "limit", 10_000_000)?.unwrap_or(RESULTS);
    let listed = triples
        .iter()
        .take(limit)
        .map(|t| {
            let t = triple(t)?;
            Ok(json!({
                "subject": term(&t.subject.into()),
                "predicate": term(&t.predicate.into()),
                "object": term(&t.object),
            }))
        })
        .collect::<Result<Vec<Value>, HttpError>>()?;
    let mut answer = json!({
        "regime": tripletrail::regime_name(regime),
        "count": triples.len(),
        "predicates": counted(predicates, "predicate")?,
        "classes": counted(classes, "class")?,
        "triples": listed,
        "truncated": triples.len() > limit,
    });
    if regime == Regime::OwlRl {
        let (limited, _deadline) = crate::with_deadline(engine, timeout);
        let found = tripletrail::store::entail::inconsistencies(&snapshot, graph, 1_000, &|| {
            limited.is_cancelled()
        });
        answer["inconsistencies"] = json!({
            "count": found.count,
            "atLeast": found.at_least,
            "first": found.first.iter().map(|i| json!({
                "rule": i.rule,
                "terms": i.terms.iter().map(term).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
    }
    answer["ms"] = json!(started.elapsed().as_millis() as u64);
    Ok(json_reply(200, &answer))
}

/// `…/reason/why` (#407): the derivation of the triple `subject`, `predicate`, `object`
/// (N-Triples terms) under the regime, down to asserted triples:
/// `{"subject", "predicate", "object", "rule", "premises": [...]}`, `rule` null for an
/// asserted triple. `entailed` says whether the triple holds (asserted or entailed);
/// `derivation` is null when it does not, or when no derivation is found.
pub(crate) fn why(
    params: &Params,
    engine: &Engine,
    skip: &[&str],
    timeout: Option<Duration>,
) -> Result<Reply, HttpError> {
    use std::str::FromStr;
    let regime = regime(params)?;
    let mut ids = Vec::new();
    // The entailment keeps to the server's timeout, as `…/reason` does; the search to it as
    // well, or to WHY_TIMEOUT when the server has none.
    let Entailment {
        snapshot,
        graph,
        triples,
    } = {
        let (limited, _deadline) = crate::with_deadline(engine, timeout);
        entailment(engine, skip, regime, &|| limited.is_cancelled())?
    };
    let (limited, _deadline) = crate::with_deadline(engine, Some(timeout.unwrap_or(WHY_TIMEOUT)));
    let cancelled = || limited.is_cancelled();
    for name in ["subject", "predicate", "object"] {
        let text = params
            .get(name)
            .ok_or_else(|| HttpError::bad_request(format!("{name}= is missing")))?;
        let t = Term::from_str(text.trim())
            .map_err(|e| HttpError::bad_request(format!("{name}: {e}")))?;
        ids.push(snapshot.term_id(t.as_ref()));
    }
    let explainer = tripletrail::store::entail::Explainer::new(&snapshot, graph, regime, &triples);
    let entailed = match ids[..] {
        [Some(s), Some(p), Some(o)] => explainer.holds([s, p, o]),
        _ => false,
    };
    let derivation = match ids[..] {
        [Some(s), Some(p), Some(o)] => explainer
            .derive([s, p, o], WHY_BUDGET, &cancelled)
            .map_err(|_| {
                HttpError(
                    503,
                    "Finding the derivation took longer than the timeout".to_owned(),
                )
            })?,
        _ => None,
    };
    fn tree(
        snapshot: &tripletrail::store::Snapshot,
        d: &tripletrail::store::entail::Derivation,
    ) -> Result<Value, HttpError> {
        let mut out = Map::new();
        for (name, id) in ["subject", "predicate", "object"].into_iter().zip(d.triple) {
            let t = snapshot
                .term(id)
                .map_err(|e| HttpError::internal_error(e.to_string()))?;
            out.insert(name.to_owned(), term(&t));
        }
        out.insert("rule".to_owned(), json!(d.rule));
        out.insert(
            "premises".to_owned(),
            Value::Array(
                d.premises
                    .iter()
                    .map(|p| tree(snapshot, p))
                    .collect::<Result<_, _>>()?,
            ),
        );
        Ok(Value::Object(out))
    }
    let answer = json!({
        "regime": tripletrail::regime_name(regime),
        "entailed": entailed,
        "derivation": derivation.as_ref().map(|d| tree(&snapshot, d)).transpose()?,
    });
    Ok(json_reply(200, &answer))
}

impl HttpError {
    fn internal_error(message: impl Into<String>) -> Self {
        Self(500, message.into())
    }
}

/// A validation result as JSON, with its `sh:detail`s nested as `details`.
fn result_json(r: &tripletrail::shacl::ValidationResult<'_>) -> Value {
    let mut v = json!({
        "focusNode": term(&r.focus_node),
        "resultPath": r.result_path.as_ref().map(|p| p.to_sparql()),
        "value": r.value.as_ref().map(term),
        "sourceShape": term(r.source_shape),
        "sourceConstraintComponent": term(&r.source_constraint_component),
        "sourceConstraint": r.source_constraint.as_ref().map(term),
        "severity": term(r.severity),
        "messages": r.messages.iter().map(|m| match m {
            Term::Literal(l) => Value::String(l.value().to_owned()),
            other => Value::String(other.to_string()),
        }).collect::<Vec<_>>(),
    });
    if !r.details.is_empty() {
        v["details"] = Value::Array(r.details.iter().map(result_json).collect());
    }
    v
}
