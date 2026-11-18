# 9. The Workbench

The Workbench is a playground in the web UI for writing shapes, rules, queries and pipelines
against a copy of some data. You see results as you type, and nothing you do there changes a
dataset. When the work is ready, you take the files to a pipeline, a test suite or a dataset's
own shapes.

Start the server with the sample network, and open <http://localhost:7878/$/ui/workbench>:

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl
```

## Workspaces

A workspace is a folder of files and a scratch store built from them:

```text
data/      RDF files: the data you work on
shapes/    SHACL shapes
rules/     SHACL rules
queries/   SPARQL queries        updates/   SPARQL updates
lift/      lift queries and mappings
tests/     regression tests      pipeline.yaml
```

**New workspace** starts an empty one or one from an example (validation, rules, a fixpoint,
rules before validation…). A scratch workspace is deleted after a day unused; make it *saved*
to keep it. With a server `--location`, workspaces outlive a restart.

## Work on a sample of real data

**Sample…** copies instances of some classes of a dataset, and what they point to, into
`data/`. On a dataset of millions of triples, a few hundred tracks with their surroundings is
enough to write shapes and rules against, and every run is instant. Pick the dataset `ds` and
the class `ex:Section`.

To work on the whole dataset instead, **Overlay…** makes the workspace read it as it is. Updates
then go into the workspace only, and its changes can be downloaded as an RDF Patch for review.

## Write and run

- The **editor** checks Turtle and SPARQL as you type and marks errors on their line. It
  completes prefixes, the classes and properties of the data, and SHACL terms, and hovering an
  IRI shows its label and counts. **Ctrl+Enter** runs the open file.
- **Query** runs a file of `queries/` with a table of results and **Explain**.
- **Validate** runs the shapes of `shapes/` on the data, after the rules of `rules/`: results
  grouped by shape and constraint, what is new or fixed since the last run, and which classes
  no shape covers.
- **Rules** runs the rules and shows each inferred triple with the rule, focus node and pass
  that made it. **why?** on any triple tells where it came from. **Show: strata** explains
  the order the rules ran in (see [below](#why-rules-run-in-this-order)).
- **Reason** shows what an RDFS schema or OWL 2 RL ontology in `data/` entails: the triples,
  how many per class and property, and under OWL 2 RL the inconsistencies. **why?** on a
  triple gives its derivation (see [below](#why-a-triple-is-entailed)).
- **Tests** runs `tests/` (see [chapter 8](08-pipelines.md#tests)), and **Save as test…** on
  the Query, Validate and Rules tabs makes a test from what the tab shows now.
- **Pipeline** runs `pipeline.yaml` step by step, with the triples each step added and removed,
  and **Query after** on the store as a step left it. **Airflow DAG** and **Python script** give
  the pipeline as code.

## Why rules run in this order

TripleTrail does not simply run rules in `sh:order`. A rule runs after every rule whose triples
it may read, and rules that feed each other form a group that repeats until nothing new comes
out. The Rules tab's **strata** view shows these groups in run order, and for each rule which
rules it reads from, through which predicates. A rule that reads a predicate only under
`FILTER NOT EXISTS` runs after every rule that writes it, whatever its `sh:order`: it would
otherwise see a half-written graph.

Take two rules: the first marks electrified sections, the second reads those marks.
The second has the lower `sh:order`, but it still runs last:

<!-- test: file rules/electric.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .

ex:SectionRules a sh:NodeShape ;
  sh:targetClass ex:Section ;
  sh:rule ex:markElectric , ex:fastIfElectric .

ex:markElectric a sh:SPARQLRule ; sh:order 2 ;
  sh:construct "CONSTRUCT { $this a <https://example.org/rail#ElectricSection> } WHERE { $this <https://example.org/rail#electrified> true }" .

ex:fastIfElectric a sh:SPARQLRule ; sh:order 1 ;
  sh:construct "CONSTRUCT { $this <https://example.org/rail#fast> true } WHERE { $this a <https://example.org/rail#ElectricSection> ; <https://example.org/rail#maxSpeed> ?v FILTER(?v >= 160) }" .
```

The plan behind the view comes from `…/infer?plan=true`:

<!-- test: run -->
```sh
W=http://localhost:7878/\$/workspaces
ID=$(curl -s -X POST $W -H 'Content-Type: application/json' -d '{"name": "order"}' \
  | sed -E 's/.*"id":"([^"]+)".*/\1/')
curl -s -X POST $W/$ID/sample -H 'Content-Type: application/json' \
  -d '{"dataset": "ds", "classes": ["https://example.org/rail#Section"]}' > /dev/null
curl -s -X PUT $W/$ID/files/rules/electric.ttl --data-binary @rules/electric.ttl
curl -s -X POST "$W/$ID/infer?plan=true" | grep -o '"group":[0-9]*,"recursive":[a-z]*'
```

<!-- test: output contains -->
```text
"group":1,"recursive":false
"group":2,"recursive":false
```

`ex:fastIfElectric` is in group 2: it reads `rdf:type ex:ElectricSection`, which
`ex:markElectric` writes in group 1. Neither group repeats, as neither rule reads what its own
group writes.

## Why a triple is entailed

The **Reason** tab runs RDFS (as [chapter 6](06-validation-and-reasoning.md#let-a-schema-do-the-work-rdfs)
does) or OWL 2 RL on the data of a workspace, leaving its shapes and rules out. **why?** on an
entailed triple shows its derivation tree: the rule that gives it, such as `rdfs9` (members of
a sub class are members of the class), and its premises, rule by rule down to triples of the
data. Under OWL 2 RL the rules have their OWL 2 RL names, such as `cax-sco`.

With chapter 6's schema in a workspace beside the sections:

<!-- test: file schema.ttl -->
```turtle
@prefix ex:   <https://example.org/rail#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

ex:Station rdfs:subClassOf ex:NetworkElement .
ex:Section rdfs:subClassOf ex:NetworkElement .
ex:HighSpeedSection rdfs:subClassOf ex:Section .
ex:from rdfs:range ex:Station .
ex:to   rdfs:range ex:Station .
```

<!-- test: run -->
```sh
W=http://localhost:7878/\$/workspaces
ID=$(curl -s -X POST $W -H 'Content-Type: application/json' -d '{"name": "schema"}' \
  | sed -E 's/.*"id":"([^"]+)".*/\1/')
curl -s -X POST $W/$ID/sample -H 'Content-Type: application/json' \
  -d '{"dataset": "ds", "classes": ["https://example.org/rail#Section"]}' > /dev/null
curl -s -X PUT $W/$ID/files/data/schema.ttl --data-binary @schema.ttl
curl -s -X POST "$W/$ID/reason?regime=rdfs&limit=1" | grep -o '"count":[0-9]*' | head -n 1
curl -s -X POST "$W/$ID/reason/why" --data-urlencode 'subject=<https://example.org/rail#BXS>' \
  --data-urlencode 'predicate=<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>' \
  --data-urlencode 'object=<https://example.org/rail#NetworkElement>' -G \
  | grep -o '"rule":[a-z0-9"-]*'
```

<!-- test: output contains -->
```text
"count":22
"rule":"rdfs9"
"rule":null
```

Brussels-South is a network element by `rdfs9`: it is a station, and stations are network
elements. Both premises are in the data (`"rule":null`).

## Configure with AI

The **AI** tab asks a model for shape changes in plain words, such as "show stations in blue and
colour sections by maximum speed". Bring your own key: **Key and model…** takes an
OpenAI-compatible service (OpenAI, Anthropic, Mistral, Scaleway, Groq, OpenRouter), or a local
model in Ollama or LM Studio. It is the same setting as the assistant of the Query page. The key
stays in your browser, and requests go only to that service.

Pick the dataset to configure and ask. The assistant:

1. Reads the dataset's profile (its classes, properties and geometry paths) and the shapes in use.
2. Writes the shape files it needs. Ask it to put a class in a profile (`tts:inProfile`), such as
   "show signals only in the maintenance profile", and it changes that class's shape for you.
3. Checks each file before it shows it to you.

Each file comes with a diff and **Apply to shapes/…**, which writes it into the workspace. You
can then validate it here and **Promote shapes…** as usual. From the Map or Search page of a dataset, **Configure with AI**
opens a new workspace with a copy of the dataset's own shapes and the AI tab ready on that
dataset, so promoting keeps the shapes you did not ask to change. The assistant never changes a
dataset. It sees values of the data only when **may query the data** is ticked; then it can run
read-only queries.

The check is the server's own, and works on any dataset:

<!-- test: file shapes/stations.ttl -->
```turtle
@prefix ex:  <https://example.org/rail#> .
@prefix sh:  <http://www.w3.org/ns/shacl#> .
@prefix tts: <http://w3id.org/tripletrail/shapes#> .

ex:StationShape a sh:NodeShape ;
  sh:targetClass ex:Station ;
  tts:color "#1f6feb" .
```

<!-- test: run -->
```sh
curl -s -X POST 'http://localhost:7878/$/datasets/ds/shapes/validate?file=stations.ttl' \
  -H 'Content-Type: text/turtle' --data-binary @shapes/stations.ttl
```

<!-- test: output -->
```text
{"errors":[],"valid":true}
```

### Preview on the map

**Preview on map**, next to **Apply**, opens the Map page drawn with the proposed file and the
workspace's other `shapes/` files, before anything is applied or promoted. The same button in
the tab's bar previews the workspace's shapes as they are. A banner says it is a preview, and
the dataset's own map is unchanged.

The preview is a server call: post the shape files, as Turtle or as JSON
`{"files":[{"name":…,"text":…}]}`, to `/ds/tiles/preview`. It answers with an id and a MapLibre
style that draws the dataset with those shapes:

<!-- test: file shapes/stops.ttl -->
```turtle
@prefix ex:  <https://example.org/rail#> .
@prefix sh:  <http://www.w3.org/ns/shacl#> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
@prefix tts: <http://w3id.org/tripletrail/shapes#> .

ex:StationShape a sh:NodeShape ;
  sh:targetClass ex:Station ;
  tts:key "Stop" ;
  tts:color "#1f6feb" ;
  tts:geometry [ sh:path ( geo:hasGeometry geo:asWKT ) ] .
```

<!-- test: run -->
```sh
id=$(curl -s -X POST http://localhost:7878/ds/tiles/preview \
  -H 'Content-Type: text/turtle' --data-binary @shapes/stops.ttl | sed 's/.*"id":"\([0-9a-f]*\)".*/\1/')
curl -s "http://localhost:7878/ds/tiles/preview/$id/style.json" | grep -o '"id":"[A-Za-z]*","instances":[0-9]*'
```

<!-- test: output -->
```text
"id":"Section","instances":12
"id":"Stop","instances":10
```

Stations are the `Stop` layer now. The style's tiles, search and filters live under
`/ds/tiles/preview/ID/`, and `/$/ui/map/ds?preview=ID` shows it. Previews are kept for an hour.
Files that do not load are refused with what is wrong, as `422`.


### Rules, queries, tests and explanations

The same tab drafts the rest of a workspace from a description (#409): "a rule that marks
sections faster than 200 km/h", "a query for the stations of line 25", "an update that renames
this property", "a test for this query". Rules go to `rules/`, queries to `queries/`, updates
to `updates/` and tests to a folder of `tests/`; it never writes `data/` or the pipeline, and
it never runs an update: you apply it and run it as any other. With **may query the data**
ticked, it also tries its queries on the workspace's data and runs the saved tests.

To have a result explained, click **explain** next to a validation result, or **Explain with
AI** in a why? panel. The assistant says what the data has and what the shape or rule asks,
names the value and path, and proposes the smallest fix as a file.

Before it shows a file, the assistant checks it the way the server would read it. The check is
the workspace's own, and you can use it too: it writes nothing.

<!-- test: run -->
```sh
WS=$(curl -s -X POST http://localhost:7878/$/workspaces \
  -H 'Content-Type: application/json' -d '{"name": "check"}' | grep -o '"id":"[^"]*"' | cut -d'"' -f4)
curl -s -X POST http://localhost:7878/$/workspaces/$WS/check \
  -H 'Content-Type: application/json' \
  -d '{"path": "queries/fast.rq", "text": "SELECT ?s WHERE { ?s ?p }"}'
```

<!-- test: output -->
```text
{"errors":["queries/fast.rq: error at 1:26: expected one of LATERAL, SERVICE, [_]"],"path":"queries/fast.rq","valid":false}
```

## Take the work out

- **Promote shapes…** makes the workspace's shapes the dataset's own shapes, which drive its
  map, describe pages and APIs ([chapter 10](10-maps-and-apis.md)). Earlier versions are kept
  for a rollback.
- **Copy as Python** gives the code that does what a tab does, with the `tripletrail` Python
  package, for a notebook or a production script.
- **Share link…** puts the files themselves in a link, to open on the same server.
- **Pull…** writes the files of a folder of a git repository over the workspace's.

## From a script or notebook

Everything the Workbench does is an HTTP API, so scripts and notebooks can drive it too. Create
a workspace, sample the sections, add a shapes file and validate:

<!-- test: file shapes/sections.ttl -->
```turtle
@prefix ex:  <https://example.org/rail#> .
@prefix sh:  <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:SectionShape a sh:NodeShape ;
  sh:targetClass ex:Section ;
  sh:property [ sh:path ex:electrified ; sh:datatype xsd:boolean ; sh:minCount 1 ] ;
  sh:property [ sh:path ex:maxSpeed ; sh:maxInclusive 250 ; sh:severity sh:Warning ] .
```

<!-- test: run -->
```sh
W=http://localhost:7878/\$/workspaces
ID=$(curl -s -X POST $W -H 'Content-Type: application/json' -d '{"name": "sections"}' \
  | sed -E 's/.*"id":"([^"]+)".*/\1/')
curl -s -X POST $W/$ID/sample -H 'Content-Type: application/json' \
  -d '{"dataset": "ds", "classes": ["https://example.org/rail#Section"]}'
echo
curl -s -X PUT $W/$ID/files/shapes/sections.ttl --data-binary @shapes/sections.ttl
curl -s -X POST $W/$ID/validate | grep -o '"conforms":[a-z]*\|"resultCount":[0-9]*'
```

<!-- test: output contains -->
```text
"instances":12
"conforms":false
"resultCount":1
```

The Python package wraps the same API, and its runs show up in the Workbench's **History**
tab:

```python
import tripletrail

ws = tripletrail.connect("http://localhost:7878", workspace="sections")
report = ws.validate()
print(report.conforms, len(report.results))
```

See [workspaces](../server.md#workbench-workspaces) for the whole API and the
[Python package](../../python/README.md#a-server-workspaces-and-datasets).

## What you learnt

- A workspace is a folder of data, shapes, rules, queries, tests and a pipeline, with a
  scratch store; it never changes a dataset.
- **Sample…** or **Overlay…** brings in real data; the tabs run queries, validation, rules,
  tests and pipelines as you edit.
- Shapes are promoted to a dataset, and the rest leaves as files, Python or a pipeline.
- The **AI** tab writes and checks shape changes, rules, queries, updates and tests with your
  own key, and explains validation results and why? answers; you apply what it writes to files
  and promote them.

Next: [maps and APIs](10-maps-and-apis.md).
