# 8. Pipelines and tests

The chapters so far ran one command at a time. A real job chains them: lift some files, load
others, fix things with updates, run rules, check the result and export it. A **pipeline**
writes that chain down in one `pipeline.yaml`, runs it the same way on your machine, in CI or
from Airflow, and reports what each step did. **Tests** check that shapes, rules and queries
keep doing what you expect.

This chapter reuses the files of chapters 6 and 7: `shapes.ttl`, `rules.ttl`, `stations.csv`
and `lift/stations.rq`.

<!-- test: file shapes.ttl -->
```turtle
@prefix ex:  <https://example.org/rail#> .
@prefix sh:  <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:SectionShape a sh:NodeShape ;
  sh:targetClass ex:Section ;
  sh:property [ sh:path ex:from ; sh:class ex:Station ; sh:minCount 1 ; sh:maxCount 1 ] ;
  sh:property [ sh:path ex:to ; sh:class ex:Station ; sh:minCount 1 ; sh:maxCount 1 ] ;
  sh:property [ sh:path ex:electrified ; sh:datatype xsd:boolean ; sh:minCount 1 ] ;
  sh:property [ sh:path ex:maxSpeed ; sh:datatype xsd:integer ; sh:maxInclusive 250 ;
                sh:severity sh:Warning ; sh:message "Above 250 km/h: check the speed" ] .
```

<!-- test: file rules.ttl -->
```turtle
@prefix ex:  <https://example.org/rail#> .
@prefix sh:  <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:prefixes sh:declare [ sh:prefix "ex" ; sh:namespace "https://example.org/rail#"^^xsd:anyURI ] .

ex:SectionRules a sh:NodeShape ;
  sh:targetClass ex:Section ;
  sh:rule [ a sh:SPARQLRule ; sh:prefixes ex:prefixes ; sh:construct """
      CONSTRUCT { ?a ex:neighbour ?b . ?b ex:neighbour ?a . }
      WHERE { $this ex:from ?a ; ex:to ?b . }""" ] ;
  sh:rule [ a sh:SPARQLRule ; sh:prefixes ex:prefixes ; sh:construct """
      CONSTRUCT { $this a ex:HighSpeedSection . }
      WHERE { $this ex:maxSpeed ?speed . FILTER(?speed >= 250) }""" ] .
```

<!-- test: file stations.csv -->
```text
code,name,lon,lat,platforms
BRG,Bruges,3.2170,51.1972,10
CRL,Charleroi-Central,4.4386,50.4045,8
KOR,Kortrijk,3.2644,50.8243,7
```

<!-- test: file lift/stations.rq -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX fx:   <http://sparql.xyz/facade-x/ns/>
PREFIX geo:  <http://www.opengis.net/ont/geosparql#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX xsd:  <http://www.w3.org/2001/XMLSchema#>
PREFIX xyz:  <http://sparql.xyz/facade-x/data/>
CONSTRUCT {
  ?station a ex:Station ; rdfs:label ?name ; ex:code ?code ; ex:platforms ?platforms ;
    geo:hasGeometry ?geometry .
  ?geometry geo:asWKT ?wkt .
}
WHERE {
  SERVICE <x-sparql-anything:location=stations.csv,csv.headers=true> {
    ?row xyz:code ?code ; xyz:name ?name ; xyz:lon ?lon ; xyz:lat ?lat ; xyz:platforms ?p .
  }
  BIND(fx:entity("https://example.org/rail#", ?code) AS ?station)
  BIND(fx:entity("https://example.org/rail#", ?code, "-geometry") AS ?geometry)
  BIND(xsd:integer(?p) AS ?platforms)
  BIND(STRDT(CONCAT("POINT(", ?lon, " ", ?lat, ")"), geo:wktLiteral) AS ?wkt)
}
```

## A pipeline

One more input: an update that connects Kortrijk to the network, as `updates/kortrijk.ru`:

<!-- test: file updates/kortrijk.ru -->
```sparql
PREFIX ex: <https://example.org/rail#>
INSERT DATA {
  ex:S13 a ex:Section ; ex:from ex:GNT ; ex:to ex:KOR ;
    ex:lengthKm 45 ; ex:maxSpeed 140 ; ex:electrified true .
}
```

The pipeline lists the steps in order. Each step is a map whose kind (`load`, `lift`,
`update`, `infer`, `validate`, `export`…) names its files, relative to the `pipeline.yaml`.
Save it as `pipeline.yaml`:

<!-- test: file pipeline.yaml -->
```yaml
name: rail-network
steps:
  - name: network
    load: network.ttl
  - name: new-stations
    lift: lift/
  - name: connect
    update: updates/
  - name: rules
    infer: rules.ttl
  - name: check
    validate: shapes.ttl
    fail_on: violation
  - name: export
    export: out/network.nt
```

- `load` reads RDF files, `lift` runs the queries and mappings of chapter 7, and `update`
  runs a SPARQL update, or every `.ru` file of a folder in name order.
- `infer` runs SHACL rules and keeps what they infer.
- `validate` is a gate: it stops the pipeline when the data has results of the `fail_on`
  severity or worse. The 300 km/h warning of chapter 6 does not stop it.
- `export` writes the data to a file, in the format of its extension.

Run it:

<!-- test: run -->
```sh
tripletrail pipeline run pipeline.yaml --report run.json
```

<!-- test: output contains -->
```text
step 1/6 network: ok in
step 2/6 new-stations: ok in
step 3/6 connect: ok in
step 4/6 rules: ok in
step 5/6 check: ok in
step 6/6 export: ok in
```

Each step prints its time and how many quads the dataset holds after it. `run.json` has the
same in detail: per step its status, time, peak memory, files, and for rules every rule's
focus nodes and new triples, to find the slow ones. Without `--location`, the pipeline runs on
an empty dataset in memory, for a job that exports what it makes; with
`--location db --dataset rail` it runs on that dataset on disk and leaves its result there.

## When the gate stops it

Add a second update that forgets to say whether the new section is electrified, as
`updates/kortrijk-bruges.ru`:

<!-- test: file updates/kortrijk-bruges.ru -->
```sparql
PREFIX ex: <https://example.org/rail#>
INSERT DATA { ex:S14 a ex:Section ; ex:from ex:KOR ; ex:to ex:BRG ; ex:lengthKm 50 }
```

<!-- test: run fails -->
```sh
tripletrail pipeline run pipeline.yaml --report run.json
```

<!-- test: output contains -->
```text
step 5/6 check: stopped
validation found results of severity violation or worse
```

The pipeline exits with status 1 and does not export, so a broken result never replaces the
last good file. `run.json` counts the results of the gate by severity. Add `report:
out/report.ttl` to the step to keep the validation report too.

`--from` and `--to` (a step's number or name) run part of a pipeline, on what earlier runs
left in an on-disk dataset: after fixing the update, `tripletrail pipeline run pipeline.yaml
--location db --dataset rail --from connect` starts again from there.

Pipelines also take parameters (`-v date=2026-01-01`), Python steps for code you already have,
and a `publish` step that writes a dataset folder a server can serve. They export to an Airflow
DAG with `tripletrail pipeline export pipeline.yaml --to airflow`. See
[pipelines](../server.md#pipelines).

## Tests

A test is a folder with an input, something to run on it, and the output you expect. This one
checks the high-speed rule: a section at 250 km/h is high-speed, one at 100 km/h is not. Make a
folder `tests/high-speed` with the input, `input.ttl`:

<!-- test: file tests/high-speed/input.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .
ex:fast a ex:Section ; ex:maxSpeed 250 .
ex:slow a ex:Section ; ex:maxSpeed 100 .
```

the rules to run, named in `test.yaml` (relative to the test's folder):

<!-- test: file tests/high-speed/test.yaml -->
```yaml
rules: ../../rules.ttl
```

and what they should infer, `expected.ttl`:

<!-- test: file tests/high-speed/expected.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .
ex:fast a ex:HighSpeedSection .
```

<!-- test: run -->
```sh
tripletrail test tests
```

<!-- test: output contains -->
```text
PASS high-speed
1 passed, 0 failed
```

A test may instead run a query (`query.rq` with `expected.csv` or `.srj`), updates
(`update.ru`, comparing the store after them), or shapes (`shapes.ttl`, with the
violations expected in `violations.yaml`). `tripletrail test` exits with status 1 when a test
fails and prints what is missing and unexpected, so run it in CI; `--junit results.xml` writes
a report CI tools read. `--record` writes what each test gives now as its expected output, to
start a test or accept a change. See [tests](../server.md#tests).

## What you learnt

- A `pipeline.yaml` chains load, lift, update, infer, validate and export steps;
  `tripletrail pipeline run` runs it and reports each step.
- A `validate` step is a gate that stops the pipeline before a bad result is exported.
- `tripletrail test` runs regression tests of rules, shapes, queries and updates.

The [Workbench](09-workbench.md) runs the same pipelines and tests step by step in the browser.
