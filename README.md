# TripleTrail

TripleTrail is an RDF triple store for spatial networks such as railways, roads and utilities.
It answers SPARQL 1.1 and 1.2 with complete spatial and routing support, built in and fast:

- **GeoSPARQL 1.1** and the **PostGIS `ST_*`** function set, answered from a spatial index.
- **pgRouting's `pgr_*`** functions (shortest paths, isochrones, flows, vehicle routing and
  more) on any data model, plus routing with constraints, SHACL vehicle rules and contraction
  hierarchies.
- **A drop-in replacement for Apache Jena Fuseki**: the same URLs, admin API and Graph Store
  Protocol, with access control, backups, blue-green releases and a web UI.
- **Data in, data out**: query CSV, JSON, XML, Excel, HTML, Shapefile and more with SPARQL
  Anything queries (Facade-X) or R2RML/RML mappings; serve features as OGC API Features and
  vector tiles; read and write GeoParquet.
- **Validation and reasoning**: SHACL Core and SHACL-SPARQL, SHACL-AF rules, RDFS and OWL 2 RL
  entailment, kept up to date as data changes.

It is built on the [Oxigraph](https://github.com/oxigraph/oxigraph) crates, with its own
storage engine and query planner.

## Install

Releases publish binaries for Linux, macOS and Windows and a Docker image,
`ghcr.io/matdata-eu/tripletrail`. Until the first release, build it from source with
[Rust](https://rustup.rs) 1.95 or later:

```sh
git clone https://github.com/Matdata-eu/tripletrail
cd tripletrail
cargo install --locked --path crates/tripletrail-server   # installs the `tripletrail` command
```

or build the Docker image yourself:

```sh
docker build -t tripletrail .
docker run -p 7878:7878 -v tripletrail-data:/data tripletrail
```

For Python, `pip install ./python` builds the `tripletrail` package from a checkout (it needs
Rust). It is not on PyPI yet: v0.1.0 came out before releases published it there, and
`pip install tripletrail` with ready-made wheels for Linux, macOS and Windows comes with a later
release. See [python/README.md](python/README.md). The JavaScript client is on npm:
`npm install @matdata/tripletrail-client`.

## Quick start

Save a few stations as `stations.ttl`:

```turtle
@prefix ex:   <http://example.org/> .
@prefix geo:  <http://www.opengis.net/ont/geosparql#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

ex:brussels a ex:Station ; rdfs:label "Brussels Central" ;
  geo:hasGeometry [ geo:asWKT "POINT(4.3571 50.8456)"^^geo:wktLiteral ] .
ex:leuven a ex:Station ; rdfs:label "Leuven" ;
  geo:hasGeometry [ geo:asWKT "POINT(4.7158 50.8813)"^^geo:wktLiteral ] .
ex:antwerp a ex:Station ; rdfs:label "Antwerp Central" ;
  geo:hasGeometry [ geo:asWKT "POINT(4.4211 51.2172)"^^geo:wktLiteral ] .
```

Start the server with them loaded into an in-memory dataset named `ds`:

```sh
tripletrail serve --load stations.ttl
```

Open <http://localhost:7878/> for the web UI. **Query** runs SPARQL with table, map and chart
views, **Map** shows the stations on OpenStreetMap, and **Workbench** is a playground for
shapes, rules and pipelines. Try the stations within 50 km of Brussels:

```sparql
PREFIX geo:  <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX uom:  <http://www.opengis.net/def/uom/OGC/1.0/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?name ?km WHERE {
  ?station rdfs:label ?name ; geo:hasGeometry/geo:asWKT ?wkt .
  BIND(geof:distance(?wkt, "POINT(4.3571 50.8456)"^^geo:wktLiteral, uom:metre) / 1000 AS ?km)
  FILTER(?km < 50)
} ORDER BY ?km
```

The same works from any SPARQL client, at Fuseki's URLs:

```sh
curl http://localhost:7878/ds/query -H 'Accept: text/csv' --data-urlencode query@near.rq
curl http://localhost:7878/ds/data -H 'Content-Type: text/turtle' --data-binary @more.ttl   # add data
curl http://localhost:7878/ds/ogc/collections                                              # OGC API Features
```

To keep the data on disk, give the server a directory. A large file loads faster offline
first:

```sh
tripletrail load --location ./data --dataset kg big-file.nt   # bulk load, while no server runs
tripletrail serve --location ./data --dataset kg
```

The server binds to `127.0.0.1` and has no authentication until you configure it. Before
anyone else can reach it, read [Running it in production](docs/server.md#running-it-in-production).

## Command line

| Command | What it does |
|---|---|
| `tripletrail serve` | Serves datasets: SPARQL, Graph Store Protocol, admin API, web UI, OGC API, Linked Data. See [server](docs/server.md). |
| `tripletrail load` | Bulk-loads RDF or GeoParquet files into an on-disk dataset. |
| `tripletrail lift` | Turns CSV, JSON, XML, spreadsheets and other files into RDF with Facade-X queries and RML mappings. See [Facade-X](docs/facade-x.md) and [RML](docs/rml.md). |
| `tripletrail validate` | Validates data against SHACL shapes. See [validation and reasoning](docs/validation-and-reasoning.md). |
| `tripletrail infer` | Runs SHACL-AF rules until they infer nothing new. |
| `tripletrail entail` | Writes the RDFS or OWL 2 RL entailment of data to a graph or a file. |
| `tripletrail export` | Writes the results of a `SELECT` as GeoParquet. See [GeoParquet](docs/geoparquet.md). |
| `tripletrail canon`, `tripletrail diff` | RDFC-1.0 canonical N-Quads and a digest of a dataset, and what changed between two releases. See [canonicalization](docs/canonicalization.md). |
| `tripletrail pipeline run` | Runs an ETL `pipeline.yaml` (lift, load, update, infer, reason, validate, export, publish and python steps) headless. See [pipelines](docs/server.md#pipelines). |
| `tripletrail test` | Runs the regression tests of a workspace, for CI. See [tests](docs/server.md#tests). |
| `tripletrail health`, `tripletrail hash-key` | A health check for containers, and the hash of an API key for the auth configuration. |

`tripletrail <command> --help` lists every option. The options of `serve` can also be set with
an environment variable (`--timeout` is `TRIPLETRAIL_TIMEOUT`, and so on), except `--load` and
the `--fx-*` options.

## Examples

**Routing.** The routing functions take any edge set as a sub-select, as pgRouting takes an
SQL query, so they work on any data model:

```sparql
PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
PREFIX ex:  <http://example.com/>
SELECT ?seq ?node ?edge ?agg_cost WHERE {
  SERVICE <http://w3id.org/tripletrail/pgr> {
    [] pgr:function pgr:pgr_dijkstra ;
       pgr:start_vid ex:a ; pgr:end_vid ex:d ;
       pgr:seq ?seq ; pgr:node ?node ; pgr:edge ?edge ; pgr:agg_cost ?agg_cost .
    { SELECT ?id ?source ?target ?cost WHERE {
        ?id a ex:Track ; ex:from ?source ; ex:to ?target ; ex:length ?cost .
    } }
  }
}
```

**PostGIS functions.** `st:` has the PostGIS names and arguments:

```sparql
PREFIX st:  <http://w3id.org/tripletrail/st#>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
SELECT ?line (st:ST_LineInterpolatePoint(?wkt, 0.5) AS ?midpoint) WHERE {
  ?line geo:hasGeometry/geo:asWKT ?wkt .
}
```

**Full-text search.** `tt:search` ranks resources by their labels, titles and descriptions
([more](docs/full-text-search.md)):

```sparql
PREFIX tt: <http://w3id.org/tripletrail/fn#>
SELECT ?s ?score WHERE { ?s tt:search ("central" ?score) } ORDER BY DESC(?score)
```

**Files as RDF.** A SPARQL Anything query reads a file and builds triples from it:

```sparql
PREFIX xyz: <http://sparql.xyz/facade-x/data/>
PREFIX fx:  <http://sparql.xyz/facade-x/ns/>
CONSTRUCT { ?station <http://ex.org/name> ?name }
WHERE {
  SERVICE <x-sparql-anything:location=stations.json> { ?s xyz:code ?code ; xyz:name ?name }
  BIND(fx:entity("http://ex.org/station/", ?code) AS ?station)
}
```

```sh
tripletrail lift queries/ --output stations.nt
```

## Documentation

New to TripleTrail? The [user guide](docs/guide/README.md) takes you through it step by step, on a
small sample railway network. The pages below are the reference.

| Topic | Page |
|---|---|
| Step-by-step user guide: install, data, spatial, routing, validation, lifting, pipelines, Workbench, maps and APIs, Fuseki, access control, production | [docs/guide/](docs/guide/README.md) |
| Running the server: endpoints, datasets, admin API, access control, backups, web UI, Workbench, pipelines, describe pages, OGC API, Linked Data, coming from Fuseki | [docs/server.md](docs/server.md) |
| Security model: what is protected, from whom, and the operator's part | [docs/security.md](docs/security.md) |
| Spatial functions, set-returning functions, vector tiles, linear referencing | [docs/spatial.md](docs/spatial.md) |
| Routing: pgRouting functions and what goes beyond them | [docs/routing.md](docs/routing.md) |
| SHACL validation, SHACL-AF rules, RDFS and OWL 2 RL entailment | [docs/validation-and-reasoning.md](docs/validation-and-reasoning.md) |
| Full-text search (`tt:search`, Jena's `text:query`) | [docs/full-text-search.md](docs/full-text-search.md) |
| Facade-X: querying files with SPARQL Anything queries | [docs/facade-x.md](docs/facade-x.md) |
| R2RML, RML and YARRRML mappings | [docs/rml.md](docs/rml.md) |
| GeoParquet import and export | [docs/geoparquet.md](docs/geoparquet.md) |
| Canonical N-Quads, dataset digests and diffs between releases | [docs/canonicalization.md](docs/canonicalization.md) |
| Rust library | [docs/library.md](docs/library.md) |
| TripleTrail Enterprise: the enterprise build, licence keys and updates | [docs/enterprise.md](docs/enterprise.md) |
| Python package | [python/README.md](python/README.md) |
| Every function and its status | [docs/function-matrix.md](docs/function-matrix.md) |
| W3C and OGC test suite results | [docs/conformance.md](docs/conformance.md) |
| Benchmarks against PostGIS, QLever, Jena, GraphDB and others | [tripletrail-bench](https://github.com/Matdata-eu/tripletrail-bench) |
| Design: architecture, storage, spatial index | [docs/architecture.md](docs/architecture.md), [docs/store.md](docs/store.md), [docs/spatial-index.md](docs/spatial-index.md) |
| Changes per release | [CHANGELOG.md](CHANGELOG.md) |

## Crates

| Crate | What it does |
|---|---|
| `tripletrail` | `Engine`: a store with everything below registered. |
| `tripletrail-server` | The `tripletrail` command and server. |
| `tripletrail-store` | Quad store with 64-bit term ids, eight compressed sorted quad indexes, geometries parsed on first use and kept by id, and a checkpoint plus write-ahead log on disk. See [docs/store.md](docs/store.md). |
| `tripletrail-geo` | `geof:` (GeoSPARQL 1.1) and `st:` (PostGIS-named) functions on `geo:wktLiteral`s. |
| `tripletrail-spatial` | R-tree over all geometry literals, and the query rewrite that answers spatial filters and joins from it. |
| `tripletrail-routing` | Data-model agnostic routing core on a pgRouting edge set (`id, source, target, cost, reverse_cost`). Knows nothing about RDF. |
| `tripletrail-pgr` | pgRouting functions as a SPARQL `SERVICE`, with runtime edge queries and named networks. |
| `tripletrail-shacl` | SHACL Core and SHACL-SPARQL validator, SHACL-AF rules, and the `compat:` vehicle rules used by routing. |
| `tripletrail-fx` | Files as RDF through `SERVICE <x-sparql-anything:…>`, following the Facade-X model. |
| `tripletrail-rml` | R2RML, RML and YARRRML mappings. |
| `tripletrail-testsuite` | The W3C and OGC test suites. The benchmarks are in [tripletrail-bench](https://github.com/Matdata-eu/tripletrail-bench). |

The queries are parsed, optimized and evaluated by patched copies of Oxigraph's `spargebra`,
`sparopt` and `spareval` crates under [vendor/](vendor), each with a `TRIPLETRAIL.md` that lists
the changes.

## License

TripleTrail is licensed under the [Apache License 2.0](LICENSE). Third-party components and
their licenses are listed in [NOTICE](NOTICE); `cargo deny check` (see [deny.toml](deny.toml))
keeps dependencies to permissive and weak-copyleft licenses.

TripleTrail is developed by Matdata as open core: this repository is the open-source engine, and
Matdata offers commercial editions and support on top of it. Contributions are welcome under
the Apache License 2.0, with no agreement to sign; see [CONTRIBUTING.md](CONTRIBUTING.md). Vulnerabilities go
through [SECURITY.md](SECURITY.md).

TripleTrail is built on [Oxigraph](https://github.com/oxigraph/oxigraph) and is not affiliated
with or endorsed by the Oxigraph project.
