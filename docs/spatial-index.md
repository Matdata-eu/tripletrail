# Spatial index

Phase 0 measured spatial FILTERs as full scans: about 2.4 µs per geometry, so roughly 0.5 s per
query at 240k geometries, whatever the size of the result. This adds an R-tree and a query
rewrite that uses it.

## How it works

`tripletrail-spatial::SpatialIndex` collects every geometry literal in the store (`geo:wktLiteral`,
`geo:geoJSONLiteral`, `geo:gmlLiteral`, `geo:kmlLiteral`, H3), whatever predicate carries it,
parses each distinct literal once, and bulk-loads its envelope into an R-tree (`rstar`). The
TripleTrail store keeps geometry literals as their own kind of term, so it finds them from the quad
ids and decodes only the geometries; Oxigraph's store is scanned. Geographic geometries (CRS84,
EPSG:4326) share one tree; each other CRS gets its own tree. Literals that are not valid, or are
empty, are skipped and counted.

Many datasets put their geometries on their own predicates (GeoNames, LinkedGeoData, CORINE,
osm2rdf), so the index is not limited to `geo:asWKT`. `IndexedPredicates` limits it to some
predicates (`only`) or leaves some out (`except`), for large geometries no query tests
spatially; set it with `Engine::set_indexed_predicates`, or the server's `--index-predicate` and
`--no-index-predicate`.

`rewrite_query` runs on the parsed query before evaluation. It looks for a FILTER conjunct of one
of these forms, where `?w` is the object of a triple pattern in the filtered pattern whose
predicate the index covers (a variable predicate when it covers every predicate). Values of `?w`
that are not geometry literals are an error for every spatial function, so the FILTER rejects
them anyway and the candidate block loses no row. Nearest-neighbour queries need every value of
`?w`, errors included, so they still need `?w` bound by `geo:asWKT`:

| Form | Candidates |
|---|---|
| `geof:sfIntersects / sfWithin / sfContains / sfTouches / sfCrosses / sfOverlaps / sfEquals / ehCovers / ehCoveredBy (?w, C)`, either argument order, and the `st:ST_*` equivalents | envelope of `C` |
| `geof:distance(?w, C [, unit]) < d` or `<= d`, either side | envelope of `C` grown by `d` metres |
| `st:ST_DWithinSphere(?w, C, d)`, `st:ST_DistanceSphere(?w, C) < d` | envelope of `C` grown by `d` metres |
| `st:ST_DWithin(?w, C, d)`, `st:ST_Distance(?w, C) < d` | envelope of `C` grown by `d` CRS units |

It then replaces `FILTER(e) { P }` with `FILTER(e') { VALUES ?w { candidates } P }`. Every
candidate has passed the test itself, on the geometry parsed once for the index, so the test is
taken out of `e` (`e'` is the rest of the FILTER, and without a rest there is no FILTER): every
row the block lets through passes it, and evaluating it again on each row would only parse and
test the same geometries a second time. A metre margin on
geographic data is converted to degrees conservatively (110 km per degree, widened for longitude
by the latitude). Only geometries in the constant's CRS are candidates, which matches the
functions: mixing CRSs is an error, and an error filters the row out.

The Egenhofer and RCC8 relations are rewritten like their Simple Features counterparts. Not
rewritten: `sfDisjoint`, `ehDisjoint` and `rcc8dc` (most geometries qualify), `?w` bound only
inside an OPTIONAL or by BIND, and anything inside a `SERVICE`.

### Joins between two variables

A conjunct testing two variables, `geof:sfIntersects(?w1, ?w2)` or `geof:distance(?w1, ?w2) < d`,
becomes `VALUES (?w1 ?w2) { candidate pairs }`. The pairs come from probing the R-tree with each
geometry of one side:

- If a constant test in the same FILTER already narrows one variable (a window), that side is
  probed and the other side is left to the join.
- Otherwise, if the two variables are bound by parts of the pattern that share no variable, each
  side is evaluated on its own (a `SELECT DISTINCT ?w` over its triples) and the smaller side
  probes, keeping only partners from the larger side.
- Otherwise every indexed geometry probes, up to 2 million candidate pairs; beyond that the
  FILTER is left to the evaluator. With the cost model and from 2000 indexed geometries, this
  join of every geometry with every other is swept instead (below). The sweep's time grows
  with its input and output, so only memory bounds it: beyond 5 million result pairs the
  FILTER is left to the evaluator (the 3.4 million pairs of the all-pairs join below took
  3.4 GB at peak).

When both sides are large, the cost model answers the join with a sweep-line join instead of
probing the R-tree once per geometry:

1. Each side's geometries are boxed (parsed geometries are cached), in parallel; the side that
   would have probed gets its boxes grown by the distance margin, exactly as for a probe
   (metres to degrees on geographic data, wider in longitude by the latitude). A side that was
   not evaluated takes the boxes of every indexed geometry. Geometries are grouped by
   coordinate space, like the index's trees.
2. The sweep axis (x) is cut into stripes holding about the same number of boxes, 4 per core.
   A box goes into every stripe its x range meets.
3. Each stripe is swept on its own, on every core: both sides' boxes in `minx` order, each side
   keeping its active set (boxes not yet closed at the sweep position). A box opening is tested
   against the other side's active boxes, after dropping those that closed before it. A pair
   is reported only by the stripe holding the lower-left corner of the two boxes'
   intersection (the larger `minx`), so a pair met in several stripes comes out once.
4. Each pair of meeting boxes gets the same exact test as a probe's candidate (the function
   itself, and the constant tests of the FILTER left to it), still on the stripe's core.

The pairs, and so the results, are those of the nested loop. As for a constant test, the
two-variable test is taken out of the FILTER: the block holds exactly the pairs that pass it.

On the tripletrail store, when both sides are triple patterns the join runs on term ids (#167):
each side's rows are read on ids, and its geometries enter the sweep as their bounding boxes,
read from the checkpoint. A checkpoint saves the box of every geometry whose binary record is
longer than 64 bytes (anything but a point or a short line) next to the records, so reading the
boxes of a side doesn't read its records: for 33 million landuse polygons that is about 1 GB
instead of about 30 GB. Pieces written before this keep reading the box from the record until
they are written again. The join then runs in two passes (#2):

1. The sweep reads the boxes of both sides and parses only the smaller side's geometries (the
   probe side, kept in the store's cache as usual). A pair is a candidate when a part of the
   probe comes near the other's box: a point, or the box of a segment of a line, grown by the
   margin. A long or bent line's box covers many polygons it never comes near, which this
   leaves out. A probe with an area keeps every pair whose boxes meet.
2. The candidates are grouped by the larger side's geometry, in id order, and tested on every
   core: each of those geometries is read once, in about the order the store keeps them, and
   is not kept in the cache.

So a join of 1.25 million tracks with 33 million landuse polygons reads only the polygons near
a track, each once, and holds few of them at a time (#754). On a synthetic set of 8 million
polygons and 308,000 lines (`cargo run --release -p tripletrail --example landuse_join`), the
join answers in 23 to 26 s at 6 GB peak; before, it kept every polygon it tested and ran out of
the machine's 15 GB. Beyond 64 million candidates (1 GB), the probe side is split in two along
x and each half is swept again against the polygons within its reach, until each part's
candidates fit (#340). Before, the pairs were then tested as the sweep found them, which kept
every polygon it read: on the synthetic set above with 2 million polygons and the cap lowered to
300,000, the join took 4.4 s at 3.4 GB before and takes 4.4 s at 450 MB now. On OSM Europe
(1.25 million tracks, 33 million landuse polygons) the heap reached 55 GB on a 64 GB machine
and the join timed out. Only a single probe geometry with more candidates than the cap still
tests them as they are found. A constant test of the FILTER still parses its side's geometries
first. This join
doesn't read the R-tree, and so doesn't wait for the index to be built or loaded.

### Exact tests

The exact tests run on the parsed geometries (`tripletrail_geo::geom_predicate`), the index entries'
for indexed candidates, not on RDF terms through the text-keyed parse cache. The relations take
fast paths before the full DE-9IM `relate()` of the `geo` crate, which builds a topology graph
of both geometries on every call:

- a point against a polygon is a point-in-polygon test, against a line an on-segment test, and
  against a point an equality; the DE-9IM matrix of these cases is fixed by the point's
  position, so every relation (Simple Features, Egenhofer, RCC8) is answered from it;
- `sfIntersects` and `sfDisjoint` of a point, line or polygon with a polygon look for a vertex
  inside the other geometry or a crossing pair of edges, and never build the matrix.

A polygon's edges are indexed once, in horizontal bands, and kept with its parsed geometry, so a
polygon met by many candidates (a region in a join) is prepared once. The index nested loop
tests its candidate pairs on every core, as the sweep does. When one side of a join is clearly
smaller and probes, the other side is still evaluated if it is at least 4 times smaller than
the index: candidates of other classes found in the same place (track elements and signals in
a region, for operational points) are then skipped before their exact test rather than in the
final join.

Only one pair block is added per FILTER; further two-variable tests are checked by the FILTER on
the joined rows. The VALUES blocks are placed first in a left-deep join, and each following join
operand is picked to share a variable with what is already joined, so evaluation starts from the
candidates instead of the cartesian product of unconnected parts.

### Joins on term ids (TripleTrail store)

On the TripleTrail store, a two-variable test whose sides are plain basic graph patterns is
answered without the evaluator and without decoding a term:

1. each side is joined on term ids by the store (`Snapshot::join_ids`: triples from the
   smallest, each by a hash join over one scan of its permutation or an index nested loop,
   whichever costs less);
2. the distinct geometry ids of each side are looked up in the store's parsed geometries, the
   constant tests left to the FILTER drop those that fail them, and the two sets are swept with
   the test's margin on the side with fewer geometries;
3. every pair of rows sharing a passing pair of geometries becomes a row of a result table of
   term ids, which the evaluation reads through virtual triples
   `_:row <urn:tripletrail:table:N:C> ?var` that the snapshot answers from the table. The table
   replaces both sides, so nothing is looked up again.

There is no pair cap: the table holds up to 128 M term ids (1 GB), beyond which the join falls
back to the `VALUES` path. Tables are registered on the thread that rewrites the query and taken
by the snapshots of its evaluation; they are dropped when the query (or update) ends. The
explain output shows the decision as `join ?a (n rows) x ?b (m rows) on term ids: ...`. With
the sweep join turned off, or on Oxigraph's store, inside `GRAPH`, with a query dataset or a
union default graph, joins go the `VALUES` way described above.

Aggregates over a plain basic graph pattern (`SUM(geof:metricLength(?w))` over every linear
element) are evaluated the same way: the pattern is joined on ids and the functions read each
geometry by its id, parsed, instead of by its text.

On benchmark v2 (large set, 4 cores), against the previous `VALUES` path:

| Query | Before | On term ids | PostGIS | QLever |
|---|---:|---:|---:|---:|
| `agg_total_length` (344k elements) | 0.72 s | 0.35 s | 0.33 s | 0.32 s |
| `join_signal_ne_20m` (316k pairs) | 5.4 s | 1.5 s | 1.07 s | 3.6 s |
| `join_op_region` | 0.56 s | 0.17 s | 0.06 s | 0.45 s |
| `agg_length_region` | 5.9 s | 1.9 s | 5.2 s | |

### Relation triples

`?a geo:sfIntersects ?b` (any relation above) follows the GeoSPARQL Query Rewrite Extension. Each
side becomes its geometry literal: its own `geo:asWKT` if it is a geometry, its default geometry's,
or any geometry's if it has no default geometry. The relation is then a FILTER on those literals,
which goes through the same planning as a written FILTER. If the store asserts relation triples
with that predicate, the original pattern is unioned in. The "no default geometry" branch uses
`MINUS` when the side is a variable: `FILTER NOT EXISTS` was evaluated once per row and made the
all-pairs query 100 times slower.

### Nearest neighbours

A SELECT whose solutions are ordered first by ascending `geof:distance(?w, C [, unit])`,
`st:ST_Distance(?w, C)` or `st:ST_DistanceSphere(?w, C)` (directly, or through a variable bound to
it) and cut by `LIMIT k` is answered by growing a radius `r`, starting at 10 (metres or CRS units)
and multiplying by 4. Each round runs the query with `?w` restricted to the geometries within `r`
(from the index) and keeps it if it returns `k` rows: every row it left out is farther than `r`,
so it sorts after those `k`. Rows whose distance is an error (another CRS, an invalid literal)
are always kept, because SPARQL sorts errors before every value. The rounds stop when the radius
takes in every geometry of the constant's CRS; after 16 rounds the plain query runs.

### Cost model

The choices above are fixed rules. With statistics (the default in `Engine`), the rewrite makes
three of them from cardinality estimates instead:

- **Triple patterns**: how many triples match, constants bound and variables free. The TripleTrail
  store answers from the block index of the quad permutation the pattern would scan: every block
  but the last holds 32 quads, so only the two blocks at the ends of the range are read, plus the
  write delta, and the count is exact. Oxigraph's store has no such count, so the matches are
  counted, stopping at 250,000. The engine caches the counts and drops those of a predicate when
  a write it knows of touches it (all of them on `load` and `invalidate`).
- **A join of patterns**: connected operands (through variables or blank nodes) are estimated
  by the smallest one, as joins through features and geometries are mostly one to one;
  unconnected groups multiply, a UNION adds up. For the rows binding a constant test's
  variable, triples alone are estimated by the TripleTrail store as its join planner does
  (#209): from the triple with the fewest matches, times the matches per value of each next
  triple, read from the per-predicate fanouts kept with the quad indexes. So
  `?v ex:country ?c . ?o ex:vendor ?v . ?o geo:hasGeometry/geo:asWKT ?w` counts a row per
  offer, not one per vendor, and a window over the offers is injected.
- **Index candidates of an envelope**: each R-tree has a 64 x 64 grid histogram of its entries'
  bounding-box centres, with the mean bounding-box size; the estimate sums the cells the
  envelope (grown by the mean half size) overlaps, by the share it overlaps. Below 4096 the
  R-tree entries are counted instead (no geometry tests). The histogram is built on the first
  estimate and dropped when the index is patched.

The decisions:

1. **Constant test: VALUES or not.** The candidates are injected only if the index estimate is at
   most half the estimated rows of the operands binding the variable (always at 32 or fewer).
   A test that takes in most of the store, or whose variable the rest of the pattern pins to a
   few rows (`?op era:opName "X"`), is left to the FILTER: each candidate costs an exact test at
   planning, a lookup and the FILTER again, where the FILTER alone costs one test per row. A
   constant test left to the FILTER still narrows a join of the same FILTER: geometries failing
   it are neither probed nor paired.
2. **Join: which side probes.** If one side is estimated at least 4 times smaller than the
   other (a side narrowed by a constant test counts with its exact size), only that side is
   evaluated and it probes; the other side is not evaluated, and its candidates are checked by
   the final join. Otherwise both sides are evaluated and compared, as before.
3. **Order of the candidate block.** Oxigraph's optimizer orders joins by guesses from which
   positions are constants: `?f a ex:C` is 1001 rows to it, whatever the data, a VALUES block
   its real size. When a lone candidate block is bigger than the guess for the triple the
   optimizer would start from, but smaller than that triple's estimate, the block and the
   triples binding its variables become `LATERAL(VALUES, triples)`, so each candidate is looked
   up in turn. Only plain triples go under the `LATERAL`, where binding the candidates first
   cannot change their solutions. When the triples joined to the block fall into groups that
   only the block connects (the two sides of a join), the optimizer joins each group and then
   takes their cartesian product: the block is pinned when it is smaller than the product of
   the groups' estimates.
4. **Join: index nested loop or sweep.** Once the geometries of each side are known (the
   probe side, and the other side or, if it was not evaluated, every indexed geometry), the
   sweep-line join is used when the smaller side has at least 2000 geometries and the larger
   at most 32 times as many: below that, the probes are few and cheap; above it, the sweep
   would sort a whole side to meet a few geometries the R-tree reaches directly. With the
   fixed rules, joins always probe.

`RewriteStats` (`Engine::last_spatial_stats()`) counts the skipped tests (`skipped`), the join
sides evaluated (`sides_evaluated`), the swept joins (`sweeps`) and the pinned blocks
(`pinned`), and lists each decision
with its estimates (`decisions`); `Engine::explain` and the server's `/explain` include them.
`Engine::set_cost_model(false)` goes back to the fixed rules, `Engine::set_sweep_join(false)`
keeps the cost model but probes in every join, and
`Engine::estimate_triples` gives the estimate of one pattern.

The `Engine` builds the index on the first spatial query. After a write whose changed quads it
knows (`load_quads`, `insert`, `remove`, and `update` on the TripleTrail store), it adds and removes
the changed geometry literals in the trees, checking the store so that a literal stays as long
as some quad with an indexed predicate still has it. After any other write (`load`, `update` on Oxigraph's
store) it drops the index, which is rebuilt on the next spatial query, or at once, in the
background, after `Engine::warm_up` (which the server calls at start: the index is then ready
before the first spatial query, see `/$/status`).

### Timeouts and services

The rewrite runs before evaluation and can take a while (it evaluates join sides and tests
candidate pairs), so an engine with a cancellation token (`Engine::with_cancellation_token`, the
server's `--timeout`) checks it between candidate tests and stops: the query or update then
fails as cancelled (HTTP 503) instead of planning on past its deadline. A join on term ids
checks it too while it reads its sides' geometry boxes and while it sorts and sweeps them, and
a join whose side was given up on for the timeout ends there: it does not fall back to probing
the index, which would evaluate both sides again and, on a large dataset, run for minutes past
the timeout (#338).

A join on term ids that has run for 5 s logs each of its steps to standard error as the step
ends (#340), so a join cut off by its timeout still shows where its time went:

```text
tripletrail-spatial: slow join, at <seconds> s: sides read: ?w1 <n> rows (<n> geometries), ?w2 <n> rows (<n> geometries)
tripletrail-spatial: slow join, at <seconds> s: <n> probes swept against <n> geometries: more than 67108864 candidates
```

The steps are reading the sides, the constant tests, each sweep (with its candidates, or
"more than" the cap when the probes are split), the tests of the candidates and the table of
rows.

A join side may start from a `SERVICE`, such as a route from the in-process `pgr` service. The
planner (`plan.rs`) starts the join from the service rows and looks up the triples connected to
them once per row, with `LATERAL` joins, instead of Oxigraph's join with a full scan of those
triples (a variable bound by `BIND` is no join key for it). On bench v2's `mixed_route_signals`
(a route, then the main signals within 50 m of its elements) that took the large set from 17 s
to 0.8 s, under PostGIS's 1.16 s.

## Results

Synthetic ERA-shaped network, `tripletrail-bench 100 100 10`: 242k geometries, 2.9M triples, 4 cores.
Median of 5 runs for the indexed column.

| Filter | Hits | Full scan | Indexed |
|---|---:|---:|---:|
| sfIntersects, large polygon | 1269 | 520–790 ms | 25–28 ms |
| sfIntersects, small window | 24 | 420–520 ms | 0.6–0.9 ms |
| geof:distance < 2 km | 9 | 460–570 ms | 0.2–0.3 ms |
| ST_DWithinSphere 3 km | 26 | 530–570 ms | 0.3–0.5 ms |

Building the index takes 2.3–2.9 s for 242k geometries. About 1.1 s of that is reading the
`geo:asWKT` quads from Oxigraph's in-memory store (about 4.5 µs per quad), about 0.6 s is WKT
parsing (spread over all cores) and about 0.3 s is the R-tree bulk load. The large-polygon query
is dominated by evaluating the remaining FILTER and joins for its 1269 hits.

Joins, same data, one run:

| Join | Rows | Time | Of which index lookups |
|---|---:|---:|---:|
| elements in a small window × elements they intersect (FILTER) | 72 | 2.9 ms | 1.0 ms |
| all pairs of intersecting elements (`?a geo:sfIntersects ?b`) | 3,378,400 | 34.6 s | 11.2 s |

Without the index both joins are nested loops over 242k × 242k pairs, which do not finish. The
all-pairs join is about 10 µs per result row, and its VALUES block of 3.4M pairs drives peak
memory to 3.4 GB; at 22k geometries it returns 308k rows in 2.0 s.

Nearest neighbours, same data, median of 5 runs:

| Query | Full sort | Indexed |
|---|---:|---:|
| 1 nearest geometry | 1047 ms | 1.6 ms |
| 10 nearest geometries | 945 ms | 1.6 ms |
| 100 nearest geometries | 991 ms | 6.1 ms |

### Cost model

In [tripletrail-bench](https://github.com/Matdata-eu/tripletrail-bench), `cargo run --release -p tripletrail-bench --bin cost [h v k]` generates the network above with an
operational point (named, typed) every 25 net elements and a signal every other one, each with a
point geometry, and times each query with the fixed rules and with the cost model (median of 5,
after a first run), checking both return the same rows. With `h = v = 40, k = 20`: 72,000 net
elements, 2,880 operational points, 36,000 signals, 952k triples, 4 cores.

| Query | Rows | tripletrail: fixed | tripletrail: costed | Oxigraph: fixed | Oxigraph: costed | Decision |
|---|---:|---:|---:|---:|---:|---|
| named OP in a polygon around the whole network | 1 | 1358 ms | 0.5 ms | 1756 ms | 0.7 ms | leave to FILTER (~107k candidates vs 1 row) |
| every geometry in that polygon | 110,880 | 2319 ms | 741 ms | 2964 ms | 927 ms | leave to FILTER (~107k vs 111k) |
| net elements in a small window | 24 | 1.3 ms | 1.3 ms | 1.5 ms | 1.5 ms | inject (34 vs 72k) |
| net elements in a larger window | 2118 | 714 ms | 78 ms | 1278 ms | 96 ms | inject, and pin the 3155 candidates before `?ne a era:LinearElement` (72k) |
| net elements within 1 km of a named OP (join) | 4 | 484 ms | 1.2 ms | 830 ms | 1.3 ms | probe the OP side by estimate (1 vs 72k); the element side is not evaluated |
| signals within 200 m of OPs in a window (join) | 80 | 591 ms | 33 ms | 570 ms | 45 ms | leave the window to the FILTER (~3155 vs 2880), probe the OPs by estimate, filtered by the window |

The first run with the cost model gathers the counts its patterns need: 27 ms on the TripleTrail
store for the first query (with the histogram), 365 ms on Oxigraph's store, which counts
`geo:asWKT` and `geo:hasGeometry` triples; later queries reuse them.

### Sweep-line join

The same command prints a second table: joins of two large sides, each timed with the cost
model and the index nested loop (`Engine::set_sweep_join(false)`, the join as it was before)
and with the sweep-line join (median of 5, after a first run; `h = v = 40, k = 20`, 4 cores
shared with other builds, so the parallel speed-up is understated). *Rewrite* is the spatial
rewrite's whole time (evaluating the sides included), *pairing* the share spent finding the
pairs once the sides are known: the part the join algorithm changes.

| Join | Rows | Pairs | Store | Nested loop: query / rewrite / pairing | Sweep: query / rewrite / pairing | Pairing |
|---|---:|---:|---|---:|---:|---:|
| signals (36,000) within 20 m of net elements (72,000) | 92,720 | 73,520 | TripleTrail | 2484 / 1042 / 416 ms | 2114 / 721 / 197 ms | 2.1x |
| | | | Oxigraph | 3481 / 1585 / 476 ms | 3582 / 1261 / 209 ms | 2.3x |
| net relations whose elements intersect (sides joined: 111k x 111k geometries) | 76,720 | 289,880 | TripleTrail | 7717 / 1747 / 1663 ms | 7560 / 845 / 773 ms | 2.2x |
| | | | Oxigraph | 7695 / 1605 / 1511 ms | 7681 / 803 / 706 ms | 2.1x |
| signals (36,000) within 50 m of operational points (2,880) | 2,528 | 12,176 | TripleTrail | 324 / 68 / 54 ms | not swept | |

- The first join evaluates both sides (comparable estimates), then sweeps 36,000 points
  against 72,000 lines. The third is small x large for the sweep: the operational points are
  estimated 12 times fewer, so only they are evaluated, and the other side would be all
  111k indexed geometries, more than 32 times as many: the cost model keeps the nested loop.
- The second join's sides share variables (through the net relation), so neither can be
  evaluated on its own: every indexed geometry is paired with every other. The fixed rules
  probe the R-tree 111k times and give up past 2 million candidates; the sweep has no such
  limit, only the 5 million result pair memory guard.
- What remains is evaluation: side evaluation for the first join, and for both, the
  evaluator joining the `VALUES` block with the triples and checking the FILTER on every row
  (about 20 µs a row for the second).
- Before this change, the first join took 531 s on the TripleTrail store (9.2 s with
  `h = v = 20`): its pairs were not pinned, and Oxigraph's optimizer started from the
  cartesian product of `?s a era:Signal` and `?ne a era:LinearElement` (2.6 billion rows).
  The pin decision now compares the block with that product. The fixed rules still do this.

The pairing speed-up stayed near 2 on 4 cores because the exact tests built their arguments
as RDF terms and looked parsed geometries up in a shared cache, which the threads contended
for; they now run on the parsed geometries (see "Exact tests").

## Next

The index is patched by writes and saved with the store's checkpoint. Still open:


- Joins on term ids on Oxigraph's store, inside `GRAPH` and with a union default graph; sides
  that are not plain basic graph patterns (`OPTIONAL`, `FILTER` on a side).
- Stream the result table instead of materialising it, for joins beyond 1 GB of ids.
- Sweep join: an interval tree on the active sets (each opening box is tested against the
  whole active set of the other side, fine while boxes are short along the sweep axis), and an
  external sort when the boxes do not fit in memory.
