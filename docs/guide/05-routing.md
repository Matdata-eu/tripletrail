# 5. Routing over a network

TripleTrail finds routes in your data: shortest and fastest paths, what can be reached within a
distance, routes that only use the tracks a train may use, and more. The functions are
[pgRouting](https://pgrouting.org)'s, with the same names, parameters and result columns. They
work on any data model, because you tell them what the network is with a query.

Start the server with the sample network:

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl
```

## The edge query

A routing function needs the network's edges, each with an id, the vertices at its two ends and
a cost. In pgRouting that is an SQL query; here it is a sub-select that projects `?id`,
`?source`, `?target` and `?cost`. For the sample network, each section is an edge between its
two stations, and its cost is its length:

```sparql
{ SELECT ?id ?source ?target ?cost WHERE {
    ?id a ex:Section ; ex:from ?source ; ex:to ?target ; ex:lengthKm ?cost .
} }
```

Data in another model only needs another edge query; the routing itself stays the same.

## The shortest route

The functions run inside `SERVICE <http://w3id.org/tripletrail/pgr>`. A blank node `[]` holds the
call: `pgr:function` names the function, triples with a value are its parameters, and triples
with a variable are the result columns you want. The shortest route from Ottignies to
Liège-Guillemins:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX pgr:  <http://w3id.org/tripletrail/pgr#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?seq ?station ?km WHERE {
  SERVICE <http://w3id.org/tripletrail/pgr> {
    [] pgr:function pgr:pgr_dijkstra ;
       pgr:start_vid ex:OTN ; pgr:end_vid ex:LG ; pgr:directed false ;
       pgr:seq ?seq ; pgr:node ?node ; pgr:agg_cost ?km .
    { SELECT ?id ?source ?target ?cost WHERE {
        ?id a ex:Section ; ex:from ?source ; ex:to ?target ; ex:lengthKm ?cost .
    } }
  }
  ?node rdfs:label ?station .
}
ORDER BY ?seq
```

<!-- test: output -->
```text
seq,station,km
1,Ottignies,0
2,Namur,33
3,Liège-Guillemins,93
```

`pgr:directed false` lets trains run both ways along every section. `?agg_cost` is the cost so
far, and the result rows join the rest of the query like any others: here they pick up the
stations' names.

## The fastest route

Change the cost and you change what "best" means. With the cost in minutes at each section's
maximum speed, the fastest route goes the long way, over the high-speed line from Leuven:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX pgr:  <http://w3id.org/tripletrail/pgr#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?seq ?station ?minutes WHERE {
  SERVICE <http://w3id.org/tripletrail/pgr> {
    [] pgr:function pgr:pgr_dijkstra ;
       pgr:start_vid ex:OTN ; pgr:end_vid ex:LG ; pgr:directed false ;
       pgr:seq ?seq ; pgr:node ?node ; pgr:agg_cost ?time .
    { SELECT ?id ?source ?target ?cost WHERE {
        ?id a ex:Section ; ex:from ?source ; ex:to ?target ;
            ex:lengthKm ?km ; ex:maxSpeed ?speed .
        BIND(60 * ?km / ?speed AS ?cost)
    } }
  }
  ?node rdfs:label ?station .
  BIND(ROUND(?time) AS ?minutes)
}
ORDER BY ?seq
```

<!-- test: output -->
```text
seq,station,minutes
1,Ottignies,0
2,Brussels-South,11
3,Brussels-Central,14
4,Brussels-North,16
5,Leuven,27
6,Liège-Guillemins,40
```

## Only where the train may go

An electric train cannot use the section from Ghent to Antwerp, which has no overhead line.
Extra columns of the edge query named `?c_…` can be tested with `pgr:constraint`, a SPARQL
expression; edges that fail it are left out. `rail:route` is `pgr_dijkstra` with constraints:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX pgr:  <http://w3id.org/tripletrail/pgr#>
PREFIX rail: <http://w3id.org/tripletrail/rail#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?seq ?station ?km WHERE {
  SERVICE <http://w3id.org/tripletrail/pgr> {
    [] pgr:function rail:route ;
       pgr:start_vid ex:GNT ; pgr:end_vid ex:ANT ; pgr:directed false ;
       pgr:constraint "?c_electrified" ;
       pgr:seq ?seq ; pgr:node ?node ; pgr:agg_cost ?km .
    { SELECT ?id ?source ?target ?cost ?c_electrified WHERE {
        ?id a ex:Section ; ex:from ?source ; ex:to ?target ;
            ex:lengthKm ?cost ; ex:electrified ?c_electrified .
    } }
  }
  ?node rdfs:label ?station .
}
ORDER BY ?seq
```

<!-- test: output -->
```text
seq,station,km
1,Ghent-Sint-Pieters,0
2,Brussels-South,55
3,Brussels-Central,57
4,Brussels-North,59
5,Mechelen,79
6,Antwerp-Central,102
```

Without the constraint the route is the direct section, 60 km. Constraints can combine
several columns, such as `"?c_gauge = 1435 && ?c_axleLoad >= 22.5"`. Rules about which
vehicle may use which track can also be written in SHACL and read from the data: see
[compatibility](../routing.md#compatibility-shacl).

## How far can you get

`pgr_drivingDistance` gives every station reachable within a cost, here 30 km of
Brussels-North:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX pgr:  <http://w3id.org/tripletrail/pgr#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?station ?km WHERE {
  SERVICE <http://w3id.org/tripletrail/pgr> {
    [] pgr:function pgr:pgr_drivingDistance ;
       pgr:start_vid ex:BXN ; pgr:distance 30 ; pgr:directed false ;
       pgr:node ?node ; pgr:agg_cost ?km .
    { SELECT ?id ?source ?target ?cost WHERE {
        ?id a ex:Section ; ex:from ?source ; ex:to ?target ; ex:lengthKm ?cost .
    } }
  }
  ?node rdfs:label ?station .
}
ORDER BY ?km ?station
```

<!-- test: output -->
```text
station,km
Brussels-North,0
Brussels-Central,2
Brussels-South,4
Mechelen,20
Leuven,30
```

## The route on a map

When the edge query also projects `?geom`, a section's line, the route can be returned as
one geometry. `rail:routeGeometry` is the whole route as a line and `rail:routeAsList` its
sections in order; asking only for route-level columns gives one row per route:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX geo:  <http://www.opengis.net/ont/geosparql#>
PREFIX pgr:  <http://w3id.org/tripletrail/pgr#>
PREFIX rail: <http://w3id.org/tripletrail/rail#>
SELECT ?km ?sections ?geometry WHERE {
  SERVICE <http://w3id.org/tripletrail/pgr> {
    [] pgr:function pgr:pgr_dijkstra ;
       pgr:start_vid ex:OTN ; pgr:end_vid ex:LG ; pgr:directed false ;
       pgr:agg_cost ?km ; rail:routeAsList ?sections ; rail:routeGeometry ?geometry .
    { SELECT ?id ?source ?target ?cost ?geom WHERE {
        ?id a ex:Section ; ex:from ?source ; ex:to ?target ; ex:lengthKm ?cost ;
            geo:hasGeometry/geo:asWKT ?geom .
    } }
  }
}
```

<!-- test: output -->
```text
km,sections,geometry
93,https://example.org/rail#S10 https://example.org/rail#S11,"LINESTRING(4.5694 50.6735,4.8622 50.4687,5.5667 50.6245)"
```

Run it on the **Query** page and open the **Map** tab of the results to see it.

## Hops over any links

Routing weighs every edge by its cost. Some questions only count steps: which stations are at
most two sections from Brussels-North? A property path with bounds, `{1,2}`, follows one or
two steps. Here a step goes from a station, through a section that starts or ends there, to
the station at the other end:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?station WHERE {
  ex:BXN (^ex:from/ex:to | ^ex:to/ex:from){1,2} ?near .
  FILTER(?near != ex:BXN)
  ?near rdfs:label ?station .
}
ORDER BY ?station
```

<!-- test: output -->
```text
station
Antwerp-Central
Brussels-Central
Brussels-South
Leuven
Liège-Guillemins
Mechelen
```

`SERVICE <http://w3id.org/tripletrail/path>` gives the paths themselves. Without a target, every
station reached is an end, with the fewest sections it takes to get there:

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX path: <http://w3id.org/tripletrail/path#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?station ?hops WHERE {
  SERVICE <http://w3id.org/tripletrail/path> {
    [] path:source ex:BXN ; path:direction "both" ; path:minLength 1 ;
       path:end ?end ; path:length ?hops .
    { SELECT ?a ?b WHERE { ?s ex:from ?a ; ex:to ?b } }
  }
  ?end rdfs:label ?station .
}
ORDER BY ?hops ?station
```

<!-- test: output -->
```text
station,hops
Brussels-Central,1
Leuven,1
Mechelen,1
Antwerp-Central,2
Brussels-South,2
Liège-Guillemins,2
Ghent-Sint-Pieters,3
Namur,3
Ottignies,3
```

The sub-select names the links, its first two columns their ends; `path:predicate ex:next`
would follow a predicate instead. `path:mode "simple"` lists every path that visits no station
twice.

With neither, the search follows every link between two resources, whatever its predicate,
straight from the indexes. Between one source and one target it searches from both ends at
once, so it stays fast on a large dataset. `path:exceptPredicate` leaves predicates out:
without it, `rdf:type` would join any two stations through `ex:Station`.

<!-- test: query -->
```sparql
PREFIX ex:   <https://example.org/rail#>
PREFIX path: <http://w3id.org/tripletrail/path#>
PREFIX rdf:  <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
SELECT ?step ?from ?edge ?to WHERE {
  SERVICE <http://w3id.org/tripletrail/path> {
    [] path:source ex:BXN ; path:target ex:ANT ; path:direction "both" ;
       path:exceptPredicate rdf:type ;
       path:step ?step ; path:from ?from ; path:edge ?edge ; path:to ?to .
  }
}
ORDER BY ?step
```

<!-- test: output -->
```text
step,from,edge,to
1,https://example.org/rail#BXN,https://example.org/rail#from,https://example.org/rail#S03
2,https://example.org/rail#S03,https://example.org/rail#to,https://example.org/rail#MEC
3,https://example.org/rail#MEC,https://example.org/rail#from,https://example.org/rail#S04
4,https://example.org/rail#S04,https://example.org/rail#to,https://example.org/rail#ANT
```

Each step is in the direction walked: the section `S03` runs from Brussels-North, so the first
step follows `ex:from` backwards. The Explore page draws its paths this way. See
[paths over any predicates](../routing.md#paths-over-any-predicates).

## Faster on big networks

The network is built from the edge query on the first call and kept in memory until the data
changes, so later calls only search. On a national network, add `pgr:engine "ch"` to answer
shortest paths from a contraction hierarchy, in about a millisecond. A network can also be
named once in the data and used by its IRI (`pgr:network`); see
[named networks](../routing.md#named-networks).

## What you learnt

- Routing functions run in `SERVICE <http://w3id.org/tripletrail/pgr>` with pgRouting's names,
  on an edge sub-select projecting `?id ?source ?target ?cost`.
- The cost decides what "best" means: length, time, or anything you can compute.
- `pgr:constraint` on `?c_…` columns keeps a route to the edges a train may use.
- `rail:routeGeometry` returns the route as a line for a map.
- Bounded paths such as `{1,2}` and the `path:` SERVICE count steps over any links.

The reference lists all of pgRouting's functions and what TripleTrail adds: time-dependent and
multi-criteria routing, turn restrictions, vehicle routing, isochrones and network checks.
See [routing](../routing.md).

Next: [check and enrich data](06-validation-and-reasoning.md).
