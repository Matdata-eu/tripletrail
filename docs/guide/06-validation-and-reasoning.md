# 6. Check and enrich data: validation and reasoning

Data from many sources has gaps and mistakes. In this chapter you check the sample network
against rules written in [SHACL](https://www.w3.org/TR/shacl/), derive new facts from it with
SHACL rules, and let TripleTrail add what an RDFS schema implies.

All three work on files from the command line, on a dataset on disk, in a
[pipeline](../server.md#pipelines) and in the [Workbench](../server.md#workbench).

## Validate with SHACL shapes

A shape says what data of a class must look like. Save this as `shapes.ttl`: every section has
exactly one station at each end and says whether it is electrified, and a speed above
250 km/h is worth a warning:

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

Validate the network against it:

<!-- test: run fails -->
```sh
tripletrail validate --data network.ttl --shapes shapes.ttl
```

The report is the W3C validation report, in Turtle. It has one result, the high-speed line:

<!-- test: output contains -->
```text
sh:focusNode <https://example.org/rail#S07> ;
sh:resultPath <https://example.org/rail#maxSpeed> ;
sh:value 300 ;
sh:resultSeverity sh:Warning ;
sh:resultMessage "Above 250 km/h: check the speed" .
sh:conforms false .
does not conform in
```

`tripletrail validate` exits with status 1 when the data does not conform (a warning counts), so
a script or CI job can stop on it. `--report report.ttl` writes the report to a file.

`--stats` adds, for each shape, how many nodes it checked, how many distinct focus nodes it
had and how many of those have a result, so `ttsh:failingFocusNodes` divided by
`ttsh:distinctFocusNodes` is the share of the network that fails the shape. Here 1 of the 12
sections is too fast:

<!-- test: run fails -->
```sh
tripletrail validate --data network.ttl --shapes shapes.ttl --stats
```

<!-- test: output contains -->
```text
ttsh:distinctFocusNodes 12 ;
ttsh:failingFocusNodes 1 .
```

Each shape's statistics also name it with `ttsh:shapeIri`, the same in every run, so that you
can compare one run with the next. The property shapes above are written inline, as blank
nodes, so TripleTrail names each from its node shape and a hash of what it says. The speed
rule is:

<!-- test: run fails -->
```sh
tripletrail validate --data network.ttl --shapes shapes.ttl --stats
```

<!-- test: output contains -->
```text
ttsh:shapeIri <https://example.org/rail#SectionShape-1928b564a0725f04> ;
```

The name changes only when that property shape changes.

The report also sums the run up: how many results and failing focus nodes it has in all, and one
`ttsh:cell` for each rule, severity and constraint component with results, which links to its
first results as `ttsh:sample`. Here there is one cell, the speed warning:

<!-- test: run fails -->
```sh
tripletrail validate --data network.ttl --shapes shapes.ttl --stats
```

<!-- test: output contains -->
```text
ttsh:severity sh:Warning ;
ttsh:component sh:MaxInclusiveConstraintComponent ;
ttsh:results 1 ;
```

To validate a dataset on disk instead of files, give its location and the named graph that
holds the shapes: `tripletrail validate --location db --dataset rail --shapes-graph
https://example.org/graph/shapes`. A running server validates too:
`/$/datasets/rail/validate?shapes=<graph>` answers the results as JSON.

### Why a nested shape fails

A shape can hand a value to another shape with `sh:node` (or to several with `sh:and`, `sh:or`
and `sh:xone`). The result then says that the value fails the other shape, and its
`sh:detail`s say why. Save this as `nested.ttl`: a station's location must be a point with a
latitude and a longitude:

<!-- test: file nested.ttl -->
```turtle
@prefix ex:  <https://example.org/rail#> .
@prefix sh:  <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:StationShape a sh:NodeShape ;
  sh:targetClass ex:Station ;
  sh:property [ sh:path ex:location ; sh:node ex:PointShape ] .

ex:PointShape a sh:NodeShape ;
  sh:property [ sh:path ex:lat ; sh:datatype xsd:decimal ; sh:minCount 1 ] ;
  sh:property [ sh:path ex:long ; sh:datatype xsd:decimal ; sh:minCount 1 ] .
```

and this as `stations.ttl`, a station whose point has its latitude as text and no longitude:

<!-- test: file stations.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .

ex:Gent a ex:Station ; ex:location ex:GentPoint .
ex:GentPoint ex:lat "51.03" .
```

<!-- test: run fails -->
```sh
tripletrail validate --data stations.ttl --shapes nested.ttl
```

The report has one result for the station, with a detail for each thing wrong with the point:

<!-- test: output contains -->
```text
sh:focusNode <https://example.org/rail#Gent> ;
sh:sourceConstraintComponent sh:NodeConstraintComponent ;
sh:focusNode <https://example.org/rail#GentPoint> ;
sh:value "51.03" ;
sh:sourceConstraintComponent sh:DatatypeConstraintComponent ;
sh:sourceConstraintComponent sh:MinCountConstraintComponent ;
does not conform in
```

Details nest as deep as the shapes do. They are not counted as results of their own, and a
focus node's results have about 1,000 details at most.

### Rules in SPARQL, with messages that name the problem

When a rule is easier to write as a query, a `sh:sparql` constraint runs a `SELECT` for each
focus node, with `$this` bound to it; each row it returns is a result. Its `sh:message` can
name what the row found: `{$this}` and `{?var}` are filled in from the row. Save as
`loops.ttl`: a section must not lead back to itself:

<!-- test: file loops.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .

ex:NoLoops a sh:NodeShape ;
  sh:targetSubjectsOf ex:next ;
  sh:sparql [
    sh:message "{$this} ({?name}) leads back to itself" ;
    sh:select """
      SELECT $this ?name WHERE {
        $this <https://example.org/rail#next> $this .
        OPTIONAL { $this <https://example.org/rail#name> ?name }
      }""" ] .
```

and this as `links.ttl`, with one section that does:

<!-- test: file links.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .

ex:S20 ex:name "Gent - Brugge" ; ex:next ex:S20 .
ex:S21 ex:name "Brugge - Oostende" ; ex:next ex:S22 .
```

<!-- test: run fails -->
```sh
tripletrail validate --data links.ttl --shapes loops.ttl
```

<!-- test: output contains -->
```text
sh:resultMessage "https://example.org/rail#S20 (Gent - Brugge) leads back to itself" .
```

A variable the row leaves unbound stays as written. The parameters of a constraint component
you declare yourself fill in the same way, as `{$min}` for its `ex:min` parameter. Rows that
would make the same result (the same focus node, value, path and message), as a query that
joins through several paths returns, give it once.

### Read vocabularies without checking them

Shapes often read more than the data: `sh:class` looks up the class of a value, and a SPARQL
constraint may read a code list. Loading that vocabulary with `--data` validates it too, and its
own resources then show up in the report. Give it with `--lookup` instead: it is read, but only
the `--data` files give focus nodes. Save this as `track-shapes.ttl`: a track's kind must be a
track kind, and labels must be in English, Dutch or French:

<!-- test: file track-shapes.ttl -->
```turtle
@prefix ex:   <https://example.org/rail#> .
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

ex:TrackShape a sh:NodeShape ;
  sh:targetClass ex:Track ;
  sh:property [ sh:path ex:kind ; sh:class ex:TrackKind ] .

ex:LabelShape a sh:NodeShape ;
  sh:targetSubjectsOf rdfs:label ;
  sh:property [ sh:path rdfs:label ; sh:languageIn ( "en" "nl" "fr" ) ] .
```

this as `tracks.ttl`, two tracks, one of an unknown kind:

<!-- test: file tracks.ttl -->
```turtle
@prefix ex:   <https://example.org/rail#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

ex:T1 a ex:Track ; rdfs:label "Spoor 1"@nl ; ex:kind ex:Main .
ex:T2 a ex:Track ; rdfs:label "Spoor 2"@nl ; ex:kind ex:Unknown .
```

and this as `kinds.ttl`, a vocabulary from elsewhere, labelled in German:

<!-- test: file kinds.ttl -->
```turtle
@prefix ex:   <https://example.org/rail#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

ex:Main a ex:TrackKind ; rdfs:label "Hauptgleis"@de .
```

With both as data, the report has two results: T2, and the vocabulary's German label. (The
`||` counts them when the data does not conform.)

<!-- test: run -->
```sh
tripletrail validate --data tracks.ttl --data kinds.ttl --shapes track-shapes.ttl --report all.ttl 2>/dev/null ||
  grep -c sh:focusNode all.ttl
```

<!-- test: output -->
```text
2
```

With the vocabulary as a lookup, T1 still finds the class of its kind there, and only T2 fails:

<!-- test: run fails -->
```sh
tripletrail validate --data tracks.ttl --lookup kinds.ttl --shapes track-shapes.ttl
```

<!-- test: output contains -->
```text
sh:focusNode <https://example.org/rail#T2> ;
does not conform in
```

It is the only result:

<!-- test: run -->
```sh
tripletrail validate --data tracks.ttl --lookup kinds.ttl --shapes track-shapes.ttl --report data.ttl 2>/dev/null ||
  grep -c sh:focusNode data.ttl
```

<!-- test: output -->
```text
1
```

On a dataset on disk, `--lookup-graph <graph>` (repeatable) reads named graphs the same way,
with `--data-graph` the graph to validate.

## Derive facts with SHACL rules

SHACL rules ([SHACL-AF](https://www.w3.org/TR/shacl-af/)) add triples. A SPARQL rule runs a
`CONSTRUCT` for each focus node of its shape, with `$this` bound to it. Save as `rules.ttl`:
each section makes the stations at its ends neighbours, and a section of at least 250 km/h is
a high-speed section:

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

Run them and write what they infer to a file:

<!-- test: run -->
```sh
tripletrail infer --data network.ttl --shapes rules.ttl --output inferred.nt
grep HighSpeedSection inferred.nt
```

<!-- test: output contains -->
```text
inferred 25 triples
<https://example.org/rail#S07> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <https://example.org/rail#HighSpeedSection> .
```

Rules run until they infer nothing new, so a rule can build on what another one inferred. Only
new triples are written; on a dataset on disk, `--into GRAPH` writes them to a named graph.
`tripletrail validate` runs the rules of the shapes file first, so constraints can check what
the rules derive.

## Let a schema do the work: RDFS

An RDFS schema says how classes and properties relate. Save it as `schema.ttl`: stations and
sections are network elements, a high-speed section is a section, and what `ex:from` and
`ex:to` point to is a station:

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

`tripletrail entail` writes what the schema implies for the data, here that every station and
section is also a network element:

<!-- test: run -->
```sh
tripletrail entail --data schema.ttl --data network.ttl --output entailed.nt
```

<!-- test: output contains -->
```text
entailed 23 triples
```

### Kept up to date on a server

A server can keep the entailment current as the data changes, as Fuseki's RDFS reasoner does.
`--rdfs ds` keeps it in the named graph `urn:tripletrail:inferred`, and
`--union-default-graph ds` lets queries read it together with the data:

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl --load schema.ttl --rdfs ds --union-default-graph ds
```

<!-- test: query -->
```sparql
PREFIX ex: <https://example.org/rail#>
SELECT (COUNT(?element) AS ?elements) WHERE { ?element a ex:NetworkElement }
```

<!-- test: output -->
```text
elements
22
```

Add a high-speed section to a new station, and the types follow at once:

<!-- test: update -->
```sparql
PREFIX ex: <https://example.org/rail#>
INSERT DATA { ex:S13 a ex:HighSpeedSection ; ex:from ex:LV ; ex:to ex:BRU }
```

<!-- test: query -->
```sparql
PREFIX ex: <https://example.org/rail#>
SELECT ?type WHERE { ex:BRU a ?type }
ORDER BY ?type
```

<!-- test: output -->
```text
type
https://example.org/rail#NetworkElement
https://example.org/rail#Station
```

`--owl-rl ds` does the same with the OWL 2 RL rules (equivalent classes, inverse and transitive
properties, `owl:sameAs` and more), and `tripletrail entail --regime owl-rl` runs them once.

## Your own functions

A calculation you need in many queries can be written once, as data, and called by its name.
SHACL-AF's `sh:SPARQLFunction` declares one: its parameters, and a query that gives its value.
Save the travel time of chapter 5 as `functions.ttl`:

<!-- test: file functions.ttl -->
```turtle
@prefix ex:  <https://example.org/rail#> .
@prefix sh:  <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:minutes a sh:SPARQLFunction ;
  sh:parameter [ sh:path ex:km ; sh:order 1 ] ,
               [ sh:path ex:speed ; sh:order 2 ] ;
  sh:returnType xsd:decimal ;
  sh:select "SELECT (60 * $km / $speed AS ?minutes) WHERE {}" .
```

`--functions` lets every query and update on the server call it:

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl --functions functions.ttl
```

<!-- test: query -->
```sparql
PREFIX ex: <https://example.org/rail#>
SELECT ?section (ROUND(ex:minutes(?km, ?speed)) AS ?minutes) WHERE {
  ?section ex:from ex:LV ; ex:lengthKm ?km ; ex:maxSpeed ?speed .
}
ORDER BY ?section
```

<!-- test: output -->
```text
section,minutes
https://example.org/rail#S07,13.0
```

A parameter is named by the local name of its `sh:path` (`$km`, `$speed`). A body that is one
expression, like this one, costs no more than writing the expression in the query; a body
can also read the data, such as `SELECT ?name WHERE { $station rdfs:label ?name }`. Functions
in a shapes file can be called by its shapes and rules too, with `tripletrail validate` and
`tripletrail infer`.

## What you learnt

- `tripletrail validate` checks data against SHACL shapes and exits with status 1 when it does not
  conform.
- `tripletrail infer` runs SHACL rules until they infer nothing new.
- `tripletrail entail` and `serve --rdfs` (or `--owl-rl`) add what an RDFS or OWL schema implies,
  once or kept up to date.
- A `sh:sparql` constraint's `sh:message` names what each row found, with `{$this}` and
  `{?var}`.
- `sh:SPARQLFunction`s are functions you write as data; `serve --functions` makes them callable.

[Validation and reasoning](../validation-and-reasoning.md) has every option, the rest of
[user-defined functions](../validation-and-reasoning.md#user-defined-functions) and the OWL 2 RL
rules supported.

Next: [turn files into RDF](07-lifting.md).
