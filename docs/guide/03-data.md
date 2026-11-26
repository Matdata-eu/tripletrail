# 3. Load, change and keep data

The server of chapter 2 kept its data in memory. In this chapter you keep a dataset on disk,
add and change data while the server runs, organise it in named graphs, and get it back out.

## A dataset on disk

`--location` names a folder for datasets, and `--dataset` the dataset to serve from it. A big
file loads fastest before the server starts, with `tripletrail load`:

<!-- test: run -->
```sh
tripletrail load --location db --dataset rail network.ttl
```

Then serve it:

<!-- test: serve -->
```sh
tripletrail serve --location db --dataset rail
```

The dataset is now at `/rail/…` instead of `/ds/…`, and lives in `db/rail`. Stop the server
and start it again: the data is still there.

<!-- test: query rail -->
```sparql
PREFIX ex: <https://example.org/rail#>
SELECT (COUNT(?s) AS ?stations) WHERE { ?s a ex:Station }
```

<!-- test: output -->
```text
stations
10
```

`tripletrail load` must not run while a server has the dataset open. On a running server, upload
instead (below). A dataset that does not exist yet is created empty, so `tripletrail serve
--location db --dataset new` gives you an empty dataset to fill.

A load that takes longer than a minute writes a line to standard error every minute, so that
you can follow it:

```text
loading planet.nt: reading, 1,204,331,870 triples, 223.4 of 932.0 GB read (24 %), 3600 s
```

The step is `reading` (parsing the file and adding its terms), `sorting` (building the indexes;
past `--memory-limit`, a sort on disk), `writing` (the dataset's files) or `entailing` (a kept
RDFS entailment). The triples are those read so far, duplicates included; they are counted as
each block of the file is taken in, so the count trails the bytes read a little. When the size
of the input is not known (a pipe), the line gives only the gigabytes read.

## Add data while the server runs

The Graph Store Protocol reads and writes whole files of triples at `/rail/data`. Save two more
stations as `more-stations.ttl`:

<!-- test: file more-stations.ttl -->
```turtle
@prefix ex:   <https://example.org/rail#> .
@prefix geo:  <http://www.opengis.net/ont/geosparql#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

ex:BRG a ex:Station ; rdfs:label "Bruges" ; ex:code "BRG" ;
  geo:hasGeometry [ geo:asWKT "POINT(3.2170 51.1972)"^^geo:wktLiteral ] .
ex:CRL a ex:Station ; rdfs:label "Charleroi-Central" ; ex:code "CRL" ;
  geo:hasGeometry [ geo:asWKT "POINT(4.4386 50.4045)"^^geo:wktLiteral ] .
```

and `POST` it, which adds it to what is there (`PUT` would replace):

<!-- test: run -->
```sh
curl -s -X POST http://localhost:7878/rail/data -H 'Content-Type: text/turtle' --data-binary @more-stations.ttl
```

The **Datasets** page of the web UI does the same with **Upload**. A file with a syntax error
is refused as a whole, so nothing is half loaded.

## Named graphs

A dataset holds a default graph and any number of named graphs, each named by an IRI. They
keep data apart by source or version: a load of one source can be replaced without touching
the others. `?graph=` names the graph to write:

<!-- test: file timetable.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .

ex:IC1 a ex:Train ; ex:calls ex:BXS, ex:BXC, ex:BXN, ex:MEC, ex:ANT .
ex:IC2 a ex:Train ; ex:calls ex:BXS, ex:BXC, ex:BXN, ex:LV, ex:LG .
```

<!-- test: run -->
```sh
curl -s -X PUT 'http://localhost:7878/rail/data?graph=https://example.org/graph/timetable' \
  -H 'Content-Type: text/turtle' --data-binary @timetable.ttl
```

`GRAPH` in a query reads a named graph; without it a query reads the default graph:

<!-- test: query rail -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?train (COUNT(?station) AS ?calls) WHERE {
  GRAPH <https://example.org/graph/timetable> { ?train ex:calls ?station }
}
GROUP BY ?train
ORDER BY ?train
```

<!-- test: output -->
```text
train,calls
https://example.org/rail#IC1,5
https://example.org/rail#IC2,5
```

To query the default graph and every named graph as one, start the server with
`--union-default-graph rail`, as Fuseki's `unionDefaultGraph` does.

## Change data with SPARQL Update

`/rail/update` takes SPARQL updates. `INSERT DATA` adds triples:

<!-- test: update rail -->
```sparql
PREFIX ex: <https://example.org/rail#>
INSERT DATA { ex:BRG ex:openedIn 1838 }
```

`DELETE … INSERT … WHERE` changes what a pattern finds. Raise the speed of every section of
line 25 to 200 km/h:

<!-- test: update rail -->
```sparql
PREFIX ex: <https://example.org/rail#>
DELETE { ?s ex:maxSpeed ?old }
INSERT { ?s ex:maxSpeed 200 }
WHERE  { ?s ex:line "25" ; ex:maxSpeed ?old }
```

<!-- test: query rail -->
```sparql
PREFIX ex: <https://example.org/rail#>
SELECT ?section ?speed WHERE { ?section ex:line "25" ; ex:maxSpeed ?speed }
ORDER BY ?section
```

<!-- test: output -->
```text
section,speed
https://example.org/rail#S03,200
https://example.org/rail#S04,200
```

From the command line, send an update the same way as a query, as `update=`:

<!-- test: run -->
```sh
curl -s http://localhost:7878/rail/update --data-urlencode \
  'update=DELETE WHERE { GRAPH <https://example.org/graph/timetable> { ?s ?p ?o } }'
```

## Get the data out

A `GET` of `/rail/data` gives the whole dataset, with its graphs, as N-Quads or TriG
(`?graph=` for one graph, `?default` for the default graph):

<!-- test: run -->
```sh
curl -s http://localhost:7878/rail/data -H 'Accept: application/n-quads' > rail.nq
wc -l < rail.nq
```

<!-- test: output -->
```text
169
```

For copies you can restore, see [backups](13-production.md#backups).

## More than one dataset

Repeat `--dataset` to serve several, each at its own `/{name}/` URLs, or create one while the
server runs, from the **Datasets** page or the admin API:

<!-- test: run -->
```sh
curl -s -o /dev/null -X POST 'http://localhost:7878/$/datasets?name=scratch&type=mem'
curl -s http://localhost:7878/scratch/query -H 'Accept: text/csv' \
  --data-urlencode 'query=ASK { ?s ?p ?o }'
```

<!-- test: output -->
```text
false
```

The new dataset is empty, so the `ASK` (does any triple exist?) answers `false`.

### Blank nodes belong to their file

A blank node label such as `_:p1` names a node within one file only, as on Fuseki: each
upload, and each file that `tripletrail load` reads, gets its own blank nodes. Save this as
`platform.ttl`:

<!-- test: file platform.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .

ex:BRG ex:platform _:p1 .
_:p1 ex:length 420 .
```

and post it twice to the new dataset:

<!-- test: run -->
```sh
for i in 1 2; do
  curl -s -X POST http://localhost:7878/scratch/data -H 'Content-Type: text/turtle' \
    --data-binary @platform.ttl
done
```

<!-- test: query scratch -->
```sparql
PREFIX ex: <https://example.org/rail#>
SELECT (COUNT(DISTINCT ?p) AS ?platforms) WHERE { ex:BRG ex:platform ?p . ?p ex:length 420 }
```

<!-- test: output -->
```text
platforms
2
```

Bruges now has two platforms: the second upload's `_:p1` is a new node. To add to a node
that is already stored, give it an IRI (`ex:BRG-1`), or keep its triples in one file.

`type=mem` keeps it in memory and `type=disk` in the `--location` folder. Once the server is
reachable by others, these requests need an admin token: see
[access control](12-access-control.md).

## Query another endpoint

`SERVICE` sends part of a query to another SPARQL endpoint, such as Wikidata's or another
TripleTrail's, and joins its answer with yours. It is off until `--service-allow` names the
endpoints a query may call. Here the other endpoint is a second dataset of the same server:

<!-- test: serve -->
```sh
tripletrail serve --location db --dataset rail --service-allow http://localhost:7878/
```

<!-- test: run -->
```sh
curl -s -o /dev/null -X POST 'http://localhost:7878/$/datasets?name=depots&type=mem'
curl -s -o /dev/null -X POST http://localhost:7878/depots/data -H 'Content-Type: text/turtle' --data-binary '
@prefix ex: <https://example.org/rail#> .
ex:ANT ex:depotTracks 14 .
ex:GNT ex:depotTracks 9 .'
```

The rail dataset can say where that endpoint is, and a query can take it from there:

<!-- test: update rail -->
```sparql
PREFIX ex: <https://example.org/rail#>
INSERT DATA { ex:depots ex:endpoint <http://localhost:7878/depots/sparql> }
```

<!-- test: query rail -->
```sparql
PREFIX ex: <https://example.org/rail#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?station ?tracks WHERE {
  ex:depots ex:endpoint ?endpoint .
  SERVICE ?endpoint { ?s ex:depotTracks ?tracks }
  ?s rdfs:label ?station .
} ORDER BY ?station
```

<!-- test: output -->
```text
station,tracks
Antwerp-Central,14
Ghent-Sint-Pieters,9
```

`SERVICE ?endpoint` calls the endpoint once for each endpoint the patterns before it find, as
Fuseki does; in an `OPTIONAL`, the rows it has no answer for stay. An endpoint that is not
allowed is refused with a `400`; `SERVICE SILENT` gives no rows when the call fails. See
[federated queries](../server.md#federated-queries) for the time and size limits.

## Find similar resources

Embedding vectors, computed outside TripleTrail by a language model from a description, say,
are stored as literals of the datatype `tt:vector`: a JSON array of numbers. `tt:cosine`,
`tt:dot` and `tt:l2` compare two of them, so a query can rank resources by how close their
vector is to another one:

<!-- test: update rail -->
```sparql
PREFIX ex: <https://example.org/rail#>
PREFIX tt: <http://w3id.org/tripletrail/fn#>
INSERT DATA {
  ex:BRG ex:embedding "[0.9, 0.1, 0.2]"^^tt:vector .
  ex:ANT ex:embedding "[0.2, 0.9, 0.1]"^^tt:vector .
  ex:GNT ex:embedding "[0.8, 0.3, 0.1]"^^tt:vector .
}
```

<!-- test: query rail -->
```sparql
PREFIX ex: <https://example.org/rail#>
PREFIX tt: <http://w3id.org/tripletrail/fn#>
SELECT ?station WHERE {
  ?station ex:embedding ?v
  BIND(tt:cosine(?v, "[1, 0, 0.1]"^^tt:vector) AS ?score)
} ORDER BY DESC(?score) LIMIT 2
```

<!-- test: output -->
```text
station
https://example.org/rail#BRG
https://example.org/rail#GNT
```

The query vector can also be a plain string in the same form, such as a parameter filled in
by an application. Vectors of different lengths give no score.

On the tripletrail store this query reads a copy of the vectors kept in memory instead of
computing every cosine, and its answer stays exact: among 100,000 vectors it takes tens of
milliseconds, not seconds.

`tt:nearest` reads the same copy, and is the faster way for very many of them: past 20,000
vectors it follows a graph, approximate. It takes the property, the query vector, how many to
find and a variable for the score. The query vector can come from the data: these are the
two stations most like Bruges, Bruges itself first:

<!-- test: query rail -->
```sparql
PREFIX ex: <https://example.org/rail#>
PREFIX tt: <http://w3id.org/tripletrail/fn#>
SELECT ?station WHERE {
  ex:BRG ex:embedding ?q .
  ?station tt:nearest (ex:embedding ?q 2 ?score)
} ORDER BY DESC(?score)
```

<!-- test: output -->
```text
station
https://example.org/rail#BRG
https://example.org/rail#GNT
```

With a few stations every vector is compared, so the answer is exact. Past 20,000 vectors of
one length, TripleTrail builds a graph of them in the background (HNSW) and then follows it:
a search compares a few thousand vectors instead of all of them, and finds nearly always, but
not always, the same ones. Rank with `tt:cosine` when you need the exact answer, or start the
server with `--vector-graph-min 0` to keep every `tt:nearest` exact. A dataset on disk keeps the
graph, so a restart does not build it again.

An application that hands a language model context (retrieval for GraphRAG) can ask for the
nearest resources with their descriptions, as JSON with each description in JSON-LD. It
gives a vector (`vector=[…]`, computed with the model of its choice) or a resource whose
vector to use:

<!-- test: run -->
```sh
curl -s -G http://localhost:7878/rail/search/similar \
  --data-urlencode property=https://example.org/rail#embedding \
  --data-urlencode iri=https://example.org/rail#BRG -d k=2 | grep -o '"iri":"[^"]*"'
```

<!-- test: output -->
```text
"iri":"https://example.org/rail#BRG"
"iri":"https://example.org/rail#GNT"
```

Each result also has its `score` and its `description`; `describe=false` leaves the
descriptions out. A real embedding has hundreds of numbers, too long for a URL behind most
proxies, so send the same parameters as a form, with `POST`:

<!-- test: run -->
```sh
curl -s http://localhost:7878/rail/search/similar \
  --data-urlencode property=https://example.org/rail#embedding \
  --data-urlencode 'vector=[1, 0, 0.1]' -d k=1 -d describe=false | grep -o '"iri":"[^"]*"'
```

<!-- test: output -->
```text
"iri":"https://example.org/rail#BRG"
```
 The web UI's Search page asks the same with a text, turned into a vector in
your browser by your own AI service: see [Find similar](10-maps-and-apis.md#find-similar).

See [vector search](../vector-search.md) for the details.

## What you learnt

- `tripletrail load` fills an on-disk dataset offline; `tripletrail serve --location DIR --dataset
  NAME` serves it.
- `POST`/`PUT` on `/{dataset}/data` upload files, `?graph=` into a named graph.
- `/{dataset}/update` takes SPARQL updates: `INSERT DATA`, `DELETE … INSERT … WHERE`.
- `SERVICE` queries another endpoint once `--service-allow` allows it.
- `tt:cosine` and `tt:nearest` rank resources by their `tt:vector` embeddings, and
  `/{dataset}/search/similar` gives the nearest with their descriptions.
- `GET /{dataset}/data` exports everything.

Next: [spatial queries](04-spatial.md).
