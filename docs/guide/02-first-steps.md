# 2. Your first dataset and query

In this chapter you start a server with the sample network, look at it in the web UI, and ask
it questions in SPARQL, from the browser and from the command line.

## Get the sample data

The sample network is [`data/network.ttl`](data/network.ttl) in this folder of the repository.
Copy it to a folder of your own and open a terminal there. If you did not clone the
repository, download it:

```sh
curl -LO https://raw.githubusercontent.com/Matdata-eu/tripletrail/main/docs/guide/data/network.ttl
```

It is [Turtle](https://www.w3.org/TR/turtle/), the most readable way to write RDF. Each
statement is a triple: a subject, a property and a value. A station and a section look like
this:

```turtle
ex:BXC a ex:Station ; rdfs:label "Brussels-Central" ; ex:code "BXC" ;
  geo:hasGeometry [ geo:asWKT "POINT(4.3571 50.8456)"^^geo:wktLiteral ] .

ex:S03 a ex:Section ; ex:line "25" ; ex:from ex:BXN ; ex:to ex:MEC ;
  ex:lengthKm 20 ; ex:maxSpeed 160 ; ex:electrified true ;
  geo:hasGeometry [ geo:asWKT "LINESTRING(4.3606 50.8603, 4.4828 51.0177)"^^geo:wktLiteral ] .
```

`a` means "is a" (`rdf:type`), `;` starts another statement about the same subject, and the
`[ … ]` is a geometry node of its own. Geometries are written as WKT text with longitude first,
as [GeoSPARQL](https://docs.ogc.org/is/22-047r1/22-047r1.html) does.

## Start the server

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl
```

The server loads the file into a dataset named `ds`, kept in memory, and listens on
<http://localhost:7878>. It runs until you stop it with Ctrl+C. Leave it running and open a
second terminal for the commands below.

## Look around in the web UI

Open <http://localhost:7878/> in a browser.

- **Datasets** lists `ds` with its number of triples.
- **Query** is where you write SPARQL. Results show as a table, and the **Map** tab of the
  results draws the geometries they hold.
- **Map** shows the whole dataset on OpenStreetMap: a layer for the stations and one for the
  sections. Click a station to read everything known about it.

Wherever a page shows an IRI, such as `https://example.org/rail#BXC` beside the station you
clicked, the small button after it copies the IRI. **Share** in the top bar copies a link to
what you see: a colleague who opens it gets the same page, with the same query, map position,
filters, open feature or shape. They still need their own access to the dataset.

## Ask a question

SPARQL queries describe the triples they look for, with `?variables` where the answer goes.
Paste this in the **Query** page and run it (Ctrl+Enter) to list the stations by name:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?code ?name WHERE {
  ?station a ex:Station ; ex:code ?code ; rdfs:label ?name .
}
ORDER BY ?name
```

<!-- test: output -->
```text
code,name
ANT,Antwerp-Central
BXC,Brussels-Central
BXN,Brussels-North
BXS,Brussels-South
GNT,Ghent-Sint-Pieters
LV,Leuven
LG,Liège-Guillemins
MEC,Mechelen
NAM,Namur
OTN,Ottignies
```

Patterns join on shared variables. This one follows each section to the names of the stations
at its ends, and keeps the sections faster than 150 km/h:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?from ?to ?speed WHERE {
  ?section a ex:Section ; ex:from ?a ; ex:to ?b ; ex:maxSpeed ?speed .
  ?a rdfs:label ?from .
  ?b rdfs:label ?to .
  FILTER(?speed > 150)
}
ORDER BY DESC(?speed) ?from
```

<!-- test: output -->
```text
from,to,speed
Leuven,Liège-Guillemins,300
Brussels-North,Mechelen,160
Brussels-North,Leuven,160
Brussels-South,Ghent-Sint-Pieters,160
Brussels-South,Ottignies,160
Mechelen,Antwerp-Central,160
Ottignies,Namur,160
```

Aggregates count and sum over groups. The length of the network per line:

<!-- test: query -->
```sparql
PREFIX ex: <https://example.org/rail#>
SELECT ?line (COUNT(?section) AS ?sections) (SUM(?km) AS ?km) WHERE {
  ?section a ex:Section ; ex:line ?line ; ex:lengthKm ?km .
}
GROUP BY ?line
ORDER BY DESC(?km) ?line
```

<!-- test: output -->
```text
line,sections,km
2,1,64
161,2,63
125,1,60
59,1,60
50A,1,55
25,2,43
36,1,30
53,1,24
0,2,4
```

## The same from the command line

Every dataset answers at the same URLs as Apache Jena Fuseki, so any SPARQL client works with
it. `/ds/query` takes a query; the `Accept` header picks the format of the answer (JSON by
default, CSV here):

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/query -H 'Accept: text/csv' \
  --data-urlencode 'query=SELECT (COUNT(*) AS ?triples) WHERE { ?s ?p ?o }'
```

<!-- test: output -->
```text
triples
158
```

A longer query is easier to keep in a file. Save the query that lists the stations as
`stations.rq`:

<!-- test: file stations.rq -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?code ?name WHERE {
  ?station a ex:Station ; ex:code ?code ; rdfs:label ?name .
}
ORDER BY ?name
LIMIT 3
```

and send it with `@`:

<!-- test: run -->
```sh
curl -s http://localhost:7878/ds/query -H 'Accept: text/csv' --data-urlencode query@stations.rq
```

<!-- test: output -->
```text
code,name
ANT,Antwerp-Central
BXC,Brussels-Central
BXN,Brussels-North
```

`Accept: application/sparql-results+json` (the default) gives JSON for programs, and
`text/tab-separated-values` TSV.

## What you learnt

- `tripletrail serve --load FILE` starts a server with a file in an in-memory dataset `ds`.
- The web UI at <http://localhost:7878/> queries, maps and describes the data.
- `/ds/query` answers SPARQL from any client, in the format the `Accept` header asks for.

The data is gone when the server stops. Next, [load, change and keep data](03-data.md).
