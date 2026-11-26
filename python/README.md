# TripleTrail for Python

The TripleTrail engine in Python: an RDF store with GeoSPARQL 1.1, PostGIS-style `st:`
functions, Jena's `spatialf:` functions, the spatial index and the pgRouting `SERVICE`, in
process, so a pipeline step queries and writes without going through HTTP and SPARQL JSON.

The package has [pyoxigraph](https://pyoxigraph.readthedocs.io)'s classes and functions
(`NamedNode`, `Literal`, `BlankNode`, `Quad`, `Triple`, `Variable`, `DefaultGraph`, `Dataset`,
`RdfFormat`, `QueryResultsFormat`, the query results, `parse`, `serialize`,
`parse_query_results`), and `Store` is the `Engine`, so code written for pyoxigraph switches by
changing `import pyoxigraph as ox` to `import tripletrail as ox`. pyoxigraph is not needed. The
store builds these objects natively, so scans and lookups run at pyoxigraph's speed. Terms of
other libraries with pyoxigraph's attributes (pyoxigraph's own, for instance) are accepted as
input too, a little slower.

```python
import tripletrail as ox
from tripletrail import Engine

e = Engine()                      # in memory; Engine("/data/kg") for an on-disk store
# e = Engine(store="oxigraph")   # Oxigraph's store instead of the TripleTrail store
e.load(path="kg.ttl")
e.load(path="step.nt", to_graph=ox.NamedNode("http://example.com/step"))
for row in e.query("""
    PREFIX geo: <http://www.opengis.net/ont/geosparql#>
    PREFIX st: <http://w3id.org/tripletrail/st#>
    SELECT ?g (st:ST_LineInterpolatePoint(?w, 0.5) AS ?p) WHERE { ?g geo:asWKT ?w }"""):
    print(row["g"], row["p"].value)
e.update("INSERT { ... } WHERE { ... FILTER(geof:sfWithin(?w, ...)) }", timeout=600)
```

- `query(q, *, base_iri=None, prefixes=None, use_default_graph_as_union=False,
  default_graph=None, named_graphs=None, substitutions=None, timeout=None)`: as in pyoxigraph,
  `QuerySolutions` (rows indexable by name, `Variable` or position, `row.get(name, default)`;
  `.variables`; `.serialize(output=None, format=...)`), `QueryBoolean` or `QueryTriples`. Past
  `timeout` seconds, `TimeoutError`. With a dataset option, the query is evaluated as written,
  without the spatial rewrite. `substitutions` keep it: a bound variable still has its
  GeoSPARQL relations (`geo:sfWithin`, …) and computed properties (`geo:hasMetricLength`, …).
- `prepare(q, *, base_iri=None, prefixes=None)`: a `PreparedQuery`, parsed once, that
  `query(prepared, substitutions={...})` runs with new values for the variables in its
  `variables` (the ones it selects), without writing them into the query's text.
- `update(u, *, base_iri=None, prefixes=None, timeout=None)`, `explain(q)` (plan with row
  counts and timings, as JSON).
- `load(input=None, format=None, *, path=None, base_iri=None, to_graph=None,
  rename_blank_nodes=False)` (or `bulk_load`): bulk load from bytes, str, a file object or a
  path. `format` is an `RdfFormat`, media type or extension. Blank node labels are kept, as
  pyoxigraph's `Store.load` keeps them; `rename_blank_nodes=True` gives the document its own
  blank nodes, as `tripletrail load`, the server's uploads and Jena do.
- `dump(output=None, format=None, *, from_graph=None, base_iri=None, prefixes=None)`. A format
  of triples (Turtle, N-Triples, RDF/XML) without `from_graph` writes the default graph when
  the store has no named graph (pyoxigraph refuses); with named graphs it is an error that
  names them, as in pyoxigraph.
- `validate(shapes_graph, *, data_graph=None, report_graph=None, statistics=False)`: SHACL validation of a graph
  against the shapes of a named graph. Returns a `ValidationReport` with `conforms`, `results`
  (one dict per result: `focus_node`, `result_path`, `value`, `source_shape`,
  `source_constraint_component`, `severity`, `message`, `messages`; `pandas.DataFrame(r.results)`
  works), `triples()` and `serialize(output=None, format=None)` (the W3C report, Turtle by
  default). With `report_graph` the report is also written into the store. With
  `statistics=True`, `shape_counts` gives per shape the focus and value nodes it was checked
  against (`shape`, `property`, `focus_nodes`, `value_nodes`), also written in the report.
- `quads_for_pattern(s, p, o, g=None)`, `add(quad)`, `extend(quads)` (or `bulk_extend`),
  `remove(quad)`, `len()`, iteration over all quads, `named_graphs()`,
  `contains_named_graph(g)`, `add_graph(g)`, `clear_graph(g)`, `remove_graph(g)`, `clear()`,
  `invalidate()` (after writing to the same on-disk store from elsewhere).
- `objects(s, p, g=None)`, `subjects(p, o, g=None)`, `predicate_objects(s, g=None)`: the terms
  of the matching quads as a list, without building `Quad` objects: the quickest way to do
  rdflib-style `objects` / `value` lookups.
- `union_default_graph` (read/write): the default graph of queries and updates is the union of
  all graphs, as Fuseki's `unionDefaultGraph`.

RDF 1.2 triple terms and directional language strings work as in pyoxigraph. Not supported:
`custom_functions` and `custom_aggregate_functions` in `query`, `Store.read_only(path)` (use
`Engine.open_read_only(path)`), `backup`, `flush` and `optimize`, and SPARQL `LOAD` (no HTTP
client). Two differences remain with the TripleTrail store (the default; `Engine(store="oxigraph")` behaves as pyoxigraph):

- **Literals keep their lexical form.** Oxigraph stores typed literals in canonical form, so
  `100.0` (`xsd:decimal`) reads back as `"100"`, `"01"^^xsd:integer` as `"1"`, `1.5e0` as
  `"1.5"`, `"1"^^xsd:boolean` as `"true"` and `"…T00:00:00.000Z"` as `"…T00:00:00Z"`. The TripleTrail
  store gives back what was loaded: `"100.0"`, `"01"`, `"1.5e0"`, `"1"`, `"….000Z"`. Comparisons
  and arithmetic are by value in both (`FILTER(?v = 100)` matches `100.0`, and `?v + 0` is
  canonical), but a triple pattern with a constant (`?s ex:maxSpeed 100`) matches only the same
  lexical form, and `str(?v)` or the literal's `value` differ. Code that writes values out
  (`maxSpeed="100.0"`) gets the loaded form; to get Oxigraph's, convert in the query (`?v + 0`,
  `xsd:decimal(?v)`) or in Python (`decimal.Decimal(lit.value).normalize()`). See
  [docs/store.md](../docs/store.md) for why.
- An empty named graph does not exist (`add_graph` does nothing), since a graph exists there
  while it has quads.

### RML mappings

`run_mapping(mapping=None, *, path=None, yarrrml=False, base_dir=None, allowed=None,
to_graph=None)` runs an RML, R2RML or YARRRML mapping over files, as `tripletrail lift` does, and
adds its quads to the store in batches as they are made, so the triples are never all in
Python or in one list. It returns how many quads the mapping made.

```python
made = e.run_mapping(path="stations.rml.ttl", to_graph=ox.NamedNode("http://example.com/step"))
e.run_mapping("""prefixes: {ex: http://example.com/}
mappings:
  station:
    sources: [[stations.csv~csv]]
    s: ex:station/$(code)
    po: [[a, ex:Station]]""", yarrrml=True)
```

The mapping is Turtle text, YARRRML text with `yarrrml=True`, or a file at `path` (YARRRML by
a `.yarrrml`, `.yml` or `.yaml` name). Source paths resolve against `base_dir`, by default the
current directory; with `allowed` (a list of directories) no file outside them is read.
Quads in the default graph go to `to_graph` when given, and a mapping's own graph maps are
kept. A mapping error is a `ValueError`, a source that cannot be read an `OSError`. Mappings
with logical targets, and database sources, are refused: use `tripletrail lift` for them. See
[docs/rml.md](../docs/rml.md).

### A server: workspaces and datasets

`tripletrail.connect` works on a `tripletrail serve` over HTTP with the same methods and result
classes, so code developed against the workbench runs in process later by swapping the object:

```python
import tripletrail

ws = tripletrail.connect("http://localhost:7878", workspace="track shapes", create=True)
ws.load(path="sample.ttl")                    # as data/sample.ttl: the workspace's files are its store
ws.write_file("shapes/track.ttl", shapes)
report = ws.validate()                        # conforms, results (dicts of terms), serialize()
for row in ws.query("SELECT ?s WHERE { ?s a <http://data.europa.eu/949/Track> }"):
    print(row["s"])
ws.infer()                                    # what rules/ infer, as Triples
ws.run_pipeline(); ws.query("SELECT …", step=3)   # pipeline.yaml, and the store after step 3

ds = tripletrail.connect("http://localhost:7878", dataset="rinf", token="…")
ds.update("INSERT …"); ds.load(path="extra.nt", to_graph=tripletrail.NamedNode("http://…"))
ds.validate(tripletrail.NamedNode("http://…/shapes"))
```

- `connect(url, workspace=None, *, dataset=None, token=None, create=False, timeout=None)`:
  a `Workspace` (by id or name; `create=True` makes it) or a `RemoteDataset`. `token` (or
  `TRIPLETRAIL_TOKEN`) goes as `Authorization: Bearer`.
- Both: `query` (`QuerySolutions`, `QueryBoolean`, or a list of `Triple` with `serialize`;
  `timeout` gives `TimeoutError`), `explain`, `load`, `dump`, `validate` (a report with
  `conforms`, `results`, `shape_counts`, `triples()`, `serialize()`), `infer`.
- A workspace's store holds what its files hold: `load` writes a file of `data/`, `update`
  is refused unless it is an overlay (put it in `updates/` and run it in the pipeline), and there are `files`,
  `read_file`, `write_file`, `delete_file`, `tests`, `pipeline`, `run_pipeline`, `blame`.
  `query(..., step=N)` and `dump(..., step=N)` read the store after pipeline step N.
  Its runs appear in the workbench's History tab, which opens a validation's report or an
  inference's triples with why? on each (`infer(provenance=True)`); `history()` lists them, and
  `url` is the workspace in the workbench.
  A run stops at a python step and waits for its code: `ws.run_step(N, function, **kwargs)`
  calls `function(base_url, dataset, **kwargs)` (code written for Fuseki) against the store
  of step N and then completes the step; `step_endpoint(N)` and `complete_step(N)` do it by
  hand.
  `ws.overlay("name")` makes the workspace an overlay of a whole dataset: its store reads the
  dataset as it is now, `update` changes the workspace only (never the dataset), `patch()`
  gives the changes as an RDF Patch (or `patch("added")`, `patch("removed")` as N-Quads),
  `drop_changes()` drops them and `detach()` makes it a plain workspace again.
- A dataset: `update`, `load` and `dump` with the Graph Store Protocol, `validate` and
  `infer` with the shapes of one of its graphs (reading only).

Only the standard library is used. In a notebook, `%load_ext tripletrail` adds two cell magics,
which show results as a table (a `pandas.DataFrame` with pandas):

```
%%sparql [ws] [--step N] [--timeout S] [--out NAME]
SELECT ...

%%shacl [engine_or_ws] [--data GRAPH] [--shapes GRAPH] [--out NAME]
<shapes in Turtle>
```

The target is a variable holding an `Engine` or a client, by default the client `connect`
gave last. `%%shacl` validates against the cell's shapes: on an `Engine` in a graph dropped
afterwards, on a workspace as `shapes/notebook.ttl`, on a dataset with `--shapes` (its graph)
and no cell.

### Releases: canonical form and diffs

```python
old, new = Engine("releases/2026-10"), Engine("releases/2026-11")
old.canonical_digest()        # SHA-256 of the RDFC-1.0 canonical N-Quads
old.canonicalize()            # the canonical N-Quads, as bytes
d = tripletrail.diff(old, new)   # added / removed lines, counts per graph, predicate, class
d["classes"]["http://example.com/Track"]   # (added, removed)
```

See [canonicalization](../docs/canonicalization.md).

### User-defined functions

```python
functions = engine.with_functions(path="functions.ttl")   # sh:SPARQLFunction declarations
functions.query("PREFIX ex: <http://example.com/> SELECT (ex:kmToM(?km) AS ?m) WHERE { ?s ex:km ?km }")
```

`with_functions` returns an engine on the same store. See [user-defined
functions](../docs/validation-and-reasoning.md#user-defined-functions).

### rdflib

`pip install tripletrail[rdflib]` and use the engine as an rdflib store (adapted from
[oxrdflib](https://github.com/oxigraph/oxrdflib), which is not needed):

```python
import rdflib
from tripletrail import Engine
from tripletrail.rdflib import TripleTrailStore

ds = rdflib.Dataset(store=TripleTrailStore(store=Engine()))
ds = rdflib.Dataset(store="TripleTrail")  # the same, through rdflib's plugin
ds.parse("kg.ttl")
```

`tripletrail.rdflib.to_tripletrail` and `from_tripletrail` convert terms between the two libraries.

The GIL is released during updates, loads and `explain`.

## Build

```sh
pip install maturin pytest   # and pyoxigraph, rdflib, pyarrow for the tests that use them
cd python
maturin develop --release                      # with on-disk stores (builds RocksDB)
maturin develop --release --no-default-features  # in memory only, faster to build
pytest tests
```
