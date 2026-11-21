# 11. Coming from Apache Jena Fuseki

TripleTrail serves datasets at the same URLs as [Fuseki](https://jena.apache.org/documentation/fuseki2/),
with the same admin API, so clients, scripts and tools written for Fuseki keep working when
only the server changes. This chapter moves a Fuseki dataset over and shows what maps to what.

## Move the data

1. Export the dataset from Fuseki, with every named graph, as N-Quads. Either over HTTP while
   Fuseki runs:

   ```sh
   curl -s http://fuseki:3030/kg/data -H 'Accept: application/n-quads' > kg.nq
   ```

   or from the TDB2 folder while it is stopped: `tdb2.tdbdump --loc /fuseki/databases/kg > kg.nq`.

2. Load it into a TripleTrail dataset on disk, with the same name so the URLs stay the same:

   ```sh
   tripletrail load --location /data/tripletrail --dataset kg kg.nq
   ```

3. Serve it on Fuseki's port, so clients need no change at all:

   ```sh
   tripletrail serve --location /data/tripletrail --dataset kg --bind 0.0.0.0:3030
   ```

Large files load fastest this way, before the server starts. `--memory-limit 8G` sorts on disk
when a load would not fit in memory.

## The same requests

Start the sample network as dataset `ds` and try a few requests a Fuseki client makes:

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl
```

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/$/ping'
curl -s http://localhost:7878/ds/sparql -H 'Accept: text/csv' \
  --data-urlencode 'query=SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }'
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:7878/ds/update \
  --data-urlencode 'update=INSERT DATA { <urn:example:a> <urn:example:b> 1 }'
curl -s 'http://localhost:7878/ds/data?default' -H 'Accept: application/n-triples' | wc -l
```

<!-- test: output -->
```text
ok
n
158
200
159
```

Code that talks to Fuseki through SPARQLWrapper, rdflib or `requests` needs only the new base
URL, if that:

```python
from SPARQLWrapper import SPARQLWrapper, JSON

sparql = SPARQLWrapper("http://localhost:7878/ds/sparql")
sparql.setQuery("SELECT ?s WHERE { ?s a <https://example.org/rail#Station> } LIMIT 3")
sparql.setReturnFormat(JSON)
print(sparql.queryAndConvert())
```

## What maps to what

| Fuseki | TripleTrail |
|---|---|
| `fuseki-server --loc=DB /kg` | `tripletrail serve --location DIR --dataset kg` |
| `tdb2.tdbloader` | `tripletrail load --location DIR --dataset kg files…` |
| `/kg/query`, `/kg/sparql`, `/kg/update`, `/kg/data`, `/kg/get` | the same |
| `/$/ping`, `/$/datasets`, `/$/backup`, `/$/backups-list`, `/$/tasks`, `/$/stats`, `/$/server` | the same paths and keys |
| `/$/compact/kg`, `/$/sleep`, `/$/logs`, `/$/validate/…` | the same (below) |
| RDF Patch service `/kg/patch` | the same (below) |
| Java functions and property functions (Fuseki modules) | SHACL-AF functions (`sh:SPARQLFunction`, [chapter 6](06-validation-and-reasoning.md#your-own-functions)) |
| `tdb2:unionDefaultGraph true` | `--union-default-graph kg` |
| RDFS reasoner in the assembler | `--rdfs kg` ([chapter 6](06-validation-and-reasoning.md#kept-up-to-date-on-a-server)) |
| `jena-text` with `text:query` | built in, nothing to configure (below) |
| SHACL service `/kg/shacl` | the same (below) |
| Shiro admin password | `--admin-token` |
| Shiro users and roles | `--auth-config` with users (`tripletrail import-shiro shiro.ini`), API keys and policies ([chapter 12](12-access-control.md)) |
| Rule files of the `GenericRuleReasoner` | SHACL-AF rules (`tripletrail import-jena-rules`, below) |
| Assembler file (`--config config.ttl`) | `--config tripletrail.yaml` (below; `tripletrail config import-fuseki` translates a `config.ttl`), command-line options or `TRIPLETRAIL_*` environment variables |

### Full-text search

Queries written for `jena-text` run unchanged; the index is built on the first search and kept
up to date with writes:

<!-- test: query -->
```sparql
PREFIX text: <http://jena.apache.org/text#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?station ?label WHERE {
  ?station text:query (rdfs:label "brussels") ; rdfs:label ?label .
}
ORDER BY ?label
```

<!-- test: output -->
```text
station,label
https://example.org/rail#BXC,Brussels-Central
https://example.org/rail#BXN,Brussels-North
https://example.org/rail#BXS,Brussels-South
```

Lucene's query syntax works too: `AND`, `NOT`, `+word` and `-word`, `"a phrase"` and a
trailing `*`. The `lang:` option keeps the literals in one language and matches the other
forms of its words (`"lang:en"` finds "stations" for `station`), and `highlight:` marks the
words matched in `?literal`:

<!-- test: query -->
```sparql
PREFIX text: <http://jena.apache.org/text#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?station ?shown WHERE {
  (?station ?score ?shown) text:query (rdfs:label "+brussels -north" "highlight:s:[ | e:]") .
}
ORDER BY ?shown
```

<!-- test: output -->
```text
station,shown
https://example.org/rail#BXC,[Brussels]-Central
https://example.org/rail#BXS,[Brussels]-South
```

The query string can come from the rest of the query, as in Jena: a `VALUES`, a `BIND` or a
triple pattern that binds it. Each string is searched on its own, with its own limit:

<!-- test: query -->
```sparql
PREFIX text: <http://jena.apache.org/text#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?q ?station WHERE {
  VALUES ?q { "central" "south" }
  ?station text:query (rdfs:label ?q) .
}
ORDER BY ?q ?station
```

<!-- test: output -->
```text
q,station
central,https://example.org/rail#ANT
central,https://example.org/rail#BXC
south,https://example.org/rail#BXS
```

The index covers `rdfs:label`, `rdfs:comment`, `skos:prefLabel`, `skos:altLabel` and the
Dublin Core titles and descriptions. The fields of your own `text:entityMap` become
`--text-properties DATASET=IRI`, once per property (or `text-properties:` in the dataset's entry
of a [configuration file](#a-configuration-file)); `tripletrail config import-fuseki` carries them
over. Here the stations' codes are indexed too:

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl --text-properties ds=https://example.org/rail#code
```

<!-- test: query -->
```sparql
PREFIX text: <http://jena.apache.org/text#>
PREFIX ex: <https://example.org/rail#>
SELECT ?station WHERE {
  ?station text:query (ex:code "bxn") .
}
```

<!-- test: output -->
```text
station
https://example.org/rail#BXN
```

Without the option, that query answers 400: `ex:code` is not an indexed property.

TripleTrail's own `tt:search` ranks the matches with a score: see
[full-text search](../full-text-search.md).

Full-text queries written for Blazegraph (`bds:search`), Stardog (`textMatch` and
`SERVICE fts:textMatch`), Virtuoso (`bif:contains`), QLever (`textSearch:` and
`ql:contains-word`) and Jena's older `apf:textMatch` run on the same index too. They match
literals, so the resource comes from a triple that has the literal:

<!-- test: query -->
```sparql
PREFIX bds: <http://www.bigdata.com/rdf/search#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?station ?label WHERE {
  ?label bds:search "brussels central" ; bds:matchAllTerms true .
  ?station rdfs:label ?label .
}
```

<!-- test: output -->
```text
station,label
https://example.org/rail#BXC,Brussels-Central
```

Virtuoso's string has its own syntax: words side by side must all match, and `AND`, `OR`,
`AND NOT`, phrases in quotes and a quoted word ending in `*` work as in Virtuoso. Declare the
`bif:` prefix, which Virtuoso has built in:

<!-- test: query -->
```sparql
PREFIX bif: <bif:>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?station WHERE {
  ?label bif:contains "'brussels*' AND NOT central" .
  ?station rdfs:label ?label .
}
ORDER BY ?station
```

<!-- test: output -->
```text
station
https://example.org/rail#BXN
https://example.org/rail#BXS
```

Stardog's search service, with a score and a limit:

<!-- test: query -->
```sparql
PREFIX fts: <tag:stardog:api:search:>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?station WHERE {
  SERVICE fts:textMatch {
    [] fts:query "central" ; fts:result ?label ; fts:score ?score ; fts:limit 5 .
  }
  ?station rdfs:label ?label .
}
ORDER BY ?station
```

<!-- test: output -->
```text
station
https://example.org/rail#ANT
https://example.org/rail#BXC
```

GraphDB's Lucene, Elasticsearch, Solr and OpenSearch connectors find resources rather than
literals; whatever index is named, it is the same index:

<!-- test: query -->
```sparql
PREFIX luc: <http://www.ontotext.com/connectors/lucene#>
PREFIX luc-index: <http://www.ontotext.com/connectors/lucene/instance#>
SELECT ?station WHERE {
  ?search a luc-index:stations ;
      luc:query "brussels -central" ;
      luc:entities ?station .
}
ORDER BY ?station
```

<!-- test: output -->
```text
station
https://example.org/rail#BXN
https://example.org/rail#BXS
```

### Spatial search

Queries that use jena-geosparql's `spatial:` property functions (`nearby`, `withinCircle`,
`withinBox`, `intersectBox`, `north`, `south`, `east`, `west` and their `…Geom` forms) run
unchanged too, with no spatial index to set up. The stations within 5 km of Brussels-Central:

<!-- test: query -->
```sparql
PREFIX spatial: <http://jena.apache.org/spatial#>
PREFIX units: <http://www.opengis.net/def/uom/OGC/1.0/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?station ?label WHERE {
  ?station spatial:nearby (50.8456 4.3571 5 units:kilometre) ;
           rdfs:label ?label .
}
ORDER BY ?label
```

<!-- test: output -->
```text
station,label
https://example.org/rail#BXC,Brussels-Central
https://example.org/rail#BXN,Brussels-North
https://example.org/rail#BXS,Brussels-South
```

The arguments, units and limits are Jena's: see
[Jena's spatial search](../spatial.md#jenas-spatial-search-spatial).

### SHACL validation

Fuseki's SHACL service is at the same place: post shapes to `/ds/shacl` and get the validation
report back. `graph=` picks the data: `default` (the default), `union` for the named graphs
as one, or a graph's IRI; `target=` checks one node only. A long validation does not hold up
writes: it checks the data as it was when it started, and updates go on meanwhile.

<!-- test: file shapes/speed.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .

ex:SpeedShape a sh:NodeShape ;
  sh:targetClass ex:Section ;
  sh:property [ sh:path ex:maxSpeed ; sh:maxInclusive 250 ] .
```

<!-- test: run -->
```sh
curl -s -X POST 'http://localhost:7878/ds/shacl?graph=default' \
  -H 'Content-Type: text/turtle' --data-binary @shapes/speed.ttl \
  | grep -oE 'sh:(focusNode|value|conforms) [^ ;]+'
```

<!-- test: output -->
```text
sh:focusNode <https://example.org/rail#S07>
sh:value 300
sh:conforms false
```

The report is Turtle unless `Accept` asks for another RDF format. [Chapter
6](06-validation-and-reasoning.md) validates with the shapes kept in the dataset or a
workspace, with a JSON summary as well.

### Jena rules

`tripletrail import-jena-rules` turns a rule file of Jena's `GenericRuleReasoner` into SHACL-AF
rules, which `tripletrail infer` and a dataset's shapes run. Each rule becomes a node shape with a
SPARQL `CONSTRUCT`: the rule's body is the `WHERE`, with its builtins (`ge`, `notEqual`,
`noValue`, `sum`, `strConcat`, `makeTemp`, …) as `FILTER` and `BIND`, and its head is the
template.

<!-- test: file fast.rules -->
```text
@prefix ex: <https://example.org/rail#> .

# A section where trains run at 160 km/h or more.
[fast: (?s rdf:type ex:Section), (?s ex:maxSpeed ?v), ge(?v, 160)
    -> (?s rdf:type ex:FastSection)]
```

<!-- test: run -->
```sh
tripletrail import-jena-rules fast.rules > fast.ttl
tripletrail infer --data network.ttl --shapes fast.ttl --output fast.nt
cut -d' ' -f1 fast.nt | sort
```

<!-- test: output contains -->
```text
<https://example.org/rail#S03>
<https://example.org/rail#S04>
<https://example.org/rail#S05>
<https://example.org/rail#S07>
<https://example.org/rail#S08>
<https://example.org/rail#S09>
<https://example.org/rail#S10>
1 rule translated, 0 left out
```

Backward rules (`<-`) whose head is a triple infer the same triples, so they are translated as
forward rules. A rule that SPARQL can't say, such as one with a functor, a builtin in its head
or `table`, is left out: the command lists it, with the reason, on standard error and at the end
of the Turtle. `@include <RDFS>` is not read; `--rdfs` on the server or `tripletrail entail` gives
the RDFS inferences.

## Without a server

Jena's command-line tools have counterparts that work on files, or on a dataset folder while
no server has it open:

| Jena | TripleTrail |
|---|---|
| `arq --data file.ttl --query q.rq` | `tripletrail query --data file.ttl --query q.rq` |
| `tdb2.tdbquery --loc DB` | `tripletrail query --location DIR --dataset kg --query …` |
| `tdb2.tdbupdate --loc DB` | `tripletrail update --location DIR --dataset kg --update …` |
| `tdb2.tdbdump --loc DB` | `tripletrail dump --location DIR --dataset kg` |
| `riot --output=nt file.ttl` | `tripletrail convert --to nt file.ttl` |
| `riot --validate`, `riot --count` | `tripletrail convert --validate`, `tripletrail convert --count` |

A query is given as text, as `@file`, or as the path of a `.rq` file. Rows come out as TSV and
graphs as Turtle, or as `--format` says (`csv`, `json`, `xml`; `nt`, `nq`, `trig`, …):

<!-- test: run -->
```sh
tripletrail query --data network.ttl --format csv \
  --query 'PREFIX ex: <https://example.org/rail#>
           SELECT ?code WHERE { ?s a ex:Station ; ex:code ?code } ORDER BY ?code LIMIT 3'
```

<!-- test: output contains -->
```text
code
ANT
BXC
BXN
```

An update on a dataset folder is kept there; `dump` writes it out as N-Quads, and `convert`
checks, counts or rewrites files as they stream, however large:

<!-- test: run -->
```sh
tripletrail load --location db --dataset kg network.ttl
tripletrail update --location db --dataset kg \
  --update 'PREFIX ex: <https://example.org/rail#> DELETE WHERE { ?s ex:maxSpeed ?v }'
tripletrail dump --location db --dataset kg --output kg.nq
tripletrail convert --count kg.nq
tripletrail convert network.ttl --output network.nt
tripletrail convert --validate network.nt
```

<!-- test: output contains -->
```text
158 quads before, 146 after
kg.nq: 146 triples
network.nt: valid, 158 triples
```

Jena's RDF/JSON (`.rj`, `application/rdf+json`), TriX (`.trix`, `application/trix`), RDF
Thrift (`.trdf`) and RDF Protobuf (`.prdf`) are read and written too, by these commands, by
`tripletrail load` and by the server's Graph Store and CONSTRUCT answers; SELECT answers come as
Thrift or Protobuf results when `Accept` asks for them:

<!-- test: run -->
```sh
tripletrail convert network.ttl --output network.rj
tripletrail convert --count network.rj
curl -s http://localhost:7878/ds/query -H 'Accept: application/rdf+json' \
  --data-urlencode 'query=CONSTRUCT { ?s ?p ?o } WHERE { ?s <https://example.org/rail#code> "ANT" ; ?p ?o }' \
  | head -2
```

<!-- test: output contains -->
```text
network.rj: 158 triples
{
  "https://example.org/rail#ANT": {
```

`tripletrail update --data file.ttl --update … --output new.ttl` runs an update on files and
writes the result, without a dataset folder. Turtle and N-Triples hold one graph, so `dump`
asks for `--graph IRI` (or `--graph default`) when the dataset has named graphs.

### A configuration file

Where Fuseki starts from an assembler file, TripleTrail can start from a YAML file: its keys are
the options of `tripletrail serve` (`bind`, `location`, `timeout`, …), and `datasets` lists the
datasets with their settings:

<!-- test: file tripletrail.yaml -->
```yaml
load: network.ttl
timeout: 30
datasets:
  - name: kg
    union-default-graph: true
    aliases: [live]
```

<!-- test: serve -->
```sh
tripletrail serve --config tripletrail.yaml
```

The dataset answers at `/kg/…`, and at `/live/…` through its alias:

<!-- test: query live -->
```sparql
PREFIX ex: <https://example.org/rail#>
SELECT (COUNT(*) AS ?stations) WHERE { ?s a ex:Station }
```

<!-- test: output -->
```text
stations
10
```

A flag or a `TRIPLETRAIL_*` variable wins over the file, so one file can serve several
environments. A dataset takes `name`, and optionally `path` (a dataset directory to attach),
`read-only`, `union-default-graph`, `rdfs`, `owl-rl`, `aliases` and `text-properties`; see
[the configuration file](../server.md#configuration-file).

A Fuseki `config.ttl` becomes such a file with `tripletrail config import-fuseki`. Given this
Fuseki configuration:

<!-- test: file config.ttl -->
```turtle
@prefix :       <#> .
@prefix fuseki: <http://jena.apache.org/fuseki#> .
@prefix ja:     <http://jena.hpl.hp.com/2005/11/Assembler#> .
@prefix tdb2:   <http://jena.apache.org/2016/tdb#> .

[] a fuseki:Server ;
   ja:context [ ja:cxtName "arq:queryTimeout" ; ja:cxtValue "30000" ] .

:service a fuseki:Service ;
    fuseki:name "kg" ;
    fuseki:endpoint [ fuseki:operation fuseki:query ; fuseki:name "sparql" ] ;
    fuseki:endpoint [ fuseki:operation fuseki:update ; fuseki:name "update" ] ;
    fuseki:endpoint [ fuseki:operation fuseki:query ; fuseki:name "ask-me" ] ;
    fuseki:dataset [ a tdb2:DatasetTDB2 ;
        tdb2:location "/fuseki/databases/kg" ;
        tdb2:unionDefaultGraph true ] .
```

<!-- test: run -->
```sh
tripletrail config import-fuseki config.ttl --output tripletrail.yaml
cat tripletrail.yaml
```

<!-- test: output -->
```text
# From a Fuseki configuration by tripletrail config import-fuseki.
timeout: 30
datasets:
  - name: kg
    # Fuseki's data in /fuseki/databases/kg: tdb2.tdbdump --loc /fuseki/databases/kg > kg.nq, then tripletrail load --location DIR --dataset kg kg.nq
    union-default-graph: true
# Not carried over:
#   - kg: the endpoint name "ask-me" (query); TripleTrail answers query at its standard names (/kg/query, /sparql, /update, /data, /get, /shacl)
1 setting(s) not carried over, listed at the end of the file
```

The services become datasets, with their union default graph, RDFS reasoner (`rdfs: true`),
OWL reasoner (approximated by `owl-rl: true`) and query timeout; a service with no update
endpoint is listed as read-only. The fields of a text index become the dataset's
`text-properties`. What the file can't say, such as endpoint names of your own or other context
settings, is listed at its end. The data itself moves with `tdb2.tdbdump` and `tripletrail load`.

### Compaction and syntax checks

Scripts that compact a TDB2 database from cron keep working: `POST /$/compact/kg` answers
with a task, as Fuseki does. TripleTrail folds its log by itself, and the task folds it now. The
syntax checks the Fuseki UI calls are there too, under `/$/validate/` (`query`, `update`,
`data`, `iri` and `langtag`):

<!-- test: run -->
```sh
curl -s -o /dev/null -w '%{http_code}\n' -X POST 'http://localhost:7878/$/compact/kg'
curl -s 'http://localhost:7878/$/validate/query' \
  --data-urlencode 'query=SELECT * { ?s ?p }' | grep -o '"parse-error-line":[0-9]*'
curl -s 'http://localhost:7878/$/validate/langtag?langtag=nl-BE' | grep -o '"valid":[a-z]*'
```

<!-- test: output -->
```text
202
"parse-error-line":1
"valid":true
```

### RDF Patch

Fuseki's patch service is at `/kg/patch`. It takes an RDF Patch, the format `tripletrail diff
--format rdf-patch` writes, and applies it as one update, all of it or nothing:

<!-- test: file change.rdfp -->
```text
TX .
PA "ex" <https://example.org/rail#> .
A ex:station-new a ex:Station .
A ex:station-new ex:name "Nieuw station"@nl .
TC .
```

<!-- test: run -->
```sh
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:7878/kg/patch \
  -H 'Content-Type: application/rdf-patch' --data-binary @change.rdfp
curl -s http://localhost:7878/kg/sparql -H 'Accept: text/csv' \
  --data-urlencode 'query=SELECT (COUNT(*) AS ?stations) WHERE { ?s a <https://example.org/rail#Station> }'
```

<!-- test: output -->
```text
200
stations
11
```

### Custom functions

Fuseki adds functions and property functions with Java code. In TripleTrail a function is a
SPARQL query, declared with SHACL-AF as an `sh:SPARQLFunction` and called by its IRI in any
query; see [chapter 6](06-validation-and-reasoning.md#your-own-functions). Spatial, text and path functions that
Jena gives as property functions are built in.

## Small differences

Where the SPARQL standard and Fuseki differ, TripleTrail does as Fuseki does: an update is
answered `200`, a Graph Store `POST` without `?graph` adds to the default graph, and `NOW()`
is the server's local time with its offset. `MIN` and `MAX` over values of different kinds
pick the value Jena picks: plain strings come before numbers, and of equal numbers the one
written last in text order wins, so `MAX` of `3.0`, `3` and `3.0e0` is `3.0e0`:

<!-- test: run -->
```sh
curl -s http://localhost:7878/kg/sparql -H 'Accept: text/csv' \
  --data-urlencode 'query=SELECT (MIN(?v) AS ?min) (MAX(?v) AS ?max) WHERE { VALUES ?v { "3" 2.50 3.0 3 3.0e0 } }'
```

<!-- test: output -->
```text
min,max
3,3.0e0
```

Division of integers and decimals, and `AVG`, also give Jena's answer: the exact quotient when
it ends, and otherwise 24 fraction digits, rounded half to even:

<!-- test: run -->
```sh
curl -s http://localhost:7878/kg/sparql -H 'Accept: text/csv' \
  --data-urlencode 'query=SELECT (1.0 / 3.0 AS ?third) (2 / 3 AS ?two) (1 / 1024 AS ?exact) (AVG(?v) AS ?avg) WHERE { VALUES ?v { 1 2 2 } }'
```

<!-- test: output -->
```text
third,two,exact,avg
0.333333333333333333333333,0.666666666666666666666667,0.0009765625,1.666666666666666666666667
```

Arithmetic on such a quotient goes by its value with 18 fraction digits, so `1 / 3 * 3` is
`0.999999999999999999` where Jena gives `0.999999999999999999999999`.

A few things differ on purpose:

- Typed literals keep the form they were loaded in (`"100.0"` stays `"100.0"`), while
  comparisons and arithmetic go by value.
- Without a property, `text:query` searches every indexed label property, not only
  `rdfs:label`, and scores are BM25 scores, not Lucene's.

The full table, with backups, blue-green releases and the rest of the admin API, is in
[coming from Fuseki](../server.md#coming-from-fuseki).

Next: [access control](12-access-control.md).
