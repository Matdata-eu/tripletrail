# Vector search

TripleTrail stores embedding vectors next to the graph and finds the resources whose vectors
are nearest a query vector (#236): "find assets like this description", "find the signals
like this one". The vectors come from outside, computed by a language model of your choice
from labels and descriptions; TripleTrail stores and compares them.

```sparql
PREFIX ex: <https://example.org/rail#>
PREFIX tt: <http://w3id.org/tripletrail/fn#>
SELECT ?s ?score WHERE {
  ?s tt:nearest (ex:embedding "[0.12, -0.5, 0.03]" 10 ?score)
} ORDER BY DESC(?score)
```

## Vectors

A vector is a literal of the datatype `tt:vector` (`http://w3id.org/tripletrail/fn#vector`):
a JSON array of finite numbers, at most 65,536 of them.

```turtle
@prefix tt: <http://w3id.org/tripletrail/fn#> .
ex:BRG ex:embedding "[0.9, 0.1, 0.2]"^^tt:vector .
```

They are loaded and written like any literal: in files, over the Graph Store Protocol, or
with SPARQL Update. A resource can have vectors of several properties (one per model, say).

## `tt:nearest`

`?s tt:nearest (property query k ?score ?vector)` binds `?s` to the resources whose vector of
`property` is nearest `query`, by cosine similarity, the best `k`. Everything after the query
is optional: without `k`, every vector of the property is a match.

- `query` is a vector literal, a string in the same form (as a parameter filled in by an
  application), or a variable bound by the rest of the query. With a variable, the search runs
  once for each value it takes, as `tt:search` does with a variable query string: so
  `ex:BRG ex:embedding ?q . ?s tt:nearest (ex:embedding ?q 5)` finds what is like `ex:BRG`
  (`ex:BRG` itself among them). A value that is not a vector finds nothing.
- `?score` is the cosine similarity, from -1 to 1, as an `xsd:double` rounded to six
  decimals. `?vector` is the vector literal matched.
- A vector of another length than the query's, or of only zeros, is never a match.
- The subject may be a constant: `ex:BRG tt:nearest (…)` asks whether it is among the best `k`
  of all the vectors.
- The rows join the rest of the group like those of any pattern, so filters, spatial
  functions and `tt:search` combine with it. Note that a `FILTER` applies after the best `k`
  are chosen: ask for more and filter, or rank with `tt:cosine` (below) to filter first.
- `tt:nearest` needs the TripleTrail store (the default). It is not supported inside `GRAPH`,
  `SERVICE` or `FILTER EXISTS`: it reads every graph of the dataset, as the full-text search
  does.

### Exact, then approximate

Up to 20,000 vectors of one length, every one is compared: the search is exact. The vectors of
a property are read into memory on the first search (2.4 s for 100,000 vectors of 768
numbers, release build), and the first search after a write adds and removes those that
changed. Comparing them all then takes about 30 ms for 100,000 vectors of 768 numbers, and
290 ms for a million.

Past 20,000 vectors of one length, the first search also starts building a graph of them
(HNSW, a hierarchical navigable small world) on a thread of its own, using half the cores.
Searches stay exact until it is ready, then follow the graph: each compares a few thousand
vectors instead of all of them, so it is fast, but approximate. The graph is walked with the
vectors in 8 bits (a quarter of the memory to read), and the 512 best it finds are compared
again with the full vectors. It finds nearly always, not always, the same resources as
comparing them all. Vectors written after the graph was made are compared one by one, so they
are found at once; once they are more than an eighth of the graph, it is extended in the
background. Removed vectors are skipped. When more than a quarter of the vectors change at
once, the copy is read again and the graph is built again.

Measured on one machine (4 cores, the graph built on 2, release build), with 768 numbers per
vector, top 20:

| Vectors | Graph built in | Search | Recall@20 |
| --- | --- | --- | --- |
| 100,000 | 55 s | 3.4 ms | 0.997 |
| 1,000,000 | 40 min (see below) | 9.4 ms | 0.946 |

The million was built before a change that made the 100,000 build a third faster (85 to
55 s), so expect less than 40 minutes now. Recall is the share of the exact top 20 the graph
finds. The test vectors are mixes of 64
random directions with noise, harder than most text embeddings, whose vectors cluster by
topic: on vectors around 1,000 centres, recall was 1.0 at 100,000. To check your own data,
compare `tt:nearest` with the exact ranking by `tt:cosine` (below) for a few queries.
`cargo test --release -p tripletrail --test vectors -- --ignored --nocapture graph_recall`
measures it (`VECTORS`, `DIMENSIONS`, `LATENT` set the data).

When you need the exact answer, rank with `tt:cosine`. To keep every `tt:nearest` exact, start
the server with `--vector-graph-min 0` (`TRIPLETRAIL_VECTOR_GRAPH_MIN=0`, or
`vector-graph-min: 0` in the [configuration file](server.md#configuration-file)); another
number moves the threshold. A program embedding the engine calls
`Engine::set_vector_graph_min`.

For a dataset on disk, the graph of the whole dataset's vectors is saved once it is built, in
`<location>/<dataset>.networks/vectors/` beside the routing networks, and read again after a
restart instead of being built again (seconds instead of minutes). Its nodes are matched to the
vectors by resource and literal, so vectors written since it was saved are compared one by
one, and removed ones skipped, as above. A file that does not read back is ignored, and the
graph built again. The graphs of callers limited to some graphs or rows are kept in memory
only.

The copy holds each vector as a unit vector of 32-bit floats (normalised in double precision
first, so very large and very small numbers keep their direction), and the pairs of the
property. It counts in the dataset's memory as `vectorIndexes` (`/$/stats/{name}` and
`tripletrail_dataset_memory_bytes`), with its graph (one byte a number, and about 270 bytes
a vector for the links). The copies of
the 16 properties searched last are kept.

## Functions

`tt:cosine(a, b)`, `tt:dot(a, b)` and `tt:l2(a, b)` compare two vectors (literals or strings in
the same form) and give an `xsd:double`: the cosine similarity, the dot product and the
Euclidean distance. They give no value when an argument is not a vector or the lengths
differ, and `tt:cosine` none for a vector of only zeros. They work anywhere an expression
does:

```sparql
SELECT ?s WHERE {
  ?s ex:embedding ?v ; ex:line "25"
  BIND(tt:cosine(?v, "[0.12, -0.5, 0.03]") AS ?score)
} ORDER BY DESC(?score) LIMIT 10
```

On the tripletrail store, a query ordered first by `DESC(tt:cosine(?v, "[…]"))` (or by a
variable bound to it) under a `LIMIT` reads the same copy of the vectors as `tt:nearest`
when `?v` is the object of one property. It compares the constant vector with every vector
of its length, then evaluates the query again with `?v` kept to the most similar ones, and
to the rows whose cosine is above what any vector left out can reach (the copy compares in
32-bit floats, so it allows for their error). Those rows are the first of the whole query,
so once there are `k` of them they are the answer. The first round keeps four times `k` and the `OFFSET`
vectors (at least 64); when that gives too few rows, a second round keeps as many more as
the first says it takes, up to sixteen times as many and a quarter of the vectors. Past
that the query is evaluated as written. The answer is exact, graph or not, and the other
patterns and filters of the query still apply.

Among 100,000 vectors of 768 numbers, the top 20 take 40 ms instead of 6.2 s. A filter that
keeps one row in a hundred is evaluated as written after the first round, at about 130 ms
instead of 93 ms: the comparison and the first round come on top. `tt:nearest` stays the
faster way past its graph size, approximate.

It works on the union of the named graphs as the default graph and for a caller limited to
some graphs, from a copy of the vectors of those graphs, as `tt:nearest` does. The query is
evaluated as written instead when it has a dataset of its own (`FROM`), when the caller has
row-level access rules, when its `OFFSET` and `LIMIT` are more than a sixteenth of the vectors of the
length, and when the property also has plain strings in the form of a vector, which have a
cosine but no place in the copy.

## Hybrid search

Similarity, an area and full text go in one query. With the cosine as the ordering, the
answer is exact and comes from the vector index (see [Functions](#functions)):

```sparql
PREFIX ex: <https://example.org/rail#>
PREFIX tt: <http://w3id.org/tripletrail/fn#>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
SELECT ?s WHERE {
  ?s tt:search "crossing" ; ex:embedding ?v ; geo:hasGeometry/geo:asWKT ?w
  FILTER(geof:sfWithin(?w, "POLYGON((4 50, 6 50, 6 51, 4 51, 4 50))"^^geo:wktLiteral))
} ORDER BY DESC(tt:cosine(?v, "[0.12, -0.5, 0.03]")) LIMIT 10
```

`tt:nearest` takes the best `k` of all the vectors first, and the other patterns then keep
some of them: fewer than `k` when they are selective.

## Retrieval for GraphRAG

`GET /{ds}/search/similar` gives the resources nearest a vector, with their descriptions: the
context an application hands a language model.

- `property`: the property holding the vectors (required).
- `vector`: the query vector, a JSON array of numbers (`[0.12, -0.5, 0.03]`), computed by
  the application with the embedding model of its choice; or `iri`: a resource whose vector
  of the property to use, in any graph the caller reads (with several, each found resource
  keeps its best score).
- `k`: how many, 10 by default, at most 100. As `tt:nearest`: exact up to 20,000 vectors of
  the length, then from the graph.
- `describe=false` leaves the descriptions out.

The answer is JSON: `{"property", "k", "results": [{"iri", "score", "description"}]}`, best
first, each resource once with its best score: a resource with several vectors does not
take the place of others. Each `description` is the resource's concise bounded description in JSON-LD (its
triples, and those of the blank nodes it links to, as on its
[describe page](server.md#linked-data)), with `"truncated": true` when it was cut. A caller
limited to some graphs or rows finds and sees only what it may read. A missing `property`, a
`vector` that is not one, both `vector` and `iri` or neither, or a `k` out of range is a
400; a resource without a vector finds nothing.

The text of a search is the caller's to turn into a vector: TripleTrail keeps no model and no
key. The web UI's Search page does it in the browser with the user's own AI service (see
[Find similar](guide/10-maps-and-apis.md#find-similar)).

```sh
curl -s -G http://localhost:7878/rail/search/similar \
  --data-urlencode property=https://example.org/rail#embedding \
  --data-urlencode 'vector=[0.9, 0.1, 0.2]' -d k=5
```

## Access control

A caller who may read only some graphs, or some rows (#203), finds only the vectors in what
they may read: the index of their view holds only those, as the full-text index does.
