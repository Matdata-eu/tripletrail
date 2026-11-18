# Routing

## pgRouting functions

Routing works on any data model. As in pgRouting, you hand the routing function an edge set; here
the edge set is a SPARQL sub-select projecting `?id ?source ?target ?cost` and optionally
`?reverse_cost` and `?x1 ?y1 ?x2 ?y2`. Parameters and result columns keep pgRouting's names.

```sparql
PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
PREFIX ex:  <http://example.com/>
SELECT ?seq ?node ?edge ?cost ?agg_cost WHERE {
  SERVICE <http://w3id.org/tripletrail/pgr> {
    [] pgr:function pgr:pgr_dijkstra ;
       pgr:start_vid ex:a ; pgr:end_vid ex:d ; pgr:directed true ;
       pgr:seq ?seq ; pgr:node ?node ; pgr:edge ?edge ; pgr:cost ?cost ; pgr:agg_cost ?agg_cost .
    { SELECT ?id ?source ?target ?cost ?reverse_cost WHERE {
        ?id a ex:Track ; ex:from ?source ; ex:to ?target ; ex:length ?cost .
        OPTIONAL { ?id ex:bidirectional true BIND(?cost AS ?reverse_cost) }
    } }
  }
}
```

- Triples with a constant object are parameters (`pgr:start_vid` and `pgr:end_vid` may repeat for
  many-to-many); triples with a variable object bind result columns.
- A negative `cost`/`reverse_cost` means no arc in that direction; `pgr:directed false` makes every
  arc usable both ways. Rows with an unbound id, source, target or cost are skipped.
- Results are ordinary bindings, so they join back to the data (edge geometries, attributes, ...).
  The join starts from the routing rows and looks their triples up. A row whose joining variable
  is unbound, such as the last row of a path (a node, no `?edge`), joins with nothing rather
  than with every triple, as if the variable were always bound: Oxigraph's own join treats
  SERVICE variables that way too, so both stores give that answer. Use `OPTIONAL` around the
  lookup to keep such rows.
- The graph built from an edge sub-select is cached by the sub-select's algebra and reused until
  the data changes, within `--network-cache-mb` (least recently used dropped first).

### Named networks

Describe the edge query in the store and refer to it by IRI:

```sparql
INSERT DATA { ex:rinf a pgr:Network ; pgr:edgeQuery "SELECT ?id ?source ?target ?cost WHERE { ... }" }
# then: [] pgr:function pgr:pgr_dijkstra ; pgr:network ex:rinf ; ...
```

The network's description is read from the default graph (the union of all graphs with
`--union-default-graph`, or the caller's graphs). Its edge query runs inside the routing
function, where no other SERVICE is answered: an edge query that calls one is refused with 400.

Implemented: `pgr_dijkstra`, `pgr_dijkstraCost` (and `pgr_dijkstraCostMatrix`), `pgr_bdDijkstra`,
`pgr_aStar` (pgRouting heuristics 0-5, plus 6 = geodesic for lon/lat with costs in metres),
`pgr_drivingDistance`, `pgr_KSP`, `pgr_trsp`, `pgr_turnRestrictedPath`, `pgr_dijkstraVia`, `pgr_connectedComponents`,
`pgr_strongComponents`, `pgr_articulationPoints`, `pgr_bridges`, `pgr_dijkstraNear`,
`pgr_dijkstraNearCost`, `pgr_bdDijkstraCost`, `pgr_aStarCost`, `pgr_bdAstar`,
`pgr_bdAstarCost` (the `*CostMatrix` forms too), `pgr_floydWarshall`, `pgr_johnson`,
`pgr_kruskal`, `pgr_prim`, `pgr_withPoints`, `pgr_withPointsCost` (and `CostMatrix`),
`pgr_withPointsKSP`, `pgr_withPointsDD`, `pgr_withPointsVia`, `pgr_bellmanFord`,
`pgr_edwardMoore`, `pgr_dagShortestPath`, `pgr_binaryBreadthFirstSearch`,
`pgr_breadthFirstSearch`, `pgr_depthFirstSearch`, `pgr_trspVia`, `pgr_trsp_withPoints`,
`pgr_trspVia_withPoints`, `pgr_kruskalBFS`, `pgr_kruskalDFS`, `pgr_kruskalDD`, `pgr_primBFS`,
`pgr_primDFS`, `pgr_primDD`, `pgr_maxFlow`, `pgr_pushRelabel`, `pgr_edmondsKarp`,
`pgr_boykovKolmogorov`, `pgr_maxFlowMinCost`, `pgr_maxFlowMinCost_Cost`,
`pgr_edgeDisjointPaths`, `pgr_maxCardinalityMatch`, `pgr_stoerWagner`, `pgr_TSP`,
`pgr_TSPeuclidean`, `pgr_lineGraph`, `pgr_lineGraphFull`, `pgr_contraction`,
`pgr_biconnectedComponents`, `pgr_degree`, `pgr_extractVertices`, `pgr_topologicalSort`,
`pgr_transitiveClosure`, `pgr_makeConnected`, `pgr_isPlanar`,
`pgr_lengauerTarjanDominatorTree`, `pgr_hawickCircuits`, `pgr_sequentialVertexColoring`,
`pgr_bipartite`, `pgr_edgeColoring`, `pgr_bandwidth`, `pgr_kingOrdering`, `pgr_sloanOrdering`,
`pgr_chinesePostman`, `pgr_chinesePostmanCost`, `pgr_pickDeliver`, `pgr_pickDeliverEuclidean`,
`pgr_vrpOneDepot`, `pgr_version` and `pgr_full_version`.

Functions that return a single value bind it with `pgr:` and pgRouting's column name
(`pgr:pgr_maxflow`, `pgr:pgr_isplanar`, ...) or a short form: `pgr:flow` (`pgr_maxFlow`),
`pgr:cost` (`pgr_maxFlowMinCost_Cost`, `pgr_chinesePostmanCost`), `pgr:planar`,
`pgr:bandwidth`, `pgr:version`. pgRouting's arrays (`contracted_vertices`, `target_array`,
`in_edges`, ...) are strings with the items separated by spaces, written as in restriction
paths. Functions that only need the network's arcs (components, orderings, colourings, flows,
...) accept an edge sub-select without `?cost`.

### Points along edges

The `withPoints` functions take a third sub-select placing extra
vertices on edges, such as signals, platforms or a train's position on a track:
`{ SELECT ?pid ?edge ?fraction WHERE { ... } }`, with `?fraction` running from 0 at the edge's
source to 1 at its target. A point's `?pid` can then be used as a `pgr:start_vid`, `pgr:end_vid`
or `pgr:via`. The edge is split at its points and each part keeps the edge's id and its share of
the costs. Points can be added to other path functions too (`pgr_aStar`, `pgr_dijkstraNear`, and
so on). pgRouting's `driving_side` and `details` options are not supported: a point is reached
along its edge in each direction the edge allows.

### Nearest target

`pgr_dijkstraNear` returns the path from any `pgr:start_vid` to the nearest
`pgr:end_vid`, such as the closest depot or siding; `pgr:cap n` returns the `n` nearest, numbered
by `pgr:path_id`. `pgr_dijkstraNearCost` returns only their costs.

### Via points

`pgr_dijkstraVia` takes the vertices to visit as repeated `pgr:via`, in the
order written, and returns one path per leg (`pgr:path_id`) with `pgr:route_agg_cost` running
over the whole route. A leg without a path is skipped, or empties the result with
`pgr:strict true`.

### Graph analysis

`pgr_connectedComponents` (arcs in either direction) and
`pgr_strongComponents` (following arc directions) return `pgr:component` and `pgr:node`; a
component is named by its smallest vertex. `pgr:size` gives the number of vertices of the
component; without `pgr:node` there is one row per component. A query that groups the rows by
`?component` and counts `?node` is answered that way, without one row per vertex. `pgr_articulationPoints` returns the vertices
(`pgr:node`) and `pgr_bridges` the edges (`pgr:edge`) whose removal disconnects the network,
treating it as undirected: on a railway, the single points of failure. `pgr_floydWarshall`
and `pgr_johnson` return the cost between every pair of connected vertices
(`pgr:start_vid ?s ; pgr:end_vid ?t ; pgr:agg_cost ?c`); that is one row per pair, so use them
on small networks or a filtered edge set. `pgr_kruskal` and `pgr_prim` return
the edges of a minimum spanning forest (`pgr:edge`, `pgr:cost`, and `pgr:agg_cost` running over
the forest), cheapest first. None of them take `pgr:start_vid`.

The other analysis functions follow pgRouting's columns: `pgr_biconnectedComponents`
(`pgr:edge`, `pgr:component`), `pgr_degree` (`pgr:node`, `pgr:degree`), `pgr_extractVertices`
(`pgr:id`, `pgr:in_edges`, `pgr:out_edges`, and `pgr:x`, `pgr:y`, `pgr:geom` when the edges have
coordinates), `pgr_topologicalSort` (`pgr:sorted_v`; nothing when there is a cycle),
`pgr_transitiveClosure` (`pgr:vid`, `pgr:target_array`), `pgr_makeConnected` (the
`pgr:start_vid`, `pgr:end_vid` pairs to join), `pgr_isPlanar`, `pgr_lengauerTarjanDominatorTree`
(`pgr:root_vertex`; `pgr:vid`, `pgr:idom`), `pgr_hawickCircuits` (every elementary circuit as
a path, parallel edges and loops included), `pgr_sequentialVertexColoring` and `pgr_bipartite`
(`pgr:vertex_id`, `pgr:color_id`; nothing when the graph is not bipartite), `pgr_edgeColoring`
(`pgr:edge_id`, `pgr:color_id`), `pgr_bandwidth`, `pgr_kingOrdering` and `pgr_sloanOrdering`
(`pgr:seq`, `pgr:node`). Where pgRouting breaks ties by id these break them by vertex and edge
term, numbers by value. The colourings and orderings are greedy or heuristic as in Boost, so
`pgr_edgeColoring` may use more colours than Boost's Misra-Gries (at most twice the largest
degree, less one).

### More shortest paths

`pgr_bellmanFord` and `pgr_edwardMoore` (queue-based Bellman-Ford) accept
negative costs: for them a negative `cost` or `reverse_cost` is an arc, not a missing one, so leave
`?reverse_cost` unbound on one-way edges. A negative cycle gives no paths. `pgr_dagShortestPath`
needs an acyclic graph and `pgr_binaryBreadthFirstSearch` costs of 0 and one other value.
`pgr_breadthFirstSearch` and `pgr_depthFirstSearch` return the traversal from each
`pgr:start_vid` (`pgr:depth`, `pgr:pred`, `pgr:node`, `pgr:edge`, `pgr:cost`, `pgr:agg_cost`),
down to `pgr:max_depth`. `pgr_kruskalBFS`, `pgr_kruskalDFS`, `pgr_primBFS` and `pgr_primDFS`
traverse the minimum spanning tree the same way, `pgr_kruskalDD` and `pgr_primDD` up to
`pgr:distance` along the tree.

### Flows

`pgr_maxFlow`, `pgr_pushRelabel`, `pgr_edmondsKarp` and `pgr_boykovKolmogorov` read
`?capacity` and optionally `?reverse_capacity` from the edge sub-select (a negative capacity means
no arc) and send flow from the `pgr:start_vid`s to the `pgr:end_vid`s. `pgr_maxFlow` returns the
total; the others return the flow on each arc (`pgr:edge`, `pgr:start_vid`, `pgr:end_vid`,
`pgr:flow`, `pgr:residual_capacity`). All give a maximum flow, but may spread it differently;
`pgr_boykovKolmogorov` is computed with Dinic's algorithm. `pgr_maxFlowMinCost` and
`pgr_maxFlowMinCost_Cost` also read `?cost` (and `?reverse_cost`) per unit of flow.
`pgr_edgeDisjointPaths` returns the largest set of paths sharing no edge, `pgr_maxCardinalityMatch`
a maximum matching (Edmonds' blossom algorithm), and `pgr_stoerWagner` the edges of a minimum cut
(`pgr:edge`, `pgr:cost`, `pgr:mincut`). `pgr_chinesePostman` returns the cheapest closed walk
along every arc, when the arcs are strongly connected.

### Travelling salesperson

`pgr_TSP` takes a cost matrix sub-select,
`{ SELECT ?start_vid ?end_vid ?agg_cost WHERE { ... } }`, which may itself call
`pgr_dijkstraCostMatrix`, and optionally `pgr:start_id` and `pgr:end_id` (the last stop before
returning). `pgr_TSPeuclidean` takes `{ SELECT ?id ?x ?y WHERE { ... } }` instead. The tour is
undirected (of two costs between a pair the smaller is used) and found by nearest neighbour,
then improved by 2-opt and or-opt moves; it returns `pgr:seq`, `pgr:node`, `pgr:cost` and
`pgr:agg_cost`, ending back at the start.

### Vehicle routing

`pgr_pickDeliver` plans pickups and deliveries with capacities and time
windows. It takes three sub-selects, told apart by what they project: the orders (`?id
?demand ?p_node_id ?p_open ?p_close [?p_service] ?d_node_id ?d_open ?d_close [?d_service]`),
the vehicles (`?id ?capacity ?start_node_id ?start_open ?start_close [?start_service]
[?end_node_id ?end_open ?end_close ?end_service] [?number] [?speed]`; the end defaults to the
start, `?number` copies a vehicle) and the travel times
(`?start_vid ?end_vid ?agg_cost`; a pair given one way only is used both ways, a pair not
given cannot be travelled). `pgr_pickDeliverEuclidean` takes no matrix: places are
`?p_x ?p_y`, `?d_x ?d_y`, `?start_x ?start_y` (and `?end_x ?end_y`) and travel times their
distances. Both take `pgr:factor` (travel time multiplier, default 1), `pgr:max_cycles`
(default 10) and `pgr:initial_sol` (1-6, accepted but always the optimized insertion, 4).
They return, per vehicle used, its start, pickups, deliveries and end (`pgr:seq`,
`pgr:vehicle_seq`, `pgr:vehicle_id`, `pgr:stop_seq`, `pgr:stop_type` 1 start, 2 pickup,
3 delivery, 6 end, `pgr:stop_id`, `pgr:order_id` (-1 at the start and end), `pgr:cargo` on
leaving, `pgr:travel_time`, `pgr:arrival_time`, `pgr:wait_time`, `pgr:service_time`,
`pgr:departure_time`), then a summary row (`pgr:vehicle_seq` -2, `pgr:stop_type` -1) with the
total travel, wait, service and vehicle time. Plans use as few vehicles as they can, then the
least total time: cheapest feasible insertion, then moves of orders and whole routes. Orders
that no vehicle can serve make the query fail with their ids (pgRouting reports violations
instead). `pgr_vrpOneDepot` delivers from a depot: `pgr:depot_id`, the orders as pgRouting's
Solomon tables (`?id ?x ?y ?order_unit ?open_time ?close_time ?service_time`, the depot one of
them), the vehicles (`?vehicle_id ?capacity`) and optionally a matrix
(`?src_id ?dest_id ?traveltime`, or `?cost`; without one, the distances between `?x ?y`).
Each order is loaded at the depot and delivered at its place; the result has pgRouting's
`pgr:oid` (-1 at the depot), `pgr:opos`, `pgr:vid`, `pgr:tarrival`, `pgr:tdepart` and the
summary row (`-1, 0, 0, -1`, total time), as well as the `pgr_pickDeliver` columns. Nothing
here knows a data model: the sub-selects can read orders and fleets from any graph, and the
matrix can come from `pgr_dijkstraCostMatrix` over a network.

```sparql
[] pgr:function pgr:pgr_pickDeliver ;
   pgr:vehicle_id ?truck ; pgr:stop_seq ?n ; pgr:order_id ?order ; pgr:arrival_time ?t .
{ SELECT ?id ?demand ?p_node_id ?p_open ?p_close ?d_node_id ?d_open ?d_close WHERE { ... } }
{ SELECT ?id ?capacity ?start_node_id ?start_open ?start_close WHERE { ... } }
{ SELECT ?start_vid ?end_vid ?agg_cost WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
    [] pgr:function pgr:pgr_dijkstraCostMatrix ; ... } } }
```

### Line graphs and contraction

`pgr_lineGraph` returns the pairs of edges where one can follow
the other (`pgr:source`, `pgr:target`, with `pgr:cost` the first edge's cost and -1 for a way that
is not possible). `pgr_lineGraphFull` splits every vertex into one vertex per arc end, numbered
-1, -2, ... (a vertex with a single arc end keeps its id), so that each turn is an arc with an
unbound `pgr:edge`. `pgr_contraction` takes `pgr:methods` 1 (dead ends) and 2 (linear), repeated
in order `pgr:cycles` times, and `pgr:forbidden` vertices; it returns the vertices that absorbed
dead ends (`pgr:type "v"`) and the shortcuts (`pgr:type "e"`, `pgr:id` -1, -2, ...), each with its
`pgr:contracted_vertices`. A vertex is a dead end when it has one neighbour, and linear when it
has two and every way into it continues to the other.

`pgr_version` and `pgr_full_version` report the version of this implementation.

### Contraction hierarchies

Add `pgr:engine "ch"` to `pgr_dijkstra`, `pgr_dijkstraCost`,
`pgr_bdDijkstra` (and its cost variant) or `pgr_dijkstraNear` to answer the search from a
customizable contraction hierarchy. The hierarchy's shape depends only on which vertices are
connected, so it is built once on the first such query and cached with the network. When a write
only changes costs, the next query re-weights the existing hierarchy (customization) instead of
contracting again. The results are the same as Dijkstra, though equal-cost paths may be picked
differently. On a generated 604k-vertex grid, building the hierarchy takes 0.61 s (a classic
contraction hierarchy took 3.8 s), re-customizing after a cost change takes 0.12 s, and a query
takes 0.27 ms. Points (`withPoints`) are not contracted, so those queries use Dijkstra.

### Keeping networks across restarts

`Engine::with_network_dir(store, dir)` saves every built
network and contraction hierarchy to `dir`, and a restarted engine on the same data loads them
instead of rebuilding. On the generated 230k-vertex network, the first contracted query goes from
5.2 s (edge query, graph build, contraction) to 0.5 s, with 45 MB on disk. Writes through the
engine (`load`, `load_quads`, `update`) mark the saved networks stale, unless an update only
restates what is there (deletes and inserts the same triples). Writes made directly on the store
do not, so don't make any while using a network directory. Networks with `?c_*` columns are
saved too; ones with capacities or geometries are not. A saved hierarchy is checked and
customized only when a query first uses it, so a restarted engine answers Dijkstra without it.

### Network state

`pgr:networkInfo` reports on a network (`pgr:network <iri>`, or an edge
sub-select), building it first if it is not cached, in one row: `pgr:network` (the IRI),
`pgr:directed`, `pgr:vertices`, `pgr:edges` (distinct ids), `pgr:rows`, `pgr:arcs`,
`pgr:has_cost`, `pgr:columns` (the `?c_*` columns, separated by spaces), `pgr:hierarchy` (whether
the contraction hierarchy is built), `pgr:filters` (constrained graphs cached for
`pgr:constraint`, `pgr:shapes` or `pgr:vehicle`), `pgr:customized` (how many of them have the
hierarchy customized), `pgr:edge_query_time`, `pgr:build_time`, `pgr:hierarchy_time` and
`pgr:customize_time` (seconds; unbound when not known, e.g. for a hierarchy loaded from disk),
`pgr:cached` (whether it was in the cache before the call) and `pgr:persisted` (saved in the
network directory; networks with `?c_*`, `?geom` or capacity columns are not saved).

```sparql
[] pgr:function pgr:networkInfo ; pgr:network ex:rinf ;
   pgr:vertices ?v ; pgr:edges ?e ; pgr:hierarchy ?ch ; pgr:build_time ?t ; pgr:persisted ?saved .
```

### K shortest paths

`pgr_KSP` takes `pgr:K` and optionally `pgr:heap_paths true`, and returns
loopless paths cheapest first, numbered by `pgr:path_id` (Yen's algorithm; each spur search is an
A* guided by exact distances to the target).

### Turn restrictions

`pgr_trsp` takes a second sub-select with the restricted edge sequences.
Traversing a whole sequence consecutively adds its `?cost`; with no `?cost` the sequence is
forbidden. Sequences of any length are supported, and edges are named by their IRI (or literal
value). Either one row per restriction, with the edges in a string:

```sparql
{ SELECT ?path ?cost WHERE { ?r a ex:NoTurn ; ex:edges ?path } }   # ?path = "http://ex/e1 http://ex/e2"
```

or one row per position, which suits restrictions stored as RDF:

```sparql
{ SELECT ?id ?seq ?edge WHERE { ?id ex:step [ ex:seq ?seq ; ex:edge ?edge ] } }
```

Without restriction rows `pgr_trsp` is plain Dijkstra. With restrictions it searches over
(vertex, position in a restricted sequence) pairs, which costs about twice a Dijkstra.
`pgr_trspVia` is `pgr_dijkstraVia` under restrictions; with `pgr:U_turn_on_edge false` a leg does
not leave along the edge the previous leg arrived by. `pgr_trsp_withPoints` and
`pgr_trspVia_withPoints` take a points sub-select as well.

`pgr_turnRestrictedPath` takes the same restriction sub-select and `pgr:K`, and runs Yen's K
shortest paths, checking each against the restrictions (which here only forbid; their costs are
not added). With `pgr:stop_on_first` (default true) the answer is the first path that follows
no restricted sequence; with `pgr:stop_on_first false` it is all K paths (and Yen's remaining
candidates with `pgr:heap_paths true`). `pgr:strict true` drops the paths that follow a
restricted sequence, so the result can be empty; with the default `false`, the paths found are
returned when none complies, as pgRouting does.

## Beyond pgRouting

Functions pgRouting does not have are named in the `rail:` namespace
(`PREFIX rail: <http://w3id.org/tripletrail/rail#>`) and run in the same SERVICE:
`[] pgr:function rail:route`. Their parameters and columns keep the `pgr:` prefix (`rail:` is
accepted too). They still only see edge sets; nothing in them knows a data model.

### Constraints

The edge sub-select may project extra columns named `?c_*` (gauge,
electrification, axle load, ...). `pgr:constraint` takes a SPARQL expression over them, and edges
with a row it does not accept are left out of the search:

```sparql
[] pgr:function rail:route ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
   pgr:constraint "?c_gauge = 1435 && ?c_electrified && coalesce(?c_axleLoad, 25) >= 22.5" ;
   pgr:seq ?seq ; pgr:edge ?edge ; pgr:agg_cost ?agg_cost .
{ SELECT ?id ?source ?target ?cost ?c_gauge ?c_electrified ?c_axleLoad WHERE { ... } }
```

`rail:route` is `pgr_dijkstra` and `rail:reachable` is `pgr_drivingDistance` (`pgr:distance`, or
`pgr:max_cost`), but `pgr:constraint` works on every path and distance function (`pgr_aStar`,
`pgr_KSP`, `pgr_trsp`, the via and `withPoints` forms, ...). The expression may use comparisons,
`&&`, `||`, `!`, arithmetic, `IN`, `BOUND`, `IF`, `COALESCE`, `STR`, `LCASE`, `UCASE`, `STRLEN`,
`ABS`, `CONTAINS`, `STRSTARTS` and `STRENDS`; as in `FILTER`, an error (an unbound column, say)
rejects the edge. It is evaluated once per edge, and the network without the rejected edges is
cached with the network (eight constraints per network), so the network is not built again and
the search loop does no extra work. With `pgr:engine "ch"` the network's hierarchy is customized
for the constrained graph instead of contracting again.
On a generated railway-like network (390k vertices, lines crossing every 20 vertices),
evaluating and filtering takes 30 ms, customizing the hierarchy for the constraint 0.23 s, and
then a long query takes 1.2 ms on the hierarchy against 75 ms for Dijkstra: the hierarchy pays
off from a few queries with the same constraint, so it is used only when `pgr:engine "ch"` asks
for it.

### Compatibility (SHACL)

Which edges a vehicle may use can also be said with SHACL, read
from the store rather than from the edge rows. A small vocabulary,
`PREFIX compat: <http://w3id.org/tripletrail/compat#>`, says how a property of the vehicle compares
with a property of what it runs on; `pgr:vehicle` and `pgr:rules` (a named graph of rules)
compile the rules with that vehicle's values into SHACL shapes, and edges whose focus node does
not conform are left out. A truck on roads with weight limits and bridges:

```turtle
GRAPH ex:roadRules {
  ex:weight a compat:Rule ; compat:vehiclePath ex:weight ; compat:edgePath osm:maxweight ;
      compat:constraint sh:minInclusive .              # every edge value >= the truck's
  ex:height a compat:Rule ; compat:vehiclePath ex:height ;
      compat:edgePath [ sh:alternativePath ( osm:maxheight ( ex:passesUnder osm:maxheight ) ) ] ;
      compat:constraint sh:minInclusive .              # on the road, or on a bridge over it
}
ex:truck ex:weight 30 ; ex:height 4.0 .
ex:s2 ex:passesUnder ex:lowBridge .  ex:lowBridge osm:maxheight 3.8 .
```

```sparql
[] pgr:function rail:route ; pgr:start_vid ex:A ; pgr:end_vid ex:D ;
   pgr:vehicle ex:truck ; pgr:rules ex:roadRules ;
   pgr:seq ?seq ; pgr:edge ?edge ; pgr:agg_cost ?agg_cost .
{ SELECT ?id ?source ?target ?cost WHERE { ?id a ex:Road ; ex:from ?source ; ex:to ?target ; ex:len ?cost } }
```

The truck detours around `ex:s2`; a lower one goes under the bridge. A rule has a
`compat:vehiclePath` and a `compat:edgePath` (SHACL paths: predicates, sequences, inverse,
alternative and repeated paths), and a `compat:constraint`: `sh:minInclusive`,
`sh:minExclusive` (every edge value at least, or above, the vehicle's), `sh:maxInclusive`,
`sh:maxExclusive` (at most, below), `sh:hasValue` (the edge has one of the vehicle's values)
or `sh:in` (every edge value is one of the vehicle's). An edge without a value passes, unless
`compat:whenMissing compat:Fail`; `compat:noneValue` names a vehicle value meaning "also runs
where the edge has none" (a bi-mode train on a line without a contact line), and
`compat:factor` scales the vehicle's value for a numeric comparison (units). A rule the
vehicle has no value for is not checked.

Categories that are ordered rather than numbers (weight classes, EN 15528 line categories)
are compared through `compat:rank`: one or more paths, read in the rules graph, from a
category to a number. With a range constraint, an edge value is accepted when every one of its
ranks is at least (`sh:minInclusive`; above, at most, below) the vehicle's; several ranks
make a partial order (line category C2 is above B2 in axle load and mass per metre, D2 is not
above C3). The vehicle gives a category, or a number when there is a single rank; with several
values, an edge value accepted for any of them passes. The order is data, not code:

```turtle
ex:W7_5 ex:tonnes 7.5 .  ex:W12 ex:tonnes 12 .  ex:W40 ex:tonnes 40 .
ex:class a compat:Rule ; compat:vehiclePath ex:weightClass ; compat:edgePath ex:weightClass ;
    compat:constraint sh:minInclusive ; compat:rank ex:tonnes .
```

The rule compiles, for that vehicle, to the list of the rules graph's categories it rejects
(`sh:not [ sh:in ( ex:W7_5 ) ]` for a 12 t class): an edge value without a rank passes like a
missing one, or, with `compat:whenMissing compat:Fail`, to the list it accepts (`sh:in`).
A rule with `compat:field` sub-rules compares the vehicle with records the edge path reaches
(a track's load capabilities, each a line category and a speed): with `compat:constraint
sh:hasValue` one record passes every field, with `sh:in` every record does; `compat:selectBy`
sub-rules pick the records that apply (a speed at least the vehicle's) and are checked only
together with a field. Each sub-rule's edge path starts at the record.

Rail works the same way:
[`profiles/rinf-compatibility.ttl`](../profiles/rinf-compatibility.ttl) holds the RINF rules
(gauge, gauging profile, energy supply, contact wire heights, current, ETCS level, class B
protection, train detection, train length, wheel diameter, and axle load: the track's
EN 15528 load capabilities, ranked in the profile by axle load and mass per metre, against the
vehicle's line category or axle load, at its maximum speed when it has one) for routing an
`era:VehicleType` over running tracks, with what of the RINF atlas rules it leaves out and why.
`rail:compatibilityShapes` (`pgr:vehicle`, `pgr:rules`, optionally `pgr:shape <iri>` naming
the shape) returns the compiled shapes as `pgr:subject`, `pgr:predicate`, `pgr:object` rows,
to inspect them or `INSERT` them into a graph.

Shapes can also be written by hand: `pgr:shapes <graph>` validates each edge's focus node
against the shapes of a named graph, by their targets (`sh:targetClass`, ...), or against
every shape named with `pgr:shape <iri>` whatever its targets. The focus node is `?id`, or
the `?c_*` column named by `pgr:focus "c_track"` (the track behind an edge, say); a row
without one passes, and an edge is left out when any of its rows fails. Validation reads the
default graph, or `pgr:data_graph <iri>`; a vehicle's values are read there and in the rules
graph. The validator is SHACL Core (in the `tripletrail-shacl` crate): every target and path, and
`sh:class`, `sh:datatype`, `sh:nodeKind`, `sh:minCount`, `sh:maxCount`, the four ranges,
`sh:minLength`, `sh:maxLength`, `sh:pattern`, `sh:languageIn`, `sh:uniqueLang`, `sh:equals`,
`sh:disjoint`, `sh:lessThan`, `sh:lessThanOrEquals`, `sh:not`, `sh:and`, `sh:or`, `sh:xone`,
`sh:node`, `sh:property`, `sh:qualifiedValueShape`, `sh:closed`, `sh:hasValue` and `sh:in`;
and SHACL-SPARQL, below. It says whether an edge conforms, not why (for the reasons, see
validation reports under [SHACL validation](validation-and-reasoning.md#shacl-validation)). Shapes are compiled once and each distinct focus node is checked once, with
direct lookups in the store, on several threads. The filtered graph is cached with the
network like a constraint's (with `pgr:engine "ch"`, the hierarchy is customized for it), by
shapes graph, or by rules graph and vehicle; it is dropped when a write touches a predicate
the shapes or rules read, or the SHACL and compat vocabularies (and on any write when the
shapes use SPARQL or rules, which can read anything). `pgr:constraint`, `pgr:shapes` and
`pgr:vehicle` can be combined.
On a generated road grid of 319,200 edges (every seventh with a weight limit, length limit
or bridge), validating every edge against a truck's three rules takes 1.0 s on the TripleTrail
store (3.2 µs per edge, on 4 threads; 2.0 s on Oxigraph's store), and a later query with the
same truck finds the filtered graph cached: 36 ms, as fast as without a filter.

### SHACL-SPARQL and rules

A shape can check what Core cannot say with a SPARQL query:
`sh:sparql [ sh:select "..." ]` (with `sh:prefixes`; `$this`, `$PATH` in a property shape,
`$currentShape` and `$shapesGraph` are pre-bound, and every solution is a violation). Here a
bridge's clearance, less the safety margin its owner sets, must be above 4 m:

```turtle
GRAPH ex:shapes {
  ex:prefixes sh:declare [ sh:prefix "ex" ; sh:namespace "http://example.com/"^^xsd:anyURI ] .
  ex:Clearance a sh:NodeShape ; sh:targetClass ex:Road ;
      sh:sparql [ sh:prefixes ex:prefixes ; sh:select """
          SELECT $this WHERE {
              $this ex:passesUnder ?bridge .
              ?bridge osm:maxheight ?h ; ex:owner/ex:safetyMargin ?m .
              FILTER (?h - ?m <= 4.0)
          }""" ] .
}
```

SPARQL-based constraint components work too (`sh:ConstraintComponent` with `sh:parameter`,
`sh:validator` ASK validators, `sh:nodeValidator` and `sh:propertyValidator` SELECT
validators; parameters are pre-bound by their local names, so the 4.0 above can be
`$vehicle ex:height ?vh` with `ex:vehicle ex:truck` on the shape), and so do SPARQL-based
targets (`sh:target [ a sh:SPARQLTarget ; sh:select "SELECT ?this ..." ]`, and target types
with parameters). Queries run on the engine's evaluator, so they can call the geo functions,
with the data graph as default graph and the shapes graph as `GRAPH $shapesGraph`. Rather than
one query per edge, the focus nodes are bound by a `VALUES` block of a few thousand at a time
(as if it were the first element of the `WHERE` clause; a query that neither projects `$this`
nor can, such as one aggregating without grouping by it, runs per node): on that grid, a
bridge clearance check takes 1.1 s as a SPARQL constraint against 0.5 s as a Core path
constraint (3.5 and 1.7 µs per edge on the TripleTrail store; 6.1 and 4.0 µs on Oxigraph's), and
2.6 s (8.2 µs) when a triple rule first copies the clearance onto the roads.

SHACL-AF rules (`sh:rule`) derive values first: an `sh:TripleRule` makes `sh:subject
sh:predicate sh:object` triples from node expressions (`sh:this`, constants, `[ sh:path ... ]`
with `sh:nodes`, `[ sh:filterShape ... ]`, `sh:intersection`, `sh:union`), an `sh:SPARQLRule`
runs a CONSTRUCT (`sh:construct`) with `$this` pre-bound. Rules run on their shape's focus
nodes that conform to their `sh:condition` shapes, in `sh:order`, in passes until nothing new
is inferred. When the `pgr:shapes` graph has rules, they run in memory before validation, so
constraints (and compatibility rules) see the derived values; nothing is written to the
store. `rail:shaclRules` (`pgr:shapes`, optionally `pgr:data_graph`) returns what they infer as
`pgr:subject`, `pgr:predicate`, `pgr:object` rows, to inspect or `INSERT`:

```sparql
INSERT { GRAPH ex:inferred { ?s ?p ?o } } WHERE { SERVICE <http://w3id.org/tripletrail/pgr> {
  [] pgr:function rail:shaclRules ; pgr:shapes ex:shapes ;
     pgr:subject ?s ; pgr:predicate ?p ; pgr:object ?o . } }
```

Severities and messages (`sh:message`) are ignored, as no reports are made; `sh:values`
property values and function node expressions are not supported.

### Geometries

When the edge sub-select projects `?geom` (a line, from source to target), path
results can bind `pgr:geom`: the edge's geometry in the direction it is traversed (part of it
for a `withPoints` point). `rail:routeGeometry` (or `pgr:route_geom`) is the whole route as one
`LINESTRING`, or a `MULTILINESTRING` where an edge does not start where the previous one ended,
and `rail:routeAsList` (or `pgr:route_list`) its edges in order, as a string separated by spaces
like pgRouting's arrays. When only route-level columns are asked for (`pgr:path_id`,
`pgr:start_vid`, `pgr:end_vid`, `pgr:agg_cost` as the total, `rail:routeGeometry`,
`rail:routeAsList`), there is one row per route rather than one per step.

### Isochrones

`rail:isochrone` takes `pgr:start_vid` and `pgr:distance` and returns, per start,
`pgr:geom`: the concave hull (`pgr:concavity`, default 2; lower is tighter) of the reached
vertices, the shapes of the reached edges and the points where the distance runs out along the
others; and `pgr:agg_cost`, the farthest cost reached. Coordinates come from `?x1 ?y1 ?x2 ?y2`
or from `?geom`.

### Time-dependent routing

`rail:routeAt` takes `pgr:departure` (a number, or an
`xsd:dateTime`, counted in seconds) and a periods sub-select,
`{ SELECT ?edge ?t_from ?t_to ?t_cost ?t_reverse_cost WHERE { ... } }`: entering `?edge` at a
time in `[?t_from, ?t_to)` costs `?t_cost` (`?t_reverse_cost` against the edge's direction)
instead of its usual cost. An unbound or negative `?t_cost` closes the edge (a possession), a
higher one slows it (a temporary speed restriction), and an unbound `?t_from` or `?t_to` leaves
the period open on that side. Periods are a separate sub-select so that they can change without
the network being built again, and an edge can have many. A train may wait at a vertex for an
edge to open or get faster (`pgr:wait false` forbids it); with waiting, arrival times never
decrease with departure times (FIFO) and the time-dependent Dijkstra is exact. Without waiting
it is exact only when no period lowers a cost. Costs then count time, waits included, and
`pgr:time` gives the time each vertex is reached, as a date-time when the departure is one.

### Hierarchical routing

`rail:routeHierarchical` routes on a coarse network (the edge
sub-select, or `pgr:network`) from `pgr:start_vid` to `pgr:end_vid`, then refines the route on a
fine network: `pgr:fine_network <iri>`, or a sub-select projecting `?fine_id ?fine_source
?fine_target ?fine_cost` (and the other columns with `fine_` in front). A mapping sub-select,
`{ SELECT ?coarse_edge ?fine_edge WHERE { ... } }`, says which fine edges make up each coarse
edge. The fine route is the cheapest from `pgr:fine_start_vid` to `pgr:fine_end_vid` (by default
the coarse start and end; several are alternatives) using only the fine edges of the coarse
route's edges (the corridor); if they do not connect, the whole fine network is searched.
Its rows add `pgr:coarse_edge` (the coarse edge a fine edge belongs to) and `pgr:corridor`
(false after the fallback). `rail:refine` does the second half for a coarse route given as a
sub-select of `?coarse_edge` rows (ordered by `?seq` when projected), with the fine network as
the edge set. `pgr:constraint` applies to the fine network.

### Multi-criteria routes

`rail:paretoRoutes` returns the Pareto-optimal paths from
`pgr:start_vid` to `pgr:end_vid` under several costs: no other path is at least as good in every
one and better in one. `pgr:criteria` names the cost columns of the edge set (or of
`pgr:network`), in a string separated by spaces or commas or as repeated triples: `cost` (`?cost`,
and `?reverse_cost` the other way) and `?c_*` columns, whose value applies both ways along an
edge. Which arcs exist still follows `?cost`/`?reverse_cost`; rows with an unbound criterion and
arcs with a negative one are left out, and `pgr:constraint`, `pgr:shapes` and `pgr:vehicle` filter
as for the other functions.

```sparql
[] pgr:function rail:paretoRoutes ; pgr:start_vid ex:a ; pgr:end_vid ex:d ;
   pgr:criteria "c_time c_energy" ; pgr:max_paths 20 ;
   pgr:path_id ?path ; pgr:total_c_time ?time ; pgr:total_c_energy ?energy ; pgr:route_list ?edges .
{ SELECT ?id ?source ?target ?cost ?c_time ?c_energy WHERE { ... } }
```

The search is Martins' label-setting algorithm (labels settled in lexicographic order of their
costs, with target pruning). Paths come in lexicographic order of their costs (the first is the
best by the first criterion, ties broken by the next), numbered by `pgr:path_id` per start and
end; paths with equal costs are reported once. Columns: `seq`, `path_id`, `path_seq`, `start_vid`,
`end_vid`, `node`, `edge`, a criterion's name (the edge's cost in it), `agg_<name>` (before the
edge), `total_<name>` (the path's), and `route_list`; `cost`, `agg_cost` and `total_cost` are the
first criterion's unless one is named `cost`. With only `path_id`, `start_vid`, `end_vid`,
`total_*` and `route_list` asked for, there is one row per path. The Pareto set can grow
exponentially with the network, so `pgr:max_paths` (default 100) caps it: at most that many
paths, and at most that many labels per vertex (a full vertex keeps its lexicographically
smallest ones). Within the cap the paths are Pareto-optimal and the first is always the
lexicographic optimum, but the set may be incomplete when the cap is reached. The
multi-criteria graph is built per query from the cached network (no hierarchy).

### Topology

These read the rows of an edge sub-select as they are, not a network:

- `rail:checkTopology` (`?id ?source ?target`, optionally `?cost ?reverse_cost`, `?geom` and
  `?x1 ?y1 ?x2 ?y2`) returns one row per problem: `pgr:problem` is `dangling_vertex` (a vertex
  with one edge end), `ends_not_coincident` (an edge end further than `pgr:tolerance` from the
  first edge's end at the same vertex, by id; `pgr:value` is the distance), `duplicate_id`
  (`pgr:value` is the count), `self_loop`, `missing_cost`, `no_direction` (both costs negative),
  `missing_geometry` (when `?geom` is projected) or `incomplete` (no id, source or target), with
  `pgr:edge` and `pgr:node`.
- `rail:analyzeGraph` returns one row: `pgr:edges`, `pgr:vertices`, `pgr:dead_ends`,
  `pgr:isolated` (edges with two dead ends), `pgr:self_loops`, `pgr:components`, and with
  `?geom` also `pgr:gaps` (dead ends within the tolerance of another edge), `pgr:intersections`
  (crossings away from edge ends) and `pgr:ring_geometries`. Without `?source ?target` the
  topology is built from the geometries first.
- `rail:buildTopology` (`pgr_createTopology`) takes `?id ?geom` and gives every edge
  `pgr:source` and `pgr:target` vertices, numbered from 1, by snapping line ends within
  `pgr:tolerance` (default 0), plus the snapped `pgr:x1 pgr:y1 pgr:x2 pgr:y2`.
- `pgr_separateCrossing` and `pgr_separateTouching` return the pieces (`pgr:id`, `pgr:sub_id`,
  `pgr:geom`) of the edges cut where another edge crosses them, or ends on them within the
  tolerance; `pgr_nodeNetwork` cuts at both and numbers the pieces' vertices (`pgr:old_id`,
  `pgr:sub_id`, `pgr:source`, `pgr:target`, `pgr:geom`).
- `pgr_findCloseEdges` takes `pgr:point` geometries (or a `?pid ?geom` sub-select),
  `pgr:tolerance` and `pgr:cap` (default 1), and returns per point the closest edges:
  `pgr:edge_id`, `pgr:fraction`, `pgr:side` (`l`, `r` or `b`), `pgr:distance`, `pgr:geom` (the
  closest point) and `pgr:edge` (a line from the point to it), with `pgr:pid` (or `pgr:point`).
  Its rows fit a `withPoints` points sub-select.

Lengths and fractions are measured in the geometries' coordinates (planar).

### Network profiles

Network profiles are ready-made edge queries for a data model, in [`profiles/`](../profiles):
`era-micro.rq` turns ERA RINF micro topology into an edge-based graph that respects switch
navigability (a train never goes from one diverging leg to the other), `era-micro-reversing.rq`
also lets trains reverse, `era-sol.rq` routes over RINF sections of line between operational
points, and `generic-triples.rq` reads edges written with the `pgr:` vocabulary. Other models (RailTopoModel, OSM, OpenTNF) are
just other queries; the routing core does not change.

## Paths over any predicates

`pgr:` routes over a weighted network built from an edge sub-select: the tool for rail routing,
costs, constraints and turn rules. For plain graph questions over any predicates, unweighted
("what is within 3 hops of this switch", "the chain of `skos:broader` up to the top", "how deep
is each class"), there are bounded property paths and the `path:` SERVICE (#234).

### Bounded property paths

`p{n}`, `p{n,m}`, `p{n,}` and `p{,m}` match `n` to `m` steps of `p`, as in Jena (from the SPARQL
1.1 drafts). Like `p+` they give each end once, however many walks reach it:

```sparql
SELECT ?near WHERE { ex:switch12 ex:connectedTo{1,3} ?near }
```

`{0,}`, `{1,}`, `{0,1}` and `{1}` are `*`, `+`, `?` and the predicate itself. Groups take
bounds too: `(ex:next/ex:next){2}`.

### Path search

`SERVICE <http://w3id.org/tripletrail/path>` returns the paths themselves, as rows, in the manner
of QLever's path search and Stardog's path queries (`PREFIX path: <http://w3id.org/tripletrail/path#>`):

```sparql
SELECT ?step ?from ?edge ?to WHERE {
  SERVICE <http://w3id.org/tripletrail/path> {
    [] path:source ex:opA ; path:target ex:opB ; path:predicate era:netRelation ;
       path:step ?step ; path:from ?from ; path:edge ?edge ; path:to ?to .
  }
} ORDER BY ?step
```

| Parameter | Meaning |
|---|---|
| `path:source`, `path:target` | Where paths start and end: IRIs or literals, given once or more (or as a list). Without a target every node reached is an end; without a source the search runs backwards from the targets. |
| `path:predicate` | The edges: predicates followed from subject to object (once or more, or a list). Or, instead, a sub-select whose first two variables are an edge's ends and an optional third its label. With neither, every triple from an IRI to an IRI, read from the indexes as the search reaches each node. |
| `path:exceptPredicate` | Without `path:predicate` or a sub-select: predicates left out (once or more, or a list), such as `rdf:type`. |
| `path:direction` | `"out"` (default), `"in"` (object to subject) or `"both"`. |
| `path:graph` | Search one named graph's edges instead of the default graph's. |
| `path:minLength`, `path:maxLength` | In edges; 0 and unlimited by default. |
| `path:mode` | `"shortest"` (default): one shortest path from each start to each end it reaches, the first found breadth first. `"simple"`: every path that visits no node twice. `"all"`: every walk, nodes repeated, which needs `path:maxLength`. |
| `path:maxPaths` | At most this many paths from each start to each end (`simple` and `all`). |

Outputs, each bound to a variable: `path:path` (the path's number, from 1), `path:start`,
`path:end`, `path:length`, and per edge `path:step` (from 1), `path:from`, `path:edge` and
`path:to`, in the direction walked. With a per-edge output there is a row for each edge (a
path of no edges gives one row with them unbound); without one, a row for each path. A call
returning more than a million edges in all is refused; it stops with the query's timeout.

Over every predicate, a shortest path from one source to one target is searched breadth first
from both ends at once, a whole level of the smaller side at a time, until the two meet. It
reads the neighbours of the nodes within about half the path's length of each end, not the
whole dataset (#819). When several shortest paths tie, it may give another of them than the
search from the source alone would.

Hop counts, as Virtuoso's `t_step('step_no')`, are the shortest mode's ends and lengths:

```sparql
# The depth of each class below owl:Thing.
SERVICE <http://w3id.org/tripletrail/path> {
  [] path:target owl:Thing ; path:predicate rdfs:subClassOf ; path:minLength 1 ;
     path:start ?class ; path:length ?depth .
}
```
