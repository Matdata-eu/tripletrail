# Phase 0 spike: findings

> **Historical.** This records the first spike, at the start of the project, and is kept as it was. The
> numbers and limits below are long outdated: see the [README](../README.md) for what TripleTrail
> does now and [the benchmark repository](https://github.com/Matdata-eu/tripletrail-bench) for current measurements.

The spike checks the plan's riskiest assumption: that spatial functions and pgRouting-style
routing can be built on the Oxigraph crates through their extension hooks, without forking. It
also measures where time goes, to choose between runtime edge queries and materialised networks.

Two benchmarks: `tripletrail-bench` on **generated** data shaped like ERA RINF micro topology, which
scales the network up to continental size, and `rinf` on the **real** RINF knowledge graph
published by ERA (see [Real RINF data](#real-rinf-data)). Machine for both: 4 vCPU, 15 GB cloud
container, release build.

## What was built

- `tripletrail-routing`: a routing core that only sees a pgRouting edge set; CSR graph; Dijkstra
  (one-to-many), bidirectional Dijkstra, A* (pgRouting heuristics plus a geodesic one), driving
  distance.
- `tripletrail-pgr`: pgRouting functions as `SERVICE <http://w3id.org/tripletrail/pgr>`; the edge set is a
  nested SPARQL sub-select (the `edges_sql` analogue) or a named network stored as
  `<net> pgr:edgeQuery "SELECT ..."`. Built graphs are cached and invalidated on writes.
- `tripletrail-geo`: 33 `geof:` and 43 `st:` functions through `with_custom_function`.
- `profiles/era-micro.rq`: the ERA network profile as a plain query. A test proves that it never
  routes from one switch leg to the other and never reverses on an element.
  `profiles/era-micro-reversing.rq` adds reversal edges, `profiles/era-sol.rq` routes over
  sections of line between operational points.

## Results on generated data

Oxigraph in-memory store.

| | 1.2 M triples (115 k elements) | 9.9 M triples (979 k elements) |
|---|---|---|
| Bulk load (Oxigraph, in memory) | 2.3 s | 37 s (266 k triples/s) |
| Edge-set rows / graph vertices | 237 k / 230 k | 1.98 M / 1.96 M |
| Edge query evaluation (ERA profile) | 2.4 s | 24.1 s |
| Term interning + CSR build | 0.8 s | 8.0 s |
| `pgr_dijkstra` search p50 / p95 | 10.9 / 20.2 ms | 137 / 228 ms |
| `pgr_bdDijkstra` search p50 / p95 | 7.8 / 21.2 ms | 100 / 248 ms |
| Routing query total p50 (cached graph, full path rows) | 14.2 ms | 168 ms |
| `sfIntersects` FILTER over every geometry | 0.23 s | 2.3–2.5 s |
| `ST_LineLocatePoint` on every line | 0.65 s | 7.1 s |
| Peak RSS (includes the generated quads) | 0.9 GB | 6.2 GB |

The 979 k-element grid is continental in size (random routes average ~6 800 elements), so the
search times are pessimistic for one country's network.

## Findings

1. **No fork needed so far.** The custom-function and `SERVICE` hooks carry the whole
   pgRouting-style API: runtime edge queries, named networks, all result columns as bindings that
   join back to the data. Two limits showed up. The handler gets no outer bindings (so routing
   start/end must be constants in the block) and reads the store directly rather than the query's
   snapshot. Both matter for phase 1's planner integration, not for the API.
2. **Per-row function overhead, not parsing, dominates spatial filters.** A FILTER costs about
   2.4 µs per row whether or not parsed geometries are cached; parsing these small linestrings is
   noise next to the evaluator's per-call term handling. Without an index every spatial filter is
   a full scan. This confirms the plan's phase-1 priorities: spatial index and own spatial
   operators below `spareval`, geometry metadata stored next to term ids.
3. **Runtime edge queries must be cached; named networks are the default at scale.** Building the
   graph from SPARQL costs about 16 µs per edge (12 µs evaluating the query, 4 µs interning RDF
   terms). That is 33 s for a 2 M-edge network, while a cached search takes 0.1 s. Runtime edge
   queries suit small or filtered subsets; a whole RINF network should be a named network.
   Phase 1's term-id storage removes most of the interning cost.
4. **Plain Dijkstra is not "blazingly fast" at EU scale.** 100–140 ms median on 2 M vertices, and
   bidirectional search gains little on this graph shape. Customisable contraction hierarchies
   (CCH, planned for phase 2) are needed for millisecond answers at EU scale; at single-country
   scale (~200 k vertices) plain search is already around 10 ms.
5. **Plan correction.** The architecture's `pgr:edges { SELECT … }` syntax is not valid SPARQL (a
   group pattern cannot be a triple object). The spike nests the sub-select directly inside the
   `SERVICE` block and tells edges from restrictions by their projected variables.
6. **Profile-writing pitfall.** In SPARQL a `FILTER` inside one `UNION` branch cannot see variables
   bound outside it, so each branch of a profile must repeat its triple patterns (as
   `era-micro.rq` does).

## Real RINF data

Source: the public `rinf-plus` repository of graph.data.era.europa.eu (ERA ontology 3.1),
exported on 2026-10-31 as N-Triples, one file per named graph: 61 graphs, 41.6 M lines, 7.5 GB,
41.1 M distinct triples. Every member state publishes sections of line and operational points;
only Norway (graph `0076`) and Lithuania (graph `1224`) publish micro topology (13 442
linear elements with length and geometry, 21 976 net relations). The public Norwegian graph is
1.6 M triples, about a tenth of a full in-house micro topology. Run with
`cargo run --release -p tripletrail-bench --features rocksdb --bin rinf -- [--store <dir>] <files>`.

Pairs are 100 (single country) or 200 (EU) random places; "routable" counts pairs with a path.
Search and query times are over the routable pairs on a cached network.

| | Norway, in memory | Lithuania, in memory | All of RINF, RocksDB |
|---|---|---|---|
| Triples | 1.59 M | 0.42 M | 41.1 M |
| Bulk load | 4.1 s | 2.0 s | 123 s (335 k triples/s), 5.6 GB on disk |
| Peak RSS | 0.85 GB | 0.83 GB | 2.4 GB |
| **Micro topology** (era-micro) | | | |
| Graph vertices / arcs | 15.3 k / 18.0 k | 11.2 k / 12.2 k | 26.5 k / 30.2 k |
| Network build (edge query + CSR) | 0.28 + 0.05 s | 0.17 + 0.02 s | 2.52 + 0.07 s |
| Routable pairs | 16 % | 1 % | 4.5 % |
| **Micro, reversing** (era-micro-reversing) | | | |
| Network build | 0.31 + 0.05 s | 0.20 + 0.03 s | 3.00 + 0.10 s |
| Routable pairs | 89 % | 12 % | 36 % |
| `pgr_dijkstra` search p50 / p95 | 0.8 / 1.8 ms | 0.3 / 0.5 ms | 1.1 / 1.9 ms |
| `pgr_dijkstra` query total p50 / p95 (~280 path rows) | 1.6 / 3.5 ms | 0.7 / 1.2 ms | 2.6 / 5.1 ms |
| `pgr_dijkstraCost` query p50 | 1.0 ms | 0.4 ms | 1.1 ms |
| **Sections of line** (era-sol) | | | |
| Graph vertices / arcs | 702 / 1.4 k | 187 / 422 | 60.1 k / 139 k |
| Network build | 6 ms + 1 ms | 2 ms + 0.3 ms | 2.97 + 0.16 s |
| Routable pairs | 52 % | 98 % | 16 % |
| `pgr_dijkstra` search p50 / p95 | 0.1 / 0.2 ms | 0.0 / 0.1 ms | 1.5 / 2.5 ms |
| `pgr_dijkstra` query total p50 / p95 | 0.3 / 0.6 ms | 0.2 / 0.3 ms | 4.2 / 15.4 ms |
| `pgr_drivingDistance` 100 km | 0.3 ms | 0.3 ms | 12.7 ms (734 points) |
| **Spatial** (no index) | | | |
| `geo:asWKT` literals (parsed) | 66 236 (all) | 10 875 (all) | 304 218 (304 135) |
| `sfIntersects` FILTER, city box | 0.33–0.50 s | 0.10–0.15 s | 2.7–3.8 s |
| `geof:distance` < 2 km FILTER | 0.20 s | 0.04 s | 2.4 s |
| `SUM(geof:metricLength)` over linestrings | 0.59 s | 0.30 s | 3.8 s (330 000 km) |
| `ST_LineLocatePoint` on every linestring | 0.25 s (25 k) | 0.08 s (8.7 k) | 4.8 s (148 k) |

For comparison, the Norway network on RocksDB instead of memory: load 6.2 s, edge query 1.9 s
instead of 0.28 s, searches unchanged.

### What the real data changed

7. **Routing is fast on real networks; building the network is what costs.** On the real micro
   topology a route with full path rows answers in 1–5 ms, and EU-wide over 60 k operational
   points the search takes 1.5 ms. The synthetic grid's 100+ ms came from its continental size
   (2 M vertices); real published networks are 10–100 times smaller. Building a network from
   its SPARQL edge query costs 2.5–3 s on the RocksDB store, 40–80 µs per edge row; the same
   Norwegian network builds 7 times faster from the in-memory store (15 µs per row). That
   strengthens finding 3: named, materialised networks are the default, and phase 1's own
   storage should make the edge scan cheap. A micro topology for all of Europe, once published,
   will be closer to the synthetic grid's size, so finding 4 (CCH) still stands.
8. **The shipped profile used the wrong vocabulary.** Published RINF uses
   `era:LinearElement` with `era:lengthOfNetLinearElement` (metres), navigability concepts under
   `http://data.europa.eu/949/concepts/navigabilities/` (only `Both` and `None` occur) and
   `era:lengthOfSectionOfLine` in km. `era-micro.rq` and the synthetic generator now match it.
   The origin flags agree with the element geometries for all 12 884 Norwegian relations
   (checked against linestring end points within 5 m).
9. **A no-reversal profile is too strict for real railways.** Norway's micro topology is one
   connected network for 96 % of its elements, yet only 16 % of random element pairs are
   reachable without reversing, because branches that meet at a junction facing away from each
   other need a reversal. With a reversal edge per element (cost: element length + 1 km),
   reachability rises to 89 %. Where reversal is allowed is a profile decision (stations only,
   with dwell-time costs); the engine needs nothing new for it, but turn restrictions
   (`pgr_trsp`) would express it more cheaply than doubling the element edges.
10. **Published topology is fragmented, so "no route" is common and must be cheap.** Lithuania's
    micro topology splits into 228 components (largest 1 725 of 5 683 elements). The EU section
    of line network has 200 components: national networks are not joined at borders (Germany
    19 k points, France with Italy 14 k, Poland 4 k, ...), so only 16 % of random EU pairs connect.
    Failed searches are cheap on networks this small (median under 1 ms), but a component id
    per vertex would answer them without searching; it belongs in the materialised network.
11. **Spatial work scales linearly with the store, as expected without an index.** Every spatial
    filter over all 304 k RINF geometries takes 2.4–3.8 s, whatever the hit count (761 hits in
    the Brussels box). Scanning the `geo:asWKT` literals alone takes 1.6 s on RocksDB, so about
    half of every filter is the store scan; the R-tree work in phase 1 removes both parts.
    The second run of each filter (parse cache on) is 10–30 % faster on RocksDB, partly from a
    warmer block cache; in memory the cache is still noise.
12. **Data hygiene issues the loader must tolerate.** 83 of 304 218 WKT literals do not parse
    (not investigated further; the 120 k with sign-prefixed coordinates such as `+49.2670` do).
    Three Lithuanian `era:digitalSchematicOverview` literals exceed Oxigraph's 16 MB N-Triples line
    limit (up to 24 MB) and were dropped. The public endpoint truncates CONSTRUCT exports after
    a few minutes, so the two largest graphs (Germany 11.9 M, France 6.6 M triples) had to be
    exported per predicate.

## Not covered yet

Turn/via restrictions (`pgr_trsp`), k shortest paths, the spatial index, metric functions for
projected CRSs other than planar, GEOS-only functions, a full-RINF in-memory run (it would need
about 22 GB at Norway's 0.53 GB per million triples) and OSM data (download.geofabrik.de still
resets connections from this environment). A comparison against PostGIS/pgRouting on the same
RINF extract is the obvious next benchmark.
