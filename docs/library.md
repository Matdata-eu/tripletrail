# Using TripleTrail as a library

The engine is a Rust crate (`tripletrail`) and a Python package (`tripletrail`, see
[python/README.md](../python/README.md)), so a pipeline can query and write in process without
going through HTTP. This page covers the Rust API; the Python package has the same methods.

## Rust

```rust
use tripletrail::Engine;
use tripletrail::oxigraph::io::RdfFormat;
use tripletrail::oxigraph::model::NamedNode;

let engine = Engine::new()?;
engine.load(RdfFormat::Turtle, std::fs::File::open("data.ttl")?)?;
engine.load_into(NamedNode::new("http://example.com/g")?, RdfFormat::NTriples, reader)?;
let results = engine.query("SELECT ...")?;
```

A load's blank nodes are its own, as Jena's loaders have it: `_:b1` in two loads is two
blank nodes. `Engine::load_with_blank_nodes(parser, reader, BlankNodes::Keep)` keeps the labels
as written, so a document can refer to blank nodes already stored.

A query run many times with different values is parsed once and given the values as
bindings, never written into its text:

```rust
use tripletrail::QueryOptions;
use tripletrail::oxigraph::sparql::Variable;

let within = engine.prepare_query(
    "PREFIX geo: <http://www.opengis.net/ont/geosparql#>
     SELECT ?f ?area WHERE { ?f geo:sfWithin ?area }",
    None, // base IRI
)?;
let options = QueryOptions {
    substitutions: vec![(Variable::new("area")?, NamedNode::new("http://example.com/zone")?.into())],
    ..Default::default()
};
let results = engine.query_prepared(&within, options)?;
```

`prepared.variables()` lists the variables it selects, the ones that can be bound. A bound
variable is bound when evaluation starts: GeoSPARQL relations and computed properties answer
as with the value written in the query, and the result cache keys on the values.

`Engine::new()` runs on the TripleTrail store (see [store.md](store.md)), as do
`Engine::new_tripletrail()` and `Engine::open_tripletrail(dir)`. `Engine::new_oxigraph()`, or
`Engine::new()` with the environment variable `TRIPLETRAIL_STORE=oxigraph`, uses Oxigraph's store
instead, and `Engine::from_store` takes an Oxigraph `Store`, for example one on RocksDB.
`engine.store()` is a `Dataset`, which wraps either store with the same methods.

The engine keeps the spatial index and routing graphs in step with its own writes (`load`,
`load_into`, `load_quads`, `insert`, `remove`, `update`). When it knows which quads a write
changed (`load_quads`, `insert` and `remove` on either store, `update` on the TripleTrail store), it
keeps the networks whose edge query reads none of the changed predicates and adds or removes the
changed geometries in the spatial index; otherwise it drops both. On the synthetic 30x30 grid
(59k elements), the next route query after adding a label costs 20 ms instead of a 2.3 s
rebuild, and the next indexed spatial query 1 ms instead of 0.45 s. After writing to
`engine.store()` directly, call `engine.invalidate()`.

## How some queries are planned

`FILTER EXISTS`/`NOT EXISTS` sub-patterns that share variables with the enclosing pattern are
planned from those variables: their triple patterns are ordered starting from the bound ones and
evaluated as nested lookups, rather than in written order as hash joins over the whole pattern
for every row (Oxigraph's optimizer does not reorder inside `EXISTS`).

A numeric range `FILTER(?km >= a && ?km <= b)` on a variable bound by `?x ex:km ?km` is answered
from a value-ordered index of `ex:km`'s numeric objects, built on first use and dropped by
writes to `ex:km`: the literals in the range become a `VALUES` block the pattern starts from,
when the range is small next to the rest of the pattern (`explain` lists the decision). On 313 k
line references, "between km 100 and 100.2 on any line" takes 1.6 ms instead of 556 ms, and
"km above 199.9" 0.6 ms instead of 86 ms; with a selective line constant the pattern keeps
starting from the line (5 ms either way).

## Saving and opening datasets

`engine.save(dir)` writes the data to a new dataset directory, and
`Engine::open_read_only(dir)` opens one memory-mapped and read-only (writes fail), with no load
step; `Engine::open_tripletrail(dir)` opens the same directory read-write. In Python:
`engine.save(path)`, `Engine.open_read_only(path)` and `engine.read_only`.

Validation, rules and entailment from the library are in
[validation-and-reasoning.md](validation-and-reasoning.md), GeoParquet in
[geoparquet.md](geoparquet.md).

## JavaScript client

`clients/js` (#223) is a client for browsers and Node 18 and later, over `fetch`, with no
dependencies and TypeScript types (`src/index.d.ts`). It is not published to npm yet: import
`clients/js/src/index.js` by its path, or copy it. Its tests start a `tripletrail serve` and run
against it (`cd clients/js && npm test`, with `TRIPLETRAIL_BIN` naming the binary when it is not
`target/debug/tripletrail`). A page on another origin needs the server started with `--cors <origin>` (see [CORS](server.md)).

```js
import { TripleTrail, toValues } from "./clients/js/src/index.js";

const kg = new TripleTrail("http://localhost:7878", { dataset: "kg", token: process.env.TOKEN });
await kg.update("INSERT DATA { <urn:a> <urn:b> 1 }");
const { variables, rows, incomplete } = await kg.select("SELECT * { ?s ?p ?o }");
```

| Call | Request |
|---|---|
| `new TripleTrail(url, {dataset, token, apiKey, headers, fetch})` | `token` as `Authorization: Bearer`, `apiKey` as `X-API-Key`; `dataset` is `ds` by default |
| `select(q, options)` | `POST /{ds}/query`: `{variables, rows, incomplete}`, each row a map from variable to SPARQL JSON term; `incomplete` when the row budget cut the answer (`partial: true`) |
| `ask(q)`, `construct(q, {accept})` | a boolean; RDF text (N-Triples unless `accept` names another format) |
| `query(q, {accept})` | the raw `Response` |
| `update(u, {usingGraphs, usingNamedGraphs})` | `POST /{ds}/update` |
| `graphs.get/put/post/delete(graph, …)` | the Graph Store Protocol on `/{ds}/data`; `null` is the default graph |
| `admin.ping/ready/server/datasets/dataset/stats` | `/$/ping`, `/$/ready` (no throw on 503), `/$/server`, `/$/datasets`, `/$/datasets/{name}`, `/$/stats` |
| `admin.createDataset(name, {type, location, mode, aliasOf, …})`, `admin.deleteDataset(name, {deleteFiles, force})` | `POST`/`DELETE /$/datasets` |
| `dataset(name)` | the same client for another dataset |

Query options: `defaultGraphs`, `namedGraphs` (the protocol's `default-graph-uri` and
`named-graph-uri`), `timeout` (seconds), `maxRows`, `partial`, `signal` (an `AbortSignal`).
A refused request throws `TripleTrailError` with `status` and `body`. `toValue(term)` gives a
number (a `bigint` for an integer beyond 2^53), a boolean or the text of a term, and
`toValues(result)` does that for every row.

