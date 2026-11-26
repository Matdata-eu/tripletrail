//! A GraphQL API generated from the shapes (#144), at `/{ds}/graphql`.
//!
//! The schema comes from the GraphQL reader's projection of the dataset's shapes, read the
//! way the REST API reads its own (#143): one object type per dataset (node shape with a
//! target class), one field per key, with the REST API's keys. A key whose values are
//! another dataset's records is a field of that dataset's type, so a query can follow
//! references; an embedded object (a blank node with a node shape) is a nested type.
//!
//! Each dataset has two root fields: a list (`stations(filter:, q:, bbox:, first:, after:)`,
//! a Relay-style connection with `edges { cursor node }`, `nodes` and `pageInfo`) and one
//! record (`station(id:)`). A query runs as the REST API's reads: one for the records, then
//! one batch per level of references the query follows, never one per record.
//!
//! Mutations (#27) write records as the REST API's writes do, with the same keys and checks:
//! `createStation(record:, graph:)`, `updateStation(id:, record:)` (only the keys given, as a
//! `PATCH`; `null` removes a key's values) and `deleteStation(id:)`. A record is a `JSON`
//! value, as the REST API's body. One that breaks its shape changes nothing and fails with the
//! violations in the error's `extensions`. Create and update answer the record as it now is;
//! delete answers its id.

use super::{
    ApiSet, Field, Sets, WriteMethod, WriteRequest, list, percent_encode, records, reference,
    schema_name, sets_for,
};
use crate::admin::Entry;
use crate::{HttpError, Params, Reply, State, View};
use async_graphql::dynamic::{
    Field as GqlField, FieldFuture, FieldValue, InputObject, InputValue, Object, ResolverContext,
    Scalar, Schema, TypeRef,
};
use async_graphql::{ErrorExtensions, SelectionField, Value as GqlValue};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tripletrail::oxigraph::model::NamedNode;
use tripletrail::shapes::Reader;

/// How deep a query may nest.
const MAX_DEPTH: usize = 16;
/// The most fields a query may select, aliases and introspection counted.
const MAX_COMPLEXITY: usize = 10_000;
/// The most records a list gives at once (`first`).
const MAX_FIRST: usize = 1_000;
const DEFAULT_FIRST: usize = 100;
/// The most values (JSON values, each object, list, text and number counted) a request may read
/// in all, over every root field and level: references copy the record they refer to into each
/// place, so an answer could otherwise grow as a power of its depth.
const MAX_ANSWER_VALUES: usize = 500_000;
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// What the resolvers of one request read from.
struct Request {
    entry: Arc<Entry>,
    sets: Sets,
    view: View,
    /// When the whole request must be done: each read gets the time left.
    deadline: Option<Instant>,
    /// The values the request may still read (MAX_ANSWER_VALUES at the start).
    left: AtomicUsize,
    base: String,
    /// The caller's right to write, for a mutation (checked before the request runs).
    writes: Option<Writes>,
}

/// What a mutation writes with: the server and the caller's write rights.
pub(crate) struct Writes {
    pub(crate) state: Arc<State>,
    pub(crate) options: tripletrail::UpdateOptions,
}

impl Request {
    /// The time left for a read; an error once the request's time is up.
    fn timeout(&self) -> Result<Option<Duration>, async_graphql::Error> {
        match self.deadline {
            None => Ok(None),
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() => Ok(Some(left)),
                _ => Err("The query timed out".into()),
            },
        }
    }

    /// Counts `n` values against the request's budget; an error once it is spent.
    fn spend(&self, n: usize) -> Result<(), async_graphql::Error> {
        let spent = self
            .left
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(n)
            });
        match spent {
            Ok(_) => {
                // Spending is also when a long expansion notices the time is up.
                self.timeout()?;
                Ok(())
            }
            Err(_) => {
                self.left.store(0, Ordering::Relaxed);
                Err(format!(
                    "The answer would have more than {MAX_ANSWER_VALUES} values: ask for fewer \
                     records, keys or levels"
                )
                .into())
            }
        }
    }
}

/// How many values `v` holds, itself included.
fn size(v: &Value) -> usize {
    match v {
        Value::Array(a) => 1 + a.iter().map(size).sum::<usize>(),
        Value::Object(m) => 1 + m.values().map(size).sum::<usize>(),
        _ => 1,
    }
}

/// The schemas made, by dataset and view, with the datasets they were made from.
type SchemaCache = HashMap<(u64, Option<u64>), (Sets, Schema)>;
static SCHEMAS: Mutex<Option<SchemaCache>> = Mutex::new(None);
const SCHEMAS_KEPT: usize = 64;

/// Reads a GraphQL request: `GET ?query=&variables=&operationName=`, or `POST` with a JSON
/// body (`application/json`) or the query itself (`application/graphql`).
pub(crate) fn read_request(
    params: &Params,
    body: Option<(&str, &[u8])>,
) -> Result<GraphQlRequest, HttpError> {
    Ok(match body {
        Some((kind, bytes)) if kind.starts_with("application/graphql") => (
            String::from_utf8(bytes.to_vec())
                .map_err(|_| HttpError::bad_request("The query is not UTF-8"))?,
            None,
            None,
        ),
        Some((_, bytes)) => {
            let v: Value = serde_json::from_slice(bytes)
                .map_err(|e| HttpError::bad_request(format!("The body is not JSON: {e}")))?;
            let query = v
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| HttpError::bad_request("The body has no query"))?
                .to_owned();
            let variables = v.get("variables").filter(|v| !v.is_null()).cloned();
            let operation = v
                .get("operationName")
                .and_then(Value::as_str)
                .map(str::to_owned);
            (query, variables, operation)
        }
        None => {
            let query = params
                .get("query")
                .ok_or_else(|| HttpError::bad_request("Missing the query parameter"))?
                .to_owned();
            let variables = match params.get("variables") {
                Some(v) if !v.trim().is_empty() => {
                    Some(serde_json::from_str(v).map_err(|e| {
                        HttpError::bad_request(format!("variables is not JSON: {e}"))
                    })?)
                }
                _ => None,
            };
            (
                query,
                variables,
                params.get("operationName").map(str::to_owned),
            )
        }
    })
}

/// A GraphQL request: its query, variables and operation name.
pub(crate) type GraphQlRequest = (String, Option<Value>, Option<String>);

/// Whether the operation `request` runs is a mutation: `Some(true)` when it may add data
/// (anything but deletes), `Some(false)` when it only deletes, `None` for a query or a
/// request that does not parse (which then fails as a query).
pub(crate) fn mutates(request: &GraphQlRequest) -> Option<bool> {
    use async_graphql::parser::types::{DocumentOperations, OperationType, Selection};
    let document = async_graphql::parser::parse_query(&request.0).ok()?;
    let operation = match (&document.operations, request.2.as_deref()) {
        (DocumentOperations::Single(op), _) => op,
        (DocumentOperations::Multiple(ops), Some(name)) => ops.get(name)?,
        (DocumentOperations::Multiple(ops), None) if ops.len() == 1 => ops.values().next()?,
        _ => return None,
    };
    if operation.node.ty != OperationType::Mutation {
        return None;
    }
    let adds = operation
        .node
        .selection_set
        .node
        .items
        .iter()
        .any(|s| match &s.node {
            Selection::Field(f) => !f.node.name.node.starts_with("delete"),
            _ => true,
        });
    Some(adds)
}

/// Answers a GraphQL request (see [`read_request`]); `writes` for a mutation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn answer(
    state: &State,
    entry: Arc<Entry>,
    (query, variables, operation): GraphQlRequest,
    base: &str,
    timeout: Option<Duration>,
    view: &View,
    writes: Option<Writes>,
) -> Result<Reply, HttpError> {
    let sets = sets_for(state, &entry, view, Reader::GraphQl)?;
    let schema = schema(&entry, view, &sets)?;
    let mut request = async_graphql::Request::new(query);
    if let Some(variables) = variables {
        request = request.variables(async_graphql::Variables::from_json(variables));
    }
    if let Some(operation) = operation {
        request = request.operation_name(operation);
    }
    let request = request.data(Request {
        entry,
        sets,
        view: view.clone(),
        deadline: timeout.map(|t| Instant::now() + t),
        left: AtomicUsize::new(MAX_ANSWER_VALUES),
        base: base.to_owned(),
        writes,
    });
    let response = pollster::block_on(schema.execute(request));
    let body = serde_json::to_vec(&response)
        .map_err(|e| HttpError(500, format!("Writing the answer failed: {e}")))?;
    Ok(Reply::bytes(200, "application/json", body))
}

/// The schema of `sets`, made once per dataset, view and datasets.
fn schema(entry: &Entry, view: &View, sets: &Sets) -> Result<Schema, HttpError> {
    let key = (entry.id, view.fingerprint());
    if let Some((made_from, schema)) = SCHEMAS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|c| c.get(&key))
        && Arc::ptr_eq(made_from, sets)
    {
        return Ok(schema.clone());
    }
    let schema = make_schema(sets)
        .map_err(|e| HttpError(500, format!("The GraphQL schema could not be made: {e}")))?;
    let mut cache = SCHEMAS.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if cache.len() >= SCHEMAS_KEPT {
        cache.clear();
    }
    cache.insert(key, (Arc::clone(sets), schema.clone()));
    Ok(schema)
}

/// A GraphQL name from `text`: letters, digits and `_`, not starting with a digit or `__`.
fn name(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) || out.starts_with("__") {
        out.insert(0, 'x');
    }
    out
}

/// `OperationalPoint` → `operationalPoint`.
fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_ascii_lowercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// `operationalPoint` → `operationalPoints`, `address` → `addresses`, `city` → `cities`.
fn plural(text: &str) -> String {
    let consonant_y = text.ends_with('y')
        && !text
            .chars()
            .rev()
            .nth(1)
            .is_some_and(|c| "aeiouAEIOU".contains(c));
    if consonant_y {
        format!("{}ies", &text[..text.len() - 1])
    } else if ["s", "x", "z", "ch", "sh"]
        .iter()
        .any(|e| text.ends_with(e))
    {
        format!("{text}es")
    } else {
        format!("{text}s")
    }
}

/// The GraphQL type of one value of a scalar key.
fn scalar_of(field: &Field) -> &'static str {
    if field.lang() {
        return "LangString";
    }
    let Some(d) = field
        .datatype
        .as_ref()
        .and_then(|d| d.as_str().strip_prefix(XSD))
    else {
        return "String";
    };
    match d {
        "boolean" => "Boolean",
        "integer" | "int" | "long" | "short" | "byte" | "nonNegativeInteger"
        | "positiveInteger" | "negativeInteger" | "nonPositiveInteger" | "unsignedInt"
        | "unsignedLong" | "unsignedShort" | "unsignedByte" => "Int",
        "decimal" | "double" | "float" => "Float",
        _ => "String",
    }
}

/// How a key reads in the schema.
enum Kind<'a> {
    /// A scalar, by GraphQL type name.
    Scalar(&'static str),
    /// Another dataset's records.
    Reference(&'a ApiSet),
    /// Embedded objects, of a nested type with these keys.
    Embedded(&'a [Field]),
}

fn kind<'a>(sets: &'a [ApiSet], field: &'a Field) -> Kind<'a> {
    if field.iri
        && let Some(target) = reference(sets, field)
    {
        return Kind::Reference(target);
    }
    if let Some(keys) = &field.embed {
        return Kind::Embedded(keys);
    }
    Kind::Scalar(scalar_of(field))
}

/// The GraphQL names of `fields` (unique, not `id`, `type` or `label` unless the key is),
/// paired with them.
fn named(fields: &[Field]) -> Vec<(String, &Field)> {
    let mut taken: HashSet<String> = ["id", "type"].iter().map(|s| (*s).to_owned()).collect();
    let mut out = Vec::new();
    for field in fields {
        let mut n = name(&field.key);
        while taken.contains(&n) {
            n.push('_');
        }
        taken.insert(n.clone());
        out.push((n, field));
    }
    out
}

/// The GraphQL type names: per dataset (by slug), and per embedded key (by owner type and key
/// name). Unique against each other, the built-in and fixed types, and the `Edge`,
/// `Connection` and `Filter` types made per dataset.
struct Names {
    sets: HashMap<String, String>,
    nested: HashMap<(String, String), String>,
}

impl Names {
    fn new(sets: &[ApiSet]) -> Names {
        let mut taken: HashSet<String> = [
            "Query",
            "Mutation",
            "Subscription",
            "JSON",
            "PageInfo",
            "LangString",
            "String",
            "Int",
            "Float",
            "Boolean",
            "ID",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let mut names = Names {
            sets: HashMap::new(),
            nested: HashMap::new(),
        };
        for set in sets {
            let n = unique(
                &mut taken,
                name(&schema_name(&set.slug)),
                &["", "Edge", "Connection", "Filter"],
            );
            names.sets.insert(set.slug.clone(), n);
        }
        for set in sets {
            let owner = names.sets[&set.slug].clone();
            names.collect(sets, &owner, &set.fields, &mut taken);
        }
        names
    }

    fn collect(
        &mut self,
        sets: &[ApiSet],
        owner: &str,
        fields: &[Field],
        taken: &mut HashSet<String>,
    ) {
        for (n, field) in named(fields) {
            if let Kind::Embedded(keys) = kind(sets, field) {
                let t = unique(taken, format!("{owner}{}", schema_name(&n)), &[""]);
                self.nested.insert((owner.to_owned(), n), t.clone());
                self.collect(sets, &t, keys, taken);
            }
        }
    }

    fn set(&self, set: &ApiSet) -> &str {
        &self.sets[&set.slug]
    }

    fn nested(&self, owner: &str, key: &str) -> &str {
        &self.nested[&(owner.to_owned(), key.to_owned())]
    }
}

/// `base`, with `_` added until it and it with each of `suffixes` are free; takes them.
fn unique(taken: &mut HashSet<String>, mut base: String, suffixes: &[&str]) -> String {
    while suffixes
        .iter()
        .any(|s| taken.contains(&format!("{base}{s}")))
    {
        base.push('_');
    }
    for s in suffixes {
        taken.insert(format!("{base}{s}"));
    }
    base
}

fn make_schema(sets: &Sets) -> Result<Schema, async_graphql::dynamic::SchemaError> {
    let names = Names::new(sets);
    let mut objects: Vec<Object> = Vec::new();
    let mut inputs: Vec<InputObject> = Vec::new();
    let mut query = Object::new("Query");
    let mut mutation = Object::new("Mutation");
    let mut roots: HashSet<String> = HashSet::new();
    let page_info = Object::new("PageInfo")
        .description("Where a page of a list is")
        .field(json_field(
            "hasNextPage",
            TypeRef::named_nn(TypeRef::BOOLEAN),
            "hasNextPage",
            Kind::Scalar("Boolean"),
            true,
        ))
        .field(json_field(
            "endCursor",
            TypeRef::named(TypeRef::STRING),
            "endCursor",
            Kind::Scalar("String"),
            true,
        ));
    objects.push(page_info);
    for set in sets.iter() {
        let type_name = names.set(set).to_owned();
        objects.push(record_type(
            sets,
            &names,
            &type_name,
            &set.fields,
            set.description.as_deref(),
        ));
        let mut nested = Vec::new();
        collect_nested(sets, &names, &type_name, &set.fields, &mut nested);
        objects.extend(nested);
        // The list's filters: a key equal to a text, as the REST API's `?key=`.
        let mut filter = InputObject::new(format!("{type_name}Filter")).description(format!(
            "Records of {} whose keys have these values (compared as text)",
            set.title
        ));
        let mut any = false;
        for (n, field) in named(&set.fields) {
            if matches!(kind(sets, field), Kind::Embedded(_)) {
                continue;
            }
            filter = filter.field(InputValue::new(n, TypeRef::named(TypeRef::STRING)));
            any = true;
        }
        if any {
            inputs.push(filter);
        }
        let edge = Object::new(format!("{type_name}Edge"))
            .field(json_field(
                "cursor",
                TypeRef::named_nn(TypeRef::STRING),
                "cursor",
                Kind::Scalar("String"),
                true,
            ))
            .field(object_field("node", TypeRef::named_nn(&type_name), "node"));
        let connection = Object::new(format!("{type_name}Connection"))
            .description(format!("A page of {}", set.title))
            .field(list_field(
                "edges",
                TypeRef::named_nn_list_nn(format!("{type_name}Edge")),
                "edges",
            ))
            .field(list_field(
                "nodes",
                TypeRef::named_nn_list_nn(&type_name),
                "nodes",
            ))
            .field(object_field(
                "pageInfo",
                TypeRef::named_nn("PageInfo"),
                "pageInfo",
            ));
        objects.push(edge);
        objects.push(connection);
        // Named after the class, not its type name, which may have had `_` added.
        let mut single = lower_first(&name(&schema_name(&set.slug)));
        let many = loop {
            let mut many = plural(&single);
            if many == single {
                many.push_str("List");
            }
            if !roots.contains(&single) && !roots.contains(&many) {
                break many;
            }
            single.push('_');
        };
        roots.insert(single.clone());
        roots.insert(many.clone());
        let slug = set.slug.clone();
        let mut list_root = GqlField::new(
            many,
            TypeRef::named_nn(format!("{type_name}Connection")),
            move |ctx| {
                let slug = slug.clone();
                FieldFuture::new(async move { list_records(&ctx, &slug).map(Some) })
            },
        )
        .description(format!("The records of {}", set.title))
        .argument(
            InputValue::new("first", TypeRef::named(TypeRef::INT)).description(format!(
                "How many (at most {MAX_FIRST}; {DEFAULT_FIRST} by default)"
            )),
        )
        .argument(
            InputValue::new("after", TypeRef::named(TypeRef::STRING))
                .description("The endCursor of the page before"),
        );
        if any {
            list_root = list_root.argument(InputValue::new(
                "filter",
                TypeRef::named(format!("{type_name}Filter")),
            ));
        }
        if set.label.is_some() {
            list_root = list_root.argument(
                InputValue::new("q", TypeRef::named(TypeRef::STRING))
                    .description("Text the label contains, regardless of case and accents"),
            );
        }
        if set.geometry.is_some() {
            list_root = list_root.argument(
                InputValue::new("bbox", TypeRef::named_nn_list(TypeRef::FLOAT)).description(
                    "minLon, minLat, maxLon, maxLat (CRS84): records whose geometry meets the box",
                ),
            );
        }
        query = query.field(list_root);
        let slug = set.slug.clone();
        query = query.field(
            GqlField::new(single, TypeRef::named(&type_name), move |ctx| {
                let slug = slug.clone();
                FieldFuture::new(async move { one_record(&ctx, &slug) })
            })
            .description(format!("One record of {}", set.title))
            .argument(InputValue::new("id", TypeRef::named_nn(TypeRef::ID))),
        );
        mutation = mutations(mutation, set, &type_name);
    }
    let any_set = !sets.is_empty();
    let mut builder = Schema::build("Query", any_set.then_some("Mutation"), None)
        .register(query)
        .register(Scalar::new("JSON").description(
            "A record as the REST API writes it: its keys, references as IRIs or {\"id\": …}",
        ))
        .register(
            Scalar::new("LangString")
                .description("Text by language tag: {\"en\": \"…\", \"fr\": \"…\"}, several texts of one language as a list"),
        )
        .limit_depth(MAX_DEPTH)
        .limit_complexity(MAX_COMPLEXITY);
    for o in objects {
        builder = builder.register(o);
    }
    for i in inputs {
        builder = builder.register(i);
    }
    if any_set {
        builder = builder.register(mutation);
    }
    builder.finish()
}

/// The create, update and delete mutations of `set` (#27), added to `mutation`.
fn mutations(mutation: Object, set: &ApiSet, type_name: &str) -> Object {
    let field = |verb: &str, method: WriteMethod, ty: TypeRef| {
        let slug = set.slug.clone();
        GqlField::new(format!("{verb}{type_name}"), ty, move |ctx| {
            let slug = slug.clone();
            FieldFuture::new(async move { mutate(&ctx, &slug, method) })
        })
    };
    let record = || {
        InputValue::new("record", TypeRef::named_nn("JSON"))
            .description("The record's keys, as the REST API's body")
    };
    let id = || InputValue::new("id", TypeRef::named_nn(TypeRef::ID));
    mutation
        .field(
            field("create", WriteMethod::Create, TypeRef::named(type_name))
                .description(format!(
                    "A new record of {}, checked against its shape; its IRI in id, or made",
                    set.title
                ))
                .argument(record())
                .argument(
                    InputValue::new("graph", TypeRef::named(TypeRef::STRING)).description(
                        "The named graph to write it in (the default graph if left out)",
                    ),
                ),
        )
        .field(
            field("update", WriteMethod::Change, TypeRef::named(type_name))
                .description(format!(
                    "Changes the keys given of a record of {}: null removes a key's values",
                    set.title
                ))
                .argument(id())
                .argument(record()),
        )
        .field(
            field("delete", WriteMethod::Delete, TypeRef::named(TypeRef::ID))
                .description(format!("Deletes a record of {}; its id", set.title))
                .argument(id()),
        )
}

/// A mutation: the REST API's write of the record, then the record as it now is.
fn mutate(
    ctx: &ResolverContext<'_>,
    slug: &str,
    method: WriteMethod,
) -> Result<Option<FieldValue<'static>>, async_graphql::Error> {
    let r = ctx.data::<Request>()?;
    let writes = r
        .writes
        .as_ref()
        .ok_or("Mutations are sent with POST, by a caller who may write")?;
    r.spend(1)?;
    let set = find(&r.sets, slug)?;
    let id = match ctx.args.get("id") {
        Some(v) => Some(v.string()?.to_owned()),
        None => None,
    };
    let body = match ctx.args.get("record") {
        Some(v) => v.as_value().clone().into_json()?,
        None => Value::Object(serde_json::Map::new()),
    };
    let mut params = Vec::new();
    if let Some(g) = ctx.args.get("graph") {
        params.push(("graph".to_owned(), g.string()?.to_owned()));
    }
    let encoded = id.as_deref().map(percent_encode);
    let mut path = vec!["v1", slug];
    path.extend(encoded.as_deref());
    let body = serde_json::to_vec(&body)?;
    let reply = super::write(
        &writes.state,
        &r.entry,
        &path,
        &Params(params),
        &r.base,
        r.timeout()?,
        &r.view,
        &WriteRequest {
            method,
            body: &body,
            options: &writes.options,
            if_match: None,
        },
    )
    .map_err(|e| {
        let status = e.0;
        async_graphql::Error::new(e.1).extend_with(|_, x| x.set("status", status))
    })?;
    let mut text = Vec::new();
    let mut answer = reply.body;
    answer.read_to_end(&mut text)?;
    let answer: Value = serde_json::from_slice(&text).unwrap_or(Value::Null);
    match reply.status {
        204 => Ok(id.map(|id| FieldValue::value(GqlValue::from(id)))),
        200 | 201 => {
            let iri = answer
                .get("id")
                .and_then(Value::as_str)
                .and_then(|i| NamedNode::new(i).ok())
                .ok_or("The record was written, but has no IRI to read it by")?;
            let mut found = fetch(r, set, &[iri])?;
            expand(r, &set.fields, &ctx.ctx.field(), &mut found, 0)?;
            Ok(found.pop().map(FieldValue::owned_any))
        }
        _ => {
            // A problem (422, 412): its title, with each violation in the extensions.
            let title = answer
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("The write was refused")
                .to_owned();
            let errors = answer
                .get("errors")
                .cloned()
                .unwrap_or(Value::Array(Vec::new()));
            let details: Vec<String> = errors
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| {
                    let message = e.get("message").and_then(Value::as_str).unwrap_or_default();
                    match e.get("key").and_then(Value::as_str) {
                        Some(key) => format!("{key}: {message}"),
                        None => message.to_owned(),
                    }
                })
                .collect();
            let message = if details.is_empty() {
                title
            } else {
                format!("{title}: {}", details.join("; "))
            };
            let violations = GqlValue::from_json(errors).unwrap_or(GqlValue::Null);
            let status = reply.status;
            Err(async_graphql::Error::new(message).extend_with(|_, e| {
                e.set("status", status);
                e.set("violations", violations.clone());
            }))
        }
    }
}

/// The nested types of the embedded keys of `fields`, named after the type that has them.
fn collect_nested(
    sets: &[ApiSet],
    names: &Names,
    owner: &str,
    fields: &[Field],
    out: &mut Vec<Object>,
) {
    for (n, field) in named(fields) {
        if let Kind::Embedded(keys) = kind(sets, field) {
            let type_name = names.nested(owner, &n);
            out.push(record_type(sets, names, type_name, keys, None));
            collect_nested(sets, names, type_name, keys, out);
        }
    }
}

/// The object type of records (or embedded objects) with `fields`.
fn record_type(
    sets: &[ApiSet],
    names: &Names,
    type_name: &str,
    fields: &[Field],
    description: Option<&str>,
) -> Object {
    let mut object = Object::new(type_name);
    if let Some(d) = description {
        object = object.description(d);
    }
    object = object
        .field(
            // Null for a blank node, which comes embedded where it is used.
            json_field(
                "id",
                TypeRef::named(TypeRef::ID),
                "id",
                Kind::Scalar("String"),
                true,
            )
            .description("The IRI; null for a blank node, embedded where it is used"),
        )
        .field(json_field(
            "type",
            TypeRef::named_list(TypeRef::STRING),
            "type",
            Kind::Scalar("String"),
            false,
        ));
    let named = named(fields);
    if !named.iter().any(|(n, _)| n == "label") {
        // A reference's label, when the query only follows it that far.
        object = object.field(json_field(
            "label",
            TypeRef::named(TypeRef::STRING),
            "label",
            Kind::Scalar("String"),
            true,
        ));
    }
    for (n, field) in named {
        let (inner, k) = match kind(sets, field) {
            Kind::Scalar(s) => (s.to_owned(), Kind::Scalar(s)),
            Kind::Reference(target) => (names.set(target).to_owned(), Kind::Reference(target)),
            Kind::Embedded(keys) => (names.nested(type_name, &n).to_owned(), Kind::Embedded(keys)),
        };
        let single = field.single || field.lang();
        let ty = if single {
            TypeRef::named(&inner)
        } else {
            TypeRef::named_nn_list(&inner)
        };
        let mut f = json_field(&n, ty, &field.key, k, single);
        if let Some(d) = field.schema.get("description").and_then(Value::as_str) {
            f = f.description(d);
        }
        object = object.field(f);
    }
    object
}

/// A field that reads `key` of its parent's JSON.
fn json_field(gql: &str, ty: TypeRef, key: &str, kind: Kind<'_>, single: bool) -> GqlField {
    let key = key.to_owned();
    let scalar = match kind {
        Kind::Scalar(s) => Some(s),
        _ => None,
    };
    GqlField::new(gql, ty, move |ctx| {
        let key = key.clone();
        FieldFuture::new(async move {
            let parent = ctx.parent_value.try_downcast_ref::<Value>()?;
            let Some(v) = parent.get(&key).filter(|v| !v.is_null()) else {
                return Ok(None);
            };
            let values: Vec<&Value> = match v {
                Value::Array(a) if scalar != Some("LangString") => a.iter().collect(),
                one => vec![one],
            };
            let convert = |v: &Value| -> Option<FieldValue<'static>> {
                match scalar {
                    Some(s) => scalar_value(s, v).map(FieldValue::value),
                    None => Some(FieldValue::owned_any(v.clone())),
                }
            };
            if single {
                Ok(values.first().and_then(|v| convert(v)))
            } else {
                Ok(Some(FieldValue::list(
                    values.into_iter().filter_map(convert),
                )))
            }
        })
    })
}

/// A field that hands on its parent's JSON object under `key`.
fn object_field(gql: &str, ty: TypeRef, key: &str) -> GqlField {
    let key = key.to_owned();
    GqlField::new(gql, ty, move |ctx| {
        let key = key.clone();
        FieldFuture::new(async move {
            let parent = ctx.parent_value.try_downcast_ref::<Value>()?;
            Ok(parent
                .get(&key)
                .filter(|v| !v.is_null())
                .map(|v| FieldValue::owned_any(v.clone())))
        })
    })
}

/// A field that hands on each object of its parent's JSON list under `key`.
fn list_field(gql: &str, ty: TypeRef, key: &str) -> GqlField {
    let key = key.to_owned();
    GqlField::new(gql, ty, move |ctx| {
        let key = key.clone();
        FieldFuture::new(async move {
            let parent = ctx.parent_value.try_downcast_ref::<Value>()?;
            let items = parent
                .get(&key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Ok(Some(FieldValue::list(
                items.into_iter().map(FieldValue::owned_any),
            )))
        })
    })
}

/// A JSON value as a GraphQL scalar of type `scalar`; `None` when it is not one.
fn scalar_value(scalar: &str, v: &Value) -> Option<GqlValue> {
    match (scalar, v) {
        ("Int", Value::Number(n)) => n.as_i64().map(GqlValue::from),
        ("Float", Value::Number(n)) => n.as_f64().map(GqlValue::from),
        ("Boolean", Value::Bool(b)) => Some(GqlValue::from(*b)),
        ("LangString", v) => GqlValue::from_json(v.clone()).ok(),
        ("Int" | "Float" | "Boolean", _) => None,
        // A reference or embedded object read as text: its IRI.
        (_, Value::Object(map)) => map.get("id").and_then(Value::as_str).map(GqlValue::from),
        (_, Value::String(s)) => Some(GqlValue::from(s.as_str())),
        (_, other) => Some(GqlValue::from(other.to_string())),
    }
}

fn find<'a>(sets: &'a [ApiSet], slug: &str) -> Result<&'a ApiSet, async_graphql::Error> {
    sets.iter()
        .find(|s| s.slug == slug)
        .ok_or_else(|| async_graphql::Error::new(format!("No dataset {slug}")))
}

fn failed(e: HttpError) -> async_graphql::Error {
    async_graphql::Error::new(e.1)
}

/// The root list of a dataset: a page of its records, as a connection.
fn list_records(
    ctx: &ResolverContext<'_>,
    slug: &str,
) -> Result<FieldValue<'static>, async_graphql::Error> {
    let r = ctx.data::<Request>()?;
    // Once the request's time or values are spent, no more reads start.
    r.spend(1)?;
    let set = find(&r.sets, slug)?;
    let first = match ctx.args.get("first") {
        Some(v) => usize::try_from(v.i64()?).map_err(|_| "first must not be negative")?,
        None => DEFAULT_FIRST,
    };
    if first > MAX_FIRST {
        return Err(format!("first is at most {MAX_FIRST}").into());
    }
    let offset = match ctx.args.get("after") {
        Some(v) => {
            let cursor = v.string()?;
            cursor
                .parse::<usize>()
                .ok()
                .and_then(|c| c.checked_add(1))
                .ok_or_else(|| {
                    async_graphql::Error::new(format!(
                        "after is not a cursor of this list: {cursor}"
                    ))
                })?
        }
        None => 0,
    };
    let mut params = vec![
        ("limit".to_owned(), first.max(1).to_string()),
        ("offset".to_owned(), offset.to_string()),
    ];
    if let Some(filter) = ctx.args.get("filter") {
        let filter = filter.object()?;
        for (n, field) in named(&set.fields) {
            if let Some(v) = filter.get(&n) {
                params.push((field.key.clone(), v.string()?.to_owned()));
            }
        }
    }
    if let Some(q) = ctx.args.get("q") {
        params.push(("q".to_owned(), q.string()?.to_owned()));
    }
    if let Some(bbox) = ctx.args.get("bbox") {
        let numbers: Vec<String> = bbox
            .list()?
            .iter()
            .map(|v| v.f64().map(|f| f.to_string()))
            .collect::<Result<_, _>>()?;
        params.push(("bbox".to_owned(), numbers.join(",")));
    }
    let page = list(
        &r.entry,
        &r.sets,
        set,
        &Params(params),
        &r.base,
        r.timeout()?,
        &r.view,
        false,
    )
    .map_err(failed)?;
    let mut nodes: Vec<Value> = if first == 0 {
        Vec::new()
    } else {
        page.get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    r.spend(nodes.iter().map(size).sum())?;
    // The nodes' references the query follows, one batch per level.
    let field = ctx.ctx.field();
    for wanted in field.selection_set() {
        match wanted.name() {
            "nodes" => expand(r, &set.fields, &wanted, &mut nodes, 0)?,
            "edges" => {
                for inner in wanted.selection_set().filter(|s| s.name() == "node") {
                    expand(r, &set.fields, &inner, &mut nodes, 0)?;
                }
            }
            _ => {}
        }
    }
    let edges: Vec<Value> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| json!({ "cursor": (offset + i).to_string(), "node": n }))
        .collect();
    let end = (!nodes.is_empty()).then(|| (offset + nodes.len() - 1).to_string());
    Ok(FieldValue::owned_any(json!({
        "edges": edges,
        "nodes": nodes,
        "pageInfo": { "hasNextPage": first > 0 && page.get("next").is_some(), "endCursor": end },
    })))
}

/// The root field of one record, by its IRI.
fn one_record(
    ctx: &ResolverContext<'_>,
    slug: &str,
) -> Result<Option<FieldValue<'static>>, async_graphql::Error> {
    let r = ctx.data::<Request>()?;
    r.spend(1)?;
    let set = find(&r.sets, slug)?;
    let id = ctx.args.try_get("id")?.string()?;
    let Ok(iri) = NamedNode::new(id) else {
        return Err(format!("id is not an IRI: {id}").into());
    };
    let mut found = fetch(r, set, &[iri])?;
    if found.is_empty() {
        return Ok(None);
    }
    expand(r, &set.fields, &ctx.ctx.field(), &mut found, 0)?;
    Ok(found.pop().map(FieldValue::owned_any))
}

/// The records of `set` with these IRIs (those that are its records), in that order.
fn fetch(
    r: &Request,
    set: &ApiSet,
    iris: &[NamedNode],
) -> Result<Vec<Value>, async_graphql::Error> {
    let mut known = Vec::new();
    for chunk in iris.chunks(1_000) {
        let values: Vec<String> = chunk.iter().map(|i| i.to_string()).collect();
        let query = format!(
            "SELECT DISTINCT ?f WHERE {{ VALUES ?f {{ {} }} ?f a <{}> }}",
            values.join(" "),
            set.class.as_str()
        );
        for row in crate::ogc::run(&r.entry, &query, r.timeout()?, &r.view).map_err(failed)? {
            if let Some(tripletrail::oxigraph::model::Term::NamedNode(n)) =
                row.into_iter().next().flatten()
            {
                known.push(n);
            }
        }
    }
    let known: HashSet<NamedNode> = known.into_iter().collect();
    let order: Vec<NamedNode> = iris
        .iter()
        .filter(|i| known.contains(*i))
        .cloned()
        .collect();
    let found = records(&r.entry, &r.sets, set, &order, r.timeout()?, &r.view).map_err(failed)?;
    r.spend(found.iter().map(size).sum())?;
    Ok(found)
}

/// Replaces the references of `objects` (records or embedded objects with `fields`) that the
/// selection follows past `id`, `type` and `label` by the records they refer to, read in one
/// batch per dataset, and does the same below them.
fn expand(
    r: &Request,
    fields: &[Field],
    selection: &SelectionField<'_>,
    objects: &mut [Value],
    depth: usize,
) -> Result<(), async_graphql::Error> {
    if depth > MAX_DEPTH || objects.is_empty() {
        return Ok(());
    }
    let named = named(fields);
    for wanted in selection.selection_set() {
        let Some((_, field)) = named.iter().find(|(n, _)| n == wanted.name()) else {
            continue;
        };
        let deeper = wanted
            .selection_set()
            .any(|s| !matches!(s.name(), "id" | "type" | "label" | "__typename"));
        match kind(&r.sets, field) {
            Kind::Reference(target) if deeper => {
                // Blank nodes come embedded, without an id: their own references are followed.
                let keys = field
                    .embed
                    .as_deref()
                    .map_or(&target.fields[..], |k| &k[..]);
                embedded(r, field, keys, &wanted, objects, depth, |v| {
                    v.get("id").is_none()
                })?;
                let mut iris: Vec<NamedNode> = Vec::new();
                let mut seen = HashSet::new();
                for o in objects.iter() {
                    for v in values(o.get(&field.key)) {
                        if let Some(id) = v.get("id").and_then(Value::as_str)
                            && seen.insert(id.to_owned())
                            && let Ok(n) = NamedNode::new(id)
                        {
                            iris.push(n);
                        }
                    }
                }
                let mut read = fetch(r, target, &iris)?;
                expand(r, &target.fields, &wanted, &mut read, depth + 1)?;
                let by_id: HashMap<String, (Value, usize)> = read
                    .into_iter()
                    .filter_map(|v| {
                        let n = size(&v);
                        Some((v.get("id")?.as_str()?.to_owned(), (v, n)))
                    })
                    .collect();
                for o in objects.iter_mut() {
                    replace(r, o.get_mut(&field.key), &by_id)?;
                }
            }
            Kind::Embedded(keys) => {
                embedded(r, field, keys, &wanted, objects, depth, |_| true)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Expands the embedded objects under `field` of `objects` (those `pick` takes) as objects with
/// `keys`, as one flat list, and puts them back in the same places.
fn embedded(
    r: &Request,
    field: &Field,
    keys: &[Field],
    wanted: &SelectionField<'_>,
    objects: &mut [Value],
    depth: usize,
    pick: impl Fn(&Value) -> bool,
) -> Result<(), async_graphql::Error> {
    let mut inner: Vec<Value> = Vec::new();
    for o in objects.iter() {
        inner.extend(
            values(o.get(&field.key))
                .into_iter()
                .filter(|v| v.is_object() && pick(v))
                .cloned(),
        );
    }
    if inner.is_empty() {
        return Ok(());
    }
    expand(r, keys, wanted, &mut inner, depth + 1)?;
    let mut inner = inner.into_iter();
    for o in objects.iter_mut() {
        match o.get_mut(&field.key) {
            Some(Value::Array(a)) => {
                for v in a.iter_mut().filter(|v| v.is_object() && pick(v)) {
                    *v = inner.next().unwrap_or(Value::Null);
                }
            }
            Some(v @ Value::Object(_)) if pick(v) => *v = inner.next().unwrap_or(Value::Null),
            _ => {}
        }
    }
    Ok(())
}

/// The values of a key: one, or each of a list.
fn values(v: Option<&Value>) -> Vec<&Value> {
    match v {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.iter().collect(),
        Some(one) => vec![one],
    }
}

/// Each reference under `v` replaced by its record, keeping the reference's label.
fn replace(
    r: &Request,
    v: Option<&mut Value>,
    by_id: &HashMap<String, (Value, usize)>,
) -> Result<(), async_graphql::Error> {
    let one = |v: &mut Value| -> Result<(), async_graphql::Error> {
        let Some(id) = v.get("id").and_then(Value::as_str) else {
            return Ok(());
        };
        if let Some((record, n)) = by_id.get(id) {
            // Each place gets its own copy, so each copy counts.
            r.spend(*n)?;
            let label = v.get("label").cloned();
            let mut record = record.clone();
            if let (Some(label), Value::Object(map)) = (label, &mut record) {
                map.entry("label").or_insert(label);
            }
            *v = record;
        }
        Ok(())
    };
    match v {
        Some(Value::Array(a)) => a.iter_mut().try_for_each(one),
        Some(v @ Value::Object(_)) => one(v),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{name, plural};

    #[test]
    fn names() {
        assert_eq!(plural("operationalPoint"), "operationalPoints");
        assert_eq!(plural("address"), "addresses");
        assert_eq!(plural("city"), "cities");
        assert_eq!(plural("day"), "days");
        assert_eq!(name("track-gauge"), "track_gauge");
        assert_eq!(name("2nd"), "x2nd");
        assert_eq!(name("__x"), "x__x");
    }
}
