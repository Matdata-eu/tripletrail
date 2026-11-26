# Running the TripleTrail server

`tripletrail serve` replaces Fuseki for TripleTrail deployments: SPARQL 1.1 Protocol and Graph Store
Protocol endpoints with Fuseki's URL layout, a Fuseki-compatible admin API under `/$/`, aliases
for blue-green dataset releases, read-only memory-mapped datasets, backups, statistics, and a web
UI. Almost every option is also an environment variable (`tripletrail serve --help` lists them;
`--load` and the `--fx-*` options have none).

## Quick start

```sh
docker run -p 7878:7878 -v tripletrail-data:/data ghcr.io/matdata-eu/tripletrail
# or, with the tripletrail command installed (see the README):
tripletrail serve --location /data/tripletrail --admin-token "$TOKEN"
```

Open <http://localhost:7878/> for the web UI. Without `--dataset` or `--attach` the server
serves one dataset named `ds`.

The image is published for linux/amd64 and linux/arm64. Each one carries an SBOM and a
provenance attestation. Images will be signed with cosign, by the workflow that built them,
once the repository is public; then this checks one:

```sh
cosign verify ghcr.io/matdata-eu/tripletrail:<tag> \
  --certificate-identity-regexp '^https://github.com/Matdata-eu/tripletrail/.github/workflows/docker.yml@' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
docker buildx imagetools inspect ghcr.io/matdata-eu/tripletrail:<tag> --format '{{ json .SBOM }}'
```

A version tag (`v1.2.3`) publishes `1.2.3`, `1.2` and `latest`. Another image can copy the
command out of it: the binary is `/usr/local/bin/tripletrail` (glibc 2.36 or newer), and
`/usr/share/doc/tripletrail` holds `LICENSE`, `NOTICE` and `THIRD_PARTY_LICENSES.md` to copy
with it (see [Ship the command in another image](guide/13-production.md#ship-the-command-in-another-image)).

## Endpoints

Every dataset `{ds}` has these endpoints, at the same paths as on Fuseki, so clients written
for Fuseki or oxigraph-server work unchanged:

- `/{ds}/query`: `GET ?query=`, `POST` form or `application/sparql-query`. Results as JSON
  (default), XML, CSV, TSV or GeoParquet by `Accept` (or `?format=json|xml|csv|tsv|parquet`);
  CONSTRUCT/DESCRIBE as
  Turtle (default), N-Triples, RDF/XML, JSON-LD, RDF/JSON (`application/rdf+json`), TriX
  (`application/trix`), RDF Thrift (`application/rdf+thrift`) or RDF Protobuf
  (`application/rdf+protobuf`)... SELECT results also come as Jena's Thrift
  (`application/sparql-results+thrift`) and Protobuf (`application/sparql-results+protobuf`),
  or `?format=thrift|protobuf`; an unbound value is `UNDEF`. A syntax error, or a wrong
  parameter of one of TripleTrail's own services (`path:`, `pgr:`, `window:`, `st:`), is answered
  400 with the reason.
- `/{ds}/update`: `POST` form or `application/sparql-update`. `/{ds}/sparql` and `/{ds}` take
  both.
- `/{ds}/data`: Graph Store Protocol. `?graph=<iri>` or `?default` for one graph, nothing for
  the whole dataset (N-Quads/TriG/TriX/Thrift/Protobuf). Besides Oxigraph's formats it reads
  and writes Jena's RDF/JSON (`application/rdf+json`, one graph), TriX (`application/trix`),
  RDF Thrift (`application/rdf+thrift`) and RDF Protobuf (`application/rdf+protobuf`), the
  last three for a graph or the dataset, by `Content-Type`, `Accept` or
  `?format=rdfjson|trix|thrift|protobuf`. Such a document is read whole before it is loaded.
  The binary formats are read in every form Jena writes (prefix names, the value forms of
  numbers, repeated terms) and written in full terms, one row a triple as it comes: triple
  rows for the default graph, quad rows for a named one. TriX is written as it comes too, a
  `<graph>` each time the graph changes. RDF/JSON groups by subject, so it is built in memory
  and holds at most 1,000,000 triples; a larger graph asks for another format. Decimals in
  the binary formats are read with a scale of at most 1,000 digits. `GET`, `PUT` (replace), `POST` (append; bulk load, the
  fastest way to write millions of triples), `DELETE`. `/{ds}/get` is read only. An upload is
  parsed in full before anything is written, so a body with a syntax error is answered with
  400 and changes nothing (`PUT` does not empty the graph). `--max-upload` (bytes) caps its size.
  An upload of up to 100,000 quads is one transaction: a `PUT` replaces the graph at once
  (readers never see it empty), and the spatial index and cached counts are updated rather
  than rebuilt. A larger one is bulk-loaded. Each upload's blank nodes are its own, as on
  Fuseki: `_:b1` in two uploads (or in two parts of a multipart upload) is two blank nodes.
- A `GET` of `/{ds}/query`, `/{ds}/sparql`, `/{ds}/update` or `/{ds}` without a query or
  update returns the [SPARQL 1.1 Service Description](https://www.w3.org/TR/sparql11-service-description/)
  (Turtle by default, or any RDF format by `Accept`): the languages, result and input
  formats, every extension function and aggregate the engine registers, the built-in SERVICEs
  (`pgr`, `st`, `path`, `window`: each an `sd:Service` of its own that supports SPARQL 1.1
  Query), and the named graphs. It passes the W3C Service Description tests. It is generated from the engine's registries, so new
  functions appear without anyone writing them down. A browser (`Accept: text/html`) is sent
  to the UI.
- `/.well-known/void` (every dataset) and `/{ds}/void`: [VoID](https://www.w3.org/TR/void/)
  descriptions with the dataset's size, distinct subjects, properties and objects, a class
  partition (instances per class), a property partition (triples, distinct subjects and
  objects per property) and one subset per graph. The numbers come from the counts kept with
  the indexes and are computed once per version of the data (a few ms at 1M triples); while
  writes not yet folded into the indexes are under 1 % of the store they are left out.
  MatGUI reads these for its dataset overview and its class and property autocompletion.
- `/.well-known/dcat`: a [DCAT-AP](https://semiceu.github.io/DCAT-AP/) catalog for data
  portals (data.europa.eu and others harvest DCAT). The catalog `<{origin}/#catalog>` lists
  each dataset as a `dcat:Dataset` (the node of its VoID description), with two
  distributions, its SPARQL endpoint (`/{ds}/void#sparql`) and an N-Quads dump
  (`/{ds}/void#dump`, downloaded from `/{ds}/get?format=nquads`), and its
  endpoint as a `dcat:DataService` (`dcat:endpointURL`, `dct:conformsTo` SPARQL 1.1
  Protocol). `--catalog FILE` (`TRIPLETRAIL_CATALOG`) adds what the server cannot know: a
  Turtle file whose relative IRIs resolve against the server's URL, `<#catalog>` for the
  catalog and `<kg/void#dataset>` for the dataset `kg` (an alias stands for the dataset it
  names). A title or description given there replaces the generated one; without them the
  catalog and each dataset still get both, as DCAT-AP requires. What the file says about a
  distribution (`<kg/void#dump> dct:license …`) or the endpoint (`<kg/sparql>`) is served
  with its dataset. Only what the catalog and the datasets the caller may list lead to is
  served, and no IRI under a dataset it may not list (`/{ds}/…`), so a hidden dataset's
  record never shows. A `Host` that gives no IRI is `400`. The file is checked when the server starts.
- `/{ds}/explain`: evaluates a query and returns its plan with row counts and timings per
  operator, and what the spatial rewrite did, as JSON.
- `POST /{ds}/shacl`: validates the dataset against shapes in the body, as Fuseki's SHACL
  service does (see [SHACL service](#shacl-service)).

### Query hints

When the planner picks a bad plan, `#pragma tt.…` comment lines in a query or update change
how that request runs (#238). Other engines read them as comments.

| Hint | Effect |
|---|---|
| `#pragma tt.optimizer off` (or `tt.join-order fixed`) | Join the patterns in the written order: no join reordering, and no planning of `EXISTS` patterns. |
| `#pragma tt.spatial-rewrite off` | Evaluate GeoSPARQL filters and joins as written, without the spatial index. |
| `#pragma tt.range-rewrite off` | Evaluate numeric range `FILTER`s without the range index. |
| `#pragma tt.id-rewrites off` | Evaluate counts and rows without the shortcuts on term ids. These shortcuts order their own joins, so a `COUNT` keeps its order under `tt.optimizer off` unless this hint is given too. |
| `#pragma tt.result-cache off` | The result cache neither answers nor keeps the query. |
| `#pragma tt.executor ids` (or `spareval`) | Which executor answers the query, over `--executor`: `ids` answers a `SELECT` or `CONSTRUCT` whose pattern runs on term ids straight from the id tables; `spareval` hands it to the evaluator. `/{ds}/explain` says which one did under `executor`. |
| `#pragma tt.timeout 30s` | The request's timeout (`500ms`, `2m`, or seconds), like `?timeout=`, which wins when both are given. Never more than the server's or the dataset's timeout. |

The answers stay the same; only the work done changes. A query with one of the first five
hints, or `tt.executor`, is neither answered from nor kept in the result cache, so timing it
measures the work.
Hints are read from comments only: a `#pragma` line inside a string literal is data. An unknown `tt.` hint, or a bad
value, is answered with 400. Another engine's `#pragma` lines are ignored. `/{ds}/explain`
lists the hints in force under `hints`. `--log-requests` and the slow query log write them
with the request (`hints`), so you can find the queries the planner gets wrong.

Blazegraph's query hints, written as triples in `hint:` (`<http://www.bigdata.com/queryHints#>`)
as Wikidata queries do, are taken out of the query before it runs, so they never match data.
`hint:Query hint:optimizer "None"` (also on `hint:Group`, `hint:SubQuery` or `hint:Prior`) is
`#pragma tt.optimizer off`; the other hints (`hint:runFirst`, `hint:analytic`, …) are ignored.
They are taken out everywhere in the query, `EXISTS` included, except inside `SERVICE`: the
other endpoint gets them. So a triple pattern with a `hint:` subject and predicate never
matches stored triples.
Virtuoso's `DEFINE` clauses before the prologue are taken out as well: `DEFINE
sql:select-option "order"` is `#pragma tt.optimizer off`. An `input:` one (`input:inference`,
`input:default-graph-uri`) would change the answers, so it is answered with 400; the others
are ignored.

With one dataset, the paths work without `/{ds}` too. Under `/{ds}` there are also
[OGC API Features](#ogc-api--features) (`/{ds}/ogc`), [vector tiles](#vector-tiles), a [REST API](#rest-api-json-ld) (`/{ds}/api`), [GraphQL](#graphql) (`/{ds}/graphql`),
[describe pages](#describe-pages) (`/{ds}/describe`), the [RDF4J REST API](#rdf4j-rest-api), and
with `--mcp` an [MCP endpoint](#mcp) for AI agents (`/{ds}/mcp`).

### Main options

```sh
tripletrail serve --dataset kg --timeout 600                                  # in memory
tripletrail serve --location /data/tripletrail --dataset kg --dataset rinf      # on disk, two datasets
tripletrail load --location /data/tripletrail --dataset kg kg.nt                 # bulk load offline
# Oxigraph's store instead, on disk in RocksDB (build with --features rocksdb):
tripletrail serve --store oxigraph --location /data/tripletrail --dataset kg
```

- `--timeout` (seconds) cancels queries and updates that run longer, with a 503; a request
  can ask for a shorter one with `?timeout=`, never a longer one. Query, update and form bodies
  over `--max-body` (16 MiB by default) are answered with 413. Cancellation is checked at each
  store access, while the spatial rewrite plans the query, in the routing algorithms and in
  long-running geo functions.
- Once the datasets are open, `serve` builds each one's string ranks (the order of its IRIs
  and strings, which `MIN`, `MAX` and `ORDER BY` on the id executor read; 8 bytes a term, for
  dictionaries of up to 16 million terms; logged as `{ds}: string ranks of N terms in S s`,
  0.14 s for 460,000 terms) and then makes its map in the background, one dataset
  after the other (#92): the map's collections (the profile of the data) and the unfiltered
  facet counts of each, kept in the caches that requests read, so the first visitor does not
  wait for them. Each facet query gets the dataset's timeout, as a map request does, and a
  dataset's warm-up at most five minutes in all: the collections left then are made by the
  first request that needs them. The log says `{ds}: map ready: N collections and their facets
  in S s`, or which collections ran out of time, with the facets that failed (a computed facet
  that times out). After a load, an update or a change of the shapes, it makes the map again
  once the dataset has kept still for 30 s (#258), and logs `{ds}: after a change, map ready:
  …`, so the first visitor after a nightly load does not wait for the facet counts either
  (on RINF's eight countries, 0.13 s instead of 68 s for the facets of all 31 collections).
  `--no-warm-up`
  (`TRIPLETRAIL_NO_WARM_UP`) leaves both to the first query or map request that needs them. A dataset that is not on
  TripleTrail's store keeps no such caches and is skipped.
- `--cors <origin>` (repeatable) allows cross-origin requests from those origins, `--cors`
  alone from every origin. Pages may send `Authorization`, `Content-Type`, `Accept` and `X-API-Key`, and
  read `X-Result-Incomplete`, `X-Cache`, `X-Truncated` and `Server` from answers. Every answer
  carries `Server: TripleTrail/<version>`, which names the engine so that query tools can
  recognise it: MatGUI's Explain view (from 6.15.0) and YASGUI's explain plugin (from 1.1.0)
  then ask `/{ds}/explain` for the plan.
- `--union-default-graph kg,build` (or `*`, `TRIPLETRAIL_UNION_DEFAULT_GRAPH`;
  `unionDefaultGraph=true` when creating a dataset over the admin API) makes the default graph
  of queries and updates the union of all graphs, as Fuseki's `tdb2:unionDefaultGraph`: rules
  without `USING` read every source graph. Inserts without `GRAPH` still go to the real default
  graph, and `FROM` / `USING` keep their own dataset. The `st:`, `window`, `path` and `pgr`
  services read the union too, including the `pgr:edgeQuery` of a named network stored in a
  named graph (#861). `COPY`, `MOVE` and `ADD DEFAULT TO <g>`
  take the stored default graph, not the union, as Fuseki does. In the `USING` of an update,
  `<urn:x-arq:DefaultGraph>` names the stored default graph, as in Jena. Queries on the union
  keep the id-level shortcuts (counts from the indexes, joins on ids): on a store with named
  graphs they read every graph and skip the copies of a triple in later graphs (#808). The
  copies are found by one scan of the store, made by the first query whose triple patterns
  match at least an eighth of the quads, and then kept up to date by each write from the
  subjects it changed. Lookups and other queries without groups, `MINUS`, `FILTER` or `ORDER
  BY` never wait for them. Until they are found, and past 4 million copies, queries on the
  union go to the generic evaluator, which is correct but can be several times slower. The
  copies count in the dataset's memory as `unionCopies`.
- `--update-token <token>` makes writes (updates, and `POST`/`PUT`/`DELETE` on `/{ds}/data`)
  need `Authorization: Bearer <token>` (or the admin token); queries stay open.
- `--network-cache-mb` (2048 by default, 0 for no limit) caps the memory of routing networks
  built from inline edge sub-selects; past it the least recently used are dropped. Named
  networks (`pgr:network`) are always kept.
- `--result-cache-mb` (256 by default, 0 turns it off) keeps the results of repeated `SELECT`
  and `ASK` queries per dataset, answered again without evaluating until the next write. The
  `X-Cache` response header says `HIT`, `MISS` (evaluated, kept if it fits in a quarter of
  the budget) or `BYPASS` (not cached: `RAND`, `NOW`, `UUID`, `STRUUID`, `BNODE`, a `SERVICE`
  other than TripleTrail's own, a user-defined function that is not inlined,
  `CONSTRUCT`/`DESCRIBE`, or Oxigraph's store). In the library,
  `Engine::set_result_cache` (off by default).
- `--executor` (`TRIPLETRAIL_EXECUTOR`, `ids` by default) picks what answers a `SELECT`
  or `CONSTRUCT` whose pattern runs on term ids (#11). `ids` answers it straight from the id tables (triple
  patterns, joins, `OPTIONAL` (with a filter inside, also on values of the left side), `MINUS`, `UNION`, `FILTER (NOT) EXISTS` (also with filters and nested `EXISTS` inside, and a filter on values of the outer row), the filters on ids (with the effective boolean value of a column, `FILTER(?ok)`),
  `BIND` and `SELECT (expr AS ?v)` of copies, constants, arithmetic with division, `ABS`, `CEIL`, `FLOOR`, `ROUND`, casts to `xsd:integer`, `xsd:decimal`, `xsd:string`, `xsd:boolean`, `xsd:double`, `xsd:float`, `xsd:dateTime`, `xsd:date` and `xsd:time`, `IF`, `COALESCE`, `STR`, `CONCAT`, `IRI`, `STRLANG`, `STRDT`, `REPLACE`, `UCASE`, `LCASE`, `STRLEN`, `SUBSTR`, `STRBEFORE`, `STRAFTER`, `LANG`, `DATATYPE`, `isIRI`, `isBlank`, `isLiteral`, `MD5`, `SHA1`, `SHA256`, `SHA384`, `SHA512` and `ENCODE_FOR_URI`, any other expression of the values alone (`TZ`, date-time and duration arithmetic, `IF` on a language, …) with the evaluator's own code, also as filters (not `NOW`, `RAND`, `UUID`, `STRUUID`, `BNODE`, `IRI`, `EXISTS` or custom functions), filters comparing or testing such strings, and conditions and string tests (`STRSTARTS`, `STRENDS`, `CONTAINS`, `REGEX` with constant arguments) as booleans (the full list and its limits are in [the store](store.md)), `VALUES`, `GRAPH` with an IRI or a variable, sub-selects (with `DISTINCT`, an order and a slice), the `GROUP BY` counts and sums kept as ids, `ORDER BY` of variables (also of numbers a `BIND` computes) and of the expressions the kernels compute, `DISTINCT`, `REDUCED`, `OFFSET` and `LIMIT` with an order), decoding
  only the terms of the answer, each once, on the query threads; anything else still goes to
  the evaluator. SPARQL JSON, CSV and TSV answers are written from the ids: each distinct term
  is written once and copied into the rows where it occurs (the bytes are those of the usual
  writers); other formats, and a `--max-rows` budget, get the solutions as terms. The result
  cache keeps such an answer as it is. On 133k orders, a three-pattern join returning every
  row took 333 ms as CSV and takes 88 ms (JSON 402 → 119 ms), and an `OPTIONAL` 180 → 15 ms. The answers are the same, and the test suites agree under both.
  `ids` is the default since the benchmark of tripletrail#11 (Trainmarks, BSBM, Sparqloscope: the
  same answers, none more than 10 % slower, Trainmarks CONSTRUCT 2.3× and BSBM BI query 2 3× faster);
  `spareval` hands every query to the evaluator, as before. A query picks its own with
  `#pragma tt.executor`. In the
  library, `Engine::set_default_executor_ids`.
  What changes for an operator under `ids`: an answer is held whole before its first byte is
  written, so a query that times out while it is being written answers 503 instead of a body
  cut short, and the first byte of a large answer comes later (0.4 s instead of 2 ms for
  180 MB of CSV), the whole answer 3 to 5 times sooner. Its memory counts against
  `--query-memory` (below) like the id joins'. An answer without an `ORDER BY` of more than
  about 8 million cells (rows times variables) streams from the evaluator instead, as under
  `spareval`; the patterns of triples that give its rows (alone, or under a `UNION`, `BIND`
  or `FILTER`, #234) are counted first when they may give that many, so it is never built
  (100 million rows of a two-pattern join: 126 MB instead of 2.1 GB at the peak, three at
  once 132 MB instead of 5 GB; 98 million rows under a `UNION` 77 MB instead of 1.9 GB, under
  a `BIND` 96 MB instead of 1.8 GB, with the first byte after 14 to 44 ms instead of 0.5 to
  0.9 s). A `FILTER` is counted as the ids would run it (`FILTER(?s = ?x)` joins fewer rows
  and stays on ids); over a large join it is handed over too, which for a filter keeping a
  few rows is also faster (0.85 s instead of 1.5 s for 1% of 100 million rows), but the
  evaluator holds the join's rows as well (1.4 GB instead of 2.0 GB). An ordered answer, or one under other
  patterns (an `OPTIONAL`, a sub-select), is built first before it is handed over. A server
  with many concurrent exports sets `--query-memory`, which keeps such answers flat (at
  `64MB`). A parallel join on ids now stays within that budget: its parts take at most half,
  so that their copy into one table fits in the rest (it took up to twice the budget).
- `--memory-limit` (`TRIPLETRAIL_MEMORY_LIMIT`, e.g. `32G`; also on `tripletrail load`) is the
  memory a bulk load (`--load`, or a Graph Store upload) may use to sort its quads. A larger
  load sorts them on disk, under the dataset's directory, and writes the indexes straight to
  files, so a dataset larger than memory can be loaded. Without it everything is sorted in
  memory.
- `tripletrail load` writes a progress line to standard error every minute while a file loads
  (#232): the store's step (`reading`, `sorting`, `writing`, `entailing`), the triples read so
  far, and how much of the file has been read. `Store::load_progress()` gives the same step
  and count to code that loads through the store.
- `tripletrail load` makes a new dataset that keeps a WKT literal only in binary form when
  its text can be written back from the geometry exactly (#152, from #370's `--render-wkt`):
  about the size of the text less on disk, and coordinates with at most 9 decimals are kept as
  fixed-point differences, 2 or 3 bytes each instead of 8 (an OpenStreetMap extract of Norway:
  178 MB instead of 731 MB), the same text in results, about 2 µs more per WKT literal a result writes out,
  spatial work unchanged. `--keep-wkt-text` keeps every text, for a new dataset or to turn it
  off on an existing one; `--render-wkt` turns it on for a dataset made before it was the
  default. Without either flag an existing dataset keeps its setting, which stays with the
  dataset (a `render-wkt` file). Loading WKT that no style writes back exactly (numbers with
  17 significant digits, a CRS IRI written another way) takes a little longer than with
  `--keep-wkt-text`, for the check: Geographica's corine and linkedgeodata, 2.5 s instead of
  2.1 s. On-disk datasets the server creates (`POST /$/datasets` with
  `type=disk`, `serve --location` with a new name) and other storages keep the WKT text, as
  before.
- `--threads` (4 by default) is the number of worker threads that answer requests.
  `--query-threads` (`TRIPLETRAIL_QUERY_THREADS`, 0 by default: as many as cores) is the size
  of the pool that large joins (the sorted joins of a basic graph pattern too), `OPTIONAL`,
  `MINUS`, `EXISTS`, `FILTER`, `GROUP BY` and the aggregates of its groups run on, for one
  query; the queries share it, and 1 keeps each query on its own thread. The answers, and
  their order, are the same with any number. The SHACL rules of an `infer`
  step run on as many threads at once (#891); 1 runs them one at a time.
- `--functions functions.ttl` (repeatable; `TRIPLETRAIL_FUNCTIONS`, comma-separated) loads
  SHACL-AF `sh:SPARQLFunction`s that the queries and updates of every dataset can call by their
  IRIs; see [user-defined functions](validation-and-reasoning.md#user-defined-functions).
- `--vector-graph-min N` (`TRIPLETRAIL_VECTOR_GRAPH_MIN`, 20,000 by default) is how many
  vectors of one length `tt:nearest` compares one by one, exactly. Past it, it builds a graph
  of them (HNSW) in the background, on half the cores, and searches that: fast, but
  approximate. `0` keeps every search exact. See [vector search](vector-search.md#exact-then-approximate).
- The spatial index is built in the background at start, and again after a write that drops
  it, rather than on the first spatial query. `/$/ready` answers 200 once every dataset is open
  and its index built (503 before), and `/$/status` gives the state per dataset.
- With `--location`, routing networks and their contraction hierarchies are kept in
  `<location>/<dataset>.networks`, so a restart does not rebuild them, and so are the graphs
  of `tt:nearest` (in `vectors/`, see [vector search](vector-search.md#exact-then-approximate)).


## Dataset directories

A dataset directory is what `tripletrail load`, the Python package's `Engine.save(path)`,
`Engine::save(dir)` and the snapshot endpoint write: a checkpoint of the TripleTrail store and an
empty write-ahead log. The same directory opens in two modes.

- **Read-write** (`--attach kg=/data/kg`, or `mode=read-write`): one server writes the
  directory. Writes go to its log and later to new checkpoints.
- **Read-only** (`--attach kg=/data/kg:ro`, `--read-only`, or `mode=read-only`): nothing on
  disk changes, so several processes can serve the same directory: updates and uploads answer
  405. This is the mode for published releases (a RINF version, a knowledge graph build).

In both modes the checkpoint files are memory-mapped, not read into the heap. The pages are
shared with the page cache and with every process that maps the same files, are read on first
touch, and can be dropped by the kernel under memory pressure.

Build a release once, offline, and publish it as a directory:

```python
import tripletrail
engine = tripletrail.Engine()
engine.load(path="network.nt")
engine.save("/data/network_20261107")        # fails if the directory exists and is not empty
tripletrail.Engine.open_read_only("/data/network_20261107")   # what the server does with :ro
```

### Upgrading TripleTrail

A store keeps its format in the `FORMAT` file of its directory (`1`; a directory without one,
from before the file, is format 1 too, and gets the file the next time it opens read-write).
The format goes up only when an older TripleTrail would misread the files or the write-ahead log
of a newer one.

- **Newer TripleTrail, older store:** it opens as it is. Files of older versions are read, and
  the next checkpoint writes them in the current version. Nothing to do.
- **Older TripleTrail, newer store:** refused before anything is read, replayed or cut, in both
  modes, with `… is in format 2, written by a newer TripleTrail: this one (0.x) reads up to
  format 1. Open it with a newer tripletrail (it was not changed)`. A single store file of a newer
  version (its magic number, such as `OXGSSEG5`) is refused with the same message instead of
  "corrupt store file". To go back, restore a store backup taken before the upgrade.
- **Backups and replicas:** store backups and bases carry their `FORMAT` file. A restore or
  copy of a newer TripleTrail's backup is refused the same way, before its logs are read, and so
  is a base copied from a leader of a newer format. A replica that already has its copy stops
  applying the log of a leader upgraded to a newer format (each log read gives the leader's
  format in `X-TripleTrail-Store-Format`), and shows the same message as its error in
  `/$/replication`, until it is upgraded too. So upgrade the replicas before the leader.

A change of format that needs more than the next checkpoint will come with a migration command
and its release notes.

## Releasing a new version without downtime

```sh
T="Authorization: Bearer $TOKEN"
# 1. Attach the new version next to the old one.
curl -H "$T" -X POST "http://srv:7878/\$/datasets?name=network_20261107&location=/data/network_20261107&mode=read-only"
# 2. Point the stable name at it. New requests go to the new version at once; requests
#    already running finish on the old one. release=true detaches the old one once idle.
curl -H "$T" -X POST "http://srv:7878/\$/datasets/network/swap?to=network_20261107&release=true"
# Roll back: swap to the previous version (GET /$/datasets/network shows it as "previous").
curl -H "$T" -X POST "http://srv:7878/\$/datasets/network/swap?to=network_20261106"
```

Clients always query `/network/sparql`. The alias is created with
`POST /$/datasets?name=network&alias-of=network_20261106`, or `--alias network=network_20261106` at start.

## Admin API

All under `/$/`. Requests that change something (`POST`, `PUT`, `DELETE`) need
`Authorization: Bearer <token>` when `--admin-token` is set. Without a token, anyone who reaches
the server can attach and detach datasets, so set one outside a private network. Parameters go
in the query string or in a form body, as with Fuseki.

Writes to the datasets themselves (SPARQL updates, `POST`/`PUT`/`DELETE` on `/{ds}/data`) need
`Authorization: Bearer <token>` when `--update-token` (`TRIPLETRAIL_UPDATE_TOKEN`) is set; the
admin token is accepted there too. Queries need no token.

| Request | Does |
|---|---|
| `GET /$/ping` | `ok`: the process answers |
| `GET /$/role/{name}` | 200 `leader` when the server takes writes to the dataset; 503 `follower` (a read replica) or `read-only`; open without a token, for load balancers ([promotion](#read-replicas)) |
| `GET /$/ready` | 200 when every dataset is open and its spatial index built; 503 while starting, building or stopping. Per dataset: `spatial_index`, and for the TripleTrail store `files` (the check of its files: `checking`, `verified`, `damaged` or `stopped`) with `damaged_file`. A damaged dataset does not hold readiness back; it is listed under `damaged` |
| `GET /$/status` | readiness per dataset |
| `GET /$/server` | version, uptime, settings, datasets |
| `GET /$/datasets` | datasets and aliases, with Fuseki's `ds.name`, `ds.state`, `ds.services` and ours (mode, location, aliases...) |
| `GET /$/datasets/{name}` | one of them, with triple and graph counts |
| `GET /$/datasets/{name}/profile` | what the dataset contains, for generated shapes and the map: see [Dataset profile](#dataset-profile) |
| `GET /$/datasets/{name}/shapes` | the SHACL shapes a reader reads (`reader=`, `profile=`): see [Shapes](#shapes) |
| `GET /$/datasets/{name}/shapes/files`, `GET`/`PUT`/`DELETE …/shapes/files/{file}` | the dataset's own shape set, file by file: see [A dataset's own shapes](#a-datasets-own-shapes) |
| `PUT /$/datasets/{name}/shapes/files` | the whole set as its next version: `{"files": {name: text}, "source": {…}}`, checked as one set (the Workbench's **Promote shapes…**) |
| `POST /$/datasets/{name}/shapes/customise`, `…/validate`, `…/rollback?version=N` | start the set from the generated shapes, check a file, go back to a kept version |
| `GET /$/datasets/{name}/shapes/outline`, `POST …/shapes/edit` | the shapes editor's view of the layers and its changes to reader settings: see [The shapes editor](#the-shapes-editor) |
| `GET`/`POST /$/datasets/{name}/shapes/sparql` | a SPARQL query over the shapes: see [Querying the shapes](#querying-the-shapes) |
| `POST /$/datasets` | `name` (or Fuseki's `dbName`) and one of: `location` + `mode` (attach a directory), `type=mem` or `type=disk` (new empty dataset; `disk` goes to `<location>/<name>`), `alias-of` (an alias), plus `from=<backup file>` to restore into a new dataset; `unionDefaultGraph=true`; `entailment=rdfs` or `entailment=owl-rl` (with `entailmentGraph=`, default `urn:tripletrail:inferred`) keeps the RDFS or OWL 2 RL entailment up to date with every write, like `--rdfs` and `--owl-rl` |
| `DELETE /$/datasets/{name}` | detach once its running requests finish; `delete-files=true` also removes its directory (only a dataset directory); refused while an alias points at it unless `force=true` |
| `POST /$/datasets/{alias}/swap?to=` | point an alias elsewhere; `release=true` detaches the old target when idle |
| `POST /$/datasets/{name}/snapshot?location=` | save the dataset as a new dataset directory |
| `POST /$/datasets/{name}/entailment?entailment=rdfs\|owl-rl\|none` | start keeping the RDFS or OWL 2 RL entailment (entailing once; `entailmentGraph=` as above) or stop. A graph that holds triples (and is not the one kept now) is refused with 409, because entailing replaces its content; give `replace=true` to replace it, as after a restart when it holds the old entailment. Moving it to another graph empties the old one. Stopping leaves the graph as it is, as data that no longer follows writes (`DROP GRAPH` it if it should go). The setting lasts while the dataset is attached; give `--rdfs` or `--owl-rl` to keep it across restarts |
| `POST /$/backup/{name}` | write `<name>_<date>_<time>.nq.gz` to the backup directory in the background; 202 with a task id. The backup holds exactly what was committed when it started. With `kind=store`, write `<name>_<date>_<time>.store` instead: a copy of the dataset's store (see below) |
| `GET /$/backups-list` | backup files, each with its kind (`nquads` or `store`), dataset, time and size |
| `POST /$/backups/{file}/verify` | check a backup in the background; 202 with a task id, whose `quads` is what the backup holds |
| `POST /$/digest/{name}` | compute the SHA-256 of the dataset's RDFC-1.0 canonical N-Quads in the background; 202 with a task id, whose `sha256` is the digest once it finishes. Isomorphic datasets (the same up to blank node labels) have the same digest; see [canonicalization](canonicalization.md). It holds the dataset in memory, so one digest or diff runs at a time (409 otherwise). Needs the admin token, or the dataset's admins |
| `POST /$/diff/{name}?to={other}` | compare two datasets (two releases) in the background; 202 with a task id, whose `diff` holds the digests and sizes of both, and the added and removed quads in all and per graph, predicate and class of the subject, as `tripletrail diff` prints them. Needs the admin token, or admin of both datasets |
| `GET /$/tasks`, `GET /$/tasks/{id}` | background tasks and how they ended |
| `GET /$/stats`, `GET /$/stats/{name}` | requests (Fuseki's keys), queries, updates, uploads, failures, triples, graphs, size on disk, memory-mapped bytes, last write, last swap |

### Dataset profile

`GET /$/datasets/{name}/profile` describes what a dataset contains, as JSON: the start of
generated SHACL shapes and of the readers that work from them (map, describe pages, OGC API
Features), (#137). For each class, most
instances first:

- `instances`, from the index counts, and `sampled`: the instances read for the rest;
- `label`, `superclasses` (`rdfs:subClassOf`) and `labelProperties`, the label properties its
  instances use (`skos:prefLabel`, `rdfs:label`, `schema:name`, `dct:title`…);
- `properties`, most used first: on how many sampled instances (`instances`), how many values,
  `minCount` and `maxCount` per instance (`minCount` 0 when some have none), how many IRIs,
  blank nodes and literals, the `datatypes`, `languages` and target `classes` of the values,
  and `distinctValues` for a code list (at most 20 values, each used twice on average);
- `geometries`: the property paths by which instances reach a geometry literal (direct
  `geo:asWKT`, `geo:hasGeometry/geo:asWKT`, or up to four links such as
  `era:netReference/geo:hasGeometry/geo:asWKT`), with the datatype, how many of the
  path-searched instances reach one, and the geometry types (`POINT`, `LINESTRING`…).

`sample` (default 200 instances per class), `path_sample` (default 20, the instances whose
links are followed for geometry paths) and `classes` (default 500) bound the work, and at
most 4,096 links are read per node, so the counts on a node with more (a hub) are a lower
bound. A profile costs about the same on large data as on small: 0.15 s for one country's
RINF graph (1.2 million triples, 40 classes). It runs under the dataset's query timeout
(`timeout=` asks for less) and answers `503` when that runs out, as a query does. It is kept
until the data changes. A caller sees the profile of a dataset they may read.

### Shapes

`GET /$/datasets/{name}/shapes` answers the SHACL shapes the readers of a dataset work from
(validation, the map, describe pages, search, OGC API Features, the REST and GraphQL APIs, the
AI assistant), in Turtle or another RDF format (`Accept`, or `format=`). A dataset without
[shapes of its own](#a-datasets-own-shapes) is read with shapes generated from its
[profile](#dataset-profile), and the answer says so in `TripleTrail-Shapes: generated`, with the
data version in `TripleTrail-Data-Generation`. The profile parameters (`sample`, `path_sample`,
`classes`, `timeout`) apply.

The generated set has a node shape per class (`urn:tripletrail:shape:` and the class IRI,
percent-encoded, so a customised set can override it by IRI) with:

- per property a property shape: `sh:path`, `sh:name` (the property's label, else its local
  name), `sh:nodeKind`, `sh:datatype` or `sh:class` when the values agree, `sh:in` for a code
  list, and `sh:minCount`/`sh:maxCount` as seen, only when every instance was read (a sample
  says nothing about the others);
- reader annotations in the `tts:` vocabulary (`http://w3id.org/tripletrail/shapes#`,
  [`crates/tripletrail/src/shapes/tts.ttl`](../crates/tripletrail/src/shapes/tts.ttl)): `tts:key` (a short name
  for APIs), `tts:labelPath`, `tts:searchable`, `tts:iriTemplate` (never generated: the IRI of a new
  record, see [writes](#rest-api-json-ld)), `tts:geometry` (each path to a geometry with
  its types), map `tts:color` and `tts:minZoom`, `tts:facet` (`tts:Facet-Values` for a code
  list, `tts:Facet-Range` for numbers and dates, `tts:Facet-Text` for other strings), `tts:time` (never generated: the OGC API's
  `datetime` filter), `tts:drawOrder` (never generated: where the map draws the class), `tts:colorClasses`
  with `tts:upTo` (never generated: a range facet's classes on the map), `tts:colorBy` (never
  generated: a facet's place at the top of the map's "Colour by", lowest first), map views
  (`tts:MapView`, never generated: a named lens on the map with `tts:key`, `rdfs:label`,
  `rdfs:comment`, `sh:order`, the classes it shows, `tts:showsClass`, the filters it sets,
  `tts:filter [ tts:key ; sh:in ( … ) ]` or `sh:minInclusive`/`sh:maxInclusive` on a range
  facet, the facet it colours by, `tts:colorFacet`, and the facets its filter panel lists
  first, `tts:listsFacets ( key … )`), computed facets (`tts:select`, never
  generated: a SPARQL group pattern, full IRIs, that binds `?value` for the feature `$this` in
  place of the facet's path; it is joined where the feature is bound, its other variables
  renamed apart, and only the map reads it: it is no property or filter of the OGC API) and
  `sh:group` for describe pages. A class
  without a geometry is `tts:hiddenFrom` the map and OGC API Features. Two kinds of classes
  with a geometry are hidden from the map only (#139), as it would draw their features twice:
  GeoSPARQL and Simple Features classes (`geo:Geometry`, `sf:Point`…), whose instances are
  other features' geometries, and umbrella classes, whose every read instance also has
  another type, and whose other types are all narrower (classes with fewer instances) (RINF's
  `era:InfrastructureElement` over operational points, tracks and sections). The sample of the
  first instances tells first; when it says yes, the profile reads every instance's types to
  confirm, as the instances outside the sample may have no other type.

`reader=` (`validation`, `map`, `describe`, `search`, `ogc-features`, `rest-api`, `graphql`,
`ai`, `forms`) answers that reader's projection: shapes it is `tts:hiddenFrom`, or not `tts:onlyFor`,
left out, deactivated shapes (`sh:deactivated true`) left out, a data-quality facet added to
each node shape for the map when a pack or the own set names a named graph holding a stored
SHACL validation report with `tts:qualityReport` (key `quality`, a `tts:select` facet: the
worst `sh:resultSeverity` of the report's results whose `sh:focusNode` is the feature,
`violation`, `warning` or `info`, else `ok`; a shape with a facet of that key keeps its own)
(#92), the generated node shapes left
out when a pack or the dataset's own set names the reader with `tts:hidesGenerated` (on a
`tts:ShapeSet` node of its own, such as `<urn:my:set> a tts:ShapeSet ; tts:hidesGenerated
tts:Reader-Map`: the map shows only the set's classes; a generated shape the set overrides or
refines stays) (#92), the facets of parts rolled up to their parent for the map (a node shape
with `tts:rollsUpTo [ sh:path P ; sh:class C ]` gives each node shape that targets `C` its
facets that the parent has no facet of that key for, read through `( [ sh:inversePath P ]
path )`, `tts:onlyFor tts:Reader-Map`, with `tts:rolledUpFrom` naming the part's shape: a
parent matches a filter when one of its parts does, counts once per value of its parts, and is
coloured by its parts' highest number; `tts:select` facets and those hidden from the map stay
with the part; `import-atlas` writes it for RINF Atlas's running tracks and tracks to the
section of line) (#248), and, for all readers but
validation, the property shapes of the superclasses' node shapes added to each node shape that
has none for that path (or key). A superclass's node shape hidden from the reader still lends its
property shapes: an abstract `ex:AssetShape` `tts:hiddenFrom tts:Reader-Map` gives no asset
layer, and the signal layer still has its owner facet. A property shape hidden from the reader
itself is not lent (`import-atlas --hidden-from` hides the facets as well as the node shape, so
the map's track facets do not reach the describe pages of sidings). `profile=` (IRIs, repeated or comma-separated) keeps only the node
shapes `tts:inProfile` one of them, and those in no profile, which are in all.

#### Querying the shapes

`GET` or `POST /$/datasets/{name}/shapes/sparql` (`query=`, a form, or an
`application/sparql-query` body) runs a SPARQL query over the shapes, answered as a query on
the dataset is (formats, `timeout`, `max-rows`). The default graph holds what `GET …/shapes`
answers for the same `reader=` and `profile=`; the named graph `urn:tripletrail:shapes:generated`
holds the generated layer, `urn:tripletrail:shapes:pack` the installed
[shape pack](#shape-packs) (with `tts:pack` and `tts:packVersion` on
`<urn:tripletrail:shapes:pack>`), `urn:tripletrail:shapes:local` the dataset's own set (with its
`tts:version` on `<urn:tripletrail:shapes:local>`), and `http://w3id.org/tripletrail/shapes` the
vocabulary, for the readers' labels. `TripleTrail-Shapes` names the set in use. A query reads
only: a `POST` needs no admin rights.

#### A dataset's own shapes

A dataset can have a shape set of its own: a folder of Turtle files, one per node shape, so
it reads and diffs well. Its node shapes go on top of the generated ones, replacing those with
the same IRI with their property shapes (a generated shape's IRI is `urn:tripletrail:shape:` and
the percent-encoded class IRI), and those whose class one of its node shapes written for the
readers targets (one with an `tts:` annotation, such as `tts:geometry`, and without
`tts:onlyFor`), whatever its IRI; a plain SHACL constraint shape adds to the generated one. A replacing shape without `tts:instances`
takes the generated shape's count. A shape with the same IRI and `tts:refines true` changes the
shape below instead of replacing it: what it says about the node shape replaces the lower
values of those predicates, each of its property shapes replaces the lower one with the same
`sh:path`, and the rest of the lower shape stays, including property shapes for paths the
data gains later. A refining shape replaces no shape by its class: when no lower shape has its
IRI, it refines the lower shapes for its class in the same way (so an edit of a generated
shape still applies once a pack replaces it), and with none it stays as it is. The Shapes page
writes such shapes. The generated shapes of the other
classes stay, so a class that appears later in the data is still read. A shape is left out with `sh:deactivated true`.
The answer of `GET …/shapes` then says `TripleTrail-Shapes: local; version=N`, and the set's node
`urn:tripletrail:shapes:local` carries `tts:version`.

- `POST /$/datasets/{name}/shapes/customise` writes the generated shapes as the dataset's set,
  one file per class (named by its `tts:key`) and `_set.ttl` for the class hierarchy. `409`
  when the dataset has a set already; `force=true` starts it again (as a new version).
- `GET …/shapes/files` lists the files of the current version, the version and the kept
  versions; `GET …/shapes/files/{file}` answers one file.
- `PUT …/shapes/files/{file}` (Turtle in the body) adds or replaces a file, `DELETE` removes
  one. Each change makes a new version, kept only when the whole set is valid: else `422`
  with the errors, each naming its file or shape.
- `POST …/shapes/validate` checks the set, or with `file=` the set with the body as that file
  (on an in-memory dataset without `--location` too, against an empty set),
  and answers `{"valid": …, "errors": […]}` without keeping anything.
- `POST …/shapes/rollback?version=N` makes a kept version current again, as a new version.

#### The shapes editor

The UI's **Shapes** page (`/$/ui/shapes/{name}`, #145) reads and changes the shapes through
two requests:

- `GET …/shapes/outline` gives every node shape of the layers together (generated < pack <
  own), with `iri`, `classes`, `layer` (`generated`, `pack` or `own`), `overrides` (an own or
  pack shape that replaces a lower one), `refines` (it changes the lower one, with
  `tts:refines`), `file` (the own file that holds it), `instances`, its
  settings, and `properties`: each property shape's `path`, `iri`, `class` (its `sh:class`) or
  `datatype`, and settings, in `sh:order`.
  Settings are `name`, `description`, `label` and `key` (text; with several languages also
  `nameByLanguage` and the like), `order`, `group`, `viewer` (IRIs), `hidden`
  (`dash:hidden`), `deactivated`, `hiddenFrom` and `onlyFor` (lists of readers), and
  `profiles` (the IRIs of its `tts:inProfile`, node shapes only), `color`, `minZoom` and
  `drawOrder` (the map's `tts:` settings of a node shape), and `facet` and `time` (IRIs of
  `tts:facet` and `tts:time`). The answer also gives
  `source`, `version`, `versions`, `bound` (the repository, when the set comes from git),
  `editable`, and `profiles`: each profile the shapes declare (`a tts:Profile`) or name, with
  its `iri`, `label` (`skos:prefLabel` or `rdfs:label`) and the number of node `shapes` in it. It works without `--location` too, read-only.
- `GET …/shapes/form` gives the fields of the page's forms, for a node shape (`node`) and a
  property shape (`property`), read from the editor shapes the server ships
  (`src/shapes/editor.ttl`: SHACL with `dash:editor`). Each field has its `setting`, `label`,
  `description`, `editor` (the `dash:` editor's local name, such as `TextFieldWithLangEditor`
  or `EnumSelectEditor`), `multiple`, and when they apply `datatype`, `pattern`, `min`, `max`,
  the values to choose from (`in`: `{value, label}`) or the `class` whose instances are
  offered (`sh:PropertyGroup`, `tts:Profile`).
  The web UI draws these, and the record forms, with one renderer, `/$/ui/forms.js`
  (`window.TripleTrailForms`): editors and viewers are looked up by the local name of a field's
  `dash:editor` or `dash:viewer` (`BooleanSelectEditor`, `EnumSelectEditor`, `TextAreaEditor`,
  `TextAreaWithLangEditor`, `TextFieldEditor`, `DatePickerEditor`, `DateTimePickerEditor`,
  `URIEditor`; `LiteralViewer`, `URIViewer`, `LabelViewer`, `ImageViewer`), else chosen by the
  field's values, and a page may register its own with `registerEditor` and `registerViewer`.
  Values come from the data, so the viewers link only `http` and `https` IRIs and show any
  other IRI (`urn:`, `javascript:`…) as text; `ImageViewer` shows only web images, fetched
  without a referrer, so their host does not learn which record shows them.
- `POST …/shapes/edit` with `{"changes": [{"shape", "path"?, "setting", "value", "lang"?}]}`
  changes settings in the own set and keeps it as its next version (`{"version", "changed"}`).
  `path` picks the property shape of that `sh:path` (none: the node shape itself); `value:
  null`, `""` or `false` removes the setting; a text replaces the values of its `lang` only;
  `hiddenFrom` and `onlyFor` take a list of reader names, `profiles` a list of IRIs. The body
  can also hold `"profiles": [{"iri", "label"}]`, profiles to declare (`a tts:Profile ;
  skos:prefLabel`) in the own file that declares profiles, or a new `profiles.ttl`; with
  them, `changes` may be empty. A shape that is not in the own set
  yet (generated, or from the pack) gets a file of its own first, holding a shape that refines
  it (`tts:refines true`, below) with the changed settings and property shapes only, so the
  rest, and properties the data gains later, still come from below. The changed files are rewritten from their triples, so their comments and
  layout are not kept. The whole set is checked before it is kept (`422` otherwise); an
  unknown shape or path answers `404`, an unknown setting or a wrong value `400`. With
  `dry=true` (or `1`, or empty; `false` and `0` make the change, other values are `400`)
  nothing is written and the answer gives the changed `files` as text, also for a
  set bound to git or a dataset attached read-only, whose other changes answer `409`; with
  `all=true` as well, `files` holds every file of the set as it would be, which the page's
  **Preview on map** sends to `POST /{ds}/tiles/preview`.

The check parses every file and then checks the `tts:` annotations: terms the vocabulary
defines, readers that are readers, colours `#rrggbb`, zooms from 0 to 24, keys used once
among the node shapes and once among a node shape's property shapes, one `sh:path` per
property shape, IRIs as targets, and computed facets' patterns (`tts:select`): inlined as
the map runs them (variables renamed, after `?f a <class> .`), they parse, balance their
braces, bind `?value` and call no `SERVICE` but TripleTrail's own (`st:`, window, `pgr`,
path). File names are letters, digits, `_`, `-` and `.`, ending in
`.ttl`.

The set is kept in the store directory's `shapes` folder (for an in-memory dataset, under the
server's `--location`, in `.shapes/{name}`; without one, `409`): `v1`, `v2`… with `CURRENT`
naming the current one. The last 10 versions are kept. Changing the set needs the server's
admin rights or, with access control, the dataset's admins; a dataset attached read-only
has its shapes read, not changed (`409`).

The shapes go with the data: a snapshot and a store backup (`kind=store`) copy the folder
into the copy, so a restore, or attaching the copy, brings them back (an N-Quads backup holds
the data only). A swap gives the alias's new target the old target's shapes when it has none
of its own. Detaching an in-memory dataset removes its shapes; a disk dataset's stay in its
directory, removed with it by `delete-files=true`.

#### Shape packs

A shape pack is a set of shapes written once for a domain (a data standard such as RINF, a
company's model) and installed on each dataset that holds such data. It is a layer between
the generated shapes and the dataset's own set (#148): its node shapes replace the generated
ones as an own set does (by IRI, or by the class a shape written for the readers targets),
and the dataset's own set goes on top of both. So a dataset follows the pack, and a file of
its own set overrides the pack for that class only. The answer of `GET …/shapes` then starts
`TripleTrail-Shapes` with `pack NAME@VERSION`: `pack rail@1.0`, or
`pack rail@1.0; local; version=3` with an own set.

```bash
curl -X PUT -H "Authorization: Bearer $ADMIN" http://localhost:7878/$/datasets/rinf/shapes/pack \
  -d '{"name": "rail", "version": "1.0", "files": {"Station.ttl": "@prefix sh: …"},
       "source": {"repository": "https://github.com/acme/rail-shapes.git"}}'
```

- `PUT /$/datasets/{name}/shapes/pack` installs a pack, in place of the one installed:
  `name` and `version` (letters, digits, `_`, `-`, `.` and `+`), `files` (file name to
  Turtle, at least one) and, optionally, `source` (`repository` becomes the pack's
  `tts:source`). The files are checked as an own set's are (`422` with the errors); nothing
  changes when they fail. It answers `{"name", "version", "files", "replaced"}`, `replaced`
  naming the pack it replaced. A new version of a pack keeps the dataset's own set.
- `PUT …/shapes/pack` with `{"git": {"url", "tag", "path", "user", "tokenFile"}}` in place of
  `files` reads the pack from a tag of a git repository (or `ref`: a branch, `HEAD` or a full
  ref; not both): the `.ttl` files under `path`, fetched and checked as a
  [git binding's](#shapes-from-a-git-repository) are (the same URL, token and
  `--git-fetch-mb` rules; `502` when the fetch fails). The pack is named after the
  repository (`rail-shapes` for `…/rail-shapes.git`) and versioned by the tag, or by the
  commit's first 12 digits for another ref, unless `name` and `version` are given; its
  `source` is the repository, ref, path and commit, the commit also as `tts:commit` on
  `<urn:tripletrail:shapes:pack>`. Unlike a binding, a pack from git is read once: install the
  next tag to upgrade. As a binding does, it names files and URLs of the server (`tokenFile`,
  `file://`), so with access control it needs the group `tripletrail:admin` (`403` for a
  dataset's admins, who install a pack's `files`).
- `GET …/shapes/pack` answers `{"installed", "name", "version", "source", "files"}`, each
  file with its `name` and `bytes`.
- `DELETE …/shapes/pack` removes the pack (`404` when there is none); the generated shapes
  are read again where the own set has no shape.
- `PUT …/shapes/pack?dry=true`, with either body, checks the release before it is installed
  and keeps nothing (the same rights; a pack from git is fetched). `dry` is `true`, `1` or
  empty, or `false` or `0` for an install; any other value is `400`, as for `…/shapes/edit`. It answers `200` with a
  report: `valid` and `errors` (each naming its layer, `pack:` or `own set:`; with errors the
  rest is left out); `name`, `version` and `replaces` (the installed pack, or `null`);
  `added`, `removed` and `changed`, the IRIs of the named node shapes the release adds, drops
  and changes against the installed one (a shape with its property shapes, compared
  canonically); `overrides`, the own set's shapes that meet a shape of the release
  (`{"shape", "packShape", "how"}`, `how` being `iri`, `refines` or `class`, as the layers
  above apply them); `orphaned`, the own shapes that met a shape of the installed release and
  meet none of the new one, and so would stand alone after the upgrade (`packShape` the
  installed one's); and `noData`, the release's node shapes whose `sh:targetClass` has no
  instance in any graph of the dataset (`{"shape", "class"}`; a check that runs out of the
  query timeout counts as found).
- `tripletrail serve --pack DIR` installs the pack in a folder as the datasets open: on the first
  `--dataset`, or `--pack NAME=DIR` on another `--dataset` or `--attach` (repeatable; not a
  `--follow` replica, which takes its leader's shapes). The folder holds the pack's `.ttl`
  files and, optionally, `pack.json` with its `name`, `version` and `source` (as in a `PUT`);
  without one the pack is named after the folder, version `local`, and one that cannot be
  read is an error. A release already installed (the same
  name, version, source and files) is left as it is, so a restart changes nothing; another replaces
  the installed one as a `PUT` does, keeping the own set. A pack that does not check, or a
  dataset that is in memory without `--location` or read-only, stops the server from starting.
- `tripletrail shapes check --pack DIR` makes the same check without a server, on folders of
  `.ttl` files: `--installed DIR` (the release installed now), `--own DIR` (the own set) and
  the data as `--data FILE…` or `--location DIR [--dataset NAME]` (without either, `noData`
  is `null`). It prints the report as text, or as the JSON above with `--json`, and exits
  with status 1 when a set has errors, and with `--strict` also when an override would stand
  alone.
- `tripletrail shapes import-atlas FOLDER --out DIR` writes the shapes of
  [RINF Atlas](https://rinf-atlas.matdata.eu) (SHACL node shapes with `atlas:` annotations,
  the `.ttl` files of `FOLDER`) as a shape set, one file per node shape, to start a pack from.
  `atlas:key`, `color`, `minZoom`, `drawOrder` and `labelPath` become the `tts:` terms of the
  same name; `atlas:geometryPath` (each branch of an alternative) and `atlas:geometryFrom P`
  (each geometry path of the set behind `P`) become one `tts:geometry`, an
  `sh:alternativePath` when there are several, so a feature with a geometry on two branches
  has both (RINF Atlas takes the first); then, as RINF Atlas derives a geometry round after
  round, a way for each of two more rounds, behind `P` and one (two) more
  `atlas:geometryFrom` links of the set, for the features the ways before gave none (a
  detector takes its track's section's); `atlas:lineBetween ( A B )` becomes a last way
  with `tts:lineBetween`, each end every geometry path of the set behind `A` or `B`, for the
  features without a geometry of their own; `atlas:enabled false` becomes `sh:deactivated`;
  facets of kind `terms` and `boolean`, `range` and `text` get `tts:facet`
  `tts:Facet-Values`, `-Range` and `-Text`, `link` facets none, with `atlas:facet` as `tts:key`
  and the label as `sh:name`; `atlas:colorClasses` become `tts:colorClasses`, a facet's
  `atlas:unit` (a QUDT unit's local name) `qudt:hasUnit`, and a node shape's `atlas:order` its
  `sh:order` (its place in the map's legend), and a facet's `atlas:colorBy` `tts:colorBy`; the
  facets of an `atlas:shared` shape go to every node shape. The views in the folder's `views/`
  become `tts:MapView`s (`view-{key}.ttl`): `atlas:viewClass` names the class of the node
  shape with that key (`tts:showsClass`), `atlas:filter` becomes `tts:filter` and an
  `atlas:colorBy` naming a facet or check `tts:colorFacet` (one on a facet none of the
  view's classes has is counted, as the map leaves it out). A view's facets
  (`sh:property [ atlas:facet ; sh:order ]`) become `tts:listsFacets`, by their order, after the
  facet of its group picker (`atlas:groupBy`), when some class of the set has them. A view's requirement checks
  (`atlas:check`) become computed facets (`tts:select`, `tts:onlyFor tts:Reader-Map`) of the
  classes it checks (`atlas:checkClass`): `atlas:Meets`, `atlas:DoesNotMeet`, or
  `atlas:whenMissing` (default `atlas:NoData`) for a feature without a compared value, none
  where `atlas:appliesWhen` does not hold, with `atlas:impliedValue` and `atlas:termValue`
  as in RINF Atlas, and the summary (`atlas:summaryCheck`, the worst result) when there are
  several. The section class (`atlas:checkSection`) gets each as the best result of its
  parts, the checked features that take their geometry from it (`atlas:geometryFrom`).
  What TripleTrail has no reader for (record facets, `atlas:geometry`, the group pickers and
  listed facets of views, the vehicle compatibility rules) is left out and counted on standard error. `--hidden-from READER`
  (repeatable) adds `tts:hiddenFrom` to the node shapes and their property shapes, for a pack
  with other shapes for that reader (a subclass's shape for that reader inherits none of them), and
  `--prefix` starts each file name.

`customise` starts the own set from what is read without one: the generated shapes with the
pack's on top. Each file it writes then holds a copy of the pack's shape, which overrides
later versions of the pack; remove the files of the classes you do not change to keep
following the pack for those. The pack is kept in the shapes folder's `pack` subfolder, with
`pack.json` naming it (a pack from git keeps its fetched objects in `.pack-git`, not copied,
and removed with the pack). A new pack is written aside and moved in place; a server that
stops halfway keeps the old pack, put back when it is next read. The pack goes with the data
as an own set does (snapshots, store backups, swaps). Installing and removing need the same rights as changing the own set; a read-only
dataset answers `409`. A dataset bound to a git repository can have a pack too: the
repository gives the own set.

#### Shapes from a git repository

A dataset's set can come from a git repository instead, the way a team keeps its shapes:
the repository is the master, and the server pulls from it. The server fetches the ref (only
its last commit), reads the `.ttl` files under the path (subfolders included, read as one
set), checks them as above and saves them as the next version, naming the commit. A commit
that fails the check leaves the shapes as they were; the error is kept for
`GET …/shapes/git`. `GET …/shapes` then says
`TripleTrail-Shapes: git; version=N; commit=<hash>`, and `urn:tripletrail:shapes:local` carries the
repository URL as `tts:source` and the commit as `tts:commit`.

```bash
curl -X PUT -H "Authorization: Bearer $ADMIN" http://localhost:7878/$/datasets/rinf/shapes/git \
  -d '{"url": "https://github.com/acme/rail-shapes.git", "ref": "main", "path": "shapes",
       "tokenFile": "/run/secrets/shapes-token", "webhookSecretFile": "/run/secrets/hook",
       "poll": 600}'
```

- `PUT …/shapes/git` binds the dataset (and takes the forge settings of proposals, below). `url` is `https://`, `http://`, `file://` or `ssh://`;
  `ref` a branch, `HEAD` (the default) or a full ref (`refs/tags/v2`); `path` a folder (the
  root by default). `tokenFile` is a file on the server with a token, sent over https as the
  password (`user`, default `x-access-token`; GitLab takes any); an http(s) URL with a user or
  password in it is refused (`400`), so a token never shows up in the binding or the shapes. `poll` pulls every that many
  seconds. The binding is kept only when its first pull gives a valid set: else `422` (the set
  fails the check) or `502` (the fetch fails), and nothing changes, the last pull of a binding
  in place included. A pull is "unchanged" only for the same repository, ref, path and commit. Binding names files and
  URLs of the server, so it needs the server's admin rights, also with access control.
- `GET …/shapes/git` shows the binding (without the server's paths) and the last pull: when,
  its commit and version, or its error.
- `POST …/shapes/pull` pulls now: `{"commit", "version", "changed"}`, `422` when the commit
  fails the check, `502` when the fetch fails. It is open to the dataset's admins, and to push
  webhooks signed with the binding's `webhookSecretFile`: GitHub's `X-Hub-Signature-256`
  (send the payload as `application/json`) or GitLab's `X-Gitlab-Token`.
- `DELETE …/shapes/git` unbinds the dataset; its set stays, editable again.

While bound, the files, customise and rollback requests answer `409`. The server also pulls
each bound dataset when it first sees it (at start, or when it is attached). The fetched
objects are kept in the shapes folder's `.git` (not copied with the shapes); `file://` and
`ssh://` repositories need the `git` and `ssh` programs, `https://` none. https servers are
checked against the Mozilla root certificates. `--git-fetch-mb` (`TRIPLETRAIL_GIT_FETCH_MB`, 256
by default, 0 for no limit) caps a pull: a fetch that adds more than that many MiB to the
`.git` folder is stopped and its pack dropped, and the `.ttl` files read from the commit may
take that many MiB together, at most 10,000 files. Either answers `502` naming the limit.
Workspace pulls get the same fetch cap, and read within the workspace's own caps. The feature
`git` of the server crate (on by default) builds this in.

The pull records where each file is in the repository (`files` of the version's source), for
proposals.

#### Proposed shape changes (TripleTrail Enterprise)

On a bound set, the shapes editor's changes can be proposed as a pull request on the
repository (#145, licence feature `review`): the server commits the changed files onto the
commit it last pulled, on a new branch, through GitHub's or GitLab's REST API (no git push),
and opens a pull request (a merge request on GitLab) against the bound branch (the
repository's default branch for `HEAD`). The repository's review decides; when the request is
merged, the push webhook pulls the new commit as for any push, so publishing is the merge.
Proposing needs the right to change the dataset's shapes (its admins, or the admin token),
or a place in the dataset's `shapeEditors` (see [dataset access](#dataset-access-with-policies)): shape
editors propose, dry-run and refresh proposals, and change nothing else on the server; the
proposal records who proposed it (`by`).

```bash
curl -X POST -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' \
  http://localhost:7878/$/datasets/rinf/shapes/proposals \
  -d '{"title": "Stations in English", "description": "For the public map.",
       "changes": [{"shape": "https://example.org/pack/rail#Station", "setting": "name",
                    "value": "Railway station", "lang": "en"}]}'
```

- `POST …/shapes/proposals` takes the body of `…/shapes/edit` (`changes`, `profiles`) with a
  `title` (at most 200 characters) and a `description`. The changes are checked as an edit is;
  a changed file keeps its path in the repository, a new one goes under the binding's `path`.
  `201` with the proposal: `proposal` (its number), `title`, `by` (the caller's agent, `admin`
  for the admin token), `at`, `forge`, `branch` (`tripletrail/{dataset}-shapes-{n}`), `base`,
  `commit` (the pulled commit it starts from), `files`, `number` and `url` of the pull request,
  and `status` (`open`). `400` without a title or when nothing changes, `403` without a licence
  that covers `review`, `409` when the set is not bound or not yet pulled, is bound to a tag
  (a pull request needs a branch), was pulled before the paths were recorded (pull again: an
  unchanged pull records them), or the binding has no token, `502` when the forge refuses (its
  answer, cut to 300 characters). A proposal the forge refused is kept with `status`
  `failed` and its `error`, so the next one takes the next number and branch (the failed
  one's branch may be left on the repository).
- Each proposal says what it changes (`effects`), and so does its pull request's description:
  for each reader (`readers`: `map`, `describe`, `search`, `validation`…) the node shapes it
  would read that it did not (`added`), no longer read (`removed`), and those it reads
  differently (`changed`: `settings` when the node shape's own settings change, and
  `rowsAdded`, `rowsRemoved`, `rowsChanged`: its property shapes by `tts:key` or `rd:key` as
  it is, else by the path written as N-Triples terms, `<http://example.com/x>`, blank nodes
  written out); and for each profile (`profiles`) the node shapes that join it or leave it.
  Readers and profiles without a change are left out. The pull request's description puts
  readers with the same effect on one line, names the first 10 shapes, rows and files of each
  list and counts the rest, writes names as inline code without backticks or control
  characters (cut to 200 characters), and keeps the effects under 30,000 characters and the
  proposal's description under 20,000; the whole `effects` stay in the proposal. `POST …/shapes/proposals?dry=true` answers the
  same without asking the forge: `files` (`path`, `new`, `text`) and `effects`, `200`.
- Once the pull request is open, the server validates all of the dataset's default graph
  with the validation reader's shapes as they are and as proposed, in the background, and
  keeps the result as the proposal's `validation`. Both sets are checked on one snapshot of the
  store, so they see the same data; writes go on meanwhile (the check does not hold the
  store's dictionary lock) and are not seen by it; with `--union-default-graph`, the default graph is the
  union of the dataset's graphs and is copied once, whole, for the check. One validation runs
  at a time on the server, at most an hour; the others wait. `validation` has `status`
  (`queued`, `running`, `done`, `failed` with an `error`, or `interrupted` when the server
  stopped before it was done), `before` and `after` (how many results each set gives), and
  `new` and `gone`: results told apart by their
  [finding key](validation-and-reasoning.md#finding-keys) (focus node, path, value, source
  shape by its stable IRI, constraint component and severity). In the answer, a blank source
  shape is written out, IRIs are as they are and other terms as in N-Triples. With them come `newByShape` (the 20 shapes with the most new results) and
  `newResults` (the first 20), and `ms`. Results are counted as they are found, some 40 bytes
  for each distinct one (about 75 MB for 3 million results on 1 million nodes); past 4 million
  distinct results, `compared` is `false` and `moreByShape` gives the shapes with more results
  than before (`before`, `after`) in place of `new`, `gone` and the lists.
- The validation is posted on the pull request as a comment (if posting fails, `comment` says
  why): the counts and the new results by shape. The new results themselves (focus nodes and
  values, which are data) are listed there only when the binding has `reviewResults: true`:
  who may read the repository need not be who may read the data. The proposal on the server
  keeps them either way. `validate=false` skips the validation; a dry run validates at once
  only with `validate=true`, within the request's time limit (`timeout`, and the dataset's
  limits), answers it as `validation`, and gets `503` while a background validation runs.
- `GET …/shapes/proposals` lists them, newest first (the last 100 are kept in the shapes
  folder's `.proposals.json`, not copied with the shapes), with `licensed` and `bound`.
  `GET …/shapes/proposals?refresh=true` first asks the forge about the pull requests of the
  20 newest `open` proposals: a merged one gets `status` `merged`, a closed one `closed`
  (GitLab's `locked` too), and each gets `checked` (when), or `statusError` when the forge
  could not say (its status stays). A plain list does not call the forge. A refresh spends
  the server's review token, so it is a change: it needs the right to change the dataset's
  shapes (its admins, or the admin token) or a place in its `shapeEditors`, and the audit log records it. It is bounded: a
  proposal checked in the last minute is not asked again, one refresh runs at a time for a
  dataset (another caller meanwhile gets the list as it stands), and each question waits at
  most 10 seconds, all of them at most 30. `403` without a licence that covers `review`,
  `409` when the set is not bound or the binding has no token.
  The Shapes page refreshes when its **Proposals** list is opened.
- The binding says which forge: `forge` (`github` or `gitlab`; read from the URL for
  `github.com` and hosts named `gitlab…`), `api` (by default `https://api.github.com`,
  `https://{host}/api/v3` for GitHub Enterprise, `https://{host}/api/v4` for GitLab), `project`
  (`owner/repo`, or the GitLab project's path; by default the URL's path) and
  `reviewTokenFile`, a file on the server with a token that may push branches and open pull
  requests (by default the `tokenFile`, which pulls may use read-only). The token is sent only
  over https, or plain http to this machine; an `api` that is neither is refused (`400`), and
  `reviewResults` (`true` or `false`, by default `false`) says whether the pull request's
  comment lists new validation results.

The build without the feature `enterprise` (or `git`) answers `501`.

### Workbench workspaces

A workspace (#400) is a sandbox of the workbench for developing shapes, rules, queries and
pipelines on a copy of some data. It is a folder of files and a scratch store built from them;
it never writes to a dataset.

```
data/      RDF (.ttl, .nt, .nq, .trig, .rdf, .jsonld, .n3, optionally .gz): the default graph
shapes/    SHACL shapes: graph <urn:tripletrail:workspace:shapes>
rules/     SHACL-AF rules: graph <urn:tripletrail:workspace:rules>
queries/   updates/   lift/   tests/   pipeline.yaml   README.md
```

```sh
# Make one, with files
curl -X POST http://localhost:7878/$/workspaces -H 'Content-Type: application/json' \
  -d '{"name": "track shapes", "files": {"shapes/track.ttl": "…"}}'
# Copy 500 tracks of rinf and what they point to, two hops out, to data/sample-rinf.nt
curl -X POST http://localhost:7878/$/workspaces/$ID/sample -H 'Content-Type: application/json' \
  -d '{"dataset": "rinf", "classes": ["http://data.europa.eu/949/Track"], "instances": 500, "hops": 2}'
# Query everything
curl http://localhost:7878/$/workspaces/$ID/sparql --data-urlencode 'query=SELECT …'
```

- `GET /$/workspaces` lists the caller's workspaces; `POST` makes one (`name`, `lifetime`
  `scratch` or `saved`, `files` as `{path: text}`), `201` with its `id` and `Location`.
- `GET /$/workspaces/{id}`: its files with their sizes and triples (and `sampled` for a file a
  sample wrote, through edits, until it is deleted or a pull writes over it), when it expires, and the graphs. `POST` (or `PATCH`) with `name` or `lifetime` changes them; `DELETE` deletes it.
- `GET`, `PUT` and `DELETE /$/workspaces/{id}/files/{path}`: one file. An RDF file that does not
  parse is refused with `400` and the parser's message, starting with the file's path.
- `POST /$/workspaces/{id}/sample`: `dataset`, `classes` (IRIs), `instances` per class (default
  100), `hops` (default 2: the instances' triples and those of the nodes they point to),
  `perNode` (most triples read per node, default 200) and `file` (default
  `data/sample-<dataset>.nt`, replaced if there; not `.gz`). It reads every graph of the dataset and
  follows outgoing triples only, takes a turn on the dataset like a query and stops at its
  timeout (`503`). It answers how many instances, nodes and triples it took, and
  `"truncated": true` when it stopped at the workspace's triple cap.
- `POST /$/workspaces/{id}/check` with `{"path": …, "text": …}` checks a file as the server
  would read it, without writing it (#409): RDF syntax, shapes and rules that compile (in
  `shapes/`, `rules/`, or a test's `shapes.ttl` or `rules.ttl`), a query (`.rq`) or update
  (`.ru`) that parses, YAML and JSON. It answers `{path, valid, errors}`; a path a workspace
  can't hold is `400`. Other files are text and pass.
- `/$/workspaces/{id}/sparql`, `/query`, `/explain`: the SPARQL Protocol on the scratch store.
  It holds what the files hold, so updates answer `405`: change the files instead (an
  overlay's store takes them, see below).
- `POST /$/workspaces/{id}/overlay` with `dataset` makes the workspace an **overlay** of a
  dataset (#406): its scratch store becomes the dataset's quads as they are now, read only,
  with the files of `data/`, `shapes/` and `rules/` on top, and its own changes kept apart.
  Updates (to `…/update`, or to `…/sparql` in any form, from callers who may change the
  workspace), validation,
  inference and the pipeline (whose first step starts from the dataset) work on the whole
  dataset; the dataset is never written, and its writers are not held up. Nothing is copied:
  the overlay shares the dataset's sorted segments and keeps its changes in runs beside them
  (its memory grows with its changes, not with the dataset), and a dataset kept on disk shares
  its memory-mapped files. The dataset must be in a TripleTrail store (`400` otherwise) that the
  caller may read. Taking a dataset again drops the changes.
  - `GET …/overlay`: the dataset, when it was taken (`at`), its `quads`, and about how many
    quads changed (`changes`, an upper bound; `null` until the store is first used).
  - `GET …/overlay/patch?format=rdf-patch|added|removed`: the changes, as an RDF Patch
    (`TX`, `D` rows, `A` rows, `TC`; the default) or the `added` or `removed` quads as N-Quads,
    for an explicit, reviewed apply to the dataset. The graphs of `shapes/` and `rules/` are
    left out; quads of `data/` files are changes like any other.
  - `DELETE …/overlay/changes` drops the changes (back to the dataset as taken);
    `DELETE …/overlay` makes it a plain workspace again.
  - Up to `--workspace-max-overlay-changes` (10,000,000) changed quads: past it, updates
    answer `413` until the changes are dropped; queries go on. A pipeline's store may hold the
    dataset's quads plus as many, and keeps snapshots of steps up to the dataset's quads plus
    `--workspace-max-triples`.
  - The changes are in memory only: after a restart the overlay answers `409` until the
    dataset is taken again or the overlay dropped. A changed file of `shapes/` or `rules/` is
    loaded again in place, keeping the changes; a changed file of `data/` starts the overlay
    over from the dataset as taken. Pipeline exports reach the store once the changes are
    dropped. An idle overlay keeps its store (the plain ones drop it after 15 minutes), and
    it keeps the dataset's segments it reads, also those a later write of the dataset
    replaced, for as long as the overlay exists: a saved overlay holds them until it is
    deleted or detached. The patch is written while it is sent.
- `POST /$/workspaces/{id}/pull` with `url`, `ref` (default `HEAD`) and `path` (a folder of the
  repository, default its root), each the last pull's when left out: fetches the ref (shallow,
  as a dataset's shapes are pulled) and writes the files under the path that a workspace holds
  over the workspace's (the repository is the master; files only in the workspace stay). The
  caps are checked and every RDF file parsed before anything is written. It answers the
  commit and the files it changed; `GET …/{id}` shows the last pull under `git`. Only an admin
  may pull (`403` otherwise), as the server fetches the URL; `502` when the fetch fails. The
  fetched repository is kept in the workspace's folder, beside its files and outside its
  byte cap.
- `GET /$/workspaces/{id}/history` (`?since=N`: only the runs after run N): what clients other
  than the workbench ran against the workspace (queries and updates, also on a pipeline step's
  store, validations, inferences, tests, pipeline runs and completed steps), newest first, with
  `last`, the newest run's number. Each run has its `op`, `client` (the `X-TripleTrail-Client`
  header, which the Python client sends as `python`, or the `User-Agent` product, such as
  `curl`), `text`, `params`, `status`, `ms` and a `summary`. A validation's or inference's JSON
  answer (up to 8 MB) is kept: `GET …/history/{N}` gives it under `result`. `DELETE …/history`
  clears it. The history keeps the last 100 runs, in memory only; the workbench's own requests
  (`X-TripleTrail-Client: workbench` or `ui`) are not in it.

A scratch workspace unused for `--workspace-ttl-hours` (24 by default) is deleted; a saved one
is kept until deleted. Workspaces are kept in `--workspace-dir` (default
`<location>/.workspaces`, or a temporary folder without `--location`), so they outlive a
restart; the scratch store is built again from the files when next used, and dropped after
15 minutes unused. Caps: `--workspace-max` workspaces (100) on the server,
`--workspace-max-per-caller` (10) per caller, `--workspace-max-triples` (1,000,000) in one
workspace's files, `--workspace-max-mb` (256) of files in one workspace, files of at most
`--max-body` bytes (also once a `.gz` is unpacked), 1,000 files; over a cap is `413` or
`429`. `--no-workspaces` turns them off. Without `--workspace-dir` or `--location`, the
temporary folder is made private to the server's user and removed when the server stops.

With access control, a workspace belongs to the agent that made it: other callers do not see
it (`404`), admins do. Anonymous callers cannot make workspaces, and a sample only reads a
dataset the caller may read. Without it, changes need the update or admin token when the
server has one; reading and querying do not.

#### Validation and inference

`POST` (or `GET`) `/$/workspaces/{id}/validate` and `…/infer` run SHACL and SHACL-AF on the
scratch store (#401):

- `…/infer` runs the rules of `rules/` (or of `shapes/` when `rules/` has none, or with
  `shapes=shapes`) on the data. JSON: `count`, `triples` (the first 10,000, `limit=` for more,
  `truncated` when some are left out; subject, predicate, object as in SPARQL JSON results),
  `runs` (what each rule did in each pass: focus nodes, constructed,
  new, ms). `max_iterations=N` caps the passes, `single_pass=true` runs one,
  `include_existing=true` also gives the constructed triples already in the data.
  `provenance=true` adds, for each triple in the same order, where it came from, and `rules`
  (each rule's `type`, `construct` or `subject`/`predicate`/`object`, `order`):

  ```json
  {"kind": "rule", "rule": "<http://example.com/labelRule>", "shape": {…}, "focus": {…},
   "group": 1, "pass": 1, "bindings": {"this": {…}, "n": {…}}}
  ```

  `bindings` is the solution of a `sh:SPARQLRule`'s pattern that made the triple (empty for a
  triple rule). With provenance, SPARQL rules run as SELECT queries whose solutions fill in
  their templates, with the same shortcuts as without it (later passes only on what the
  last pass inferred, and, for `tripletrail infer`, the queries on the store itself): the same
  triples, in about the same time. `tripletrail infer --provenance FILE` writes the same, one
  JSON line per triple.

  `plan=true` adds `plan`, which says why the rules run in the order they do. The rules are split into groups that
  run one after the other. A group repeats until nothing new is inferred when one of its rules
  reads what a rule of the same group writes (`recursive`); otherwise it runs once. A rule
  runs in a later group than every rule whose output it reads, whatever their `sh:order`,
  and a rule that reads a predicate only under a negation (`FILTER NOT EXISTS`, `MINUS`)
  runs after every rule that writes it, as rules in the same group would see a half-written
  graph. Only `sh:order` orders the rules within a group.

  ```json
  {"rules": [{"rule": "<http://example.com/step>", "shape": {…}, "order": 2,
              "reads": ["http://example.com/next", "http://example.com/reach"],
              "writes": ["http://example.com/reach"]}, …],
   "groups": [{"group": 1, "rules": [1], "recursive": false}, …],
   "edges": [{"from": 1, "to": 2, "through": ["http://example.com/reach"], "positive": false}, …]}
  ```

  `rules` are in the order of `runs`, and `groups` and `edges` name them by their index.
  An edge says that rule `to` reads what rule `from` writes, `through` those predicates
  (the `rdf:type` IRI and the class, space-separated, for a class); `positive` is false
  when `to` reads them only under a negation. `reads` or `writes` is `null` when TripleTrail
  cannot tell what a rule reads or writes (a variable predicate, say): such a rule is taken
  to read (write) everything, and has no edges of its own. At most 10,000 edges are listed
  (`truncated` when more are left out): rules that share predicates make an edge for each
  pair.
- `…/reason?regime=rdfs|owl-rl` (#407) gives what RDFS (Jena's default rules) or OWL 2 RL
  entails from the workspace's data, its shapes and rules left out, as the pipeline's
  `reason` step and `tripletrail entail` do. JSON: `count`, `predicates` and `classes` (how
  many entailed triples each has, most first), `triples` (the first 10,000, `limit=` for
  more, `truncated`), `ms`, and under OWL 2 RL `inconsistencies`: `count`, `atLeast` and the
  `first` 1,000 (`rule`, such as `cax-dw`, and the `terms` it found contradicting each
  other). RDF by `Accept` (or `format=turtle`): the entailed triples.
  `…/reason/why?regime=…&subject=…&predicate=…&object=…` (N-Triples terms) gives the
  derivation of a triple, down to asserted triples:

  ```json
  {"regime": "rdfs", "entailed": true, "derivation": {"subject": {…}, "predicate": {…}, "object": {…},
    "rule": "rdfs9", "premises": [{…, "rule": null, "premises": []}, …]}}
  ```

  `rule` is Jena's name for an RDFS rule (`rdfs9`) or the OWL 2 RL name (`cax-sco`, also
  for the RDFS rules under OWL 2 RL), `null` for an asserted triple. The entailment keeps
  no trace of which triples made which, so the derivation is searched for afterwards, from
  the triple back to the data, and the shallowest one found is given. `derivation` is
  `null` when the triple is not entailed (`entailed` is then `false`) or no derivation is found (a few corner cases of
  OWL 2 RL, such as triples about a schema property used as data), and also when the search
  goes beyond its bounds: 20,000 triples, and the first 256 entries of each index lookup.
  The entailment keeps to the server's timeout (`timeout=` for less), as `…/reason` does, and
  the search too, or to 10 s when the server has none; either answers `503` past it.
- `…/validate` checks the data, plus what the rules of `rules/` infer (`rules=false` for the
  data alone), against `shapes/`. JSON: `conforms`, `resultCount`, `results` (the first
  10,000, `limit=` for more: `focusNode`, `resultPath` as a SPARQL path, `value`,
  `sourceShape`, `shapeIri` (its stable IRI), `sourceConstraintComponent`, `severity`,
  `messages`, `key` (the [finding key](validation-and-reasoning.md#finding-keys), 32
  hexadecimal digits), and for a failure of `sh:node`, `sh:and`, `sh:or`, `sh:xone` or `sh:qualifiedValueShape` its `details`: the
  value node's results against the nested shapes, as `sh:detail` gives them), `shapes` (focus and
  value nodes each shape was checked against), `summary` (the run's
  [summary and cell index](validation-and-reasoning.md#run-summary-and-cell-index):
  `results`, `failingFocusNodes`, `bySeverity`, and `cells` with `shapeIri`, `severity`,
  `component`, `results`, `failingFocusNodes` and `samples`) and `coverage`: `untargetedClasses`, the
  classes with instances that no shape targets, directly or through a superclass, and
  `emptyShapes`, the node shapes that matched no focus node.
- Both answer RDF by `Accept` (or `format=turtle`…): the W3C validation report with shape
  statistics, or the inferred triples (all of them). Shapes that do not compile answer `422`
  with the reason; a run longer than the timeout (`timeout=` seconds, within the server's
  limits) answers `503`. On a workspace, one timeout covers the rules and the validation after
  them, and the parameters may also come as a form body.

The same requests on a dataset, reading only: `/$/datasets/{name}/validate?shapes=<graph>` and
`…/infer?shapes=<graph>`, with `graph=<iri>` for a data graph other than the default one, all
parameters in the URL. They take a turn on the dataset like a query (its concurrency limit and
queue, then `429`), run under its query timeout, count in its statistics and never write to it;
provenance is for workspaces.

#### Tests

A workspace's `tests/` holds regression tests for its shapes, rules and queries (#403). A test
is a folder with an input, one target and the expected output, found by its file names:

| | Files |
|---|---|
| input | `input.ttl`, `.nt`, `.trig` or `.nq` (none: an empty store) |
| target | `query.rq`; `update.ru`, or `updates/*.ru` run in name order; `rules.ttl` (SHACL-AF rules); `shapes.ttl` |
| expected | `expected.ttl`, `.nt`, `.trig` or `.nq`: a graph, compared up to blank node renaming; `expected.srj`, `.srx`, `.tsv` or `.csv`: a SELECT's rows (or an ASK's boolean), in any order; for shapes, `violations.yaml` or none (the data must conform) |

What is compared: the triples a CONSTRUCT or DESCRIBE gives, the store (every graph) after the
updates, the triples the rules infer (not those already in the input), or the validation
results. `expected.csv` compares the cells as text, as CSV has no types; the other result
formats compare terms. An optional `test.yaml` names other files or folders, relative to the
test (inside the workspace, on the server), and options:

```yaml
input: input                # a file or a folder of RDF files; also a list
rules: ../../rules          # the workspace's own rules
shapes: ../../shapes        # with rules too, the rules run first and the shapes see what they infer
query: ../../queries/tracks.rq
expected: tracks.srj
ordered: true               # rows must come in this order
output: all                 # rules: the data plus what they infer, instead of only the inferred
max_iterations: 1           # rules: passes at most
prefixes: {ex: "http://example.com/"}
violations:                 # or a count: `violations: 3`
  - focus: ex:track1        # every field is optional; prefixes of the input, shapes and rules count
    shape: ex:TrackShape    # a blank property shape matches by its node shape
    component: MinCount     # sh:MinCountConstraintComponent, its local name, or without the suffix
    path: ex:length
    severity: Warning       # sh:Violation, sh:Warning or sh:Info, or the local name
```

The listed violations must each match a different result, and every result must be listed; a
count only checks the number. `tripletrail test [DIR]` runs the tests of a workspace folder, of a
`tests/` folder, or one test, prints each with what is missing (`-`) and unexpected (`+`), and
exits with status 1 when one fails: for CI. `--junit FILE` also writes a JUnit report, and
`--timeout` (300 s) bounds each test. `--record` writes what each test gives as its expected
output (`expected.nt` or `.nq`, `expected.srj`, `violations.yaml`, replacing the expected files
there are) instead of checking it: to start a test or to accept a change.

On the server, `GET /$/workspaces/{id}/tests` lists the tests and `POST` runs them (all, or
`?test=NAME`, or `{"tests": [...]}`), answering for each `name`, `kind`, `passed`, `error`,
`missing` and `unexpected` (the first 50 of each), their counts and `ms`. Each test runs
under the server's query timeout (`timeout=` seconds; 300 s when the server sets none), stops
past `maxRows` rows, triples or validation results (100,000 when the server sets none), and may
load at most `--workspace-max-mb` of files, unpacked. `POST …/tests?record=true&test=NAME`
records, which needs the token a change of the files needs; what it writes must fit in the room
the workspace has left, counted again for each test.

#### Pipelines

A pipeline is an ETL as ordered steps in a `pipeline.yaml` (#404). Each step is a map with one
kind key naming its files (a file, a folder, or a list), relative to the `pipeline.yaml`:

| kind | runs |
|---|---|
| `lift` | Facade-X CONSTRUCT queries and RML / R2RML / YARRRML mappings, as `tripletrail lift`; their triples go into the step's graph. `base:` is the folder their relative file locations resolve against (the pipeline's folder by default), `jobs:` how many run at the same time. `for_each:` lifts once per file it names (a folder, or a pattern with `*` and `?` in its last part, such as `tms/*.xml`), in name order, with the file's path bound to the variable `as:` (`file`: `?_file` in the queries), its name to `?_file_name` and its name without extension to `?_file_stem`. `then:` lists SPARQL updates (`.ru` files or folders) run on what each lift gave, on its own in the default graph of a store of its own, before it goes into the step's graph: e.g. to drop the M ordinate of WKT literals |
| `load` | RDF files (`.gz` too) into the step's graph |
| `update` | a SPARQL update, or a folder of `.ru` files in name order |
| `infer` | SHACL-AF rules on `data:` (the default graph if omitted); what they infer goes into the step's graph (`data` if omitted). `max_passes:` (64), `all_constructed: true` to also write the constructed triples that were already there. `ordered: true` runs each rule once, in `sh:order` and then by rule IRI, each seeing what the rules before it inferred: one pass, no fixpoint, as a list of `INSERT`s run in that order (rail KG's rule stages) |
| `reason` | `reason: rdfs` (or `owl-rl`, which adds owl:equivalentClass/Property, inverseOf, SymmetricProperty, TransitiveProperty, the value restrictions, intersections, unions, property chains, owl:sameAs, (inverse) functional properties, a maximum cardinality of 1, owl:hasKey and owl:oneOf; see `tripletrail entail --regime`): what Jena's default RDFS rules derive from the other graphs (domain, range, subproperty and subclass, with their closures; see `tripletrail entail`), written to the step's graph (`urn:tripletrail:inferred` if omitted), replacing what it held. An `owl-rl` step reports the contradictions it finds, and fails on one with `fail_on_inconsistency: true` (see [Consistency](validation-and-reasoning.md#consistency)) |
| `validate` | a gate: validates `data:` against the shapes and stops the pipeline when results of `fail_on:` (`violation`, the default; `warning`, `info`, or `never`) or worse remain. `report:` writes the validation report |
| `export` | the step's graph, or a CONSTRUCT `query:` (the path of a `.rq` file, or the query itself: a value with a `{` is the query), to a file (format from its extension). Without either, the default graph; a `.nq` or `.trig` file without a graph gets every graph |
| `publish` | a new on-disk dataset folder where it says (serve it with `tripletrail serve --attach NAME=DIR`, or publish to `LOCATION/NAME` for `--location LOCATION`; a server already serving it keeps the old data until it is restarted): `graphs:` lists the graphs it gets (IRIs; one ending in `*` stands for every graph starting with the rest; `default` is the default graph; all of them if omitted), `merge: true` puts them all into its default graph instead of under their names. It is written beside the folder and swapped in when complete, replacing the dataset there; a folder that holds other files, and the store the pipeline runs on, are refused. `tripletrail pipeline run` only |
| `python` | `module:function`, called as `function(base_url, dataset, **kwargs)`, or a `.py` script run with `args:`, against the dataset served for the step (see below). `path:` lists the folders of the modules (the pipeline's folder by default), `kwargs:` the keyword arguments, `dataset:` the dataset's name (`ds`) |

Every step may also have `name` (its number and kind by default), `graph` (the graph it writes or
exports; an update names its own), `clear: true` (drop `graph` before the step, so running it
again replaces what it wrote), `variables:` (a map of name to value that this step binds over the
parameters, with `${name}` in a value being a parameter's value) and `timeout` (seconds). `parameters:` gives defaults for
parameters, which `-v name=value` overrides; they bind as for `tripletrail lift`: `?_name` is the
literal value, `?_name_iri` the IRI, `?_name_number` the number, in lift queries, updates and
export queries, and `${name}` in a python step's `args` and `kwargs`. `union_default_graph: true`
at the top makes the default graph of queries, updates and rules the union of all graphs while
the pipeline runs, as Fuseki's `unionDefaultGraph` (the shapes a step loads stay out of it).
`datasets: [metadata]` at the top declares more datasets, empty and in memory for the run, that
python steps reach on the same loopback server as `{base_url}/metadata/update` and so on: for
scripts that also write to another Fuseki dataset, such as an ETL's lineage in `/metadata`.
Only `tripletrail pipeline run` serves them; a Workbench python step reaches only its own store.

A python step serves the dataset on a loopback port for as long as it runs, as Fuseki does:
`{base_url}/{dataset}/sparql` (and `/query`) for queries, `/update` for updates (form or
`application/sparql-update`), `/data` for the Graph Store Protocol. So code written for Fuseki
with `requests` or SPARQLWrapper runs unchanged, given the base URL and dataset name it already
takes. The endpoint has no token: anyone on the machine may use it while the step runs. A
script gets the same in its environment: `TRIPLETRAIL_BASE_URL`, `TRIPLETRAIL_DATASET`,
`TRIPLETRAIL_ENDPOINT` (the two joined), `TRIPLETRAIL_GRAPH` (the step's graph, or empty) and
`TRIPLETRAIL_PARAMETERS` (JSON). It runs in the pipeline's folder; its output goes to standard
error, and the end of it into the step's `error` when it fails (a non-zero exit status or an
exception). `--python` (or `TRIPLETRAIL_PYTHON`) is the interpreter, `python3` by default. The
step's timeout stops the interpreter.

A server does not run python steps. In a workspace, a run stops at one with the status
`waiting` and serves the store the step works on (what the step before it left, with `graph`
dropped when `clear`) as a Fuseki dataset named by the step's number under
`/$/workspaces/{id}/pipeline`: `…/pipeline/{N}/sparql`, `/query`, `/update` and `/data`,
read-write for whoever may change the workspace (the stores after other steps are read-only;
past four times the workspace's triple limit only `DELETE …/data?graph=` is taken). The step's code runs from a notebook
against it, as `function(base_url, dataset)`; `POST …/pipeline/{N}/complete` then takes what it
left as the step's result, and the next run goes on from the step after it. In Python,
`ws.run_step(N, function)` does both, and `ws.step_endpoint(N)` gives `(base_url, dataset)`;
the Pipeline tab shows the code to run and a **Complete step** button.

```yaml
name: network-etl
parameters:
  date: 2026-01-01
steps:
  - lift: 01-lift/
    base: /data/tabular/
  - load: codes.ttl
    graph: http://example.com/codes
  - name: post-process
    update: 03-post-process/
    timeout: 3600
  - infer: rules/
  - name: track-geometry
    python: src2.trase:run_track_geometry
    path: dags/
    graph: https://example.com/_graph_track_geometry
    clear: true
  - name: derived-15
    infer: rules/derived/15/
    ordered: true
    all_constructed: true
    graph: https://example.com/_graph_derived_15
    clear: true
  - name: lift-tms
    lift: 01-lift/tms/
    for_each: /data/tabular/tms/*.xml
    then: 02-fix/drop-wkt-m.ru
    graph: https://example.com/_graph_lift_tms
    clear: true
    variables:
      system: TMS
  - validate: shapes/era/
    report: out/era-report.ttl
  - export: out/kg.nt
  - publish: /data/published/kg
    graphs: ["https://example.com/_graph_*"]
    merge: true
```

`tripletrail pipeline run pipeline.yaml --location DIR --dataset NAME` runs it on an on-disk
dataset (without `--location`, an empty dataset in memory: for a pipeline that exports what it
makes), from Airflow, CI or cron. `--from` and `--to` (a step's number or name) run part of it,
on what earlier runs left in the dataset. It prints each step as it goes on standard error and
writes a JSON report to `--report FILE` (standard output by default): per step its `status`
(`ok`, `failed`, `stopped` by its gate, or `skipped` after one of those), `seconds`,
`peakMemory` (bytes, Linux), `quadsBefore` and `quadsAfter`, its `files` with their times
(and triples, for lift), `triples` inferred or exported, and for validate the `results` by
severity, and for infer its `rules`: each rule run in order with its `pass`, `seconds`,
`focusNodes`, `constructed` and `new` triples, to find the slow ones. It stops at the first step that fails or whose gate stops it, and then exits with
status 1. `peakMemory` is the process's, so it is the step's only when nothing else runs in
it. An export or a validation report is written as `NAME.partial` and renamed when
done, so a failed step leaves the last good file in place. A lift step spools its triples to
an N-Triples file before loading them: in `--location` (in the system's temporary folder
without it), so it needs room for them there, about the size of what it lifts as N-Triples.

`tripletrail pipeline export pipeline.yaml --to airflow --location DIR` writes an Airflow DAG (2
or 3) whose tasks run the steps one by one with `tripletrail pipeline run --from N --to N` on the
dataset in `DIR` (`--one-task`: one task for all); the pipeline's parameters become the DAG's
params, given to each task with `-v`. `--to python` writes a script that runs the steps in
process with `tripletrail.Engine` (on `Engine(DIR)` with `--location`, else in memory): files are
read when it runs, except updates and queries that use parameters, which are bound when it is
made (`-v name=value` over the defaults) and written into it; lift steps run `tripletrail lift`,
and the script stops at a python step, which needs a served dataset, at a lift with `for_each` or
`then`, and at a publish of some graphs. `-o FILE` writes to a file.
`GET /$/workspaces/{id}/pipeline/export?to=airflow|python` (`one_task=true`, `location=`,
`dataset=`) gives the same for a workspace.

In a workspace, `GET /$/workspaces/{id}/pipeline` gives the steps of its `pipeline.yaml` with
what their last run did (`report`, as above; `diff`: `added`, `removed`, `predicates` and
`classes` with their counts, and samples; `changed` when the step's files changed since it ran;
`outdated` for it and the steps after it), or `error` when the file does not parse. `POST` with
`{"from": STEP, "to": STEP, "values": {...}}` (a step by number or name; `from` defaults to the
first step not run, failed or changed, `to` to the last) runs them and answers the same. While a
run goes on, `GET` answers at once with the steps as they were before it and `"running": true`,
a second `POST` answers `409`, and so do blame and the queries of the steps' stores. Each
step runs on a copy of the store the step before it left, which is kept (a snapshot) while it
holds at most `--workspace-max-triples` quads, so a step runs again from its input; without a
snapshot the run starts at the latest step before it that has one. `…/pipeline/{N}/query` (and
`/sparql`) queries the store after step N, read only. `GET …/pipeline/blame?subject=&predicate=&object=`
(N-Triples terms) answers which steps of the last run added or removed that triple, in any graph:
`events`, each with `step`, `name`, `kind`, `change` (`added` or `removed`) and `file` (the
step's only file, or for a load, update or lift step of several files the one that made the
change, found by running the step again file by file on its input; none for rules), and
`present`, whether it is there after the last step checked (`checkedSteps`: steps whose store is
not kept are not checked). A run needs the token a change of the
files needs, as exports and reports are written into the workspace. Each step runs under the
server's query timeout (300 s when it sets none); the store may hold at most four times
`--workspace-max-triples` quads after a step, files are read and written only inside the
workspace, what the steps write must fit in the room the workspace has left, and lift steps read
only the workspace's files. Snapshots are kept in memory until the server restarts.

### Backups

Backups are written to `--backup-dir` (`TRIPLETRAIL_BACKUP_DIR`), by default `<location>/backups`;
a server with neither has no backup directory, and the backup requests answer 400.

A backup is gzipped N-Quads by default: portable, readable by any RDF tool, and restored by
parsing it. `kind=store` writes a copy of the dataset's store instead: a checkpoint and an
empty log, the same directory `snapshot` writes. Writing it does not serialize anything, and
restoring it parses nothing: `POST /$/datasets?name=…&type=disk&from=<name>.store` copies its
files into the new dataset's directory and opens it (`type=mem` loads its quads). Writes to
the dataset wait while a store backup is written, and the backup holds what was committed when
it started.

`POST /$/backups/{file}/verify` checks a backup without restoring it. A store backup is
opened read-only, which checks every checkpoint file against its checksum; an N-Quads backup
is read through. The task fails on a damaged backup and otherwise reports its quads.

`--backup-keep N` (`TRIPLETRAIL_BACKUP_KEEP`) keeps the N newest backups of each dataset: when a
backup succeeds, older ones of the same dataset, of either kind, are deleted. Without it,
backups are kept until removed. Backups of a dataset are found by their names
(`<dataset>_<date>_<time>…`), so other files in the backup directory are never deleted.

#### Restoring to a point in time

Each commit in a dataset's write-ahead log carries its time, and a checkpoint retires the log
it folds in. `--wal-archive DIR` (`TRIPLETRAIL_WAL_ARCHIVE`) keeps those logs instead, in
`DIR/<dataset>/<timeline>` (a hard link when `DIR` is on the same file system, a copy
otherwise). The timeline is a random id each store directory gets when it is created, so a
dataset deleted and created again, or restored, under the same name never mixes its logs
with an earlier one's. A store
backup records where it starts in the chain of logs, so with the archive it is a base from
which any later moment can be restored:

```sh
curl -X POST -H "Authorization: Bearer $TOKEN" \
  "http://localhost:7878/$/datasets?name=kg-before&type=disk&from=kg_2026-11-10_080000.store&until=2026-11-10T09:15:00Z"
```

`until` is an RFC 3339 time, or `latest` for every write logged. The new dataset gets the
backup's checkpoint with the transactions committed up to that time replayed onto it: from the
archive, and then from the current log of the backed-up dataset when it is attached. The archive
and the backed-up dataset are only read. A time before the backup was taken is refused with
`409`, and so is a chain of logs with a gap (a log missing while a later one exists). A bulk load larger than 100,000 quads, or into an
empty store, is written to a checkpoint instead of the log, so it cannot be replayed: a restore
to a time after one stops with `409`; take a new store backup after such loads. With
`--backup-keep`, the archive is pruned with the backups: after a backup, the archived logs of
the dataset that none of its kept store backups needs are deleted (in each timeline, the logs
older than its oldest kept backup, and every log of a timeline with no kept backup). Without
`--backup-keep`, archived logs are kept until removed. A store backup moved out of the backup
directory is not counted: keep the logs it needs elsewhere too. With only N-Quads backups
kept, no backup can use the archive, so every backup empties the dataset's archive.

### Read replicas

A read replica serves queries on a copy of a dataset of another server, its leader, and keeps
the copy up to date by applying each transaction the leader commits, in the leader's order
(#217). It answers reads only: updates and uploads to it answer `405`.

The replica applies all the transactions of one read of the log in one commit, so it keeps up
with a busy leader: with 4,600 small inserts a second on the leader, a write is visible on
the replica within 5 ms (p95).

```sh
# The leader: an ordinary server with on-disk datasets and an admin token.
tripletrail serve --location /data --dataset kg --admin-token "$TOKEN"
# A replica of its dataset kg, served as kg too.
tripletrail serve --location /replica --follow kg=http://leader:7878/kg --follow-token "$TOKEN"
```

`--follow NAME=URL` (`TRIPLETRAIL_FOLLOW`, comma-separated) names the dataset the replica
serves and the leader's dataset URL; `--follow-token` (`TRIPLETRAIL_FOLLOW_TOKEN`) is the
leader's admin token, or an API key of its `tripletrail:admin` group with `--auth-config`. The
replica first copies a base backup of the dataset into `--location` (`NAME.r1`); the dataset
is served once it is in place, and `/$/ready` answers `503` (listing it under `copying`) until
then. While the leader cannot be reached or has no such dataset yet (both started together,
say), the replica retries, waiting longer each time up to 30 s, and serves its other datasets
meanwhile. It then reads the
leader's write-ahead log from where the base starts, waiting up to `--follow-wait` seconds
(10) for a new commit per request, and applies each transaction in a commit of its own. Where
it is in the leader's log is kept in `NAME.replica`, so a restart goes on from there. A
transaction applied again after a crash changes nothing.

`/$/status` and `GET /$/datasets/{name}` show a `replica` object for a followed dataset:
`state` (`copying`, `following` or `error`, with `error`), the leader's timeline,
`generation` and `offset` in its log, `behindBytes`, `lagSeconds` (0 when the replica has
everything the leader has on disk, otherwise the seconds since it last had), `applied`
transactions, `bases` copied and `lastCommit` (the leader's time of the last transaction
applied, in milliseconds since 1970). Both lag figures are as of the replica's last read
of the leader's log, so for up to `--follow-wait` after a leader commit they can still read
0.

The replica copies a new base, serving the old copy until the new one is in place, when:

- the leader's dataset is another one: deleted and created again, or restored (`409`);
- the log it needs is gone (`410`). A checkpoint retires the log it folds in, every 256 MB
  of log and when a dataset is detached or the server stops. `--keep-logs N`
  (`TRIPLETRAIL_KEEP_LOGS`, default 1) on the leader keeps the N newest retired logs in each
  dataset's directory, so a replica that was behind reads them to their end; logs in the
  `--wal-archive` are read too;
- the leader bulk loaded data: a load of more than 100,000 quads, or into an empty store,
  goes into a checkpoint, not the log.

The leader serves replicas through three admin endpoints (`GET`, admin token or
`tripletrail:admin` only): `/$/replication/{name}` gives the dataset's timeline, the generation
of its log and its length on disk; `/$/replication/{name}/log?timeline=…&generation=…&offset=…`
gives the log's bytes from `offset` (only what is synced, `X-TripleTrail-Log-End`,
`X-TripleTrail-Log-Retired` and `X-TripleTrail-Store-Format` headers; `max=` bytes, `wait=` milliseconds up to 30 s); and
`/$/replication/{name}/base` sends a store backup as a tar archive (written next to the
backups first, deleted once sent; writes to the dataset wait while it is written, as for a
store backup).

To read your own writes from a replica, take the `TripleTrail-Version` header of the leader's
answer to a write (`timeline:generation:length`: where its log stands once the write is on
disk) and send it back as `TripleTrail-Min-Version` with the read. The replica holds the request
until it has applied the leader's log that far, for up to 10 s, then answers `503`. The wait
counts under the dataset's `maxConcurrent`, and at most half of the server's `--threads` wait
at once (over all its replicas): past that, and for a version of another timeline (the leader's dataset was
replaced), the replica answers `503` at once. Datasets that are not replicas ignore the
header.

**Synchronous replicas.** With `--sync-replicas N` (`TRIPLETRAIL_SYNC_REPLICAS`, default 0) on the
leader, a write to an on-disk dataset answers only once N replicas hold it, so losing the leader
loses no write it answered. A replica holds a write once it has applied it and saved its
position: it then asks for the log from past it, which is how the leader knows. Each replica
names itself in its reads of the log (`follower=…`). The answer carries `TripleTrail-Replicas`: how
many hold the write. If fewer than N do within `--sync-timeout` seconds (`TRIPLETRAIL_SYNC_TIMEOUT`,
default 10), the write answers anyway (it is on the leader's disk), and the dataset's writes stop
waiting: each answers at once, its `TripleTrail-Replicas` saying how many replicas hold it (often
0), so a replica that is down costs one timeout, not one per write. Once N replicas hold the last
write that answered that way, writes wait for them again. The leader logs both changes
(availability over safety while a replica is down). A write waits for its replicas on a thread
of its own, not on one of the `--threads` workers, so writers waiting never hold up the replicas'
reads of the log. On one machine, a single-triple write with one synchronous replica took
1.9 ms (p50) instead of 0.7 ms.

`GET /$/replication/{name}` (admin) shows the leader's view under `sync`: `syncReplicas` (N),
`waiting` (false while writes answer without waiting), and the `followers` it has heard from in
the last 10 minutes, each with its `timeline`, `generation`, `offset` and `secondsAgo`.

A replica's read of the log that waits for a commit is woken by the next write the leader
answers, so an idle replica sees a write within a few milliseconds.

**Promotion.** `POST /$/replication/{name}/promote` (admin) on a replica makes it the leader of
the dataset: it stops reading its leader's log, takes writes from then on (from the server's
`--update-token` callers, as for any read-write dataset), and serves its own log to replicas.
Its copy has had a timeline of its own since it was opened, so the old leader's versions do not
apply to it. The copy's `HISTORY` file notes the promotion: the old leader's timeline
(`parent`), the position on it the copy held (`generation`, `offset`), and where the copy's own
log stood (`fromGeneration`, `fromLength`); a base taken of it carries the file on. The answer
gives the new position and that entry (`promotion`). The replica's position file says it was
promoted, so a start serves it as a leader, with or without its `--follow` (each `NAME.replica`
under `--location` marked promoted); no default `ds` is created then, and a `--dataset` of the
same name is refused. Promoting a dataset that is no replica answers `409`. If writing the
promotion down fails (`500`), the replica goes on following. Promote the replica that is
furthest along (`generation` and `offset` in its status): with `--sync-replicas 1` and two
replicas, only one may hold the last answered write. The old leader must not take writes
again: stop it before promoting, and start it as a replica of the new leader with a fresh
`--location` (the same goes for turning a promoted server back into a replica). Nothing
fences it yet (#217 step C).

A replica may be given several URLs: `--follow rail=http://leader:7878/rail,http://standby:7878/rail`
(a bare URL after `NAME=URL` adds to it, as does the same NAME again). It reads from the first;
when that one fails, or answers `409` (another timeline), it asks each URL for
`/$/replication/{name}`, which also gives `role` (`leader`, or `follower` for a replica) and
`history`, and goes on with a leader on its own timeline, or with one whose history has a
promotion (or a chain of them) from its timeline at its very position: it then reads that
leader's log from where the promotion started it, without a new base. A replica behind or ahead
of the replica promoted copies a new base from the leader promoted from its timeline. A replica
gives up on a read 5 s after its `--follow-wait`, so a leader whose machine is gone (no
connection refused) costs that long. With several URLs, a base is copied only from one whose
`role` is `leader`.

`GET /$/role/{name}`, open like `/$/ready`, is for load balancers: `200 leader` when this
server takes writes to the dataset, `503 follower` for a replica, `503 read-only` for a dataset
served read-only, `404` for none.

A replica does not keep RDFS or OWL 2 RL entailment of its own: the leader's entailed
triples come with its transactions.

## Running it in production

Without tokens or `--auth-config`, `tripletrail serve` has no authentication on its SPARQL and
Graph Store endpoints: anyone who can reach the port can query, update and delete. It binds to
`127.0.0.1` by default (`--bind 0.0.0.0:7878` to listen on every interface; the Docker image
does).

- To serve other machines, put it behind a reverse proxy that authenticates (and terminates
  TLS), bind it to a private network only, or turn on [HTTPS](#https) and
  [authentication](#authentication) in the server itself.
- Set `--update-token` (and `--admin-token` for `/$/`) whenever the port is reachable by
  anyone who should only query. A browser sends a form `POST` to `/{ds}/update` from any web
  page without asking first, so without a token any page a user opens can change the data.
- Prefer `--cors https://your.app` over `--cors` alone, which answers every origin.
- Set `--timeout` so a single query cannot hold a worker for long; queries, updates, routing
  functions and long geo functions stop at the deadline with a 503.

## Authentication

`--auth-config FILE` (`TRIPLETRAIL_AUTH_CONFIG`) says who may call, with the identity model and
the configuration of [sparql-guard](https://matdata-eu.github.io/sparql-guard/guide/authentication):
a sparql-guard configuration file works as is (its `auth` object is read, the rest ignored), and
so does a file holding only that object. `${NAME}` in a string is replaced by the environment
variable; an unset one stops the server from starting.

```json
{
  "auth": {
    "apiKeys": [
      { "sha256": "9cca4fb4…", "agent": "https://example.org/people/jan", "groups": ["tripletrail:writer"] },
      { "sha256": "51f2c0e8…", "agent": "urn:example:rinf-portal", "datasets": ["rinf"], "expires": "2027-06-30" }
    ],
    "users": [
      { "name": "jan", "password": "$pbkdf2-sha256$600000$…", "groups": ["tripletrail:writer"] }
    ],
    "trustedHeaders": {
      "agent": "x-forwarded-user", "groups": "x-forwarded-groups",
      "agentPrefix": "https://example.org/people/", "trustedProxies": ["10.0.0.0/8"]
    },
    "anonymous": false
  }
}
```

The methods are tried in order, as in sparql-guard:

1. **Single sign-on tokens** (TripleTrail Enterprise): with `oidc` configured, a
   `Authorization: Bearer` value that is a JWT is checked as a token of an OIDC identity
   provider, as described in [single sign-on](#single-sign-on-oidc). Other bearer values
   go on to the API keys.
1. **API keys**, sent as `Authorization: Bearer <key>` or `X-API-Key: <key>`. Only the SHA-256
   hash is stored: `tripletrail hash-key` prints it (it reads the key from standard input when not
   given). Several keys may share an agent, for rotation. A key that matches nothing, or has
   expired, is a 401, never a fall-through to anonymous. After 10 unknown keys in a minute, an
   address gets 429 for the rest of that minute. Behind a proxy listed in `trustedProxies`,
   the address counted is the last one in `X-Forwarded-For`, so one bad client does not lock
   out everyone behind the same proxy. TripleTrail adds `datasets` (the datasets the key
   may use: others are 403, also through their Linked Data IRIs, and the key gets no
   `/$/` admin or statistics pages), `expires` (a date or date-time) and `label`.
2. **Users with passwords**, sent with HTTP Basic (`Authorization: Basic`, as Fuseki's Shiro
   users sign in; a TripleTrail addition to sparql-guard). The file holds a hash of each
   password: `$pbkdf2-sha256$ROUNDS$SALT$HASH` (PBKDF2-HMAC-SHA256, passlib's form), which
   `tripletrail hash-password` prints with 600,000 rounds, or Shiro's
   `$shiro1$SHA-256$ROUNDS$SALT$HASH` (also SHA-512). A password in the clear stops the server
   from starting. `tripletrail import-shiro shiro.ini` prints a Fuseki `shiro.ini`'s `[users]` as
   `users` entries (roles become groups, `admin` becomes `tripletrail:admin`, passwords in the
   clear are hashed). A user has `name` (no `:`), `password`, `groups`, and optionally
   `agent` (by default `urn:sparql-guard:user:` and the name), `datasets` and `expires`, as
   keys have. A wrong password, an unknown user or an expired one is a 401, never a
   fall-through; guesses count towards the same 429 as unknown keys. A checked password is
   remembered for 15 minutes (by a keyed hash, in memory), so clients that send it with every
   request do not pay for the slow hash each time. With users, a 401 also carries
   `WWW-Authenticate: Basic`, so browsers and curl ask for them. Without users, a Basic
   header is ignored, as before. With users, it is checked against them first, before
   trusted headers (as an API key is): behind a proxy that forwards its own Basic header
   (nginx `auth_basic`, oauth2-proxy `--pass-basic-auth`) and sets the user headers, don't
   configure users, or have the proxy drop the header (`proxy_set_header Authorization ""`).
   Passwords travel with every
   request: serve over [HTTPS](#https); the server warns at start when users sign in over
   plain HTTP on an address other than loopback.
3. **Trusted headers**, set by an authenticating reverse proxy (oauth2-proxy, an ingress with
   external auth, …). A user that is not an IRI is URL-encoded and appended to `agentPrefix`
   (default `urn:sparql-guard:user:`); groups are comma-separated. The headers are believed
   only from the addresses or CIDR ranges in `trustedProxies` (a TripleTrail addition). Without
   it they are believed only from loopback (a proxy in the same pod or host), and then never
   grant the server's own groups `tripletrail:writer` and `tripletrail:admin`: list the proxy to let
   its groups write or administer. (sparql-guard believes the headers from anyone; TripleTrail is
   stricter because here groups grant writes.)
4. **Anonymous**, when `anonymous` is true: no agent and no groups.

Without policies, the groups decide what a caller may do: every identified caller (anonymous
included, when allowed) may read, the groups `tripletrail:writer` and `tripletrail:admin` may write,
and `tripletrail:admin` may change things under `/$/`. A request with no identity is 401 (with
`WWW-Authenticate: Bearer`, and `Basic` when there are users), an identity that is not enough is 403. `/$/ping`, `/$/role/{name}` and `/$/ready`
stay open for health checks. `--update-token` and `--admin-token` keep working as two built-in
keys (agent `urn:tripletrail:admin`, groups `tripletrail:writer` and `tripletrail:admin`).

### Single sign-on (OIDC)

TripleTrail Enterprise ([enterprise](enterprise.md#single-sign-on)) takes the access tokens of an
OpenID Connect identity provider (Keycloak, Microsoft Entra ID, Okta, Auth0, …), so callers
sign in with the organisation's accounts instead of keys. A client (a script, an ETL job, an
app behind the provider's sign-in) sends the token as `Authorization: Bearer <token>`. The
`oidc` object of `auth` lists the issuers trusted:

```json
{
  "auth": {
    "oidc": {
      "issuers": [
        {
          "issuer": "https://login.example.org/realms/rail",
          "audience": "tripletrail",
          "agent": "https://example.org/people/{preferred_username}",
          "groups": ["groups", "realm_access.roles"],
          "groupNames": { "rail-editors": "tripletrail:writer", "rail-admins": "tripletrail:admin" }
        },
        {
          "issuer": "https://login.microsoftonline.com/<tenant-id>/v2.0",
          "audience": ["api://tripletrail", "<application-id>"],
          "agent": "urn:example:entra:{oid}",
          "groups": "roles"
        }
      ]
    }
  }
}
```

| Key | Meaning |
|---|---|
| `issuer` | The provider's issuer, exactly as the tokens' `iss` claim has it. |
| `audience` | One or more audiences: a token is taken only when its `aud` names one of them, so tokens the provider made for other applications are refused. |
| `jwksUri` | Where the signing keys are. By default they are found through `{issuer}/.well-known/openid-configuration`. |
| `agent` | The caller's agent IRI, from claims: `{name}` is replaced by the claim's value, percent-encoded. Default `urn:sparql-guard:user:{sub}`. A token without a claim it names is refused. |
| `groups` | The claims that hold the caller's groups (a string or an array of strings), one or more; a dot reaches into an object (`realm_access.roles`). |
| `groupNames` | Renames groups. The server's own groups (`tripletrail:writer`, `tripletrail:admin`, …) are given only through it: a group named `tripletrail:…` in the token itself is ignored, so whoever can name groups at the provider cannot make themselves an admin here by accident. |
| `clockSkew` | Seconds of clock difference allowed for `exp`, `nbf` and `iat` (default 60). |

A token is taken when its issuer is listed, its signature checks out against the issuer's
keys (RS256/384/512, PS256/384/512, ES256/384 or EdDSA; `none` and the shared-secret `HS…`
algorithms are refused), it has not expired (`exp` is required) and is already valid, and its
audience is listed. Anything else is a 401 with `WWW-Authenticate: Bearer` and the reason,
never a fall-through to anonymous, and counts towards the same 429 as unknown keys. The keys
are fetched when first needed and kept for an hour; a token signed with a key not known yet
fetches them again, so the provider can rotate its keys. The provider is asked at most
once a minute per issuer, by one request while the others go on with the keys at hand, and
when it cannot be reached the keys fetched before stay in use, so signed-in callers are not
refused while it is down. The issuer
and key addresses must be `https` (or `http` on loopback, for tests).

The caller's agent and groups are then used as those of a key: policies name them with
`sg:agent` and `sg:group`. Tokens need a licence that covers `sso`: without one (or after its
grace period) a token is a 401 that says so, while keys and passwords keep working. In the
open build, a configuration with `oidc` stops the server from starting.

#### Signing in with a browser

An issuer with a `browser` object lets people sign in to the web UI: register TripleTrail at the
provider as a web application (Keycloak: a client with *Standard flow*; Entra ID: an app
registration with a *Web* redirect URI) with the redirect URI
`https://<your server>/$/sso/callback`, and give its client:

```json
"browser": {
  "name": "Example Rail",
  "clientId": "tripletrail-ui",
  "clientSecret": "${TRIPLETRAIL_OIDC_SECRET}",
  "redirectUri": "https://tripletrail.example.org/$/sso/callback",
  "scopes": "openid profile email",
  "sessionHours": 8
}
```

| Key | Meaning |
|---|---|
| `name` | The name on the sign-in button. |
| `clientId` | The client id at the provider; the ID tokens' audience. |
| `clientSecret` | The client secret of a confidential client (best from the environment, `${…}`). Leave it out for a public client: the code is always protected with PKCE. |
| `redirectUri` | This server's public URL, ending in `/$/sso/callback`, exactly as registered: `https` (`http` only on this machine, for tests). Its origin is the server's public origin (below). |
| `scopes` | The scopes asked for (default `openid profile email`); `openid` is needed. |
| `sessionHours` | How long a session lasts (default 8, at most 720). |

A browser that opens the web UI without credentials (and without `anonymous`) is sent to
`/$/sso/login`, then to the provider; the provider sends it back to `/$/sso/callback`, where
the server exchanges the code at the provider's token endpoint itself (with the PKCE verifier
and the client secret), checks the ID token (signature, issuer, audience `clientId`, expiry and
the nonce of this sign-in), and opens a session. The claims of the ID token give the agent and
groups, as a token's do: have the provider put the group claim in ID tokens. A sign-in's
`state` works once and for 10 minutes, and only in the browser that started it: `/$/sso/login`
gives that browser a cookie, `tripletrail_sso_flow` (`HttpOnly`, path `/$/sso`, 10 minutes), which
the callback needs back, so someone else's sign-in cannot be finished in your browser (login
CSRF). The address a sign-in returns to is always a path on this server. One address may have
20 sign-ins under way; past that its oldest is forgotten.

The browser holds only the session's id, in the cookie `tripletrail_session` (`HttpOnly`,
`SameSite=Lax`, `Secure` when the redirect URI is https); the server keeps a hash of it. A
session is used like a key: the web UI's requests carry it by themselves. A change with a
session (any request but GET and HEAD) must come from the server's own pages: its `Origin`
must be the redirect URI's origin (or, without `Origin`, `Sec-Fetch-Site: same-origin`),
otherwise it is a 403, so another site's page cannot make a signed-in browser change things.
A session that ended or is unknown (sessions live in memory: a restart signs everyone out;
changing other parts of the configuration does not) is a 401, and the web UI signs in again.
A token or key sent in `Authorization` wins over the cookie.

| Endpoint | |
|---|---|
| `GET /$/sso` | The sign-in choices (`signIn`: `name`, `issuer`, `login`) and who is signed in (`signedIn`: `agent`, `groups`, `groupClaims` (the claim each group came from), `issuer`, `ends`, or `null`). |
| `GET /$/sso/login?issuer=N&return=PATH` | Starts a sign-in with issuer `N` (by default the first with a `browser`), coming back to `PATH`. |
| `GET /$/sso/callback` | Where the provider sends the browser back. |
| `POST /$/sso/logout` | Ends the session (from the server's own pages) and clears the cookie: `204`. It does not sign out at the provider. |

These answer without credentials. Signing in needs a licence that covers `sso` (`403`
otherwise), and so does using a session. In the web UI, the key button shows who is signed
in, with **Sign out**, or the buttons to sign in.

#### Setting up Keycloak

This setup is tested against Keycloak 26. In the realm that holds your people (`rail` here):

1. **A client for scripts and apps.** Create a client, such as `tripletrail-cli`, with *Client
   authentication* on. A person's script uses *Direct access grants*, and a job with no person
   behind it uses *Service accounts roles*. Its tokens are what callers send as `Bearer`.
2. **The audience.** By default, Keycloak's access tokens name only `account` as their
   audience. In the client's dedicated scope, add a mapper *By configuration → Audience* with
   *Included Custom Audience* `tripletrail` and *Add to access token* on. Then set
   `"audience": "tripletrail"`. Tokens Keycloak makes for the realm's other clients do not
   name it, so they are refused (`The token is not for this server (audience)`).
3. **Groups.** Add a mapper *Group Membership* with *Token Claim Name* `groups` and *Full group
   path* off. Turn on *Add to ID token* too, for the browser sign-in. Realm roles are already in
   `realm_access.roles`, and the roles of a client are in
   `resource_access.<client>.roles`. Name the claims you use in `groups`, such as
   `["groups", "realm_access.roles"]`.
4. **The browser sign-in** (optional). Create a second client, such as `tripletrail-ui`, with
   *Client authentication* and *Standard flow* on. Its *Valid redirect URIs* is your
   `redirectUri`, and its client id and secret go in `browser`. Give it the group mapper of
   step 3 as well. Keycloak checks the PKCE code challenge if you set *Proof Key for Code
   Exchange Code Challenge Method* to `S256`, and TripleTrail always sends one.

The issuer is `https://<keycloak>/realms/<realm>`. The keys are found through its discovery
document, so `jwksUri` is not needed. For an agent IRI that survives renaming a person, use
`{sub}` rather than `{preferred_username}`: Keycloak's `sub` is the user's id, which does not
change.

```json
{
  "issuer": "https://login.example.org/realms/rail",
  "audience": "tripletrail",
  "agent": "https://example.org/people/{preferred_username}",
  "groups": ["groups", "realm_access.roles"],
  "groupNames": { "rail-editors": "tripletrail:writer", "rail-admins": "tripletrail:admin" },
  "browser": {
    "name": "Example Rail",
    "clientId": "tripletrail-ui",
    "clientSecret": "${TRIPLETRAIL_OIDC_SECRET}",
    "redirectUri": "https://tripletrail.example.org/$/sso/callback"
  }
}
```

#### Setting up Microsoft Entra ID

In the Entra admin center, under *App registrations*:

1. **Register TripleTrail.** Create an app registration, such as `TripleTrail`, for accounts
   in this organisational directory only. Under *Expose an API*, set the *Application ID URI*
   (`api://tripletrail`, or the default `api://<application-id>`) and add a scope, such as
   `access_as_user`. Scripts ask for that scope. A job with no person behind it uses the client
   credentials flow with the scope `api://tripletrail/.default`.
2. **Version 2 tokens.** In the app's *Manifest*, set `"requestedAccessTokenVersion": 2`
   (`accessTokenAcceptedVersion` in the older manifest). Otherwise Entra issues version 1 tokens
   for the API, whose issuer is `https://sts.windows.net/<tenant-id>/` and not the issuer
   below. In version 2 tokens, `aud` is the application id. List both forms in `audience`, as
   the example below does.
3. **Groups or roles.** Either:
   - define *App roles* (such as `Rail.Editor`) and assign people, groups or applications to
     them under *Enterprise applications*. They arrive in `roles`, by name. This is the
     simpler way, and the only one for applications.
   - or, under *Token configuration*, add a *groups claim* (security groups). Groups then
     arrive in `groups` as object ids (GUIDs), which `groupNames` renames. A person in more than
     200 groups gets no `groups` claim at all (Entra sends a link to Microsoft Graph instead,
     which TripleTrail does not follow), so assign the groups to the application and choose
     *Groups assigned to the application*, or use app roles.
4. **The browser sign-in** (optional). Under *Authentication*, add the *Web* platform with your
   `redirectUri`. Under *Certificates & secrets*, add a client secret for `clientSecret`. The
   `clientId` is the application id. App roles and the groups claim are in ID tokens too.

The issuer is `https://login.microsoftonline.com/<tenant-id>/v2.0`. Use `{oid}` for the agent:
`oid` is the person's (or application's) object id, the same in every application of the
tenant, while `sub` differs from one application to the next.

```json
{
  "issuer": "https://login.microsoftonline.com/<tenant-id>/v2.0",
  "audience": ["api://tripletrail", "<application-id>"],
  "agent": "urn:example:entra:{oid}",
  "groups": ["roles", "groups"],
  "groupNames": {
    "Rail.Editor": "tripletrail:writer",
    "5f0c6a1e-0000-0000-0000-000000000000": "rail-planning"
  },
  "browser": {
    "name": "Example Rail (Entra ID)",
    "clientId": "<application-id>",
    "clientSecret": "${TRIPLETRAIL_OIDC_SECRET}",
    "redirectUri": "https://tripletrail.example.org/$/sso/callback"
  }
}
```

When a token is refused, its reason says which check failed, and an admin can send it to the
explain endpoint, `GET /$/access/datasets/{ds}/explain` with the header
`X-TripleTrail-Explain-Token` ([dataset access](#dataset-access-with-policies)), to see the
agent and groups it gives and the claim behind each group.

### Dataset access with policies

Who may read and write each dataset is written as
[sparql-guard policies](https://matdata-eu.github.io/sparql-guard/reference/policy-language), in
Turtle or TriG files named in the same configuration file (paths relative to it):

```json
{
  "auth": { "anonymous": true, "apiKeys": [ … ] },
  "datasets": {
    "rinf": { "policies": ["rinf.ttl"] },
    "network": { "policies": ["network.ttl"], "admins": ["network-admins"] }
  }
}
```

```turtle
@prefix sg: <https://matdata.eu/ns/sparql-guard#> .
@prefix ex: <https://example.org/policies/> .

# rinf.ttl: RINF is open data
ex:RinfPublic a sg:Policy ; sg:agent sg:Anyone ; sg:readAllGraphs true .

# network.ttl: everyone signed in reads, the asset team writes
ex:NetworkReaders a sg:Policy ; sg:agent sg:AuthenticatedAgent ; sg:readAllGraphs true .
ex:NetworkWriters a sg:Policy ; sg:group "network-assets" ; sg:writeAllGraphs true .
```

- A policy applies to its `sg:agent`s (an IRI, `sg:Anyone` including anonymous callers, or
  `sg:AuthenticatedAgent`) and `sg:group`s. The grants of all policies that apply add up.
- `sg:readAllGraphs true` lets the caller query the dataset and read it over the Graph Store
  Protocol and Linked Data. `sg:writeAllGraphs true` (a write term proposed for the
  sparql-guard spec) also lets it update the dataset, over SPARQL Update and the Graph Store
  Protocol; writers read too.
- A top-level `policies` (as in a sparql-guard configuration) applies to every dataset without
  its own. Once any policies are configured, a dataset without policies is closed: only admins
  reach it. Policies named under an alias apply to the alias whatever it points to, so a swap
  does not change who may read.
- The groups `tripletrail:writer` (write everywhere) and `tripletrail:admin` (everything) still apply.
  `datasets.<name>.admins` lists agents or groups that may detach or back up that dataset;
  new datasets, swaps and snapshots stay with `tripletrail:admin`.
  `datasets.<name>.shapeEditors` lists agents or groups that may propose changes to the
  dataset's shapes as pull requests (TripleTrail Enterprise, see
  [proposed shape changes](#proposed-shape-changes-tripletrail-enterprise)) without being its
  admins; the forge's review decides who approves.
- `/$/datasets` and `/$/stats` list only the datasets the caller may read, and
  `/.well-known/void` describes only those. `/$/server`, `/$/metrics`, `/$/status`, `/$/tasks`
  and `/$/backups-list` name every dataset, so with policies they need `tripletrail:admin` or
  `tripletrail:monitor` (give the Prometheus scraper a key in that group).
- `sg:readGraph <g>` (repeatable) lets the caller read only those graphs of the dataset (#201);
  `<urn:x-arq:DefaultGraph>` names the store's default graph, as in Fuseki. Grants of several
  policies add up, and `sg:readAllGraphs` of any of them lifts the limit. A query then sees the
  union of the readable graphs as its default graph and the readable named graphs as its named
  graphs; `FROM`, `FROM NAMED` and `default-graph-uri` / `named-graph-uri` only narrow that.
  Other graphs look empty: `GRAPH ?g` does not list them, and the Graph Store Protocol answers
  404 for them (the default graph, unless granted, is empty). The limit is applied to the
  dataset the engine evaluates on, so property paths, `GRAPH ?g` and the result cache keep to it
  too. Such a caller gets the query endpoint, Graph Store `GET`, describe and Linked Data, and
  the service description, all keeping to those graphs (#203): describe and Linked Data
  describe a resource from the readable graphs only and answer 404 for one that is only in
  others (not 403, which would tell it exists), and their `ETag` differs between callers with
  other grants and is a hash of the description itself, not of the store's write generation,
  so a write to other graphs does not change it; such a caller's query answers carry no
  `X-Cache` header, which would also tell that a write happened; the service description lists only the readable named graphs, and gives no
  size for the default graph. Spatial filters and joins go through a view of the spatial index
  that holds only the geometries of the readable graphs: it is made on the first spatial
  query of a set of grants (a lookup per indexed geometry) and kept, for up to eight sets of
  grants, until the next write. Nearest-neighbour shortcuts, counts from statistics and the
  range index are left out for such callers. Explain evaluates the query on the readable
  graphs and answers its row count alone (`{"rows":N,"restricted":true}`): the plan and the
  spatial rewrite's counters are chosen from what the whole store holds, so they would tell
  whether other graphs hold a guessed triple. `/{ds}/void`
  gives only the subsets of the readable graphs, without the counts, classes, properties and
  vocabularies of the whole dataset. OGC API Features and the vector tiles list only the
  collections with a feature in the readable graphs, and read features, extents and tiles
  from those graphs only; tiles and extents are cached per set of grants. Their collections,
  property schemas (`/queryables`), filter parameters and `/api` come from shapes generated
  from the readable graphs only (`Engine::dataset_profile_in`), with the dataset's own shapes
  over them, so they name no class or property that is only in other graphs. That profile
  is made at the first such request after a write, and kept for up to 16 sets of grants.
  The RDF4J API answers such a caller its reads (queries, `statements`, `contexts`, `size`,
  `namespaces`) from the readable graphs, and refuses its writes and transactions (403). The
  dataset listings (`/$/datasets`, `/.well-known/void`, the RDF4J `repositories`) name the
  dataset, without its counts or its place on disk (read-only in RDF4J's, described from the readable graphs in
  VoID). A workspace sample of the dataset takes the readable graphs only; an overlay,
  which would share every graph, is refused (403). Of the services, the `st:` (PostGIS
  set-returning functions), window, path search and pgRouting (`pgr:`) services may be
  called: their sub-selects read the same readable graphs as the query, narrowed by its
  `FROM`; path search follows only the edges of those graphs (a `path:graph` outside them has
  no edges), and a routing network is built from them and cached apart for each set of
  graphs. `pgr:shapes`, `pgr:vehicle`, `pgr:compatibilityShapes` and `pgr:shaclRules`, which
  check shapes on the store itself, are refused (#203). An update may call the same services:
  their sub-selects read the graphs of its `WHERE` (the readable named graphs, or its `USING`
  narrowed to them), and the operations of one update that call one must read the same
  graphs. Under row rules (#202), queries and updates with a `SERVICE` are refused (400). Any other `SERVICE`, one called inside another, and `SERVICE` in an explain are
  refused (400), and the
  dataset's admin reads (profile, shapes, validate, infer, stats) still answer 404
  for it.
- `sg:writeGraph <g>` (a write term proposed for the sparql-guard spec) lets the caller write
  those graphs, and read them. An update is checked before it runs, and refused as a whole
  (403, nothing applied) if it may change another graph: `INSERT DATA`, `DELETE DATA`, the
  templates of `DELETE`/`INSERT` (with `WITH`), `LOAD`, `CREATE`, `CLEAR` and `DROP` must name
  writable graphs. A template with `GRAPH ?g`, and `CLEAR`/`DROP` of `NAMED` or `ALL`, are refused,
  since what they change is not known beforehand. The `WHERE` of an update by a caller that
  reads only some graphs reads those graphs only: `USING` and `WITH` are narrowed to them, and
  without either it reads the union of the readable named graphs, never the stored default
  graph. So for a caller that may write only the default graph, `DELETE WHERE { <s> ?p ?o }`
  matches nothing and deletes nothing (200); use `DELETE DATA` there. A `WHERE` that names a
  graph the caller may not read sees it empty, so `COPY <unreadable> TO <mine>` empties
  `<mine>`. `COPY DEFAULT TO <mine>` reads the caller's default graph (the union of the graphs it
  reads) as it was before the request, so `<mine>` ends up holding all of it, its own triples
  included. Over the Graph Store Protocol such a caller writes one writable graph, named with
  `?graph=` (or `?default` with `<urn:x-arq:DefaultGraph>`); a TriG or N-Quads body that names
  any other graph is refused (403, nothing applied). Other services do not take its writes.
- Row rules (#202, **TripleTrail Enterprise**, #818; see [enterprise](enterprise.md#row-level-policies)): `sg:visibleIf <shape>` hides the targets of the shape that do not conform
  to it, as subject and (unless `sg:hideReferences false`) as object; `sg:hideProperties
  <shape>` hides the predicates of the shape's `sh:property` paths on its targets, or
  everywhere when it has none; `sg:editableIf <shape>` (a proposed term, TripleTrail only for
  now) hides nothing but lets a writer change only the targets of the shape that conform to
  it: every resource an update, Graph Store or RDF4J write changes must conform before and
  after (so a writer cannot claim a resource, nor give one away), else 403 and nothing
  changes. `sg:requestingAgent` in `sh:hasValue`, `sh:in` or
  `sh:targetNode` stands for the caller (`urn:sparql-guard:anonymous` when anonymous). An
  RDF 1.2 triple term whose triple the rules hide is hidden too, with every triple that names
  it (`rdf:reifies`, annotations), and under rules that hide anything a nested triple term
  (one whose object is a triple term) is hidden whole, as in sparql-guard. The
  shapes are in the same policy files. Rules of every policy that applies are added up (deny
  wins); visibility is judged over the graphs the caller may read, not over a query's own
  dataset. A literal target (of `sh:targetObjectsOf`) is a value, not a resource, and is
  never hidden. Admins of the server or the dataset see everything.

  They are enforced inside the store: every quad scan of the caller's query drops what the
  rules hide, so joins, `OPTIONAL`, `MINUS`, `EXISTS`, aggregates and `*`/`+` paths (allowed
  whatever `sg:allowTransitivePaths` says) see only the visible rows. The hidden set is
  computed once per store version, rules and graphs (and agent, for shapes that use
  `sg:requestingAgent`), and shared. After a write it is brought up to date by judging again
  only the resources the write touched (2 to 4 ms instead of 440 ms for rules on 200,000
  resources), when every shape is local: its targets and checks read only the focus node's
  own quads and the quads along its property paths (single predicates or sequences of up to
  four, with `sh:class` on the values counting as one more step; no `sh:node` on values, no
  other kind of path, no `sh:qualifiedValueShape`, `sh:closed` or SPARQL constraints) and the
  write did not change `rdfs:subClassOf`. The nodes that reach a touched resource along those
  paths are judged again with it (enterprise #7: a write under 20 shapes, one of them on
  `ex:vendor/ex:country`, went from 6 to 7.5 s to 4 to 47 ms on 200,000 offers). Otherwise,
  after bulk loads or writes of more than 65,536 quads, and when more than 65,536 resources
  would be judged again, the rules are compiled anew. An "own records" shape, one that names
  `sg:requestingAgent` only as the `sh:hasValue` of its own property shapes (`sh:property [
  sh:path ex:owner ; sh:hasValue sg:requestingAgent ]`), is not judged again for every agent:
  its targets, and those that fail the rest of the shape, are worked out once per store version
  and shared, as a bitmap, by every agent's filter, which only lists the agent's own records
  (enterprise #7: with 1,000 agents on 200,000 offers, a first query as a new agent went from
  200 to 250 ms to 3 to 6 ms, and the server from 1.5 GB to 160 MB). Full-text search (`tt:search`, `text:query`, the map search) reads
  an index of the visible labels alone, ranked as on a store holding only them (#203; see
  [full-text search](full-text-search.md#access-control)). The built-in services (`st:`, the window service, path search and `pgr`)
  read the same view: their sub-selects, the edges path search follows and the networks routing
  builds hold only the visible rows, and the networks are cached apart per rules. `pgr:shapes`,
  `pgr:vehicle`, `pgr:compatibilityShapes` and `pgr:shaclRules`, which read the store
  directly, are refused, as are other services and a service called inside another (400). The
  query cache is not used. Compiling the rules stops with the query's timeout. Such a caller may
  also read RDF descriptions (`describe`, Linked Data; a hidden resource is not found, 404)
  and the Graph Store Protocol (`GET`; a named graph with nothing visible is not found), from
  one view of the store judged as a whole, and the RDF4J API's reads (queries, `statements`,
  `size`, `contexts`; its writes answer 403), and OGC API Features and vector tiles: the
  collections, queryables, extents, items and tiles come from the view, with the shapes
  generated from it, so a hidden class or property is no collection or queryable. Spatial
  filters and joins use a view of the spatial index holding only the geometries the view
  shows, kept per store version and rules. The HTML describe page and its view model read
  the same view, and `explain` gives the row count alone (`{"rows":N,"restricted":true}`), as
  for a caller who reads only some graphs. A caller with row rules who may write sends SPARQL
  updates (`INSERT DATA`, `DELETE DATA`, `DELETE`/`INSERT … WHERE`; other operations such as
  `CLEAR`, `LOAD` or `COPY` answer 403) under the same rules: the `WHERE` matches only the
  visible rows, deleting a quad the caller cannot see changes nothing, and adding a quad about
  a hidden resource, pointing to one, or with a hidden property is refused (403), as is an
  update that leaves a resource it touched hidden from the caller (so a new resource must be
  visible once written; only the resources an update changes are judged, so a small update
  stays fast on a large store). Adding a hidden quad that is there already is refused too, so the
  answer does not tell whether it exists; that a 403 names a hidden resource (where an unused
  IRI would be accepted) is known and accepted. A refused update changes nothing. Graph Store
  Protocol writes (`PUT`, `POST`, `DELETE`) work the same way: they name one graph (`?graph=` or
  `?default`, else 403), `PUT` and `DELETE` remove only the quads the caller sees of it (hidden
  ones stay), and a graph with nothing visible in it is absent (`DELETE` answers 404, a write to
  it 201). The RDF4J API's statement writes (`POST`/`PUT`/`DELETE` of `statements`, updates)
  and transactions do too (`PUT` removes only what the caller sees; reads in a transaction
  read the caller's view), for a caller who may write the whole dataset. Namespace changes,
  bulk loads, the service description and the admin reads answer 403 for now, and dataset
  listings show the dataset without counts or graphs. The Access page does not edit a policy with row rules (edit its file).

  `GET /$/access/datasets/{ds}/explain?agent=&groups=` (server admins) explains a caller's
  access (`agent` an IRI or `anonymous`, the default; `groups` separated by commas): whether
  it reads and writes the dataset, the graphs it may read or write when only some, the IRIs of
  the policies that apply and the row rules in force. With `resource=<iri>` it tells whether
  the caller sees that resource, which `sg:visibleIf` shapes hide it, which of its properties
  `sg:hideProperties` hides and which `sg:editableIf` shapes keep it from being changed (each
  rule judged on its own), whether it is the subject of any quad (`exists`) and of one in a
  graph the caller may read (`readable`); `editable` is about the graphs the caller may write
  when only some. With `query=` it lists the query's triple patterns with how many matches
  each has and how many the caller sees (`restricted` when fewer); patterns inside `FILTER
  EXISTS`/`NOT EXISTS` and property paths are not listed.
  In TripleTrail Enterprise, a [single sign-on](#single-sign-on-oidc) token sent in the header
  `X-TripleTrail-Explain-Token` (never in the URL, where it would end up in logs) takes the place
  of `agent` and `groups`: the answer is for the token's bearer, and `token` has its `issuer`
  and `groupClaims`, the claim each group came from (`{"group": "tripletrail:writer", "claim":
  "roles"}`). A token the server would refuse is a 422 with the reason.

  The shapes may only use the SHACL subset of the sparql-guard spec (targets, paths, value
  type, cardinality, value range, string, `sh:equals`, `sh:disjoint`, logical and shape-based
  constraints, `sh:in`, `sh:hasValue`, `sh:deactivated`, and descriptive terms); anything else
  (`sh:sparql`, `sh:closed`, `sh:lessThan`, …), a recursive shape, a `sg:hideProperties` path
  that is not a plain IRI, or a shape that is not in the files makes the file fail to load,
  rather than show more than it says. So do unknown `sg:` terms and policies without an agent
  or group.

The file and its policy files are checked for changes every second and reloaded without a restart; a file that no
longer parses keeps the previous configuration and the error goes to standard error. Without
`--auth-config` nothing changes: reads are open and the two tokens guard writes and admin.

### The access admin API

Server admins (`tripletrail:admin`) read and change who may do what under `/$/access` (#205),
the API the access pages use:

- `GET /$/access`: for each dataset, its policies (as JSON: `id`, `agents`, `groups`,
  `readAllGraphs`, `writeAllGraphs`, `readGraphs`, `writeGraphs`), the files they come from,
  whether it has policies of its own, whether they can be edited (Turtle files only) and whether
  it is closed (policies are on and none apply to it); the API keys (label, agent, groups,
  datasets, expiry, and the first 8 hex digits of the hash as an id, never the hash); the
  users (name, agent, groups, datasets, expiry; never the password hash); the
  groups in use, with how many keys and which policies name them; anonymous access and the
  trusted headers.
- `GET /$/access/datasets/{name}/graphs`: the named graphs, with their triple counts and a
  label (`dct:title` or `rdfs:label` of the graph, in any graph), for the graph access matrix.
- `PUT /$/access/datasets/{name}/policies` with a policy as JSON writes it into the dataset's
  Turtle policy file (the file that holds it, or the first one), and
  `DELETE /$/access/datasets/{name}/policies?id=IRI` removes it. Only the statements about that
  policy are rewritten: comments, other policies and the policy's own triples outside `sg:`
  (a label) stay. A dataset without policies of its own first gets `policies/{name}.ttl` next
  to the configuration file, starting as a copy of the default policies, and the configuration
  names it (the JSON file is written again, with its keys sorted). An edit that would not load
  puts every file back and answers 422. The change applies at once.
- `POST /$/access/keys` with `{"agent": IRI, "label", "groups", "datasets", "expires"}` (all
  but `agent` optional, as in `auth.apiKeys`) creates an API key: a random `mg_…` key that the
  answer (201, `{"id", "key"}`) shows once. Only its SHA-256 hash goes into the configuration's
  `apiKeys`. `DELETE /$/access/keys/{id}` revokes the key with that id; 409 for a built-in
  token (`--admin-token`, `--update-token`) or an id that is the start of several hashes. The
  overview shows when each key was last used since the server started (`lastUsed`).
- `PUT /$/access/anonymous` with `{"anonymous": true}` or `false` turns anonymous access on or
  off.

Every change made through `/$/access` is kept: the configuration file and the policy files
it names are saved as a version in `.access-history/` next to the configuration (with the
configuration's file permissions, as they hold the key hashes), with the time, the admin's
agent and what changed. The first change first keeps the files as they were found. The last
100 versions are kept. `GET /$/access/history` lists them, `GET /$/access/history/{n}` gives
one with the files of the version before it, and `POST /$/access/history/{n}/restore` puts
the files back as they were then. A restore keeps the current `apiKeys`, so it never brings
back a revoked key or takes away a new one; policy files that the access pages made after
that version are removed (they stay in the history). The restore is a new version, so it can
be undone too. Before every change and restore, files changed by hand (or by another tool)
since the last version are kept first, as a version "Changed outside the access pages", so a
restore never drops them without a trace. `GET /$/access/history/{n}` also gives the files as
they are now (`current`), so a page can show what a restore would change; key hashes in it
are cut to the 8 hex digits the overview shows.

With **TripleTrail Enterprise**, `"review": true` in the `auth` object (#207) puts four eyes on
these changes: a policy written or removed, anonymous access turned on or off, a restore, and
a key whose groups include `tripletrail:admin` are not made but proposed. The answer is `202`
with the proposal (`proposal` number, `status` `pending`, `by`, `time`, `description` and
the `change` itself); a policy that cannot be read is refused at once (400). `GET
/$/access/proposals` lists them, newest first (pending ones and the last 100 decided). `POST
/$/access/proposals/{n}/approve`, by another admin than the one who proposed it (403
otherwise, for a caller without an agent, and for the built-in `--admin-token`, a shared
secret that stands for nobody, which also cannot propose), makes the change then, onto the files as they
are, and answers what the change answers (a new key, shown once, to the approver) with the
proposal, now `approved` with `decidedBy` and `decided`; when the change no longer applies, it
fails as it would have and stays pending. `POST …/reject`, by any admin (the proposer can
withdraw), drops it. A decided proposal answers 409. The access history records the change as
the approver's, with "(proposal N by AGENT, approved)". Revoking a key and keys without
`tripletrail:admin` are not held back: neither lets one admin do more alone. The proposals are
kept in `.access-history/proposals.json`. A dataset that is neither attached nor configured
is refused when proposed (404). With `X-TripleTrail-View-As`, every change under `/$/access`
(made, proposed, approved or rejected) answers 403: changes are made under the admin's own
name, and the audit log names the admin who sent a view-as request, with `viewAs` (agent and
groups) for whom they viewed as. The Access page of the web UI lists the pending
proposals with Approve and Reject buttons. Like the audit log, review does not depend on the
licence, and the open build refuses a configuration with `review`. It covers changes made
through `/$/access`; whoever can edit the files on the server is not held back by it.

A server admin can run a request as someone else, to check what they see ("view as"): the
header `X-TripleTrail-View-As` names an agent IRI (or `anonymous`) and
`X-TripleTrail-View-As-Groups` that caller's groups, separated by commas. The request then has
exactly that caller's access, and none of the admin's own (so `/$/` admin pages answer 403).
A caller outside `tripletrail:admin` sending the header gets 403.

Once one dataset has policies, every dataset without any is closed to all but admins: give
each dataset its policies before opening one to the public.

The UI's **Access** page (`/$/ui/access`) is built on this API: an overview of the datasets
(with a warning for the closed ones), the API keys (create one, shown once, or revoke one) and the groups, an anonymous access switch, and per dataset a graph
access matrix. Its rows are the graphs (with "All graphs" and the default graph), its columns
the policies, one per group or agent (a new column makes a policy
`urn:tripletrail:access:{dataset}:{name}`), and each cell is none, read or write. "Grant many
graphs at once" sets a column for every graph whose IRI starts with a prefix; the grants are
written one `sg:readGraph` or `sg:writeGraph` per graph, so the file stays plain sparql-guard. "View
as someone else" opens the query page as an agent with groups (or anonymously), with a banner
saying whose view it is until you stop it; it lasts for the browser tab. The map, the
graph explorer and the top bar's search box show that caller's view too; describe pages, opened
as plain links, still show your own. The **History**
table lists every change, shows what it changed as a line diff, and restores an earlier
version.

## Operations

- **Readiness and health**: `/$/ready` for load balancers and Kubernetes readiness probes,
  `/$/ping` for liveness. The Docker image has a `HEALTHCHECK` (`tripletrail health`, no curl
  needed).
- **Shutdown**: on SIGTERM (or Ctrl-C) the server stops taking requests, `/$/ready` answers
  503, running requests get `--shutdown-grace` seconds (30 by default) to finish, and read-write
  datasets written to since their last checkpoint are checkpointed, so the next start replays
  no log (a dataset that was not written to keeps its checkpoint as it is). Read replicas
  (`--follow`) finish the read of the leader's log they are applying, then apply no other and
  copy no new base, so nothing writes into their folders once the server has stopped.
- **Damaged store files**: a store whose checkpoint is larger than 1 GiB opens without reading
  its files and checks them against their checksums in the background. The check pauses while
  the dataset is being written and reads at most about half as fast as the disk can, so it can
  take a while on a large store (on a billion triples, tens of minutes). `/$/ready`, the dataset
  description and `/$/stats` (`fileCheck`, `damagedFile`) show the check. If a file is damaged,
  the dataset still answers queries, but updates and uploads answer 503 and name the file.
  Restore the dataset from a backup.
- **Request log**: `--log-requests` (on in the Docker image) writes one JSON line per request
  to standard output: time, method, path, dataset, status, milliseconds and a hash of the query
  text (not the text itself).
- **Metrics**: `/$/metrics` in the Prometheus text format, for a scrape job:
  `tripletrail_requests_total` (by dataset, operation and status),
  `tripletrail_request_duration_seconds` (a histogram per dataset and operation, body included),
  `tripletrail_timeouts_total` (queries and updates stopped by their timeout),
  `tripletrail_budget_trips_total` (by dataset and `budget`: `memory` for a query stopped by
  `--query-memory`, `rows` for one refused past `max-rows`, `partial` for one cut there with
  `partial=true`; #189), `tripletrail_triples`, `tripletrail_graphs`, `tripletrail_mapped_bytes`,
  `tripletrail_spatial_index_ready`, the result cache's hits, misses and bytes,
  `tripletrail_requests_in_flight`, `tripletrail_uptime_seconds`, `process_resident_memory_bytes`
  (Linux) and `tripletrail_build_info`. For each writable on-disk tripletrail store, by dataset:
  `tripletrail_wal_bytes` and `tripletrail_wal_generation` (the write-ahead log being written, as
  far as it is synced; writable stores), `tripletrail_commits_total`,
  `tripletrail_commit_seconds_total` (until durable) and `tripletrail_slow_commits_total` (a
  second or more, each also logged), `tripletrail_checkpoints_total`,
  `tripletrail_checkpoint_seconds_total` and `tripletrail_last_checkpoint_seconds`, `tripletrail_runs`
  (sorted runs not merged yet) and `tripletrail_merge_running`, and `tripletrail_file_check` (1 on
  the `state` label of the check of its files: `checking`, `verified`, `damaged` or
  `stopped`). The counts start when the server opens the store. For [read
  replicas](#read-replicas): `tripletrail_replica_following` (0 while copying a base or failing),
  `tripletrail_replica_lag_seconds`, `tripletrail_replica_behind_bytes`,
  `tripletrail_replica_applied_total` and `tripletrail_replica_bases_total` on a replica, and on a
  leader `tripletrail_replication_followers` (replicas heard from in the last 10 minutes) and,
  with `--sync-replicas`, `tripletrail_sync_replicas_waiting` (0 while writes answer without
  waiting). It needs no token, like the other `GET`s under `/$/`.
  ```yaml
  scrape_configs:
    - job_name: tripletrail
      metrics_path: /$/metrics
      static_configs: [{ targets: ["tripletrail:7878"] }]
  ```
  A Grafana dashboard for these is in `deploy/grafana/tripletrail.json` (import it under
  Dashboards → New → Import and pick the Prometheus data source): uptime, quads and graphs,
  requests and errors per second by operation and status, query and update latency (p50,
  p95, p99), timeouts, requests running, queued and rejected by the limits file, the result
  cache's hit ratio, mapped and resident memory, the spatial index, commits (and slow ones),
  commit time, the write-ahead log, checkpoints, runs and merges, damaged files, replica lag
  and bytes behind, and a leader's replicas and sync state. A test checks that every metric and
  label it reads is one the server exports.
- **Slow query log**: `--slow-query-ms 2000` writes each query or update that took at least
  that long to standard error, as a JSON line with its full text, dataset, status,
  milliseconds and query hash (the hash of the request log, to match the two). Off by
  default, since query text can hold data.
- **Audit log** (TripleTrail Enterprise, #207): `--audit-log FILE` (`TRIPLETRAIL_AUDIT_LOG`)
  appends a JSON line for each request that changes something and for each refused attempt
  at one: SPARQL updates, Graph Store Protocol and RDF4J writes, loads, and every change under
  `/$/` (datasets, backups, access policies and keys, shapes, workspaces, sign-out).
  Queries and other reads of a dataset are logged only with `--audit-reads`
  (`TRIPLETRAIL_AUDIT_READS`), as `query` and `read` events (a read refused before its kind is
  known is a `read` too). A line has `seq`, `time`, `event` (`update`,
  `upload`, `admin`, or `change` for a change refused before its kind is known; `start` when
  the server starts), `method`, `path` (without the query string, which can hold an
  update's text; `graph` is kept), `dataset`, `status`, `outcome` (`done`, `refused` for
  401, 403 and 429, `failed`), `agent`, `groups` and `auth` (`apiKey`, `password`,
  `trustedHeaders`, `oidc`, `session`, `anonymous`; no `auth` and a null agent when the
  caller was not identified), `client` (the address, behind a trusted proxy the forwarded
  one), `bytes` (the request body), `textHash` (of an update's text, as in the request
  log: the text itself is not kept), `ms`, and `hash`. A write that changed quads has
  `graphs`, what it did to each graph (`{"graph": IRI or null for the default graph,
  "added": N, "removed": N}`, net: a quad removed and inserted again is neither; at most 100,
  `moreGraphs` counts the rest) and the totals `added` and `removed`. They count the
  request's own writes on the tripletrail store: SPARQL updates and Graph Store Protocol
  writes of up to 100,000 quads. A bulk load (a larger upload, a dataset made `from` a
  file or a backup) and a write to another store are not counted: the line has
  `"uncounted": true` instead. A line with neither wrote no quads (an update that changed
  nothing; an admin call that holds no data, or removes a whole dataset, which its `path`
  says).

  `hash` is the SHA-256 (hex) of the previous line's `hash` (64 zeros before the first), a
  newline, and the line without `hash` as canonical JSON (keys sorted, no spaces), so a line
  that is removed, changed or moved breaks the chain. `tripletrail audit verify FILE` checks it:
  it prints each problem by line number and exits 1, or prints the number of lines, the
  starts and the last hash and exits 0. Kept elsewhere from time to time, the last hash also
  shows lines cut from the end. `tripletrail audit report FILE` sums the log up by agent and
  then dataset: the changes done, refused and failed, the reads, the quads added and removed
  per graph, the bulk loads not counted, how the agent signed in (`auth`) and from which
  addresses, first and last time seen; agents without one are `anonymous`, admin calls
  without a dataset `-`. `--since` and `--until` (times in UTC as the log writes them, such
  as `2026-11-15T10:00:00Z`, or prefixes such as `2026-11-15`; another offset is refused;
  `until` is exclusive), `--agent` and `--dataset` narrow it, `--json` prints it as JSON. A
  change whose line named only its first 100 graphs is counted in `partialGraphs`: the
  per-graph sums leave out the rest, the totals do not. It checks the chain first: when it is
  not whole, the report says so and the command exits 1. A line is written and
  flushed before the answer is sent. A
  restart goes on with the chain; a last line cut off by a crash is ended, and the `start`
  that follows has `"cutBefore": true`, which `verify` accepts. Rotate the file by moving it
  away while the server is stopped (a new file starts a new chain), or keep it and ship it
  with a log collector. `--audit-syslog udp://HOST[:PORT]` or `tcp://HOST[:PORT]`
  (`TRIPLETRAIL_AUDIT_SYSLOG`, port 514 by default) also sends each line, as it is written, to a
  syslog collector or SIEM: one RFC 5424 message per line (facility 13, log audit; severity
  notice, or warning for `refused` and `failed`; APP-NAME `tripletrail`, MSGID the `event`, the
  line's JSON as the message; over TCP framed by octet counting, RFC 6587). Over UDP a line
  too long for a datagram (over 65,000 bytes, such as an update that touched a hundred graphs
  with long names) goes cut: its fields without `graphs`, long strings shortened, and
  `"cut": true`; its `seq` and `hash` find it in the file. Over TCP the connection is checked
  before each line, so a line sent while a collector closes it can still be lost uncounted,
  but not the first line after it went. Lines are sent
  from a queue of 10,000 by a thread of their own, so a slow collector never holds a request
  up; when the queue is full or the collector cannot be reached, lines are dropped (for a
  second after a failure, then it is tried again) and counted in
  `tripletrail_audit_syslog_dropped_total` on `/$/metrics`. The file keeps every line. The log
  is written whatever the licence says: a lapsed licence never
  leaves a gap.
- **Traces**: `--otlp-endpoint http://collector:4318` (or the standard
  `OTEL_EXPORTER_OTLP_ENDPOINT`) sends an OpenTelemetry trace of each request to an OTLP
  collector, over HTTP with JSON: the base URL gets `/v1/traces`, a URL that ends in it is
  used as it is. The OpenTelemetry Collector, Jaeger, Grafana Tempo and the vendors' agents
  take it on port 4318. Each request is a SERVER span named after its method and route
  (`GET /{dataset}/query`, `POST /$/datasets`), with `http.request.method`, `url.path`,
  `http.route`, `http.response.status_code`, `db.namespace` (the dataset),
  `db.operation.name` (`query`, `update`, `read`, `upload`) and `tripletrail.query_hash` (the
  hash of the request log; never the query text). Its child spans are the steps:
  `sparql.prepare` (parsing and planning, and for a result cached by `--result-cache-mb` the
  lookup), `sparql.evaluate` (the rows evaluated before the answer starts), `sparql.update`,
  and `http.send` (the answer sent; a streamed result evaluates its later rows while it is
  sent, so they count here). A step that fails has status ERROR with its message; the
  request's span has it only for a 5xx, as OpenTelemetry's HTTP conventions say. A request
  with a W3C `traceparent` joins the caller's trace and follows its sampled flag; others are
  traced at `--otlp-sample` (`TRIPLETRAIL_OTLP_SAMPLE`, 1 by default: all). `--otlp-headers`
  (`OTEL_EXPORTER_OTLP_HEADERS`, `key=value,…`) are sent with each export (a hosted
  collector's API key), and `--otlp-service-name` (`OTEL_SERVICE_NAME`, `tripletrail`) names
  the service. Spans go in batches of up to 512 from a thread of their own, at least every 2
  seconds; up to 4,096 wait, and more are dropped rather than slow the requests down. A
  collector that cannot be reached is logged once. At shutdown the queue is sent first (5
  seconds at most). Without an endpoint, nothing is traced and a request pays one check. The
  spans are written by TripleTrail itself (no OpenTelemetry SDK); build without the `otel`
  feature to leave them out.
- **Timeouts**: `--timeout` seconds for every query and update; `?timeout=` on a request
  can ask for less, never for more.
- **Query memory**: `--query-memory` (`TRIPLETRAIL_QUERY_MEMORY`, e.g. `256M`) bounds the
  memory of one query's intermediate rows. Joins on term ids that would pass it are
  evaluated row by row instead: slower, but in little memory and cancelled at the timeout.
  A hash join of the evaluator whose held rows would pass it fails the query with 503
  ("needs more memory than its limit"), and so do the rows `ORDER BY` sorts, the rows
  `DISTINCT` has seen, the groups of `GROUP BY` in the evaluator and the nodes a property
  path (`+`, `*`) reaches. A `GROUP BY` answered from the id tables (such as a plain
  `COUNT(*)` per key) whose groups would pass it goes to the evaluator, which fails it the
  same way. The values an aggregate keeps count too: the text of `GROUP_CONCAT` and the
  values a `DISTINCT` aggregate (`COUNT(DISTINCT …)`) has seen, and a count of a
  property path (`COUNT(*) { ?a :p+ ?b }`) whose edges would pass the limit goes to the
  evaluator. An `ORDER BY` under a `LIMIT`
  holds only about twice the rows the `LIMIT` and `OFFSET` read (#874). These parts of the
  evaluator count as one total per query (#874): a `DISTINCT` under an `ORDER BY` holds its
  rows in both and fails where either alone would fit, and an operator's rows stop counting
  once it is done with them. A join on ids has the limit to itself. Without the option,
  id joins take up to 1 GB and the evaluator has no limit, as
  before. On a shared server with many concurrent queries, a lower value caps what each
  query takes before its timeout. Raise it for ETL queries with
  large joins. Measured on a 6-way cross join on 20 triples: 1.1 GB peak by default, 300 MB
  at `256M`, 103 MB at `64M` (77 MB at start). On four subqueries joined on a key with 7
  values over 1,000 subjects (#476): 2.4 GB at the timeout by default, and a 503 after
  0.26 s at 245 MB with `256M`. The limit counts the rows' allocations, so actual memory is
  close to it.
  A dataset's `queryMemory` limit (below) applies to its queries only, to both kinds of
  join, and a SPARQL query may ask for less with `query-memory=` (e.g. `16MB`) in its URL or
  form body, never for more; `query-memory=0` (or any size under one byte) answers 400. Each
  query keeps the lower of the server's and its own limit. On 200 subjects, a three-way id
  join with a filter took 412 MB without a limit and 104 MB at `64MiB`, 4 times slower.
- **Lookup joins**: `--lookup-joins` (`TRIPLETRAIL_LOOKUP_JOINS`, on by default; `--lookup-joins
  false` turns it off, and `true`/`false`, `1`/`0`, `yes`/`no` and `on`/`off` are accepted; an
  empty variable counts as not set, and the other commands such as `infer` and `lift` read the
  variable too) is a safety net for plans that join on no key, or on a key with few
  distinct values, while the other side could be looked up: when one side of such a join is a
  basic graph pattern and the rows of the other side bind one of its variables that is not a
  key (typically an IRI made by `BIND`, which the planner cannot use as a key since it may be
  unbound), the pattern is
  evaluated once per row with that row's values instead of building a product. Rows that
  bind none of these variables are joined as before. Answers are the same; row order may
  differ. On the synthetic rail KG reproductions, an `OPTIONAL` on a `BIND`-made IRI (#484) took
  20.5 s and now takes 8 ms, and a join on a `BIND`-made IRI and a weak literal key (#486)
  took 23.5 s and now takes 92 ms.
- **Row budget**: `--max-rows N` caps how many rows (solutions, or triples of a
  `CONSTRUCT`/`DESCRIBE`) one query answers. A larger result is refused with 413 before any
  row is sent; `partial=true` on the request answers the first N rows instead, with an
  `X-Result-Incomplete: max-rows=N` header. A request can ask for fewer with `max-rows=`,
  never for more. With a budget the rows are read before the answer starts (to know whether
  there are more), so keep it to what a client can take in one response; without one, rows
  stream as before.
- **Body limits**: `--max-body` bytes (16 MiB by default) for a query, update or form body,
  and `--max-upload` bytes for a Graph Store upload (no limit by default); larger bodies get
  413. An upload is parsed in full before the graph changes, so a body with an error leaves
  the data as it was.
- **Per-dataset limits**: `--limits FILE` (`TRIPLETRAIL_LIMITS`) sets limits per dataset, so
  one busy dataset cannot take the whole server. The JSON file has a `default` entry and one
  per dataset name under `datasets`; a dataset's entry overrides `default`, and what neither
  sets comes from `--timeout`, `--max-rows` and `--max-upload`. The file is read again when it
  changes (an invalid file keeps the previous limits and logs why).
  ```json
  {
    "default": { "maxConcurrent": 3, "maxQueued": 4, "queueTimeout": 10 },
    "datasets": {
      "network": { "maxConcurrent": 2, "timeout": 30, "maxTimeout": 600, "maxRows": 100000 },
      "rinf": { "maxUpload": 1073741824 }
    }
  }
  ```
  - `maxConcurrent`: requests the dataset answers at once (queries, updates, Graph Store,
    RDF4J and Linked Data requests alike). Keep it below `--threads` so the other datasets
    always find a free worker.
  - `maxQueued` (0 by default) and `queueTimeout` (10 s): a request over `maxConcurrent` waits
    for a turn while fewer than `maxQueued` others wait, for at most `queueTimeout` seconds. A
    waiting request holds a worker thread. Otherwise the answer is `429 Too Many Requests`
    with `Retry-After: 1`.
  - `timeout`: the timeout of a request that asks for none; `maxTimeout`: the most a request
    may ask for with `?timeout=` (the dataset's `timeout` when not set). Setting either
    replaces `--timeout` for the dataset.
  - `maxRows` and `maxUpload`: as `--max-rows` and `--max-upload`, for this dataset.
  - `queryMemory`: the memory one query of this dataset may hold in its joins, as a size such
    as `"256MiB"` (see **Query memory** above). A request's `query-memory=` may ask for less.
  - `maxDisk` and `maxMemory`: quotas, in bytes or as a size such as `"10GB"` or `"512MiB"`
    (KB, MB, GB and TB are powers of 1000; KiB, MiB, GiB and TiB of 1024). `maxDisk` is the
    size of the dataset's directory (on-disk datasets); `maxMemory` is the heap held by its
    dictionary, quad index, parsed geometries, spatial and full-text indexes (with their
    views for callers who see some graphs only), range indexes, result cache and cached
    routing networks (TripleTrail's store only; memory-mapped checkpoint files are not
    counted). The sizes are estimates, and an index is counted once it is built: the first
    geometry a query reads also allocates a 6 MB table of parsed geometries.

    A read can therefore take a dataset over its memory quota: the first `tt:search`,
    spatial query or numeric range FILTER builds an index that stays, and from then on
    writes get 507 although no writer added anything. Size `maxMemory` for the data plus
    the indexes your queries use. On 470,000 triples with 150,000 geometries, labels and
    numbers, the store took 57 MB and the full-text, spatial (with its parsed geometries)
    and range indexes another 28, 31 and 19 MB: about twice the data with all three.
    `memory` in `/$/stats/{name}` shows the parts. Writes come back once data is removed,
    the quota is raised, or the server restarts (the indexes are then built again only
    when a query needs them). While a dataset is at or over a quota, writes that may add data
    (uploads, `PUT`, updates with `INSERT`, `LOAD` or `CREATE`, RDF4J `ADD` and commits)
    are answered with `507 Insufficient Storage`. Reads, Graph Store `DELETE`, and updates
    that only remove data (`DELETE DATA`, `DELETE WHERE`, `DELETE` without `INSERT`, `CLEAR`,
    `DROP`) still work, so the dataset can be brought back under. A write is checked before
    it starts: the one that crosses a quota completes. The terms of removed data stay in the
    dictionary, so deleting data frees less memory than it held.

  A limit isolates worker threads, not cores: to keep other datasets fast during a flood,
  the `maxConcurrent` of the heavy datasets together should stay below the server's cores.
  On a 4-core machine, a flood on a dataset with `maxConcurrent: 1` left the p95 of light
  queries on another within about 20% of idle; with `maxConcurrent: 2` it doubled.

  The limits can be read and changed over the admin API (admin token or `tripletrail:admin`;
  reading also allows `tripletrail:monitor`). Changes are written to the file and apply at once:

  - `GET /$/limits`: the file. `PUT /$/limits` replaces it with the JSON body.
  - `GET /$/limits/{name}`: the dataset's entry as written (`written`) and as applied
    (`applied`). `PUT /$/limits/{name}` sets its entry; `DELETE /$/limits/{name}` removes it.

  An invalid file or entry is refused with 400 and changes nothing. Without `--limits`,
  these answer 409.

  `/$/stats/{name}` shows each dataset's limits as applied (`limits`), its `running` and
  `queued` requests, the totals `queuedTotal`, `rejectedBusy` and `rejectedQuota`, and its
  `memoryBytes` and `sizeOnDisk`. `memory` splits `memoryBytes` by part: `dictionary`,
  `quads`, `geometries`, `spatialIndex`, `textIndex`, `vectorIndexes` (the copies of
  vectors that `tt:nearest` keeps), `rangeIndexes`, `resultCache`,
  `routingNetworks` and `unionCopies`. `/$/metrics` has `tripletrail_dataset_requests_running`,
  `tripletrail_dataset_requests_queued`, `tripletrail_dataset_requests_rejected_total`,
  `tripletrail_dataset_writes_rejected_quota_total`, and `tripletrail_dataset_memory_bytes` with
  the same parts as a `part` label.

  The same file limits callers across all datasets under `callers`:
  ```json
  {
    "callers": {
      "default": { "maxConcurrent": 4, "requestsPerMinute": 600 },
      "anonymous": { "maxConcurrent": 2, "requestsPerMinute": 60, "burst": 10 },
      "agents": { "https://example.org/people/etl": { "maxConcurrent": 8 } },
      "groups": { "partners": { "requestsPerMinute": 1200 } }
    }
  }
  ```
  - `maxConcurrent`: requests at once; one more gets 429 at once (no queue).
  - `requestsPerMinute` and `burst`: a token bucket that holds `burst` requests (a tenth of the
    rate by default) and refills at the rate. An empty bucket gives 429 with `Retry-After` set
    to the seconds until the next token.
  - `agents` entries apply to one agent IRI (from [Authentication](#authentication)), `groups`
    entries to all members of the group together, `anonymous` to each client address
    without an identity (without `--auth-config`, every caller), and `default` to each
    caller that no `agents` or `anonymous` entry names. A request must pass its own entry and
    those of each of its groups. Behind a reverse proxy listed in the auth configuration's
    `trustedProxies`, an anonymous caller's address is the last `X-Forwarded-For` address;
    behind any other proxy, anonymous callers share the proxy's address.
  - `/$/ping` and `/$/ready` are never limited. `/$/metrics` counts the refusals as
    `tripletrail_caller_requests_rejected_total` by `reason` (`rate` or `concurrency`).

### Memory allocator

The `tripletrail` binary (and the Docker image) uses [mimalloc](https://github.com/microsoft/mimalloc)
rather than the system's allocator (#934). An `INSERT … WHERE` evaluates its `WHERE` on one thread
and writes the triples on another, and glibc's allocator slows down when one thread frees what
another allocated: an update writing 7.4M triples took 46 s with it and takes 12 s with mimalloc.
SHACL rules (`tripletrail infer`) on a 12.5M-quad railway knowledge graph run about 13 % faster too. Peak memory is the same for
updates and loads; `infer` peaks about 10 % higher. A build without the default `mimalloc` feature
(`cargo build -p tripletrail-server --no-default-features --features …`) keeps the system allocator.

### HTTPS

`--tls-cert` (the certificate chain, PEM) and `--tls-key` (its private key: PKCS#8, PKCS#1
or SEC1 PEM) make the server speak HTTPS on its `--bind` address; `TRIPLETRAIL_TLS_CERT` and
`TRIPLETRAIL_TLS_KEY` do the same. With them:

- Plain HTTP on that address gets no answer. TLS 1.2 and 1.3 are offered, with ALPN
  `http/1.1`.
- The server's own URLs (service descriptions, VoID, linked-data pages) are `https`.
- Caller limits, failed-key counting and `trustedProxies` see the client's own address.
- A renewed certificate is taken without a restart: the files are looked at, at most once a
  second, when a connection arrives, and are read again when either changed, or at once on
  `SIGHUP`. Connections already open keep the old certificate. While the new files are
  invalid (a key written before its certificate, say), the old certificate stays in use and
  the log says why.

```sh
tripletrail serve --location /data --dataset kg \
  --tls-cert /etc/letsencrypt/live/kg.example.org/fullchain.pem \
  --tls-key /etc/letsencrypt/live/kg.example.org/privkey.pem
```

`tripletrail health` (the Docker image's `HEALTHCHECK`) uses HTTPS when `TRIPLETRAIL_TLS_CERT` is
set, or for an `https://` `--url`. It does not verify the certificate, because it only asks
whether the server is ready. Kubernetes `httpGet` probes need `scheme: HTTPS`.

Behind a reverse proxy or an Ingress that ends TLS itself, leave these out. A build without
the default `tls` feature has no TLS support.

### Kubernetes

The Helm chart in [`deploy/helm/tripletrail`](../deploy/helm/tripletrail/README.md) sets this up:

- datasets on a persistent volume;
- the admin token and auth config from Secrets, and the limits file from a ConfigMap;
- probes, an optional Ingress and a ServiceMonitor;
- `helm test`.

By hand, the essentials are:

```yaml
containers:
  - name: tripletrail
    image: ghcr.io/matdata-eu/tripletrail:main
    env:
      - { name: TRIPLETRAIL_ATTACH, value: "network_20261107=/data/network_20261107:ro" }
      - { name: TRIPLETRAIL_ALIASES, value: "network=network_20261107" }
      - name: TRIPLETRAIL_ADMIN_TOKEN
        valueFrom: { secretKeyRef: { name: tripletrail, key: admin-token } }
    ports: [{ containerPort: 7878 }]
    readinessProbe: { httpGet: { path: /$/ready, port: 7878 }, periodSeconds: 5 }
    livenessProbe: { httpGet: { path: /$/ping, port: 7878 }, periodSeconds: 15 }
    volumeMounts: [{ name: data, mountPath: /data }]
terminationGracePeriodSeconds: 45
```

## Web UI

`/$/ui/` (and `/`, which redirects there) has these pages, each over the full width of the
window. An IRI shown on a page (a feature on the map, a node on Explore, a search result, a
shape, a dataset's endpoint) has a button after it that copies it. **Share** in the top bar
copies a link that opens the same view: the page's own link, which holds what it shows, and on
the Query page MatGUI's tab as `?query=…&tabTitle=…&outputFormat=…` (never its request headers,
which hold the admin token or a "view as"). Opening such a link adds the query as a tab and
then takes it out of the address bar.

- **Datasets**: totals, every dataset and alias with its mode and readiness, and per dataset
  its statistics, back up, snapshot, detach, upload (Graph Store Protocol, with progress) and,
  for an alias, swap and roll back. New datasets are created, attached or restored from a
  backup in one dialog.
- **Query**: [MatGUI](https://github.com/Matdata-eu/Matgui) on the chosen dataset, with its
  table, map, chart and graph views, and **Explain**, which shows TripleTrail's plan with row counts,
  timings and the spatial and range decisions. MatGUI's own **Explain** tab of the results asks
  `/{ds}/explain` too, as it recognises the server by its `Server` header (its Engine list
  reads "Detect (TripleTrail)"), and draws the plan as a tree of steps with their rows and times.
- **Map** (`/$/ui/map/{ds}`): the dataset's features on a map, a layer per collection (see
  [Vector tiles](#vector-tiles)), with a layer list, facets to filter each collection with
  their counts, and the features under a click. A feature's describe page opens in a panel
  beside the map (at once when the click hit one feature), and its links to other resources
  open there too. The page's link holds the view, so it can be shared: the position in its
  hash (`#zoom/lat/lon`), the hidden layers as `hide=id,id`, the feature open beside the map as
  `feature=IRI` and the filters as the tiles take them (`{collection}.{key}=value`, `.min`,
  `.max`) in its query. A collection's filter panel
  opens its features in the OGC API (*OGC API*) and copies their GeoJSON link for a GIS
  (*Copy GIS link*), with the filters as CQL2 (`IN`, `>=`, `<=`), and a drawn area as
  `S_INTERSECTS`, `S_WITHIN`, … on `geometry`; outside the area (no geometry meets it), a
  circle, corridor, feature or other layer as the area stays `area=`. *Colour by* draws a collection in a colour
  per value of one of its values facets (its 10 most frequent values under the filters, the
  others grey), or per class of a range of numbers (the shape's `tts:colorClasses`, else five
  of equal width on round numbers, blue to red), with a key that counts the features of each
  class under the filters; the link keeps it as
  `color.{collection}={key}`. A range facet's numbers show with their unit (`qudt:hasUnit`).
  The search box finds the map's features by the words of their labels; a result goes on the
  map (its layer shown) and opens its page beside it.
- **Explore** (`/$/ui/explore/{ds}?iri=…`): the graph around a resource, drawn as nodes and
  links. Clicking a node opens its links: up to 8 per predicate and direction are drawn, the
  rest stay in a group box (`has (9,950 more)`) that shows 50 more at each click. The panel
  beside the graph has the selected node's values, its links with how many are shown, and
  *Collapse*, *Pin* (drag a node to pin it too), *Hide*, *Open page* (its describe page) and
  *Start here*. *Filter* hides predicates and classes. At most 1,500 nodes are drawn. *SVG*
  saves the picture, and *CONSTRUCT* the query that gives the links shown
  (`explore.rq`). The link holds the start (`iri`), the opened nodes (`open`), the opened
  groups (`groups`, `node|in or out|predicate|pages`) and what is hidden (`hide`,
  `hidePredicates`, `hideClasses`), so it can be shared. The Map page's detail panel and the
  Search page's results link to it. *Path from here*, then another node or an IRI (*Find*), adds
  the shortest path between them: TripleTrail's [path search](routing.md#path-search), at most 8
  links followed either way, over every link between IRIs but `rdf:type` and the hidden
  predicates, searched on the indexes from both ends (#819). The path is drawn highlighted and kept in the link as `paths=from|to` (at most 10
  reopened); *Clear paths* removes the paths.
  *Map* opens a map beside the graph (`map=1` in the link), over the dataset's base map (that of
  its map style, without its own layers). It shows the shown nodes that have a geometry
  (`geo:asWKT`, or `geo:hasGeometry/geo:asWKT`, the first of each, as `geof:asGeoJSON`) in
  their graph colours. Selecting a node selects it on the map, and a click on a feature selects
  its node. The map fits what is shown until it is moved.
- **Shapes** (`/$/ui/shapes/{dataset}?shape={IRI}`): the dataset's node shapes by class, with their layer,
  and a table of each one's property shapes to change what the readers read (names per
  language, order, groups, keys, hidden, hidden from, deactivated); the Turtle of an own file
  with live checks; the kept versions with rollback. See
  [The shapes editor](#the-shapes-editor).
- **Import** (`/$/ui/import/{ds}`): drop a file (CSV, JSON, XML, YAML, a spreadsheet, HTML,
  Markdown, an archive...), see it as Facade-X triples, edit the drafted `CONSTRUCT`, preview
  the triples it makes and add them to a named graph. *Download query* saves the query as a
  lift query that reads the file by its name. See [Importing files](#importing-files).
- **Workbench** (`/$/ui/workbench/{workspace}/{file}`): develop shapes, rules and queries in a
  [workspace](#workbench-workspaces), described below.
- **Access** (`/$/ui/access`): API keys, groups, anonymous access and who may read and write
  each dataset and graph, with a history of the changes (see
  [The access admin API](#the-access-admin-api)). It needs `--auth-config`.
- **Tasks & backups**: background tasks, and backups with restore.
- **Server**: version, uptime and settings.

### Workbench

The Workbench lists your workspaces and starts new ones, empty or from an example (validation,
triple rules, SPARQL rules, conditions and order, a fixpoint, rules before validation). In a
workspace:

- **Files** of `data/`, `shapes/`, `rules/`, `queries/`, `updates/` and `tests/`, with upload, new
  and delete. **Sample…** copies instances of some classes of a dataset, and what they point to,
  into `data/` (the dataset's profile suggests classes).
- **Editor** (CodeMirror 6) for Turtle, SPARQL and YAML. A file is saved as you type; the server
  parses RDF files, and a syntax error is marked on its line and keeps the file unsaved. Prefixes,
  the classes and properties of the scratch store, the names used in the workspace and SHACL terms
  complete; snippets add constraints and rules. Hovering an IRI shows its label and counts, with a
  link to describe it. **Ctrl+Enter** runs the open tab, **Ctrl+S** saves, and **Run while typing**
  runs it after each save in workspaces up to 50,000 triples.
- **Query** runs the open file of `queries/` (or `updates/`, on the scratch store) with a table of
  the results, **Explain**, and **Open in MatGUI**, which opens the Query page on the workspace.
- **Validate** runs `…/validate`: results grouped by shape (a blank property shape by its node
  shape and path), then constraint, then focus node, with counts, a severity filter, and what is
  new or fixed since the last run; the shapes checked with their focus and value nodes; coverage;
  and the Turtle report. Shapes that do not compile show the reason and mark the shape in its file.
  A focus node opens its describe panel with the triples the result is about highlighted.
- **Rules** runs `…/infer` with provenance, to a fixpoint or a single pass: the inferred triples
  with their rule, focus node and pass, or the rules with their passes, focus nodes, constructed
  and new triples and time, or the strata: the groups in the order they run, each saying
  whether it repeats to a fixpoint and in how many passes, with each rule's `sh:order`, what it
  writes and which rules it reads from, through which predicates, and whether only under a
  negation (which orders the groups but never makes one repeat). **Try in Query** turns a SPARQL rule into a query (`$this` as `?this`).
  Downloads: the inferred triples as Turtle, or the data and inferred triples as N-Triples.
- **Reason** runs `…/reason` under RDFS or OWL 2 RL: the entailed triples, how many by class
  and by predicate, and under OWL 2 RL the inconsistencies. **why?** on a triple shows its
  derivation tree: each step names its rule (with what the rule says) and its premises,
  down to asserted triples. **why?** on a `cax-dw` inconsistency shows how the resource
  came to be in both disjoint classes.
- **Tests** runs the workspace's [tests](#tests): each passes or fails with what is missing
  and unexpected, can be run on its own, and **Accept output** makes what it gives now its
  expected output. **Save as test…** on the Query, Validate and Rules tabs makes one from what
  the tab runs: a copy of the data files as its input, the query (or `../../shapes` and
  `../../rules`, or `../../rules` and the passes) as its target, and the output it gives now as
  expected.
- **Pipeline** runs the workspace's [`pipeline.yaml`](#pipelines) step by step: **Run** runs the
  steps not run yet or changed since (from the first such step), **Run to here** up to a step,
  **Run this step** one step again from what the step before it left, and **Run all** from the
  first. Each step shows its time, the quads after it, the triples added and removed by predicate
  and by class with samples, validation results and errors; a step whose files changed since it
  ran, and those after it, are marked. **Query after** runs the Query tab on the store as the step
  left it. The pipeline's parameters can be set for a run. **Airflow DAG** and **Python script**
  give the pipeline as code (see `tripletrail pipeline export`).
- **Copy as Python** on the Query, Validate and Rules tabs copies the `tripletrail.Engine` code
  that does what the tab does on the workspace's folders, for a notebook or a production script
  (and how to `tripletrail.connect` to the workspace instead).
- **Promote shapes…** makes the Turtle files of `shapes/` a dataset's own shapes: the next
  version of its set (`PUT /$/datasets/{name}/shapes/files`, an admin's), which keeps the one
  before for a rollback and names the workspace as its source.
- **Pull…** writes the files of a folder of a git repository over the workspace's (see
  `…/pull`; an admin's).
- **Overlay…** makes the workspace an overlay of a whole dataset (see `…/overlay`): updates
  run in the Query tab, the pipeline and validation then work on the dataset as it is, and
  the dataset stays as it was. A chip in the bar shows the dataset and about how many quads
  changed; its dialog downloads the changes as an RDF Patch, drops them, or detaches.
- **Share link…** gives a link that holds the workspace's files themselves (deflated, in the
  URL's hash, which no server sees; up to 64 KB): opening it on the same server makes a new
  scratch workspace with a copy. Files a sample of a dataset wrote are left out (edited or not),
  and so are compressed and large files. Other files are not checked: a file made from sampled
  data, by a pipeline export or by hand, goes in the link like any other.
- **History** lists what other clients ran against the workspace (the Python client, a notebook,
  curl; see `…/history`), refreshed every 5 seconds, with a count of new runs on its tab. Opening
  a run shows it where the workbench shows its own: a query or update as a file of `queries/` or
  `updates/` (`history-N.rq`), the query run again on the store it ran on; a validation's report;
  an inference's triples, with why? on each (when the client asked for provenance, as
  `ws.infer(provenance=True)` does; otherwise the rules run again); the tests; the pipeline.
- **AI** (#146, #409) asks a model of the user's choice for shape changes, rules, queries,
  updates and tests in plain words, and explains validation results and why? answers. The
  connection is the one MatGUI's assistant on the Query page keeps in the browser (the
  `tripletrail-ui_ai` entry of local storage), so one key serves both. It is set with **Key and
  model…**: an OpenAI-compatible base URL, key and model (OpenAI, Anthropic, Mistral, Scaleway,
  Groq, OpenRouter, or Ollama or LM Studio on the user's computer). Requests go from the browser
  to that service only, as `…/chat/completions` with tools. The model gets a system prompt with
  the `tts:` vocabulary (`/$/ui/tts.ttl`), where each kind of file goes, and the workspace's
  file names. Its tools read
  through the server under the user's token:
  - `get_profile`: [Dataset profile](#dataset-profile).
  - `get_dataset_shapes`: `…/shapes`, or one class's node shape with its property shapes through
    `…/shapes/sparql`; with `profile`, those of a [profile](#shapes) (`profile=`).
  - `set_profile`: puts a class's node shape in a profile or takes it out. It reads the
    profiles the shape is in and those the shapes name (`tts:inProfile`, `tts:Profile`), and
    answers the profiles before and after with the shape in use changed to them, for the model
    to propose as a file; it writes nothing. A shape in no profile is in all, so taking one out
    of a profile puts it in the others that are named, and is refused when there are none.
  - `list_files` and `read_file` of the workspace.
  - `check_shapes`: `POST …/shapes/validate?file=`.
  - `check_file`: [`POST /$/workspaces/{id}/check`](#workbench-workspaces), for any file it proposes.
  - Only with **may query the data**: `run_sparql` on the dataset's endpoint, 50 rows at most
    (asked with `max-rows=51`, so a large result is not downloaded); `run_query` on the
    workspace's own store (`…/query`, 50 rows or triples); and `run_tests`, the saved tests
    (`POST …/tests`, without `record`), with the first 10 missing and unexpected results of
    each.

  **explain** beside a validation result, and **Explain with AI** in the why? panel of an
  inferred triple, ask the model in the tab, with the result (focus node, path, value, shape
  and the node shape a blank property shape belongs to, constraint, severity, messages) or the
  rule, focus node and bindings as JSON.

  An answer's files (a `File: path` line and a fenced block) show as a diff against the
  workspace's file, with **Apply**. A file may go in `shapes/` or `rules/` (`.ttl`), `queries/`
  (`.rq`), `updates/` (`.ru`) or a test's folder `tests/NAME/`; a bare name goes to the folder of
  its kind, and any other path (`data/`, `pipeline.yaml`, `..`) is shown as text only. **Apply**
  writes the file into the workspace and opens it in the editor, staying on the AI tab
  (Ctrl+Z takes it back). The assistant writes nothing itself; a dataset changes only through **Promote shapes…**.
  **Preview on map**, beside **Apply** and in the tab's bar, posts the workspace's `shapes/`
  files (with the proposed one in place of its namesake) to
  [`/{ds}/tiles/preview`](#previews-of-proposed-shapes) and opens the Map page on the preview
  (`/$/ui/map/{ds}?preview=ID`), under a banner that says it is not saved.

  `/$/ui/workbench?configure={name}` (the **Configure with AI** link of the Map and Search pages)
  makes a workspace "Configure {name}" holding a copy of the dataset's own shape files
  (`GET …/shapes/files`, none when it has no `--location`), and opens it on the AI tab with that
  dataset picked (`?ai={name}` on a workspace link does the same). Promote replaces the whole set,
  so the copy keeps the shapes the assistant does not touch.
- **why?** on any triple (inferred, in query results, in a describe panel) shows where it came
  from: the rule, its shape, the focus node, the pass and the bindings of a SPARQL rule's
  solution, or that it is in the data. Once the pipeline has run, it also shows which steps
  added or removed the triple and from which file (blame).

The Workbench never writes to a dataset. CodeMirror 6 and Tabulator 6.3.1 (both MIT) are built in;
`crates/tripletrail-server/ui/vendor/codemirror/src/build.sh` rebuilds the CodeMirror bundle from its
pinned versions.

A short tour opens on the first visit and again from the **?** button; **About** has credits,
feedback links and **What's new**, the release notes from `crates/tripletrail-server/ui/whats-new.json`
(add an entry with every release: a test checks that the newest one is the crate's version). A
returning browser gets a dot on About when a release it has not seen is out.

The admin token is asked for when a change needs it and kept in the browser's local storage.
MatGUI 6.15.0 (MIT) is built into the binary, so the UI works without internet access;
`--matgui-url https://cdn.jsdelivr.net/npm/@matdata/yasgui@6.15.0/build/` loads it from a CDN
instead. `--no-ui` turns the UI off.

### Importing files

The Import page's API. The file is kept in the server's memory, never written to its disk, and
read as [Facade-X](facade-x.md) only by the queries sent for it. Every request needs the right
to read and write the whole dataset (the update or admin token, or a policy that grants
reading and writing every graph): callers whose policies give them only some graphs, or rows
under row rules, are answered 403, as a mapping's `WHERE` could read past them. A read-only
dataset answers 405.

- `POST /{ds}/import?name=NAME[&options=…]`: the file as the body. `NAME` (letters, digits,
  `.`, `-`, `_`) chooses the format as a file name does; `options` are Facade-X options
  (`csv.headers=true` by default for `.csv` and `.tsv`); options that name another source
(`location`, `from-archive`, `content`, `table`, `sql`...) are refused. Answers 201 with `id`, `name`,
  `bytes`, `options`, `location` (`upload:NAME`), `tree` (the first 300 triples, as N-Triples),
  `triples` and `truncated`, and `query`: a `CONSTRUCT` that maps every key of the records (the
  rows of a table, the objects of a JSON array), with IRIs under `https://example.org/import/`.
  A file that cannot be read is answered 422.
- `POST /{ds}/import/{id}/preview`: a `CONSTRUCT` as the body; answers its triples (Turtle by
  default, any format `/{ds}/query` gives by `Accept`) and writes nothing.
- `POST /{ds}/import/{id}/run?graph=IRI`: adds the triples of the `CONSTRUCT` (without `FROM`)
  to the graph, in one update, and answers `{"after", "before", "graph"}`: the graph's size
  before and after.
- `DELETE /{ds}/import/{id}`: forgets the file (204).

The queries may read only the upload: `location=upload:NAME`, or `location=NAME` of its own
name (so a lift query that names the file runs as it is), and `from-archive` of it. Other
locations are refused. A file is forgotten an hour after it was last used, and the server keeps
at most 16 files and 256 MiB, forgetting the least recently used first; then a request for it
answers 404.

The page's **Draft with AI** (#20) asks the user's AI service (the Workbench's connection, see
[the AI tab](#workbench)) for the `CONSTRUCT`, from the upload's `tree`, the query on the page
and an optional request. The model's tools are `preview` (the endpoint above, answering the
number of triples and the first 40, or the error) and `get_shapes` (`GET
/$/datasets/{ds}/shapes`, or one class's node shape with its property shapes through
`…/shapes/sparql`). The page takes the last fenced query of the answer, keeps the previous
one for **Back to the previous query**, and previews it. Requests go from the browser to the
AI service only; the server only answers the previews and the shapes, under the user's token.

## Linked Data

With `--base-iri DATASET=PREFIX`, the IRIs of a dataset dereference: a `GET` of a URL that
starts with `PREFIX` (and is none of the server's own paths) answers the description of the
resource with that IRI.

```sh
tripletrail serve --location /data --dataset kg --base-iri kg=https://data.example.org/
curl -H 'Accept: text/turtle' https://data.example.org/op/BE123
```

- **Formats.** A suffix (`.ttl`, `.jsonld`, `.nt`, `.rdf`, `.html`) or `?format=` picks the
  format, else `Accept`; Turtle when nothing is asked for. A browser gets the resource's
  describe page (see [Describe pages](#describe-pages)); `?format=view` gives that page's view
  model as JSON.
- **What is returned.** The concise bounded description over all graphs: the resource's
  triples and those of the blank nodes they reach, the blank nodes named `b0`, `b1`… in the
  order they come; Turtle declares prefixes for the namespaces it uses (well-known ones such
  as `rdfs:` and `geo:`, others used twice or more after their last path segment). For an IRI without a `#`, the resources
  `IRI#...` come with it, so a hash-namespace vocabulary is one document. A description stops
  at 100,000 triples (with an `X-Truncated` header).
- **Caching.** `Vary: Accept`, `Link` headers to the other formats, and an `ETag` that
  changes with every write (for a caller who reads only some graphs, with every change of
  the description); `If-None-Match` with it answers `304 Not Modified`.
- **303.** With `--see-other`, a request for the resource itself answers `303 See Other`
  to its document (`/op/BE123` → `/op/BE123.ttl`), as recipe 5 of the W3C "Best Practice
  Recipes for Publishing RDF Vocabularies" does. Most publishers answer 200 directly, so it is
  off by default.
- **Any IRI.** `/{ds}/describe?iri=<iri>` describes any resource, also outside the base
  IRIs, with the same formats. With `preview=<id>`, a preview's shapes (`POST
  /{ds}/tiles/preview`) lay out the page in place of the dataset's own, and its links to other
  resources keep the preview; an unknown or expired id is `404`.

A dataset name or a service path (`/kg`, `/sparql`, `/$/...`) answers as before, so keep
those out of your IRIs' first path segment. Datasets are also found by their VoID description
at `/.well-known/void`, and by data portals in the DCAT-AP catalog at `/.well-known/dcat`.

Behind a reverse proxy, forward the original host and scheme, so the URL the server rebuilds
is the IRI:

```nginx
location / {
    proxy_pass http://127.0.0.1:7878;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Host $host;
    proxy_set_header X-Forwarded-Proto $scheme;
}
```

With Traefik, the `Host` header is passed through and `X-Forwarded-Proto` is set by default.

## Describe pages

Every resource has a readable page: its HTML answer at `/{ds}/describe?iri=<iri>` and, under a
base IRI, when its IRI is dereferenced by a browser. It works without configuration and follows
the dataset's DASH shapes when it has them (#140).

- **Layout.** A header with the resource's classes, title and IRI, and its actions: Copy IRI,
  Share (copies the page's URL with the open tab, `&tab=<key>`), Open in map
  (`/$/ui/map/{ds}?feature=<iri>#zoom/lat/lon`, for longitude/latitude geometries), Explore
  (`/$/ui/explore/{ds}?iri=<iri>`) and Export (the documents below). Then tabs, each a
  `<section class="tab-panel" data-tab="…">`: `overview` (the map and the groups without
  `rd:tab`), one per `rd:tab` of the shapes, `properties` (every property with its first 100
  values; literals over 1,000 characters are cut), `incoming` (every link to the resource,
  20 per property, read within 10,000 links), `graph` (one hop out and in, at most 24 nodes a
  side and 6 per property; literal values only when there are at most 12), `model` (the
  resource's classes and superclasses, at most 12, as UML boxes from the property shapes'
  `sh:path`, `sh:datatype`/`sh:nodeKind`/`sh:in`, `sh:class`, `sh:minCount` and
  `sh:maxCount`, with the classes whose shapes point at them; each property on the most
  specific class; also as a table and as a Mermaid `classDiagram`) and `rdf` (the
  description as Turtle in the page when it has at most 5,000 triples and 1 MiB, the other
  formats fetched on a click). With the web UI, `/$/ui/describe.js` makes the sections tabs
  and the buttons work; without it (`--no-ui`), or without script, the sections follow each
  other and only the links work.

- **From the shapes.** The page reads the shapes of the describe reader: the dataset's own
  (`/$/datasets/{ds}/shapes`), over those generated from its profile. The node shapes of the
  resource's classes and their superclasses apply (`sh:targetClass`, `dash:applicableToClass`,
  or a class that is itself a `sh:NodeShape`), the dataset's own before the generated ones.
- **Rows.** Each property shape gives a row: its name (`sh:name`, in the browser's
  `Accept-Language`), its group (`sh:group`, with the group's `rdfs:label` and `sh:order`), its
  order (`sh:order`) and its viewer (`dash:viewer`: `LabelViewer`, `LiteralViewer`,
  `URIViewer`, `DetailsViewer`, `ValueTableViewer`, `ImageViewer`, `HTMLViewer`, ...).
  `dash:hidden true` and `sh:deactivated true` hide a row. A path is any SHACL path: a
  property, `[ sh:inversePath p ]` (the resources linking to this one), a sequence `( p q )`,
  `sh:alternativePath`, `sh:zeroOrMorePath`, `sh:oneOrMorePath` or `sh:zeroOrOnePath`, nested.
  A path other than a property or its inverse is followed from the resource reading at most
  100,000 triples (a path of more than 256 nodes, counting shared named paths each time they
  are used, gives no row), and its row's tooltip (and `path` in the JSON) gives it in SPARQL syntax;
  without `sh:name` the row is labelled with that path in local names. A path that is not well
  formed (a sequence of one, a list that does not end) gives no row. A named node with path
  triples (`ex:parts sh:alternativePath ( ... )`) is that path wherever a path goes, so shape
  files can share paths. Two shapes with the same `tts:key` (or path; for a row of the
  dataset's own shapes without a key, also its name, as rinf-describe keys it: *Part of* is
  `part-of`) give one row, from the
  first: the shapes of the resource's most specific classes come first (a resource typed with
  a class and its superclass gets the subclass's rows), and the dataset's own property shapes
  before the generated ones. `[ tts:key "parts" ; sh:deactivated true ]` turns off the row of
  that key that a superclass's shape gives.
- **Key figures, units, tabs, summaries.** A row with `dash:propertyRole dash:KeyInfoRole` is
  also a badge under the title (only a badge without a `sh:group`); `dash:LabelRole` gives the
  title. The rinf-describe terms load as they are: `rd:key` (as `tts:key`), `rd:unit` (written
  after the values), `rd:tab` on a group (the overview first, then each tab as a heading over
  its groups; with shapes of the dataset's own, the generic *Attributes* and *Links* groups go
  to an *Other properties* tab), `rd:class` (only the values of that class), `rd:hidden`, and
  `rd:over` with `rd:aggregate` (`count`, `distinct-count`, `range` or `sum` of the path's
  values on the resources `rd:over` reaches, read as rinf-describe reads them: `count` counts
  those resources, a distinct count ends with `none (n)` for the ones without a value, so it
  is a row even when none has one, and a range of only some of them ends with `(k of n)`; a
  range or sum without any value is no row; the links and the values are each read up to
  100,000 triples, and past that the row has `truncated` and the page says the summary is of
  the first ones; without a key, rows with the same path differ by `rd:over` and
  `rd:aggregate`). A property shape without `sh:path` is accepted
  when it is `sh:deactivated` or names a `dash:viewer` (a widget that reads no path of its
  own). A rinf-describe widget (`dash:viewer rd:ValidityViewer`, `rd:LrsLocationViewer`...)
  gets the values of the paths it names (`rd:begin`, `rd:end`, `rd:km`: any `rd:` term with a
  path, not a setting; up to 10 values each, read within 100,000 triples) as `widget` and
  `params` in the view; the page shows a validity as "from … until …" and other widgets'
  values by name. A widget row without `sh:path` (keyed by its `sh:name`) is shown when one
  of its paths has a value. Three widgets are drawn on the page: `rd:DocumentViewer` shows
  each value as a card with its file type, named by its `name` cell and opening its `url`
  cell (`sh:node` columns, web addresses only); `rd:ScaleBarViewer` draws a number as a bar
  against `rd:marks` ("400 Passenger train; 740 Freight train", ticked when reached), or the
  `rd:min` to `rd:max` range on `rd:scale` ("4 7"), and the view has `marks` and `scale`;
  `rd:ConceptTreeViewer` draws its values as a tree by their `broader` cells, with their
  `notation`, the page's own concept in bold (up to 32 levels; concepts that are each
  other's broader concept are drawn once; values that are not IRIs are counted below it).
  `rd:FacilitiesViewer` draws each `rd:facility` property as a tile (yes, no, not given, or
  its value), named by the property's label; those properties get no rows of their own.
  `rd:KeyFiguresViewer` draws the summary rows named in `rd:figures` as tiles, in that
  order, and the view has them as `figures`; a hidden row it names is read for it and stays
  hidden elsewhere. `rd:LrsLocationViewer` writes the values' railway location from its `km`
  and `offset` paths (`km 53+240`). `rd:StepChartViewer` draws a profile written
  `value(length),...` (a gradient profile) as a step chart along the distance, rising steps
  green and falling ones red, with `rd:unit` and `rd:lengthUnit`. `rd:LineStopsViewer` lists
  the operational points at the ends of the sections of its `rd:table` row (columns `from`,
  `to`, `start-km`, `start-offset`, `end-km`, `end-offset`) in km order, each once. `rd:RouteDiagramViewer` draws a section
  from its `start` to its `end` operational point with their km and its `length`.
  `rd:AlongLineViewer` draws a section as a strip between the km of its `rd:table` row
  (`startKm`, `startOffset`, `endKm`, `endOffset`), with the values of each `rd:markers` row
  at their km: a stretch (`start-km`... `end-offset` columns) as a band, a point (`km`,
  `offset`) as a tick, `km` with `length` in metres as a short band; what lies outside the
  section is left out, and at most 500 markers are drawn. `rd:LineProfileViewer` draws a
  line along km with a lane per `rd:lanes` column of its `rd:table` sections: numbers as
  steps (the highest of a section), other values as coloured bands (the most common), the
  operational points at the sections' ends as ticks; a section is placed by `start-km`
  (+ `start-offset`) and `end-km` or `length` (in km, unlike a marker's metres), sections
  without a km or with a span that is not a finite number of km are left out (the
  page says so), and `rd:profiles` lanes (gradient profiles) are not drawn yet.
  `rd:LinePositionViewer` shows where a kilometric post (the page's resource, found in its
  `rd:posts` row by its `km` column) is on its line: a strip over the km of the line's
  sections (`rd:table`) and posts, with the operational points as stops, the other posts as
  ticks and this one marked, then the stops on either side and how far they are.
  `rd:PlatformViewer` sketches a platform edge side-on: its `height` (a number of mm, or a
  concept labelled so) to scale against the TSI heights 550 and 760 mm, its usable `length`
  (m, not to scale) and whether it is `curved`; a height in words is written as it is.
  `rd:CrossSectionViewer` draws a track end-on, not to scale: the `gauge`, the gauging
  profile (`loadingGauge`), the contact wire from `wireMin` to `wireMax` (m) and the rail
  `inclination`. `rd:TrackLayoutViewer` draws an operational point's tracks (the row, by its
  `id` column without the start all ids share, "2" before "10") as bars with their platform
  edges (the `rd:platforms` row: `track`, `length`, `height`) and the sidings below (the
  `rd:sidings` row: `length`), platforms and sidings to their length against the longest.
  `rd:ConnectionsViewer` draws the sections that start or end at the resource (columns
  `start`, `end`, `line`, `length` in km, `wkt`) as spokes to the operational point at their
  other end: with the resource's and every section's geometry, each spoke points where its
  section goes, north up; else they are spread evenly. A table lists them clockwise from
  north. `rd:TopologyViewer` draws the net relations of the row (columns `a`, `b`,
  `navigability`, `origin-a`, `origin-b`) as a graph: the page's element marked, the others
  by how many relations away they are, arrows the way a train can pass (from the
  navigability concept's local name `Both`, `AB`, `BA`; `None` dashed), the meeting ends in
  each connection's tooltip. With the geometries of the `rd:elements` row (`wkt`, `length`)
  each element sits where its middle lies, north up; else in rings around the page's element
  (at most 200 elements and 400 relations). `rd:PositionedViewer` draws what is positioned on
  a linear element (columns `type`, `on`, `offset` in m): the points on this element as ticks
  at their offset, a lane per type (its most specific one), over the page's `length` row (else
  the furthest offset), then everything by type, with what runs over the element.
  `rd:ChangesViewer` puts parameters that apply from or until a date (columns `parameter`,
  `value`, `begin`, `end`; `xsd:date` or `dateTime`) on a time axis from today (or the first
  begin) to a year on (or the last end), with today marked and past, current and future
  periods apart. `rd:SymbolViewer` puts a small symbol beside each value by `rd:symbol`:
  `loading-gauge` (the default), `track-gauge`, `country` (a flag, from an EU authority
  country IRI), `platform-height`, `direction` (against the kilometres) and `energy-supply`
  (OpenRailwayMap's colour of "AC 25kV-50Hz", "DC 3kV"...). When the resource marks parameters
  `era:notApplicable` or `era:notYetAvailable` (RINF), they are one *Data status* row
  (`rd:DataStatusViewer`, `params` `notApplicable` and `notYetAvailable`), last on the
  overview, two folded lists by name, instead of two rows of links. rinf-describe's
  `rd:GraphViewer` (a neighbourhood graph read from ERA's endpoint) is not drawn: the page
  lists the links in and out. `rd:ParameterIndexViewer` lists every RINF parameter of the
  resource: the properties with an `era:rinfIndex` in the dataset (ERA's ontology loaded with
  the data), also those of the resources it links to that are not infrastructure elements
  (its contact line system, ETCS...; up to 100 of them, not itself), and those it marks not
  applicable or not yet available. The table lists the first 1,000 by index and is then
  `tableTruncated`.
  They are ordered by index under the resource's index root (`rd:indexRoot`, else the root
  most of them share), under the two innermost chapters of the index (the labels of the
  properties with that index), with the unit of `era:unitOfMeasure`. In the view they are the
  row's `values`, with a table of columns `index`, `value`, `on` and `status`, and hidden
  `unit`, `chapter` and `section`. A map row's `rd:context` (the section of line a track is part
  of) is drawn on the page's map, muted, under the resource's own geometries, also when it has
  none (`context` in the view); the map row itself is that map, not a row. A widget drawn here
  with nothing to draw (an along-the-line strip without the section's kilometres, a line
  profile whose sections have none), on a row with nothing else, is left out of the page, as
  rinf-describe leaves it out. Rows of
  one property with different `rd:class` (tracks, sidings) each keep their own row. A widget gets the rows it names in `rd:table`, `rd:markers`, `rd:posts`,
  `rd:platforms`, `rd:sidings` and `rd:elements` as `rows` in the view (by key; hidden ones
  are read for it and given whole, a row the page also shows is given as `{"key": ...,
  "shown": true}`, and every row of a shape has its `key`), and its literal `rd:` settings as `settings`. Table
  columns with `rd:hidden` or `dash:hidden` are read for widgets (up to 20, apart from the
  20 shown ones), marked `hidden` in the view and left off the page: hiding is presentation,
  and the view JSON still has them (access control is for row rules and graph grants). A drawn table still says when its cells were not all read, and very
  large or small numbers on a scale bar are in exponent form. Other widgets fall back to
  their values, table and named paths. With rinf-describe's `shapes/` on its Luxembourg sample,
  every one of its 237 pages has the groups and rows rinf-describe shows, in the same order;
  `RINF_DESCRIBE_DIR` runs a check of an operational point and a section of line in
  `tests/describe_pages.rs`.
- **Tables.** A row with `sh:node` shows its values as a table: one line per value (up to
  100), one column per property shape of that node shape (in `sh:order`, up to 20, without
  hidden ones, with `rd:unit` in the header), up to 10 values per cell, all read within
  100,000 triples. `?format=view` gives `columns` and `cells` (per value, per column); when the budget
  runs out, `tableTruncated` is set and the page says an empty cell may not have been read.
- **Without shapes.** The resource's other properties are rows too: under *Attributes* when
  every value is a literal, under *Links* otherwise.
- **Values.** Links show their label (`tts:labelPath` of the shapes, then `rdfs:label`,
  `skos:prefLabel`, `dcterms:title`, `schema:name`, `foaf:name`, in the browser's language),
  literals their datatype or language, a blank node its own rows. A row shows 100 values and
  counts the rest; the first 100,000 triples of the resource are read (the page says when there
  are more). `URIViewer` links to the IRI itself only for `http(s)` IRIs.
- **Map.** The resource's geometries (WKT or GML literals of its own, of its blank nodes, and
  of the geometries it links to by `geo:hasGeometry` or `geo:hasDefaultGeometry`, up to 20,
  not those of a property its shape hides) are drawn as an SVG at the top of the page, in the
  first geometry's CRS, with a link to OpenStreetMap at their centre and extent for
  longitude/latitude coordinates. Geometries of many points are thinned to about 50,000
  points, keeping their shape; a short ring or line keeps 4 points at least, so it stays
  visible next to a long one. With the web UI on (not `--no-ui`), the page also loads
  `/$/ui/describe-map.js`, which shows the geometries on an interactive map with MapLibre
  GL JS 6.16.0 (served by the server at `/$/ui/vendor/maplibre/`, BSD-3-Clause) and hides
  the drawing. The map gets every geometry put in longitude/latitude (a CRS that cannot be
  leaves the drawing only), thinned to about 20,000 points. Its base map, and the Map
  page's, is OpenStreetMap's standard tiles by default, with their credit. `--map-tiles`
  (`TRIPLETRAIL_MAP_TILES`) names other XYZ raster tiles, with `--map-attribution` (HTML) for
  their credit, and `--map-tiles none` draws no base map, so the pages fetch nothing from
  elsewhere. `--map-basemap 'TITLE|URL|ATTRIBUTION'` (repeatable; `TRIPLETRAIL_MAP_BASEMAPS`,
  one per line) adds base maps the Map page's **Base map** menu offers besides it, such as
  aerial photos; the describe pages keep `--map-tiles`. A fourth field,
  `TITLE|URL|ATTRIBUTION|MAXZOOM`, gives the last zoom (0 to 24, default 19) the provider has
  tiles for: past it the map enlarges that zoom's tiles instead of asking for tiles that are
  not there. The attribution is everything after the URL, so it may hold a `|`, and only a
  last field of one or two digits is taken as the zoom (a year at the end of a credit stays
  the credit's). Each viewer's browser fetches the tiles of the place it looks at, so the tile
  server learns where the viewed resources lie: check its usage policy (OpenStreetMap's
  allows light use, not heavy or commercial traffic), and use your own tile server or `none`
  for private data. The page sends only its origin as `Referer` (`strict-origin`), as tile
  servers ask, never the resource's IRI. Without the web UI or script, the drawing stays.
- **Incoming links.** Every link to the resource, per property, with up to 20 of the resources
  they come from (the first 10,000 links are read), on the page's Incoming links tab.
- **JSON.** `?format=view` gives the view model (label, classes, groups of rows, and in
  `incoming` the links to the resource that no row shows), for a page that draws it itself.
  `reader=forms` reads the shapes for the forms reader (`tts:Reader-Forms`), as the web UI's
  **Records** page (`/$/ui/records/{ds}`) does; `reader=describe` is the default, and another
  reader a 400.

The page has no `ETag`: it also depends on the shapes, which can change without a write to the
data.

## Search by name

`GET /{ds}/search/labels?q=…` finds resources by their labels, for a search box: those of the
classes the `search` reader's shapes offer (`/$/datasets/{ds}/shapes?reader=search`; every
class with instances when the dataset has no shapes of its own) with a label holding every word
of `q`, the last as a prefix so it works while typing, case and Latin accents aside, from the
[full-text index](full-text-search.md). Labels with the fewest other words come first, then by
score.

- `class` limits the search to one class or several (comma-separated), by the REST API's
  dataset id (`Station`) or by IRI; an unknown one is a 404. Without it, a class gives at most
  5 results, so that one class with many alike labels leaves room for the others.
- `limit` is 20 by default and at most 100.
- Each result has `iri`, `label`, `class` and `classTitle` (the class's id and title), and the
  links `describe` (its [describe page](#describe-pages)) and `record` (its REST API record).
  The answer also lists the `classes` that can be searched.
- A caller limited to some graphs or rows finds only what it can read.

The web UI has the same search in its top bar: type a name (or press `/` to get there), pick
a result with the arrow keys and Enter, and its describe page opens. It searches the dataset
the page shows, else the one picked next to the box. In a window under 1280 px wide the box is a
magnifier icon that opens it.

### Similar resources

`GET /{ds}/search/similar?property=…&vector=[…]` (or `&iri=…`), or the same parameters as a
`POST` form for a long vector, gives the `k` resources
nearest a vector, with their scores and descriptions in JSON-LD, for retrieval (GraphRAG): see
[vector search](vector-search.md#retrieval-for-graphrag).

The Search page's **Find similar** card asks it with a text or an IRI. It lists the
properties with `tt:vector` values from the dataset's profile, and shows only when there are
some. A text is turned into a vector in the browser by the user's AI service (the one set up
for the Workbench's AI tab), whose OpenAI-compatible `POST {base URL}/embeddings` gets the text,
the embedding model and the key; the server gets only the vector. The model is kept per service
in the browser's `localStorage` (`tripletrail-ui_embedding-models`).

### The search builder

The web UI's **Search** page (`/$/ui/search/{ds}`) builds a search from a class and conditions
on its properties ("Find Section where maxSpeed > 120 and from is Brussels-South"), shows the
results with a link to each describe page, keeps the search in its link, and opens its SPARQL
on the Query page. Three endpoints serve it, and programs; they read the same `search`
reader's shapes, and a limited caller sees only what it can read.

- `GET /{ds}/search/classes` lists the classes to search: `id` (the REST API's dataset id),
  `title`, `class` (IRI), `count` of instances, `geometry`, and `properties`, each with `key`,
  `title`, `path` (SPARQL), `single`, and `kind`: `text`, `number`, `boolean`, `date`,
  `reference` (with the `target` class's id), `iri`, `object` or `geometry`, and `required`
  (`sh:minCount` 1 or more). When the caller may write the dataset (#82), a class also has
  `writable` and `rest`, the REST API's dataset its records are written to, and each property
  `writable` (its path is one predicate or the inverse of one there); `iriTemplate` is the
  class's `tts:iriTemplate`. The **Records** page edits a record through these: `+ New` and
  `Edit` only for a `writable` class, a field for each `writable` key, checked with
  `?dry=true` on the REST API's writes and saved with `If-Match`.
- `GET /{ds}/search/results?m=…` runs a search, given as JSON:
  `{"class": "Section", "where": [{"key": "maxSpeed", "op": "gt", "value": 120}], "show": ["lengthKm"]}`.
  At most 20 conditions, all of which must hold, and 20 extra columns. A condition can be
  a group `{"any": [condition, …]}` (1 to 10 conditions, not nested) of which at least one
  must hold. The operators:
  - `exists`, `missing`: the property has a value, or none;
  - `eq`, `ne`, `lt`, `le`, `gt`, `ge` with a number, and `between` with `"10..20"` (both
    ends included);
  - `contains`, `starts`: in the value's text, case aside; `is`: the text is exactly
    `value`, or, for a reference or IRI, the value is the IRI `value`;
  - `before`, `after`, `on`: dates and times, compared as text, `on` by prefix (`2026-11`);
  - `true`, `false`: booleans.

  `"area": [minLon, minLat, maxLon, maxLat]` (or the same as text, `"4.3,50.8,4.4,50.9"`, in
  CRS84) keeps the resources whose geometry (the class's first `tts:geometry` way, as the map
  draws it) meets that box, with `geof:sfIntersects`; a box that crosses the antimeridian
  (`minLon > maxLon`) is two. A bad box is a 400, and a class without a geometry a 422. The
  Map page's **Search this area** (in a layer's Filter panel) opens the Search page with the
  area the map shows, and value suggestions keep to it too.

  The answer has `count` (all matches), `more`, `columns` (the conditions' properties, then
  the shown ones: `key`, `title`, `kind`), `rows` and the `sparql` it ran. The rows (`limit`:
  100 by default, at most 1,000) come in label order, each with `iri`, `label`, `describe` and
  `values` by key: a number, boolean or text, or for a resource `{iri, describe, label}`. A
  condition's property with several values shows the smallest that meets it (the SPARQL
  `MIN`), so the same data always gives the same answer. A shown property that may have
  several values (no `sh:maxCount 1`) is an array of them, the first 10 (resources in label
  order, other values in their own), and the row's `moreValues` says how many more there are
  by key (`{"code": 2}`); they are read by a query
  of their own for the rows answered, so columns do not multiply the search's rows, and
  `sparql` leaves them out. `/search/sparql` has them joined (`GROUP_CONCAT`) instead. An unknown class is a 404; an unknown key or operator, or a
  value that does not fit, a 422.

  For the Records page (#81) a search also takes:
  - `"sort": "lengthKm"` or `{"key": "lengthKm", "desc": true}`: the rows in the order of that
    key's value (numbers as numbers, other values by their text, case aside; records without
    a value last), then by label. The key is a condition's or a shown one, and has at most one
    value (`sh:maxCount 1`), else a 422; `label` is the default.
  - `"q": "text"`: the records whose label (or IRI, when they have none) contains the text,
    case aside.
  - the `offset` parameter: the rows after the first `offset` (0 to 1,000,000), and the answer
    says `offset`.
  - `f=csv`: the rows as CSV (`text/csv`, as a download named after the class): `iri`,
    `label`, then a column per key, a resource as its IRI and several values joined with
    ` | `. `limit` is then at most 10,000. A cell that a spreadsheet would run as a formula
    (starting with `=`, `+`, `-`, `@`, a tab or a carriage return, not a number) gets a `'` before it. Another `f` is a
    400.
- `GET /{ds}/search/sparql?m=…` gives the SPARQL of a search, as `application/sparql-query`.
- `GET /{ds}/search/values?m=…&key=…` suggests values for a condition: the values property
  `key` takes among the resources the search finds, most frequent first (then by value),
  each with `value` (its text, or the IRI with `"iri": true` and a reference's `label`) and
  `count`, the resources that have it. `at` leaves out the condition at that index of `where`
  (the one being edited); `q` keeps the values whose text, or label, contains it, case aside;
  `limit` is 20 by default and at most 100, and `more` says whether there are more. A
  geometry or embedded object has no values to list (422).

## OGC API – Features

`/{ds}/ogc` serves a dataset as [OGC API – Features](https://ogcapi.ogc.org/features/) Part 1
(conformance classes Core, GeoJSON, HTML and OpenAPI 3.0), Part 2 (CRS) and Part 3
(filtering with CQL2), so QGIS, ArcGIS, Leaflet or OpenLayers can
load its features without SPARQL.

```sh
curl http://localhost:7878/kg/ogc/collections
curl 'http://localhost:7878/kg/ogc/collections/Station/items?bbox=5.5,58.2,6,58.6&limit=100'
```

- **Collections** are the node shapes the `ogc-features` reader reads
  (`/$/datasets/{ds}/shapes?reader=ogc-features`): each one with an `sh:targetClass` and an
  `tts:geometry` way. Its id is the shape's `tts:key`, its title `sh:name`. Generated shapes
  give one per class with a geometry; a dataset's own shapes choose the classes, their ids and
  properties, and hide one with `tts:hiddenFrom tts:Reader-OgcFeatures`.
- **Features** are the class's instances. The geometry is the first way's (by `sh:order`)
  value as GeoJSON in CRS84; an instance with no value on it takes the next way's, and so on
  (#259). When the collections are made (again after each write), each later way is narrowed
  to the instances the ways before it leave without a geometry, when there are at most 1,000
  (and dropped when none of them has a value on it), so that a tile does not look for every
  instance on it.
- **A line between two features.** A geometry way with `tts:lineBetween ( A B )` in place of
  `sh:path` (two SHACL paths, each to a geometry literal) gives the straight line from the
  geometry at the end of `A` to the one at the end of `B` (`st:ST_MakeLine`), such as a
  section of line between its two operational points. The map, OGC API Features, the REST
  API and search take it for the instances that have no value on any way with
  `sh:path`, and filter on it as on any geometry. Such lines have no spatial index: the
  server reads the box of every line once per store generation (and per caller's access), in
  the background, and a tile, `bbox` or search area builds only the lines whose box meets it.
  Until the boxes of the store's latest write are read (a second per 100,000 lines), and on a
  store written to without pause, a window builds every line. The boxes kept take at most
  128 MB. A window that more
  than 10,000 boxes meet (a map of a whole network at a low zoom) builds every line, about a
  second per 100,000 on a laptop; give such a class an `tts:minZoom`. A feature with several values there gets the most detailed
  one (most coordinates, then most digits), as data often holds a rounded and an exact copy;
  `geometries=all` gives a `GeometryCollection` of them all instead; the properties are the values of the shape's property
  shapes under their `tts:key`, numbers and booleans typed, IRIs as strings, several values as
  an array. The feature id is its IRI; `…/items/{id}` takes it percent-encoded. A dataset with
  one base IRI (`--base-iri`) gives the features under it their IRI's local part as id
  (`https://data.example.org/s1` is `s1`, at `…/items/s1`); the whole IRI still finds them, and
  a feature outside the base, or whose local part is an IRI itself, keeps its IRI.
- **Items** take `limit` (10 by default, at most 10,000), `offset` and `bbox` (four numbers,
  or six with heights, in CRS84; `minx > maxx` crosses the antimeridian), and give a `next`
  link while there are more. A page holds `limit` distinct features; with `bbox`, a feature
  is in it when any of its geometries intersects the box, and comes once. They also take the
  map's spatial filter, `area`, `rel` and `dist` ([vector tiles](#vector-tiles)), and with
  `area=layer:{id}` that collection's own filters (`{id}.{key}`). Other parameters
  are refused with `400`. Features come in the order
  the store finds them (no sort, so the first page of a large class is fast), which stays the
  same while the dataset does not change: a write between two pages can shift them.
- **Property filters.** A property with an `tts:facet` in the shapes (generated shapes give
  one to properties with a few values or a range) filters the items by its key:
  `…/items?tracks=2&open=true` keeps the features with those values, compared as text (an
  IRI by its full form). Several filters must all hold. The OpenAPI definition lists them per
  collection; a property without a facet is refused as an unknown parameter. A key that is
  also a parameter of the items (`limit`, `offset`, `bbox`, `bbox-crs`, `crs`, `datetime`,
  `geometries`, `filter`, `filter-lang`, `filter-crs`, `area`, `rel`, `dist`, `f`, `timeout`) is never a filter:
  give the property another `tts:key`.
- **CQL2** (Part 3). `filter` takes a CQL2 expression over the collection's queryables,
  `…/collections/{id}/queryables` (a JSON Schema): every property of the collection, typed
  from its `sh:datatype`, and `geometry`. `filter-lang` is `cql2-text` (the default) or
  `cql2-json`, and `filter-crs` the CRS of the filter's geometries (as `bbox-crs`).

  ```text
  …/items?filter=tracks > 1 AND name LIKE 'Sta%'
  …/items?filter=S_INTERSECTS(geometry, BBOX(5.5, 58.2, 6, 58.6)) AND opened >= DATE('2020-01-01')
  ```

  Read: comparisons (`=`, `<>`, `<`, `>`, `<=`, `>=`, of a property with a value or another
  property), `AND`, `OR`, `NOT`, `IS [NOT] NULL`, `[NOT] LIKE` (`%`, `_`), `[NOT] BETWEEN`,
  `[NOT] IN`, `CASEI`, `TIMESTAMP`, `DATE`, and the spatial functions `S_INTERSECTS`,
  `S_EQUALS`, `S_DISJOINT`, `S_TOUCHES`, `S_WITHIN`, `S_OVERLAPS`, `S_CROSSES` and
  `S_CONTAINS` with WKT, `BBOX` or (in JSON) GeoJSON, and the temporal functions `T_AFTER`,
  `T_BEFORE`, `T_CONTAINS`, `T_DISJOINT`, `T_DURING`, `T_EQUALS`, `T_FINISHEDBY`,
  `T_FINISHES`, `T_INTERSECTS`, `T_MEETS`, `T_METBY`, `T_OVERLAPPEDBY`, `T_OVERLAPS`,
  `T_STARTEDBY` and `T_STARTS` on instants (a property, `TIMESTAMP`, `DATE`) and
  `INTERVAL`s (`..` for an open end); as for `datetime`, a date covers its day and a value
  without a timezone is UTC. For `T_MEETS` and `T_METBY`, which ask whether two ends touch, a
  date ends at the next midnight, so `INTERVAL('2026-02-01', '2026-02-28')` meets 1 March. Not read, and refused with `400`: arithmetic, the array
  (`A_*`) functions, `ACCENTI`, and other functions. A property is compared the way the value asks: as text against a string, as a
  number against a number, as an `xsd:dateTime` against a `TIMESTAMP`, and by its date part
  against a `DATE`. A property `IN` a list of strings matches a value by its text, whether it is
  a string, a language-tagged string or an IRI; when the property's values come only in those
  forms, they are looked up as terms (each string in each form), so a list of a thousand
  strings stays fast, and otherwise the text of each value of the collection's features is
  compared. A property may have several values: a predicate holds when some of them
  make it true, `NOT` when none does, and `IS NULL` when it has none. An unknown property is
  refused with `400`, and so is a filter longer than 64 KiB, nested deeper than 64 levels,
  with more than 200 predicates, with more than 1,000 values in its `IN` lists, or with a
  geometry of more than 1,000 vertices.
- **JSON-FG.** With `Accept: application/vnd.ogc.fg+json` (before any other JSON type) or
  `f=jsonfg`, items and features are [JSON-FG](https://docs.ogc.org/is/21-045r1/21-045r1.html)
  1.0, which is GeoJSON with more members: `conformsTo` (the Core class, also listed in
  `/conformance`) and `featureType` (the collection id) on the document, and on each feature
  `time` from its `tts:time` values (`date`, a UTC `timestamp`, or a start and end as an
  `interval`, `..` for a missing end; `null` without one). With a `crs` other than CRS84,
  `place` holds the geometry in that CRS, `geometry` the CRS84 one, and the document's
  `coordRefSys` names the CRS; otherwise `place` is `null`.
- **Time.** `datetime` keeps the features whose time meets an instant or an interval
  (`2026-02-01`, `2026-01-01T00:00:00Z/2026-03-31`, `2026-04-01/..`, `../2025-12-31`; RFC 3339
  dates or date-times, a date covering its whole day). A feature's time comes from the
  property shapes marked with `tts:time`: `tts:Time-Instant` (it holds then), or
  `tts:Time-Start` and `tts:Time-End` (a missing start or end leaves that side open, so work
  without an end still holds). An instant meets the filter when it is in the interval, a
  start and end when the two overlap; a feature without any time is left out. Values and
  bounds without a timezone are read as UTC (RFC 3339 asks for one in `datetime`), and a date
  with a timezone covers its day in that timezone. Generated shapes mark no time, so `datetime` is ignored until
  the dataset's own shapes add `tts:time`, for example in a file of their own:

  ```turtle
  <urn:tripletrail:shape:http%3A%2F%2Fexample.com%2FWork:http%3A%2F%2Fexample.com%2Ffrom>
      tts:time tts:Time-Start .
  ```
- **Coordinate systems** (Part 2, CRS): `crs` gives the geometries of items and features in
  CRS84 (the default), EPSG:4326 (latitude first, as EPSG defines it) or EPSG:3857 (Web
  Mercator metres), named by their IRI (`http://www.opengis.net/def/crs/EPSG/0/3857`), and the
  answer says which in `Content-Crs`. `bbox-crs` does the same for `bbox`. The data is kept in
  CRS84 (`storageCrs`); other CRSs are refused with `400`.
- **Extents.** A collection gives `extent.spatial` (the box around its geometries in CRS84, each
  reprojected as the items are, so literals in different CRSs add up; empty geometries and
  ones in a CRS that cannot be converted are left out) and, with `tts:time`, `extent.temporal`
  (the earliest start or instant to the latest end or instant; open when a feature has a start
  and no end), so QGIS can zoom to a layer. They read every geometry of the class, so they are
  made in the background when the data or shapes change: the first answer waits up to 250 ms
  for them and goes without the ones not made yet, or with those of the last finished fill (an
  older data version) while the new ones are made, so a dataset under steady writes keeps its
  boxes. A fill for an older version stops when a newer one starts. The fill parses geometries
  without the geometry cache, so it does not grow memory. The HTML page of a collection shows
  them.
- **Conformance.** The OGC test suite (Team Engine, `ogccite/ets-ogcapi-features10`) passes
  with no failures (567 tests); it skips the checks of `numberMatched`, which is not given:
  counting every match of a large class would slow every page.
- **`/{ds}/ogc/api`** is an OpenAPI 3.0 definition with a path per collection, and
  `/{ds}/ogc/conformance` lists the conformance classes. Queries run under the dataset's
  limits and timeout, like any other read.
- **HTML.** Every resource is also a page: with `f=html`, or without `f` for a client whose
  `Accept` names HTML before any JSON type (a browser), so `http://localhost:7878/kg/ogc` can
  be browsed. A page is made from the JSON document of the same resource and links back to
  it (`f=json`). Items and features show a table of their properties and an SVG sketch of
  their geometries, each one a link to its feature. The JSON documents link their page as
  `rel=alternate`. `f=json` (or `geojson` for items and features) always gives JSON.

### Vector tiles

`/{ds}/tiles/{collection}/{z}/{x}/{y}.mvt` answers a Mapbox Vector Tile (Web Mercator, the
XYZ scheme of MapLibre, OpenLayers, Leaflet and QGIS's "Vector Tiles" source) with one layer,
named by the collection's id; `all` instead of a collection gives a layer per collection.

```text
https://tripletrail.example.org/kg/tiles/all/{z}/{x}/{y}.mvt
```

- The layers are the collections the `map` reader reads (`?reader=map` on the shapes), as for
  the OGC API but with the map's choices: a collection with `tts:minZoom` is left out of
  tiles below that zoom (generated shapes give 10 from 5,000 instances and 12 from 50,000).
- Each feature has one geometry (the most detailed, as in the items), clipped to the tile
  with a 64-unit buffer in a 4096 extent, and its IRI as the attribute `iri`. Below zoom 14,
  lines and polygons are simplified to one tile unit (Douglas–Peucker), so low zooms carry
  no detail they cannot show.
- Below zoom 8, a layer of more than 1,000 features in one tile is generalised, as RINF Atlas
  does (#92): a 512 extent (and an 8-unit buffer) instead of 4096, and one feature per
  position, the one drawn on top (the last by IRI). Lines and polygons smaller than a unit are
  left out. A layer of fewer features keeps them all at every zoom.
- Collections that read their geometry through the same path are fetched with one query
  (the spatial index is asked once per tile), and those queries run on up to four threads.
- Tiles are kept in memory, 256 MiB at most over all datasets (oldest out first; a layer tile
  over 4 MiB is not kept), per store generation and version of the dataset's shapes: the
  first write or shapes change after a tile was made gives a new one. The collections are
  kept the same way, so a cached tile is a lookup; after a write, the collections made
  before it are used while new ones are made in the background (making them profiles the
  data), so a class a write adds shows up a moment later. Tiles made meanwhile are not kept,
  and the extents follow the collections they were made for. A layer with more than 200,000 features
  in one tile is refused with `413`: zoom in, or set an `tts:minZoom`.
- An empty tile is `200` with an empty body.

`/{ds}/tiles/style.json` is a [MapLibre style](https://maplibre.org/maplibre-style-spec/) of the
dataset, the one the web UI's Map page draws:

- One vector source, `tripletrail`, of the `all` tiles (fetched up to zoom 18, deeper zooms
  enlarge those), and the `--map-tiles` base map under it when the server has one, or a plain
  background.
- Three layers per collection, `{id}-fill`, `{id}-line` and `{id}-point`, filtered by geometry
  type, in the collection's `tts:color` (generated shapes give each class one; others take the
  next of a palette of twelve), from its `tts:minZoom`. Areas are drawn first and points last,
  so nothing hides what is smaller. Within each, classes are drawn by their shape's
  `tts:drawOrder` (an integer, default 0, higher on top), then those with more instances
  first, so a few stations are not hidden under thousands of track sections.
  The legend lists them by their node shape's `sh:order`, then by title, and last those whose
  class the profile did not find in the data.
- `metadata."tripletrail:collections"` lists the collections for a legend: `id`, `title`,
  `description`, `class`, `color`, `minZoom`, `instances` (from the profile) and `time`
  (`true` when its shapes mark a `tts:time`, which `datetime` filters on).
- `metadata."tripletrail:dataVersion"` is the data version the map shows, for citing it: the
  store's write generation, and with the dataset's own shapes `-` and their version. A write or
  a shapes change gives a new one. Stores that keep no generation leave it out.
- `metadata."tripletrail:views"`, when the shapes have map views (`tts:MapView` not
  `sh:deactivated`), lists them by `sh:order`, then by title: `key`, `title`, `description`,
  the `collections` each shows, and the `filters` (`{id: {key: [values] | {"min", "max"}}}`)
  and `colors` (`{id: key}`) it sets, as the tiles take them, and the `facets` its filter
  panel lists first (`{id: [key]}`, also of a collection it hides, for when it is shown). A filter or colour on a facet a
  collection lacks is left out for it, and a view shows only the collections the caller
  sees: one that shows none is not listed. The Map page offers them in a **View** list above
  its layers and keeps the one picked in its link (`view={key}`).
- `metadata."tripletrail:basemaps"` lists the base maps the Map page lets viewers switch
  between: the server's `--map-tiles` first (`key` `map`, titled OpenStreetMap for its tiles,
  else Base map), then each `--map-basemap` (`basemap-1`, `basemap-2`…), with `key`, `title`,
  `tiles`, `attribution` and `maxzoom` (the last zoom of its tiles: 19 for `--map-tiles`, the
  base map's own for the others); empty without any.

The Map page draws this style with buttons of its own: an eye per layer to show or hide it
(and **Show all**/**Hide all**), a magnifier on a layer that is not drawn at this zoom, which
zooms in to its `minZoom`, and a funnel that opens its filters, with the number of filters
on. On the map, under the zoom buttons: **Zoom to all the data** (the collections' extents
from the OGC API), the **Base map** menu (the base maps above, the first also in grey, and
none), **Tools**, **Time**, **Export**, **Cite this map** and **Fullscreen**, which takes the
layer list and the detail panel along. **Time** (`/$/ui/map-share.js`, #37), shown when a
collection has a `tts:time`, sets `datetime` from two sliders over the span of the
collections' temporal extents (an open end up to today), or from dates typed; it goes into the
page's link and the tiles', facets' and OGC API links' requests. **Export** downloads the map as
a PNG with a title, a scale bar, a north arrow, the legend of the layers shown and a credit line
(the base map's attribution, the dataset, its data version, the time and the day). **Cite this
map** gives a text with the dataset, its data version, the day, the page's link, the time, and
per layer shown its OGC API items and the SPARQL of its features under the filters (`query` of
its facet counts).
**Tools** (`/$/ui/map-tools.js`, #33) draws an area every layer is filtered by (a rectangle
or polygon as a `POLYGON`, a circle as its centre with `rel=dwithin` and its radius as
`dist`, a corridor as a `LINESTRING` with `rel=dwithin`, or the geometry of the feature open
in the detail panel by its IRI, with **Filter by it**, or the features of another layer as its
filters keep them, `layer:{id}`), with a relation picked from those of
`rel`; the area goes into the page's link and into the tiles', facets' and OGC API links'
requests as `area`, `rel` and `dist` ([vector tiles](#vector-tiles)). It also
measures lengths (each leg's length and bearing) and areas with perimeter, on the sphere, and
shows or copies the coordinates under the pointer, in CRS84 and in an EPSG code the viewer
types (transformed by `geof:transform` through `/{ds}/sparql`, with the page's headers). The feature
open in the detail panel is outlined: an area with a light fill and an edge, a line with a
light band and an edge on both sides, a point with a ring. The base map picked goes in the
page's link (`basemap={key}`, left out for the default) and is remembered in the browser for
links without one. With `--map-tiles none` the page starts without a base map, and draws one
only when a viewer picks it.

#### Previews of proposed shapes

`POST /{ds}/tiles/preview` (#146) draws the dataset with shape files that are not its own yet,
for the Workbench's **Preview on map**. The body is one Turtle file (`text/turtle`) or JSON
`{"files":[{"name":"stations.ttl","text":"…"}]}`; the files take the place of the dataset's own
shapes, over its packs and the generated ones, as a promote would. It needs read access only,
and answers `201` with `{"id": …, "style": "…/tiles/preview/{id}/style.json"}`:

- `/{ds}/tiles/preview/{id}/` serves `style.json`, tiles, `search` and `facets` as above, with
  those shapes, and `/{ds}/describe?iri=…&preview={id}` a describe page. `preview={id}` on
  `/{ds}/search/…` and on reads of `/{ds}/api/…` (OpenAPI definition included) answers with
  the preview's shapes too; a write with it is `400`. The UI's Search page and the REST API's
  docs page take `?preview={id}` and pass it on. Rows the caller may not read are left out as in the dataset's own tiles.
- Files that do not load are `422` with what is wrong, a body over 4 MiB is `413`, and an
  unknown or expired id is `404`.
- The id is 128 random bits. A preview is kept in memory for an hour, 64 at most (the oldest
  go first), and belongs to its dataset.

Facets (#139) count and filter a collection's features by its property shapes with an
`tts:facet`: `tts:Facet-Values` by values (generated shapes give it to code lists),
`tts:Facet-Range` by a range (numbers and dates) and `tts:Facet-Text` by text the values
contain (generated for other plain and language-tagged strings, such as names):

- `/{ds}/tiles/facets/{collection}` answers the number of features the filters keep
  (`features`; only those with a geometry, which the map draws and the OGC API serves) and per facet its 50 most frequent values (`value`, the IRI or text; `label`,
  from `rdfs:label` or `skos:prefLabel`; `count`), with `more` when it has others. A facet's
  counts are under the filters of the other facets only, so they show what choosing another of
  its values gives. A range facet instead has `range: true` and the `min` and `max` of its
  values, with `count`, the features that have one, `unit` (the symbol of its property
  shape's `qudt:hasUnit`, such as `km/h`) and `classes` (its `tts:colorClasses`: `upTo` and
  `label` each, low to high, the last `upTo` null) when it has them. With `color={key}`
  (`color.{collection}={key}`) naming a range facet, as the map's *Colour by* sends it, its
  `classes` are those the map colours by (its `tts:colorClasses` when they are 2 to 8, else up
  to 8 of one round width over `min` to `max`) and each has a `count`: the features drawn in
  that class under all the filters, its own too, each by its highest number, as the tiles
  colour it (#92). A text facet has `text: true` and no
  counts: it is searched, `{key}.text=…` keeping the features with a value that contains the
  text, whatever its case. `query` is a SPARQL query of the same features, each once with one
  of its geometries (`?f ?geometry`, at most 1,000 features), which the Map page opens in the
  query page. A computed facet (`tts:select`) whose query fails, such as one too slow for the
  data, comes with no values and its `error`, and the others still answer. Its queries, and
every count and tile filtered or coloured by it, get at most 10 seconds (less under a shorter `--timeout`), and a timeout is remembered per caller,
filter and data version: until the data or the shapes change, the same request answers the
same error at once. Other failures (a memory limit) are tried again on the next request. Facets are counted
  in parallel, a query each; values tied at the 50th place come in the store's order. On RINF's DE+FR running
  tracks (234,811, 44 facets) this takes about 0.5 s unfiltered and 0.8 to 1.7 s under a
  filter, and repeats come from the result cache in milliseconds.
- `/{ds}/tiles/search?q=…` finds the map's features by name, with the
  [full-text index](full-text-search.md): those with a label holding every word of `q` (the
  last as a prefix, so it works while typing; case and Latin accents aside), labels with the
  fewest other words first, then by score, at most 5 of a class and 20 in all. Each result has
  `iri`, `label`, `collection` and `bbox` (CRS84, around its geometries). The first search
  builds the index (about 4 s on RINF DE+FR); a search then takes milliseconds. As
  `tt:search`, a caller limited to some graphs or rows searches only what it can read.
- `color.{collection}={key}` on the `all` tiles (`color={key}` on a collection's own) gives
  each feature of the collection the value of that values facet as the property `value` (the
  least as text, when it has several), or of a range facet its highest number, for a map
  coloured by it. A key that is no values or range facet of the collection is left out, as
  filters on such keys are.
- Filters are query parameters, `{key}=value` on the facets and on a collection's own tiles,
  and `{collection}.{key}=value` on the `all` tiles. Values of one key are alternatives (up to
  1,000), keys must all hold, and values compare with the property's values as text, as the OGC
  API's property filters. A range takes `{key}.min` and `{key}.max` (either may be left out),
  read as the facet's `sh:datatype` (numbers, or dates and times), and keeps the features with
  a value within it. A filtered collection's tiles are made and kept apart.
- `/{ds}/tiles/facets?collection=A&collection=B` counts several collections in one request
  (#92, up to 64): `collections` holds each one's answer, as `/{ds}/tiles/facets/{id}` gives
  it; `features` their sum; and `shared` the facets whose key more than one of them has,
  with `collections` naming them. A shared values facet adds up the counts of each value (its
  50 most frequent kept, `more` when any collection had more; then the sums are lower bounds,
  as each collection counts only its own 50 most frequent values), a range takes the lowest
  `min`, the highest `max` and the sum of `count` (numbers, or dates and times compared as
  text), and a text facet is listed as such. A facet of different kinds in different
  collections, a range with different units, or with numbers in one and dates in another, is
  not shared. The request has one timeout for all its collections. Filters work as on one collection: a
  bare `{key}=value` on each collection that has the facet, `{collection}.{key}=value` on that
  one only. A feature of two of the classes is counted in each.
- Three filters apply to every collection, on the facets and on the tiles alike (#92), with
  the same names on `all` tiles: `bbox=minLon,minLat,maxLon,maxLat` (CRS84) keeps the features
  with a geometry that meets the box, read through the spatial index as the OGC API's `bbox`;
  `graph=IRI`, repeated for up to 100 graphs, the features with triples in one of those named
  graphs (under a union default graph, or a caller's graphs); `q=words`, the features with a
  literal in the [full-text index](full-text-search.md) that holds every word (up to 8, the
  last as a prefix; case and Latin accents aside), as `/{ds}/tiles/search` matches them. A
  bad box or graph IRI is `400`. A facet named `bbox`, `graph` or `q` is no longer filtered by
  that bare name: use `{collection}.{key}`.
- `datetime` (#37) applies the same way: an instant or `start/end` (`..` for an open end),
  read as the [OGC API's `datetime`](#ogc-api--features), keeps the features whose `tts:time`
  meets it. A collection whose shapes mark no time keeps all of its features; a bad value is
  `400` on every collection. A facet named `datetime` is filtered as `{collection}.datetime`.
- A spatial filter (#33) applies the same way, and to the [OGC API's items](#ogc-api--features):
  - `area` is a geometry. It is WKT, in CRS84 unless it names its CRS first as a
    `geo:wktLiteral` does, of at most 200,000 bytes. It can also be the IRI of a resource whose
    geometry (`geo:asWKT`, or through `geo:hasGeometry`; several are taken together) is the
    area, read as the caller sees the data. Or it is `layer:{id}`, "select by location": the
    features of collection `{id}` with a geometry, as that collection's own filters keep them
    (`{id}.{key}=…`, `.min`, `.max`, `.text`; never the bare ones), each feature taken
    against every one of them but itself. The two geometries meet in a spatial join, worked
out once per request: a tile joins its own features with all of the other layer's, and
`disjoint` takes the matching features away all at once. The other layer's filters still
run over that whole layer for each tile (about 130 ms a tile with 5,000 areas and 60,000
points), so a large layer is best narrowed by its filters. The
    OGC API's items take that collection's `{id}.{key}` filters too; an unknown collection is
    `400`.
  - `rel` is one of `intersects` (the default), `within`, `contains`, `touches`, `crosses`,
    `overlaps`, `disjoint` or `dwithin`. Each is judged as `geof:sf…` judges it, between one
    of the feature's geometries and the area. `disjoint` keeps the features with a geometry,
    none of which meets the area.
  - `dist` is in metres, from 0 to 1,000,000, with `rel=dwithin` only. It keeps the features
    with a geometry at most that far from the area, as `geof:distance` measures it.

  The candidates come from the spatial index: the area's box, widened by `dist`. The exact
  test runs on those only. A bad `area` (not WKT, or an IRI without a geometry), `rel` or
  `dist`, or a `rel` or `dist` without an `area`, is `400`. The tile and facet caches key on
  the filter. A facet named `area`, `rel` or `dist` is filtered only as `{collection}.{key}`
  on the tiles and facets, and is not a property filter of the OGC API's items (which have no
  `{collection}.{key}` form): give such a property another `tts:key` to filter the items by
  it. `dwithin` does not reach across the antimeridian: the widened box stops at ±180°.

## REST API (JSON-LD)

`/{ds}/api` is a REST API made from the dataset's shapes (#143): the node shapes the
`rest-api` reader reads (`/$/datasets/{ds}/shapes?reader=rest-api`), which are the generated
ones until the dataset has [its own](#a-datasets-own-shapes). Developers get JSON records and
an OpenAPI definition without writing SPARQL, and the JSON is still RDF.

```sh
curl http://localhost:7878/kg/api/v1
curl 'http://localhost:7878/kg/api/v1/OperationalPoint?country=BE&limit=50'
curl 'http://localhost:7878/kg/api/v1/OperationalPoint/http%3A%2F%2Fexample.com%2Fop1'
curl http://localhost:7878/kg/api/openapi.json
```

- **Datasets.** `/{ds}/api/v1` lists one dataset per node shape with an `sh:targetClass`: its
  `id` (the shape's `tts:key`, else the class's local name in kebab case), title, class, URL
  and, for generated shapes, `count`. Generated shapes give every class and property an
  `tts:key`, its local name as written (`OperationalPoint`, `hasTrackCount`); the kebab-case
  and camel-case names below are for shapes of the dataset's own that set no key.
- **Records.** `GET /{ds}/api/v1/{dataset}` gives a page of the class's instances (IRIs only,
  in the store's order), `GET /{ds}/api/v1/{dataset}/{id}` one of them; `id` is the
  percent-encoded IRI, its slashes too. A record has `id`, `type` (the class) and a key per property shape: its
  `tts:key`, else the predicate's local name without a leading `has` or `is`, in lower camel
  case (`id`, `type`, `truncated` and the list parameters get a `_` after them). With `sh:maxCount 1` a key
  has one value, otherwise a list. Numbers and booleans are JSON's own; an IRI whose
  `sh:class` is another dataset's class is a reference `{id, type, label}` (the label from that
  dataset's `tts:labelPath`); `rdf:langString` values are a language map (`{"en": …, "fr": …}`).
  A key whose values have a node shape (its `sh:node`, else the node shape of its
  `sh:class`) embeds them: a blank node, or an IRI that is no dataset's record, is an object
  with its `type`, the IRI as `id`, and a key per property shape of that node shape
  (`{"type": …, "city": "Gent", "region": {…}}`). Objects embed two levels deep; blank nodes
  below that, and blank nodes with no node shape, are left out. A key gives at most 1,000 values (the first in SPARQL order);
  a record whose keys have more says how many under `truncated` (`{"platform": 300000}`), and an
  answer that would hold more than 200,000 values is refused (413: ask for a smaller `limit`).
- **JSON-LD.** Answers are `application/ld+json` with an `@context` that maps each key to its
  predicate (`@reverse` for an inverse path; keys over longer paths are not in it; an
  embedding key has a scoped `@context` for its objects' keys), so a JSON-LD
  processor reads a record back as the triples it came from. A reference's `label` is not
  mapped. `f=json` gives `application/json` without the `@context` (a page's records are then
  under `records` instead of `@graph`).
- **GeoJSON and CSV.** `f=geojson` gives the records as GeoJSON features (CRS84) for a dataset
  with a geometry: the shape's first `tts:geometry` path, several values as a
  `GeometryCollection`, the rest of the record as `properties`. `f=csv` gives a column per key,
  `id` and `type` first; several values are joined with ` | `, a reference is its IRI, an
  embedded blank node its JSON, and a language string `text@lang`, and the next and previous pages are in the `Link` header. A cell
  that starts with `=`, `+`, `-` or `@` and isn't a number gets a `'` before it, so that a
  spreadsheet doesn't run it as a formula. Without `f`, the `Accept` header chooses:
  `application/ld+json`, `application/json`, `application/geo+json` or `text/csv`, JSON-LD
  otherwise.
- **Filters and paging.** `?{key}=value` keeps records with that value, compared as text (an
  IRI for references); `q` keeps those whose label contains the text, regardless of case and of the accents of Latin letters (`koln` finds `Köln`); `bbox`
  (CRS84, `minlon,minlat,maxlon,maxlat`) those whose geometry (the shape's first
  `tts:geometry`) meets the box. `limit` (100 by default, at most 10,000) and `offset` page,
  and a page has `next` and `prev` URLs. An unknown parameter is a 400.
- **Writes.** Records are written as they are read (a reference as `{"id": …}` or its IRI, a
  language map, JSON numbers, booleans and strings as literals of the key's `sh:datatype`, an
  embedded object without `id` as a new blank node with its keys' values, of the key's
  `sh:class` when it gives no `type`):
  - `POST /{ds}/api/v1/{dataset}` makes a record, its IRI in `id`: 201 with the record and its
    URL in `Location`, 409 when it exists. Without `id` (#82), the IRI comes from the node
    shape's `tts:iriTemplate` (`{uuid}` a random UUID, `{key}` the body's value of that key,
    percent-encoded; a missing value is a 422), else it is `urn:uuid:` and a random UUID.
  - `PUT /{ds}/api/v1/{dataset}/{id}` replaces the record's writable keys: a key the body leaves
    out loses its values. It makes the record when it is missing (201).
  - `PATCH` (JSON Merge Patch) changes only the keys in the body; `null` removes a key's values.
  - `DELETE` removes the record's type and its writable keys' values (204).
  - `?dry=true` on any of them (#82) runs every check (the body, `If-Match`, the shape, the
    graphs the caller may write) and writes nothing: 200 with `{"dry": true, "id": …,
    "created": …}`, and for `DELETE` `referencedBy`, the links to the record that would be
    left; a refusal is the same as the write's.

  A record read alone has an `ETag` (plain JSON a tag of its own), and so has the answer of a write. `PUT`, `PATCH` and
  `DELETE` with `If-Match` (that tag, or `*` for any existing record) are refused with 412
  when the record has changed since: a client that reads, edits and writes back keeps others'
  changes.
  A key is writable when its path is one predicate or the inverse of one; other keys, unknown
  keys, `truncated`, and an `id` or `type` that isn't the record's are refused. A record that
  was read, changed and written back with `PUT` stays as it was.
  Before writing, the record as it would be is validated against its node shape in the
  dataset's [own shapes](#a-datasets-own-shapes), over the data the caller can read, and a
  record with a violation is refused: 422, `application/problem+json`, with `errors` naming
  each `key`, `value`, `constraint` (`sh:minCount`), `message` (the shape's `sh:message`, when
  it has one) and `shape`. The generated shapes describe the data as it is, so they constrain
  nothing: they only decide how values are read (a text in an `xsd:integer` key is still
  refused). The record's triples are in the default graph. When the default graph is the
  union of all graphs, a key's values are removed from every graph that holds them, new values
  go to the graph of the record's type triple, and a new record to the default graph or
  `?graph=`. Removing an embedded blank node removes its triples too, and those of the blank
  nodes below it, from every graph that holds them, when nothing else links to them once the
  record's link is gone (in any graph, those the caller can't read too; a cycle among them
  goes as a whole). A blank node something else links to, or with triples in a graph the
  caller may not change, keeps its triples: only the record's link goes. On the tripletrail
  store the write is then one transaction of exactly those triples; on other stores it is a
  SPARQL update that removes the link only. An embedded object with an `id` is only a
  link: its other keys are written through its own dataset, or SPARQL. An embedded object that
  fails its `sh:node` is explained key by key (`"key": "address.city"`). REST writes to a dataset take turns, from reading the record to writing
  it, so each is checked against the record as the one before left it; a SPARQL update or
  Graph Store write at the same time is not held back, and can still change the record
  between the check and the write. The write has the rights of an update: the update token,
  or with authentication a policy that may write those graphs; under row-level rules writes go
  through SPARQL only (403).
- **`/{ds}/api/openapi.json`** (and **`openapi.yaml`**, the same as YAML) is an OpenAPI 3.1 definition made from the same shapes: a path
  per dataset and per record, with their reads and writes, and a schema per dataset.
- **Docs page.** With the web UI on, `/$/ui/api/{ds}` shows the definition in Swagger UI
  (5.34.0, Apache-2.0, built in), where each request can be tried out; `/{ds}/api/v1` links to it
  as `docs`.
- Requests run under the caller's access like queries: a caller limited to some graphs or
  rows sees datasets made from those, and records from those only.

## GraphQL

`/{ds}/graphql` answers GraphQL queries over the records of the [REST API](#rest-api-json-ld),
made from the shapes the `graphql` reader reads (`/$/datasets/{ds}/shapes?reader=graphql`; the
generated shapes unless the dataset has its own). It only reads: write with the REST API.

- **Requests.** `POST` with a JSON body (`query`, `variables`, `operationName`) or an
  `application/graphql` body, or `GET` with `query`, `variables` (JSON) and `operationName`
  parameters. The answer is `application/json`, `{"data": …}` with `errors` when a field
  failed; a request without a query is a 400. A browser that opens `/{ds}/graphql` (asking for
  `text/html`) is sent to the playground.
- **Schema.** Each dataset is an object type (`Station`) with two root fields: `station(id:
  ID!)`, one record by its IRI or null, and `stations(first, after, filter, q, bbox)`, a
  connection with `edges { cursor node }`, `nodes`, `pageInfo { hasNextPage endCursor }`. `first` is 100 by default and at most 1,000; `after` takes a cursor; `filter`
  takes a text per key, as `?key=` on the REST API; `q` searches the labels and `bbox` (`[minx,
  miny, maxx, maxy]`, WGS 84) the geometry. A key's GraphQL name is its REST key with
  characters outside `[_0-9A-Za-z]` replaced by `_`. Every type has `id` (null for a blank node) and
  `type`.
- **Values.** Numbers and booleans by their datatype (`Int`, `Float`, `Boolean`), language
  strings as a `LangString` map (`{"en": …, "fr": …}`), other literals as `String`; a key with
  `sh:maxCount 1` is one value, others a list. A reference to another dataset's records is an
  object of that type, and an embedded object (a blank node, or a value with a node shape and
  no dataset) a nested type (`StationAddress`) or its class's type. References are read one
  batch per level the query follows, not one query per record.
- **Limits.** A query is at most 16 levels deep and selects at most 10,000 fields (aliases
  and introspection counted). A request reads at most 500,000 values in all (each object,
  list, text and number, over every root field and level; a record that several places refer
  to counts once per place), and the dataset's query timeout (or `timeout=`) is for the whole
  request, not each read. Past either, the fields not yet read fail with a GraphQL error.
- **Type names** come from the classes. A name taken by a GraphQL type (`Int`, `String`,
  `Query`, `PageInfo`, `LangString`, …) or by another class's `…Edge`, `…Connection` or
  `…Filter` type gets `_` added (`Int_`); the root fields keep the class's name (`ints`).
- **Introspection** works, so GraphQL clients and code generators read the schema from the
  endpoint.
- **Playground.** With the web UI on, `/$/ui/graphql/{ds}` has the schema (where **Use as
  query** writes a query for a list or a record), a query editor with variables, and the
  answer. `/{ds}/api/v1` links to the endpoint as `graphql` and to the playground as
  `playground`.
- Queries run under the caller's access like SPARQL queries, with the dataset's query timeout
  (`timeout=` too). The schema is kept until the data, the shapes or the caller's view change.

## MCP

`tripletrail serve --mcp` (`TRIPLETRAIL_MCP`) serves each dataset to AI agents and assistants
over the [Model Context Protocol](https://modelcontextprotocol.io) at `/{ds}/mcp` (and `/mcp`
with one dataset). It is off by default: without `--mcp` the path is a 404.

- **Transport.** Streamable HTTP without streams: each JSON-RPC 2.0 message is a `POST`
  answered with one `application/json` answer; a notification gets 202 and no body. `GET` is a
  405. Protocol versions `2025-06-18`, `2025-03-26` and `2024-11-05`; the server answers the
  client's version when it knows it, else the newest. Methods: `initialize`, `ping`,
  `tools/list`, `tools/call`. No sessions, resources or prompts.
- **Tools**, all read-only:
  - `sparql_query` (`query`, `max_rows`): a SELECT, ASK, CONSTRUCT or DESCRIBE query; SPARQL
    JSON results, or Turtle for CONSTRUCT and DESCRIBE. An update is refused.
  - `describe` (`iri`): the triples about one resource, as Turtle.
  - `search` (`text`, `limit`, 20 by default): [full-text search](full-text-search.md) (`tt:search`) as SPARQL
    JSON results, with `resource`, the matched `literal` and its `score`.
  - `list_shapes`: the dataset's node shapes as JSON (`iri`, `label`, `targetClass`, `paths`),
    as the `describe` reader reads them: its own shapes and packs, or the generated ones.
  - `shape` (`iri`): one node shape with its property shapes, as Turtle.
  - `list_datasets`: the datasets the caller may read, with their MCP and SPARQL endpoints.
- **Access.** A request needs what a query of the dataset needs (an API key, a token, a
  session; 401 or 403 otherwise), and the tools read only the caller's graphs and rows. Each
  call has the dataset's query timeout. An answer holds at most 1,000 rows (and `--max-rows`
  when lower), and ends with a note when there were more. A tool's failure (a syntax error, a
  timeout) is a result with `isError`, which the agent sees; a wrong method or tool is a
  JSON-RPC error.
- **Records.** The query a tool runs (the query of `sparql_query`, and the `DESCRIBE` or
  `tt:search` query that `describe` and `search` make) is recorded as a query to
  `/{ds}/sparql` is: operation `query`, with its text in the slow-query log
  (`--slow-query-ms`) and its hash in the request log, audit records and traces.
- **Browsers.** A request with an `Origin` header (a web page) is refused with 403 unless
  `--cors` allows that origin, against DNS rebinding. MCP clients send none.

## RDF4J REST API

Every dataset is also an RDF4J repository, so RDF4J's `HTTPRepository`, `RemoteRepositoryManager`
and tools written for an RDF4J server or GraphDB work with only the URL changed:

```java
Repository repo = new HTTPRepository("http://localhost:7878/rdf4j-server", "kg");
try (RepositoryConnection c = repo.getConnection()) {
    c.begin();
    c.add(new File("network.ttl"), RDFFormat.TURTLE, vf.createIRI("urn:graph:network"));
    c.remove(oldTrack, null, null);
    c.commit();
}
```

The server speaks protocol 12 (RDF4J 4 and 5) under `/rdf4j-server`, and at the root
(`/repositories/kg`) as GraphDB does, unless a dataset is called `repositories` or `protocol`.

| Path | |
|---|---|
| `/rdf4j-server/protocol` | `12` |
| `/rdf4j-server/repositories` | the datasets the caller may read, with `readable` and `writable` |
| `/repositories/{id}` | SPARQL queries (`GET ?query=`, `POST` form or `application/sparql-query`) |
| `/repositories/{id}/statements` | `GET` by `subj`, `pred`, `obj`, `context`; `POST` adds (any RDF format, also `application/x-binary-rdf`) or runs `update=`; `PUT` replaces; `DELETE` removes by pattern |
| `/repositories/{id}/contexts`, `/size` | named graphs, and the number of statements (by `context`) |
| `/repositories/{id}/namespaces[/{prefix}]` | prefixes; kept in `namespaces.json` in the dataset directory |
| `/repositories/{id}/transactions[/{txn}]` | `POST` begins; `PUT ?action=ADD`, `DELETE`, `UPDATE`, `QUERY`, `GET`, `SIZE`, `PING`, `COMMIT`, `ROLLBACK` |

`context=null` is the default graph. A transaction's changes are kept by the server and
applied at `COMMIT` as one update, so a commit is all or nothing; reads inside a transaction see
the committed data, not the transaction's own changes. Transactions idle for an hour are
dropped, and one whose queued changes pass `--max-upload` is rolled back. Every write in a
transaction (`ADD`, `DELETE`, `UPDATE`, `COMMIT`) needs the update token or write access, as
on the SPARQL endpoints, and a transaction belongs to the caller who began it.

Queries and updates are read as RDF4J's server reads them:

- **Default graph.** A query without `FROM` (or `default-graph-uri`) reads every statement as
  its default graph, as on RDF4J and GraphDB. Such queries take the general evaluation path,
  without the spatial index rewrite; spatial queries are faster on `/{ds}/sparql`, or with
  `--union-default-graph`.
- **Prefixes.** `rdf`, `rdfs`, `owl`, `xsd`, `fn`, `sesame` and the repository's namespaces
  may be used without a `PREFIX` declaration.
- **Requests.** `baseURI`, `$name` bindings, `default-graph-uri` and `named-graph-uri` are
  read. A bound variable keeps GeoSPARQL's relations (`?f geo:sfWithin $area`) and computed
  properties (`geo:hasMetricLength`), as an unbound one does. Update datasets (`using-graph-uri`, `remove-graph-uri`, `insert-graph-uri`) are
  refused; write `USING`, `WITH` or `GRAPH` in the update instead.
- **Uploads.** Uploads may be gzip or zip files. Their blank nodes are new ones unless the
  client sends `preserveNodeId=true`.
- **Errors.** Errors carry RDF4J's types (`MALFORMED QUERY`, `MALFORMED DATA`, `QUERY
  EVALUATION ERROR`), which the client turns into its exceptions.

RDF4J's repository test suite (`rdf4j-repository-testsuite` 5.3.2) passes 383 of its 497 tests
against `HTTPRepository`. The differences:

- **Transactions.** A transaction's changes are applied at `COMMIT`, all at once. Reads inside
  the transaction don't see its own changes, and statements added in it get new blank nodes,
  so a blank node can't be removed later by value.
- **DESCRIBE.** `DESCRIBE` returns the statements about the resource, not those that point to
  it.
- **Speed test.** One test runs about 98,000 requests in a minute.

Not available: creating or deleting repositories through `/repositories/{id}` (use the
[admin API](#admin-api)), the RDF4J Workbench, and RDF-star statements.

## Federated queries

`SERVICE <https://...>` calls a remote SPARQL endpoint, for an endpoint the server allows.
There are none by default, so a remote SERVICE is refused (400) until you list it:

```sh
tripletrail serve --location /data --dataset kg \
  --service-allow https://query.wikidata.org/sparql \
  --service-allow https://data-interop.era.europa.eu/
```

- `--service-allow` (`TRIPLETRAIL_SERVICE_ALLOW`, comma-separated) takes endpoint IRIs or
  prefixes of them. An endpoint is allowed when an entry has the same scheme, host and port
  (80 or 443 when not written) and its path is a prefix of the endpoint's on a `/`
  boundary: `https://example.org/kg` allows `https://example.org/kg/sparql` but not
  `https://example.org/kgx`, and `https://example.org` never allows
  `https://example.org.evil.com`. Only `http` and `https` endpoints are called, endpoints
  with user info (`user@host`) never, and redirects are not followed, so an allowed
  endpoint cannot send a call elsewhere.
- Each call sends the SERVICE pattern as a `SELECT *` query (POST,
  `application/sparql-query`) and reads SPARQL JSON, XML or TSV results.
- `--service-timeout` (30 s) bounds a call, and `--service-max-bytes` (64 MiB) bounds its
  answer. A larger answer is an error, never a cut result.
- `--service-cache-seconds` (0, off by default) reuses an answer for the same endpoint and
  query for that long, for up to 256 answers.
- `SERVICE SILENT` turns a failed call into no rows. `SERVICE ?endpoint` calls each endpoint
  the patterns before it in its group bind, once per row of them, as Fuseki does
  (`?project void:sparqlEndpoint ?endpoint SERVICE ?endpoint { ... }`), also under `OPTIONAL`,
  with or without more patterns in its group, where a row whose endpoint answers nothing
  stays. `LATERAL` does the same anywhere:
  `VALUES ?ep { <https://query.wikidata.org/sparql> } LATERAL { SERVICE ?ep { ... } }`. A row
  that reaches the SERVICE without an endpoint is an error (500), as on Fuseki.
- Errors: an endpoint that is not allowed is 400. A remote failure (an HTTP error, a timeout
  or an answer that is too large) is 502, with the endpoint's status and message.
- Bind join: when a SERVICE is joined with the rest of its group (or is the only thing in an
  `OPTIONAL`), that rest is evaluated first, and its distinct values of the variables the
  SERVICE shares are sent along as a `VALUES` block, 100 rows per call. The remote endpoint
  then only answers for what can join. With more than 10,000 distinct rows, a blank node
  or an unbound value among them, a SERVICE under `GRAPH`, or `SERVICE SILENT` (whose
  failure must stay one empty solution), the SERVICE is sent as written; keep such a remote pattern selective (a constant subject, a `FILTER`). With no
  rows to join, no call is made.
- Queries with a remote SERVICE are never kept in the result cache.

## Live database tables

Queries can read PostgreSQL live, without loading it, through Facade-X
(`SERVICE <x-sparql-anything:table=rail.station,database=rail>`, or `sql=`; see
[facade-x.md](facade-x.md#databases) for the rows, values, FILTER push-down and bind joins).
Off by default: the administrator names the connections.

```
tripletrail serve --location data \
  --fx-connection rail=postgresql://reader:secret@db/rail
```

- `--fx-connection NAME=URL` (repeatable): a connection queries pick with `database=NAME`.
  A query must name one; there is no default.
- Any caller who may query a dataset may run read-only queries on these connections, as
  `table=` or as `sql=` (one statement, in a read-only transaction). What they can read is
  what the connection's role can read: give it a role with `SELECT` on what callers may
  see, and nothing more. Dataset access policies do not filter these rows.
- `--fx-timeout` (seconds, default 30) is the longest one call may take, reading the rows
  included: it is the PostgreSQL `statement_timeout`, and TripleTrail also cancels the query
  when it is up, so SQL that changes the setting does not run on.
- `--fx-max-rows` (default 100,000) is the most rows one call may read (`database.max-rows`
  in a query may ask for fewer), and `--fx-max-bytes` (default 100,000,000) the most bytes,
  counted in PostgreSQL's text form as they arrive. Rows stream in, so a call stops at a
  cap without holding more than it in memory (a single value is read whole: PostgreSQL
  allows up to 1 GB).
- Facade-X on a server reads no files and no URLs (`location=` is refused); inline
  `content=` still works. A query may lower the size limits of a source
  (`yaml.max-alias-nodes`, `spreadsheet.max-cells`, `spreadsheet.max-bytes`) but not raise
  them above their defaults.
- `--fx-graph IRI=SERVICE` (repeatable) names a virtual graph: `GRAPH <IRI> { … }` in a
  query reads the Facade-X SERVICE instead of the store, so clients query a table without
  writing `SERVICE`:
  `--fx-graph https://example.org/stations=x-sparql-anything:table=rail.station,database=rail`.
  `GRAPH ?g` and `FROM NAMED` do not list virtual graphs, and updates read the store. A
  caller with access to only some graphs, or under row rules, may not call a SERVICE, so for
  them `GRAPH <IRI>` reads the store's graph of that name, as without the option. A virtual
  graph is read even when the query's `FROM NAMED` leaves it out.
- `--fx-cache-seconds` (default 0: never) reuses a result for the same connection and
  statement (the SQL sent, with its FILTER and join values) for that long, up to about 256 MB
  of memory for results (as RDF terms, several times their size as text); a result over a
  quarter of that is not kept. A cached call still takes a session to prepare
  the statement, but reads no rows.
- Each connection keeps up to `--fx-sessions` (default 4) PostgreSQL sessions, shared by
  every dataset and query; more calls at once wait for a free one. A session is reset after
  each call (`DISCARD ALL`: advisory locks, settings and `LISTEN`s a query left go). A session whose query was cancelled (at the
  timeout or a cap) is closed, and the next call opens a new one; so is a connection the
  database closed, retrying the call once.

## Offline tools

Four commands work without a server, as Jena's command-line tools do, on files loaded in memory
(`--data a.ttl b.nq`, or `--data` repeated; format from the extension) or on a dataset folder
(`--location`, `--dataset`, `--store`) that no server has open. Blank node labels belong to
their file, as in Jena: when several files are read, those of file *i* become `f<i>_<label>`,
so two files that both say `_:b1` have two blank nodes. `tripletrail load` and `serve --load`
do the same, as `tdb2.tdbloader` does: each file, and each load into a dataset that already
holds data, gets its own blank nodes (each label becomes a 32-digit hash of it, keyed for that load, so a dump loaded again keeps labels of the same length). To add to a blank
node already stored, give it an IRI, or load its triples in one file.

- `tripletrail query --query Q` runs a query. `Q` is the text, `@file`, or the path of a `.rq`
  file. Rows are written as TSV and graphs as Turtle, unless `--format` (`tsv`, `csv`, `json`,
  `xml`; `ttl`, `nt`, `nq`, `trig`, `rdf`, `jsonld`, `rj`, `trix`, `trdf`, `prdf`) or the extension of `--output` says
  otherwise. `--union-default-graph` queries the union of all graphs, as on the server, and
  `--text-properties IRI` (repeated, or a comma-separated list) indexes more properties for
  full-text search, as `serve --text-properties` does for a dataset.
- `tripletrail update --update U` runs an update. A dataset folder keeps the changes. With
  `--data`, the dataset after the update is written to `--output` (or as N-Quads to standard
  output). The number of quads before and after goes to standard error.
- `tripletrail dump` writes every quad of a dataset folder as N-Quads, or in `--format`;
  `--graph IRI` (or `--graph default`) writes one graph, which Turtle, N-Triples and RDF/XML
  need when the dataset has named graphs. Without it they are refused before anything is
  written.
- `tripletrail disk-usage --location DIR [--dataset ds] [--json]` shows what a dataset
  folder takes (#152): its files by kind, numbered pieces and runs counted under their kind
  (`dict.bin`, `geoms.bin`, `spog.seg`, …), and, read from every key, the dictionary's keys by
  kind of term (`iri`, `blank`, `string`, `langString`, `typed`, `wkt`, `wktBinary` for WKT
  kept in binary form by `--render-wkt`, `geometry` for other geometry literals, `triple`),
  with the bytes their datatype IRIs or language tags take and, for the IRIs written in full,
  their namespaces (up to the last `/` or `#`; an IRI that shares its namespace with an
  earlier one refers to it, #152). It opens the dataset read-only, so a dataset a server has open
  is refused (stop the server, or run it on a copy of the folder), as is a folder that is not
  TripleTrail storage. The index of key ends (`dict.idx`) is not in the dictionary's count. Keys
  written since the last checkpoint are counted with the dictionary but are still in
  `wal.log`, not `dict.bin`: the output says so when the log is not empty (`keysInLog` in
  JSON).
- `tripletrail convert files…` reads RDF files as a stream and writes them as N-Quads, in `--to`,
  or in the format of `--output`. `--syntax` names the input format when the extensions
  don't, and `--base` the base IRI. `--validate` only checks each file and prints its first
  error; `--count` prints how many triples each holds. Either exits with status 1 when a file
  is not valid.

## Coming from Fuseki

| Fuseki | TripleTrail |
|---|---|
| `fuseki-server --loc=DB /ds` | `tripletrail serve --location DB --dataset ds` |
| `tdb2.tdbloader` | `tripletrail load --location DB --dataset ds files...` |
| `arq`, `tdb2.tdbquery` | `tripletrail query --data files... \| --location DB --dataset ds --query (text\|@file\|q.rq)` ([offline tools](#offline-tools)) |
| `tdb2.tdbupdate` | `tripletrail update --location DB --dataset ds --update ...` (or `--data files... --output new.nq`) |
| `tdb2.tdbdump` | `tripletrail dump --location DB --dataset ds [--graph IRI] [--format nq\|trig\|ttl\|nt]` |
| `riot` (`--output`, `--validate`, `--count`) | `tripletrail convert files... [--to FORMAT \| --output file] [--validate] [--count]` |
| assembler file (`fuseki-server --config`) | `tripletrail serve --config tripletrail.yaml` ([configuration file](#configuration-file)), `TRIPLETRAIL_*` variables and the admin API |
| `/$/datasets`, `/$/backup`, `/$/backups-list`, `/$/tasks`, `/$/stats`, `/$/server`, `/$/ping` | same paths and keys |
| `POST /$/compact/{ds}` (TDB2 compaction) | the same path: a `Compact` task (`202`, `taskId`) that folds the write-ahead log now, since TripleTrail folds it by itself; `deleteOld` has nothing to do (#831) |
| `/$/validate/query`, `update`, `data`, `iri`, `langtag` | the same paths, GET or POST, parameters `query`, `update`, `data` (with `languageSyntax`, Turtle by default), `iri` (one or more) and `langtag`; JSON with `input`, `errors` (`parse-error`, `parse-error-line`, `parse-error-column`) and `warnings`, or for IRIs `iris` with each `iri`'s `error` list, and for a language tag `valid` (#831) |
| RDF Patch service `POST /{ds}/patch` (`application/rdf-patch`) | the same path: `A` and `D` rows (triples or quads), `PA`/`PD` prefixes, `TX`/`TC`/`TA` (an aborted transaction's rows are left out) and `H` headers, applied as one SPARQL update with the update service's access checks, `200` as Fuseki answers; a `D` row with a blank node is refused (400), since an update can't name one (#831) |
| `POST /$/sleep?interval=ms`, `GET /$/logs` | a `Sleep` task (10 minutes at most); a plain-text note on where the log goes (stderr, `--request-log`) (#831) |
| Java functions and property functions (Fuseki modules) | SHACL-AF `sh:SPARQLFunction`s and `--functions` ([user-defined functions](validation-and-reasoning.md#user-defined-functions)); the spatial, text and path property functions are built in |
| `tdb2:unionDefaultGraph true` | `--union-default-graph ds` (or `unionDefaultGraph=true` in the admin API) |
| shiro.ini admin password | `--admin-token` (Bearer) |
| shiro.ini write access to datasets | `--update-token` (Bearer) |
| shiro.ini users and roles | `--auth-config` with users who sign in with HTTP Basic (`tripletrail import-shiro shiro.ini`), API keys or trusted headers ([Authentication](#authentication)) |
| `jena-text` | `text:query` and `tt:search` on TripleTrail's own full-text index |
| SHACL service `fuseki:shacl` (`POST /ds/shacl?graph=&target=`) | the same: `POST /{ds}/shacl` ([below](#shacl-service)) |
| jena-geosparql `spatial:` property functions and its spatial index | the same `spatial:` calls on TripleTrail's own spatial index, nothing to configure ([spatial](spatial.md#jenas-spatial-search-spatial)) |
| `GenericRuleReasoner` rule files | `tripletrail import-jena-rules rules.txt > rules.ttl` prints them as SHACL-AF SPARQL rules for `tripletrail infer --shapes` or a dataset's shapes: the body's triples and builtins (comparisons, `noValue`, arithmetic, `strConcat`, `uriConcat`, `regex`, `makeTemp`, `now`, …) become the `WHERE`, the head the `CONSTRUCT`, and `<-` rules run forward; rules with functors, builtins in the head or `table` are left out and listed, and `@include` is not read |
| RDFS inference assembler (`ja:reasoner`) | `tripletrail serve --rdfs ds` keeps RDFS materialised in a named graph (`urn:tripletrail:inferred`, or `--rdfs ds=GRAPH`), up to date with every write; `--owl-rl ds` the same with the OWL 2 RL rules; `tripletrail entail` materialises it once; `tripletrail infer` runs SHACL-AF rules |

The SPARQL endpoints follow the SPARQL 1.1 Protocol as Fuseki does: `default-graph-uri` and
`named-graph-uri` set a query's dataset, `using-graph-uri` and `using-named-graph-uri` an
update's, and relative IRIs resolve against the endpoint's URL. A request that repeats `query`
or `update`, sends one in both the body and the URL, or names a charset other than UTF-8 is
refused. A successful update is answered `200` with a short HTML page, as Fuseki answers
it (clients written for Fuseki check for `200`; the protocol also allows `204`). Graph
Store `PUT` and `POST` also take `multipart/form-data` uploads, one file per
part; a `POST` without `?graph` adds to the default graph, as on Fuseki, where the protocol
would create a new graph. `NOW()` is the server's local time with its offset
(`2026-11-12T00:41:35.123+01:00`), as Jena gives it, not UTC: a rule that takes today's date
from `SUBSTR(STR(NOW()), 1, 10)` sees the same day as on Fuseki. Run the server with `TZ` set
to change it. `IF` and `COALESCE` give back the term of the branch they pick as it is stored, as
Jena does: a loaded `28.0e0` or `07` keeps its text, also when another branch is computed
(it used to come back as `"28"^^xsd:double` then). `MIN` and `MAX` order values as Jena's `NodeValue.compareAlways` does (#517):
by kind first (IRIs, plain strings, language-tagged strings by tag, numbers, booleans, dates
and times, durations, other datatypes last), then by value, and equal values by their text, so
`MAX` of `3.0`, `3` and `3.0e0` is `3.0e0` and a plain string is below every number. `ORDER BY`
keeps SPARQL's order of terms (blank nodes, IRIs, then literals), and orders literals by the
same kinds first (#148), then by value within a kind, and values of a kind that don't compare
(different datatypes, language strings of different tags) by datatype and then their text: a
number and a string compare only as written, so sorting a mix of them as written made a cycle
(`2 < 10`, `10 < "1z"`, `"1z" < 2`) and the sort failed with a 500. Durations are ordered by one
number of seconds, a month being 1/12 of 365.2425 days, then by datatype and text: XSD leaves
`P1M` and `P30D` unordered, and the order it does give is kept. Numbers are compared as doubles, and an integer or decimal
equal to another number as a double then by its exact value, so large integers keep their order. Both read a date, time or dateTime without a timezone as UTC
when it meets one with a timezone (#78), as Jena's F&O comparison does, and order values of the
same instant by their text: XSD leaves `2020-01-01T03:00:00` and `2020-01-01T02:00:00+02:00`
unordered, which made a sort of such values panic. `NaN` sorts after every other number. A division of decimals or integers, `AVG` and `win:avg` give
what Jena's `XSDFuncOp.decimalDivide` gives (#8): the exact quotient when it ends (`1 / 8` is
`0.125`), else 24 digits after the point rounded half to even (`2 / 3` is
`0.666666666666666666666667`). A product of decimals is exact, as Jena's `BigDecimal` product
is (`0.1234567891 * 0.123456789` is `0.0152415787625361999`); it used to give no value past 18
digits after the point. Arithmetic and comparisons read a decimal with more than 18
digits after the point, computed or loaded, at 18 digits rounded half to even. [Conformance](conformance.md) lists the W3C test results.

### Configuration file

`tripletrail serve --config tripletrail.yaml` (or `TRIPLETRAIL_CONFIG`) reads the server's options from
a YAML file (#826). Its keys are the long options of `tripletrail serve`, with `-` or `_`
(`bind`, `location`, `timeout`, `max-body`, `admin-token`, `cors`, …): a value, a list for an
option given several times, `true` for a flag. `datasets` lists the datasets, each a map:

| Key | Means |
|---|---|
| `name` | the dataset's name (required) |
| `path` | a dataset directory to serve, as `--attach name=path`; without it the dataset is `--dataset name` (in `location`, or in memory) |
| `read-only` | with `path`: opened read-only (`:ro`). The datasets of `location` are read-only with `read-only: true` at the top |
| `union-default-graph` | `true`: the default graph is the union of all graphs (`--union-default-graph`) |
| `rdfs`, `owl-rl` | `true`, or the IRI of the graph to keep the entailment in (`--rdfs`, `--owl-rl`) |
| `aliases` | a name or a list of names that answer for the dataset (`--alias`) |
| `text-properties` | an IRI or a list of IRIs whose literals full-text search indexes too (`--text-properties name=IRI`, see [full-text search](full-text-search.md#what-is-indexed)) |

```yaml
bind: 0.0.0.0:3030
location: /data/tripletrail
timeout: 30
datasets:
  - name: kg
    union-default-graph: true
    rdfs: true
    aliases: [live]
  - name: rinf
    path: /snapshots/rinf
    read-only: true
```

The file is read as if its options came after the command line's: an option given as a flag,
or set by its environment variable, wins over the file, and datasets given with `--dataset` or
`--attach` replace the file's. An unknown key, or a value of the wrong kind, stops the server
with a message that names it. `GET /$/server` gives the file in use as `configFile`.

`tripletrail config import-fuseki config.ttl [--output tripletrail.yaml]` translates a Fuseki
configuration (the assembler file `fuseki-server --config` reads) into such a file:

| Fuseki | Configuration file |
|---|---|
| `fuseki:Service` with `fuseki:name` | a dataset of that name |
| `tdb2:unionDefaultGraph true` (or TDB1's) | `union-default-graph: true` |
| `tdb2:location` | a comment with the `tdb2.tdbdump` and `tripletrail load` commands that move the data |
| `ja:InfModel` with the RDFS reasoner | `rdfs: true` |
| `ja:InfModel` with an OWL reasoner | `owl-rl: true` (OWL 2 RL, noted as an approximation) |
| `arq:queryTimeout` (server, service or dataset `ja:context`, ms) | `timeout` in seconds (the overall limit, the largest) |
| `ja:data` files of the first dataset | `load` |
| `text:TextDataset`, `geosparql:GeosparqlDataset` | the dataset they wrap (TripleTrail's full-text and spatial indexes are built in) |
| `text:entityMap` fields on other properties than the ones always indexed | `text-properties` |

What it can't carry over is listed at the end of the file as comments, and counted on stderr:
endpoint names other than TripleTrail's standard ones, services with no update endpoint (read-only
in Fuseki), other context settings, other reasoners and dataset types, and `ja:namedGraph`.

### SHACL service

`POST /{ds}/shacl` (#829) validates a dataset against shapes sent with the request, as Fuseki's
SHACL service does. The body holds the shapes, in any RDF format by its `Content-Type`
(Turtle when there is none, or when it is curl's form data); the answer is the W3C validation
report (`sh:ValidationReport` with `sh:conforms` and its `sh:result`s), `200` whether or not
the data conforms, in Turtle or the format `Accept` or `format=` asks for.

- `graph=default` (the default), `graph=union` (the named graphs as one, as
  Jena's union graph: not the default graph), or `graph=` a graph's IRI
  (`404` when the dataset has no such graph).
- `target=` an IRI: only that node, against the shapes that target it.
- SHACL-AF rules in the shapes run first, in memory, and validation sees what they infer;
  SHACL-SPARQL constraints and functions declared in the shapes work as in
  [validation](#validation-and-inference).
- Shapes that do not parse are a `400`, shapes that do not compile a `422`, another method
  than `POST` a `405`. `timeout=` and the dataset's limits apply as for a query.
- The data is read from one snapshot of the store, taken when the validation starts. Writes
  go on while it runs and it does not see them (it does not hold the store's dictionary lock
  for the run). The same holds for `/$/datasets/{name}/validate`.
- It needs read access to the whole dataset: a caller limited to some graphs, or under row
  rules, is refused.

Difference from Fuseki: report nodes are blank nodes with other labels.
