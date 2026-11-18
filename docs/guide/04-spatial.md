# 4. Spatial queries

TripleTrail answers spatial questions in SPARQL: distances, what lies within an area, what is near
what, lengths and areas, and geometry operations. It has two sets of functions that do the same
work with different names: GeoSPARQL's `geof:` and PostGIS's `st:`. Use the ones you know.
A spatial index answers them; there is nothing to set up.

Start the server with the sample network, as in chapter 2:

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl
```

## How geometries are written

A feature (a station) points to a geometry with `geo:hasGeometry`, and the geometry holds its
shape as WKT text with `geo:asWKT`. In a query, `geo:hasGeometry/geo:asWKT` follows both at
once:

```sparql
?station geo:hasGeometry/geo:asWKT ?wkt .
```

Coordinates are longitude then latitude, in degrees (the CRS84 system), unless the literal
starts with another coordinate system's IRI. Lines are `LINESTRING(x y, x y, …)` and areas
`POLYGON((x y, …, x y))`, with the first point repeated at the end.

## Distance

The stations within 30 km of Brussels-Central, nearest first. `geof:distance` takes two
geometries and a unit; `uom:metre` measures on the Earth's surface:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX geo:  <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX uom:  <http://www.opengis.net/def/uom/OGC/1.0/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?name ?km WHERE {
  ex:BXC geo:hasGeometry/geo:asWKT ?here .
  ?station a ex:Station ; rdfs:label ?name ; geo:hasGeometry/geo:asWKT ?wkt .
  BIND(geof:distance(?wkt, ?here, uom:metre) / 1000 AS ?distance)
  FILTER(?distance < 30)
  BIND(ROUND(?distance * 10) / 10 AS ?km)
}
ORDER BY ?km
```

<!-- test: output -->
```text
name,km
Brussels-Central,0
Brussels-North,1.7
Brussels-South,1.9
Mechelen,21.1
Ottignies,24.3
Leuven,25.6
```

The nearest station to a point, here the Atomium, is the first one ordered by distance:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX geo:  <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX uom:  <http://www.opengis.net/def/uom/OGC/1.0/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?name ?metres WHERE {
  ?station a ex:Station ; rdfs:label ?name ; geo:hasGeometry/geo:asWKT ?wkt .
  BIND(ROUND(geof:distance(?wkt, "POINT(4.3417 50.8949)"^^geo:wktLiteral, uom:metre)) AS ?metres)
}
ORDER BY ?metres
LIMIT 1
```

<!-- test: output -->
```text
name,metres
Brussels-North,4072
```

## Within an area

`geof:sfWithin` keeps what lies inside an area, and `geof:sfIntersects` what touches it. The
stations in a box around Brussels:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX geo:  <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?name WHERE {
  ?station a ex:Station ; rdfs:label ?name ; geo:hasGeometry/geo:asWKT ?wkt .
  FILTER(geof:sfWithin(?wkt,
    "POLYGON((4.2 50.75, 4.8 50.75, 4.8 51.05, 4.2 51.05, 4.2 50.75))"^^geo:wktLiteral))
}
ORDER BY ?name
```

<!-- test: output -->
```text
name
Brussels-Central
Brussels-North
Brussels-South
Leuven
Mechelen
```

Sections are lines, so ask which of them cross the box, even when they end outside it:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX geo:  <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
SELECT ?section ?line WHERE {
  ?section a ex:Section ; ex:line ?line ; geo:hasGeometry/geo:asWKT ?wkt .
  FILTER(geof:sfIntersects(?wkt,
    "POLYGON((4.2 50.75, 4.8 50.75, 4.8 51.05, 4.2 51.05, 4.2 50.75))"^^geo:wktLiteral))
}
ORDER BY ?section
```

<!-- test: output -->
```text
section,line
https://example.org/rail#S01,0
https://example.org/rail#S02,0
https://example.org/rail#S03,25
https://example.org/rail#S04,25
https://example.org/rail#S05,36
https://example.org/rail#S06,53
https://example.org/rail#S07,2
https://example.org/rail#S08,50A
https://example.org/rail#S09,161
```

## What is near what

A spatial join compares two sets of geometries. The pairs of stations less than 2.5 km apart
(`?a < ?b` keeps each pair once):

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX geo:  <http://www.opengis.net/ont/geosparql#>
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX uom:  <http://www.opengis.net/def/uom/OGC/1.0/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?a ?b ?metres WHERE {
  ?s1 a ex:Station ; rdfs:label ?a ; geo:hasGeometry/geo:asWKT ?p .
  ?s2 a ex:Station ; rdfs:label ?b ; geo:hasGeometry/geo:asWKT ?q .
  FILTER(?a < ?b)
  FILTER(geof:distance(?p, ?q, uom:metre) < 2500)
  BIND(ROUND(geof:distance(?p, ?q, uom:metre)) AS ?metres)
}
ORDER BY ?metres
```

<!-- test: output -->
```text
a,b,metres
Brussels-Central,Brussels-North,1654
Brussels-Central,Brussels-South,1880
```

The spatial index turns such a filter into a search of the index rather than a comparison of
every pair, so the same query stays fast on millions of geometries.

## Measure

Some GeoSPARQL properties are computed from the geometry rather than stored:
`geo:hasMetricLength` is the length of a line in metres. Compare it with the length the data
states, to find sections whose figures look wrong:

<!-- test: query -->
```sparql
PREFIX ex:  <https://example.org/rail#>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
SELECT ?section ?stated ?measured WHERE {
  ?section a ex:Section ; ex:lengthKm ?stated ; geo:hasGeometry ?geometry .
  ?geometry geo:hasMetricLength ?metres .
  BIND(ROUND(?metres / 1000) AS ?measured)
  FILTER(ABS(?stated - ?measured) >= 5)
}
ORDER BY ?section
```

<!-- test: output -->
```text
section,stated,measured
https://example.org/rail#S05,30,25
https://example.org/rail#S08,55,49
https://example.org/rail#S09,30,24
https://example.org/rail#S11,60,53
https://example.org/rail#S12,60,54
```

They differ because the sample's sections are straight lines between stations, while the
stated lengths follow the real tracks.

A client that binds variables instead of writing values into the query, as RDF4J's and
GraphDB's clients do with `$name` parameters, gets the same computed properties and relations.
The measured length of one section:

<!-- test: run -->
```sh
curl -s http://localhost:7878/repositories/ds -H 'Accept: text/csv' \
  --data-urlencode 'query=PREFIX geo: <http://www.opengis.net/ont/geosparql#>
    SELECT ?section (ROUND(?metres) AS ?m) WHERE {
      ?section geo:hasGeometry ?g . ?g geo:hasMetricLength ?metres }' \
  --data-urlencode '$section=<https://example.org/rail#S05>'
```

<!-- test: output -->
```text
section,m
https://example.org/rail#S05,25112
```

## PostGIS functions

The `st:` functions have PostGIS's names and arguments. The point halfway along the
Leuven–Liège section, and the same point as GeoJSON for a web map:

<!-- test: query -->
```sparql
PREFIX ex:  <https://example.org/rail#>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX st:  <http://w3id.org/tripletrail/st#>
SELECT ?middle ?geojson WHERE {
  ex:S07 geo:hasGeometry/geo:asWKT ?wkt .
  BIND(st:ST_LineInterpolatePoint(?wkt, 0.5) AS ?point)
  BIND(st:ST_AsText(?point) AS ?middle)
  BIND(st:ST_AsGeoJSON(?point) AS ?geojson)
}
```

<!-- test: output -->
```text
middle,geojson
POINT(5.14125 50.7529),"{""type"":""Point"",""coordinates"":[5.14125,50.7529]}"
```

Projected coordinate systems work too: `st:ST_Transform(?wkt, 25831)` gives UTM zone 31N
metres, and `geof:transform` takes the system's IRI. Besides WGS 84, Web Mercator, the UTM zones
and ETRS89-LAEA, TripleTrail knows 65 national grids, with their datum shifts: Belgian Lambert 72
and 2008 (31370, 3812), Lambert-93, RD New, Gauss-Krüger, LV95, the British National Grid and
more (the list is in [the spatial reference](../spatial.md#jena-spatialf)), and most other projected
coordinate systems of the EPSG registry by their code. The midpoint above in
Belgian Lambert 72 and 2008, rounded to the centimetre:

<!-- test: query -->
```sparql
PREFIX ex:  <https://example.org/rail#>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
PREFIX st:  <http://w3id.org/tripletrail/st#>
SELECT ?lambert72 ?lambert2008 WHERE {
  ex:S07 geo:hasGeometry/geo:asWKT ?wkt .
  BIND(st:ST_LineInterpolatePoint(?wkt, 0.5) AS ?point)
  BIND(st:ST_AsText(st:ST_SnapToGrid(st:ST_Transform(?point, 31370), 0.01)) AS ?lambert72)
  BIND(st:ST_AsText(st:ST_SnapToGrid(st:ST_Transform(?point, 3812), 0.01)) AS ?lambert2008)
}
```

<!-- test: output -->
```text
lambert72,lambert2008
POINT(204511.47 160549.38),POINT(704511.84 660556.36)
```

Coordinates in a national grid are good to about a metre where the grid's datum is not ETRS89
(Lambert 72's BD72, for one): TripleTrail shifts them with the EPSG's 7-parameter transformation,
as PROJ does without its grid files.

## On the map

Two places in the web UI draw geometries:

- On the **Query** page, a query that returns WKT literals has a **Map** tab in its results.
  Run the distance query above with `?wkt` added to the `SELECT`.
- The **Map** page (<http://localhost:7878/$/ui/map/ds>) shows the whole dataset: a layer per
  class with a geometry, filters for their values, and a panel describing what you click.

## What you learnt

- Geometries are WKT literals reached with `geo:hasGeometry/geo:asWKT`.
- `geof:distance`, `geof:sfWithin` and `geof:sfIntersects` filter by distance and area, and a
  spatial index answers them.
- `geo:hasMetricLength` and the other computed properties measure, and `st:` has PostGIS's
  functions.

Every function, with what it supports, is in [spatial functions](../spatial.md) and the
[function matrix](../function-matrix.md).

Next: [routing over a network](05-routing.md).
