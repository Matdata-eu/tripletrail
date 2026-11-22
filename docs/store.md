# The TripleTrail store

`tripletrail-store` is the first part of the own storage layer from the architecture (decision D2 in
[architecture.md](architecture.md)). The engine, server and Python binding run on it by default,
since it was measured on real RINF data (below). Oxigraph's store is still
there: `Engine::new_oxigraph()` or `TRIPLETRAIL_STORE=oxigraph`, `tripletrail serve --store oxigraph` and
`Engine(store="oxigraph")` in Python.

## Design

- **Term ids.** Every term gets a 64-bit id. The low 4 bits are the kind (IRI, blank node,
  string, language string, other typed literal, geometry literal, integer, boolean, RDF 1.2
  triple term). The other
  60 bits are a dense dictionary ordinal, or the value itself for canonical `xsd:integer`
  (±2^58) and `xsd:boolean` literals. Dense ordinals let derived structures (spatial index,
  routing graphs) index plain arrays by id.
- **Exact terms.** Literals keep their lexical form. `"+7"^^xsd:integer` and `"1.5e0"^^xsd:double`
  are stored as written. Oxigraph stores them as `7` and `1.5`, so a triple pattern
  `{ ?s ex:x 7 }` matches `"+7"` there but not here. RDF says these are different terms, so here
  the pattern doesn't match. Comparisons by value (`FILTER(?x = 7)`) match in both.
- **Dictionary.** All keys sit back to back in one byte arena, with an end-offset array and a
  hash table of ordinals. That costs about 12 bytes per term on top of the key itself.
- **Quad indexes.** There are eight permutations (SPOG, POSG, OSPG, GSPO, GPOS, GOSP, PSOG,
  GPSO), so every triple pattern is one range scan. PSOG and GPSO hold the same quads as POSG
  and GPOS with the subject before the object: the joins on term ids read `?s <p> ?o` from them
  when they want it sorted by subject (a merge join on `?s`, the first triple of a subject
  star), so neither side of a subject star is sorted first (#168). Plain scans and the SPARQL
  evaluator read POSG / GPOS as before. Stores saved with six permutations build the other two
  from SPOG when they open, and save them at the next checkpoint. Each one is an immutable segment of sorted quads cut into
  blocks of 16 (32 in stores saved by earlier versions; they still open, and are rewritten with
  16 at the next compaction). A block keeps its first quad as is; each later quad stores only how it differs
  from the one before, as varints (the first column that changes, the gap in that column, then
  the remaining columns). Writes go to a sorted delta of added and removed quads, kept in a
  persistent ordered set (`imbl::OrdSet`): a snapshot (an open iterator) keeps its version for
  a reference count, and a write next to it copies only the path it changes. Once the delta
  holds 65,536 quads it is folded into new segments.
- **Counts.** Each segment keeps counts of its quads, built with it: how many there are, how
  many distinct values each key prefix has, and how many quads and distinct values each term
  kind has in the first column. It also keeps the same counts per value of the first column
  when that column has at most 4,096 values (the graph in GSPO / GPOS / GOSP, the predicate in
  POSG). `Snapshot::count_triples` answers `COUNT(*)`, `COUNT(DISTINCT ?s)` and the like over
  `?s ?p ?o`, also with `FILTER isLiteral / isIRI / isBlank` on one variable, from these counts.
  The engine replaces such a `GROUP` with a one-row `VALUES` block (#166). On BSBM 1M, the six
  Sparqloscope `number-of-*` queries drop from 77-218 ms to under 2 ms. The counts are saved
  in the checkpoint as `stats.bin`. Checkpoints written before they existed count on first
  use. A permutation with a delta not yet folded into its segment doesn't use them.
- **Fanouts for the planner.** Per predicate, the counts of POSG and PSOG also keep the sum of
  the squares of the quads per object and per subject. Divided by the predicate's quads, that
  is how many quads one subject or object has on average, weighted by how often values come
  up, as when a join binds it from the data: what the join-order estimator used to sample
  (256 quads and a range count for each) in every query (#210). It is read from the segment
  even while a delta exists, and sampled only for predicates the segment doesn't hold or when
  there are more than 4,096 predicates. On BSBM 1M, sampling took about two thirds of the
  server's CPU on the explore mix: the mix runs at 87k instead of 34k queries per hour, and
  query 5 gets a better join order (24 ms → 2.3 ms). The Sparqloscope means don't change
  (their queries are larger, so planning is a small share of them). `stats.bin` is format 2
  (`OXGSSTA2`); format 1 files are counted again on first use.
- **Counts on term ids.** Counts the stored counts can't answer are computed on term ids
  without building SPARQL solutions (#167): a basic graph pattern is joined in the store
  (merge, hash or index nested-loop joins, `Snapshot::count_ids`), and joins, `OPTIONAL`,
  `MINUS`, `UNION`, `FILTER (NOT) EXISTS`, transitive paths and filters over them run on tables of ids (`crates/tripletrail/src/idtree.rs`). A small left side passes its values to the right
  side, which then looks them up instead of scanning (`Snapshot::join_ids_from`). Shapes whose
  result on ids isn't certain (a shared variable that may be unbound, `=` between different
  literals, a filtered `OPTIONAL`) are evaluated as usual. Filters on ids: `BOUND`, `!`,
  `&&`, `||` (with SPARQL's error rules), `sameTerm`, `=` and `!=` between variables, IRIs and
  literals (values of different literals are compared by the evaluator), `isIRI` /
  `isBlank` / `isLiteral` / `isNumeric`, `LANG(?v) = "x"`, `langMatches(LANG(?v), "x")` and
  `DATATYPE(?v) = <iri>`; the last three decode each distinct literal once. `?v IN (…)` and
  `?v NOT IN (…)` over IRIs and literals look each row up in a hash set of the items' ids, and
  compare a literal by value only with the literal items of another kind (`1` and `1.0`):
  1,000 IRIs over 133k rows 933 → 16 ms, against 11 ms for the same list as `VALUES`. BSBM 1M:
  `filter-many-results` (`?s != ?o`) 26 → 5.5 ms, `filter-language-en` 7.8 → 2.0 ms. `GROUP BY` with counts is grouped on
  ids too and becomes a `VALUES` block of the keys and counts (at most 2^20 groups, IRI or
  literal keys). `MIN`, `MAX` and `SAMPLE` of a variable are taken per group on ids too, also
  without `GROUP BY`: IRIs and simple strings compared by their dictionary keys, integers and
  booleans by value; other values (blank nodes, language strings, other typed literals, mixed
  literal kinds) leave the group to the evaluator. Under `ORDER BY` of counts with `LIMIT k`,
  only the first k groups are kept, before their other aggregates are computed. BSBM 1M:
  `group-by-complex-aggregate` 54 → 16 ms, `group-by-implicit-string-min` 11 → 4 ms.
  Inline integers, decimals, dates and date-times (#125) are read as values from their ids
  (`crates/tripletrail/src/idvalue.rs`, #170): `<`, `<=`, `>`, `>=` and `=` between them and
  numeric or date constants, `+ - *` and unary minus, `ABS`, `CEIL`, `FLOOR`, `ROUND`, `YEAR`,
  `MONTH`, `DAY`, `HOURS` and `MINUTES` in filters, and `SUM` of such expressions per group
  (an integer, or a decimal written as the evaluator writes it). `SUM` and `AVG` also take
  inline `xsd:double` values (#62), mixed with integers and decimals: the sum is then a
  double. The doubles are summed exactly and rounded once at the end, so the sum doesn't
  depend on the order of the rows; the integers and decimals of the group are summed exactly
  apart, and their total is rounded to a double before it is added, which can cost a last
  digit against the exact sum of all of them. A group with a double that is not inline (more
  than 16 digits, or an exponent form such as `1.5E2`) is summed by the evaluator in row
  order, so the same query can round differently on other data. The evaluator, and Fuseki, round after each addition in row order, so the last
  digit can differ: ten times `0.1` sums to `1` here and to `0.9999999999999999` there. An
  `AVG` of integers and decimals only (a decimal division) stays with the evaluator.
  Comparisons and `+ - *` take inline doubles too (#11), with integers and decimals turned
  into doubles as the evaluator turns them, and `-0` equal to `0`. Dictionary literals
  (`1.5E2`), a double result that is not finite, overflow, decimal products with more than 18
  fraction digits (they are exact on 256 bits otherwise), dates
  against date-times or with and without a time zone, and unbound values give the expression
  back to the evaluator. On 133 k doubles: `FILTER(?a > 4000)` 60 → 15 ms,
  `FILTER(?a * 1.21 > 5000 && ?a < 9000)` 89 → 27 ms. On 300 k rows: `SUM(YEAR(?d))` 83 → 14 ms,
  `SUM(?int)` 24 → 8 ms, `FILTER(?decimal > 5000.5)` 670 → 10 ms. On 133 k orders of 10 k
  customers, `SUM` of a double per customer 170 → 50 ms, `AVG` 160 → 53 ms.
  Eager grouping before a filter is skipped when there are more than half as many distinct
  values as rows (it took 600 ms instead of 30 on 300 k distinct prices).
  IRIs and simple strings are compared by their string ranks (#171,
  `Snapshot::string_ranks`) under `MIN` and `MAX`, falling back to their dictionary keys
  for terms added since the ranks were built. `STRSTARTS(?v, "p")` and `REGEX(?v, "^p")`
  (no flags, no special character in `p`) are filters on ids: a simple string passes when
  its rank is in the range `Snapshot::string_prefix` finds by binary search; language
  strings and strings without a rank are read from the dictionary; other values are an
  error. On 300 k labels: `REGEX(?o, "^C")` count 52 → 6.5 ms, `MIN` of a string predicate
  16 → 6.4 ms. The first query to need the ranks builds them (one sort of the strings).
- **Rows on term ids.** The same tables also answer parts of queries that return rows
  (`crates/tripletrail/src/rows.rs`): a part with a `MINUS` or a `FILTER` the tables handle is
  evaluated on ids when it gives at most two thirds as many rows as its triple patterns match.
  Its rows are registered as a result table and read back by the evaluator through virtual
  triples, with an `OPTIONAL` for cells that may be unbound. Plain joins and `OPTIONAL` are
  left to the evaluator, because reading rows back costs about as much as its own join. Parts
  under a `LIMIT` without an order are left to the evaluator too, because it can stop early.
  BSBM 1M, the Sparqloscope MINUS / EXISTS / UNION queries returning their rows instead of a
  count: geometric mean 70 → 39 ms (`minus-join-small-large` 55 → 2 ms,
  `exists-join-3-star-2` 84 → 17 ms).
  Transitive paths are in the tables too (#11): `?s <p>+ ?o`, `?s ^<p>+ ?o`, and `<p>*`
  with one end constant, are the pairs of a search over the predicate's edges on ids, each
  end once per start, as SPARQL's paths give them. The search starts from a constant end;
  else, joined with other patterns, from the values they give (the path comes after them in
  the join), backwards when they bind the object; else from every node with an edge. A few
  starts are searched by looking up their edges level by level, many by reading all the
  predicate's edges once. A part with a path joined with patterns outside it (a `VALUES`
  block, say) is left to the evaluator, which also starts from those values. On a tree of
  200,000 nodes (1.48 million pairs): `GROUP BY ?o` with `COUNT` over `?s <p>+ ?o`
  1.05 → 0.19 s, the path joined with a class and counted 1.61 → 0.05 s, and
  `?s ex:id "rare" . ?s <p>+ ?o` stays at about 1 ms. A path with the same variable at both
  ends, `*` between two free variables, or other path expressions are left to the evaluator.
  `ORDER BY ?a DESC(?b) ... LIMIT n` (with `OFFSET`, `DISTINCT` or `REDUCED`) over a pattern
  the tables cover is ordered on ids too: IRIs and simple strings by their string ranks
  (#171), integers and booleans by value, inline doubles, decimals, dates and date-times by
  the order key of their ids (#64), unbound values first, and only the first
  `OFFSET + LIMIT` rows are read back (the evaluator orders those few again). A column with
  blank nodes, other literals or two kinds of literals (an integer and a double, a date and
  a date-time, date-times with and without a time zone) is left to the evaluator, as are
  order expressions other than variables. On 300 k labels, `ORDER BY ?label LIMIT 10`
  1.1 s → 11 ms, `SELECT DISTINCT ?t ... ORDER BY ?t LIMIT 10` 1.0 s → 12 ms. The top 50 of
  133 k double amounts 54 → 17 ms, and with a three-pattern join 319 → 34 ms.
- **Whole queries on term ids.** Under `--executor ids` (#11, `crates/tripletrail/src/direct.rs`)
  a `SELECT` without a dataset of its own whose pattern the tables cover (as above) is answered
  from its table alone: the projection, `DISTINCT` and `REDUCED` on the ids of the projected
  columns, `ORDER BY` of variables as above, then `OFFSET` and `LIMIT`. The answer is an
  `IdSolutions`: each distinct term once (decoded in chunks on the query threads, each chunk
  under one lock of the dictionary) and the rows as numbers of those terms. It rides on the
  solution iterator as its attachment (`QuerySolutionIter::with_attachment`, a TripleTrail
  change to the vendored spareval), so the server writes JSON, CSV and TSV from it
  (`crates/tripletrail-server/src/id_results.rs`) and the result cache keeps it whole; any
  other reader iterates the solutions as usual. `BIND` and `SELECT (expr AS ?v)` are a
  column of the table too (`idtree::Eval::extend`): a copy of a column, a constant, or a
  number the expression kernels above compute, as the literal the evaluator writes (decimals
  in Jena's form, doubles as `oxsdatatypes` writes them): a number the store inlines is made
  straight into its id, and any other value is given its id once; an error leaves the cell unbound. A `FILTER` on such a value is evaluated on the
  expression itself, under the `BIND` (`BIND(?a * ?q AS ?g) FILTER(?g > 400000)` on 133k
  orders 222 → 49 ms; a select expression over a join 197 → 59 ms). A computed double that
  doesn't fit an inline id gets one made up for the query, which doesn't order, so an
  `ORDER BY` of a computed value is left to the evaluator. `VALUES` is a table of the ids of
  its terms (`UNDEF` an unbound cell), and a sub-select's projection the columns it keeps, so
  the groups a count pass reads back from a table of ids are answered from that table. A
  sub-select's `DISTINCT`, `REDUCED`, order of variables and slice (under an order; a slice
  without one keeps the rows the evaluator chooses) are applied to its table as at the top
  of a query (`direct::sub_select`). A term the store doesn't hold (`2.5e0` when it holds
  `2.5`) gets an id made up for the query, which joins only itself; a filter that would have
  to compare it by value (`=`, `IN`, `isLiteral`, `DATATYPE`) leaves the query to the
  evaluator. Before any work, the plan checks that each `BIND` expression is one the kernels
  compute, so a query with `STR` or a division doesn't run its patterns on ids first. An
  order on an expression the kernels compute (`ORDER BY DESC(?q * 3)`) orders a column of its
  values, computed as a `BIND` would be (133k orders, `LIMIT 10`: 117 → 15 ms). A `LIMIT`
  without an order, other order expressions, columns the ids don't order, an expression the kernels
  don't compute (`STR`, a division), and every other operator (`GROUP BY` beyond the count
  shortcuts, property paths outside the tables) leave the query to the evaluator, which then
  runs it as before.
- **Geometries.** Geometry literals (`geo:wktLiteral`, `geoJSONLiteral`, `gmlLiteral`,
  `kmlLiteral`, H3 cell lists) get their own term kind. They are parsed the first time
  something asks for them (`Snapshot::geometry(id)`, the spatial index) and then kept by term
  id, so a load doesn't pay for geometries no query looks at, such as schematic drawings.
- **SPARQL.** `Snapshot` implements `spareval::QueryableDataset`. Queries run on it through the
  same evaluator, custom functions and SERVICE handlers as on Oxigraph's store:
  `evaluator.parse_query(q)?.on_queryable_dataset(store.snapshot())`. Terms a query makes up
  (constants missing from the data, computed strings) get ids private to that query.
- **Transactions.** There is one writer at a time, and readers work on snapshots. A
  transaction sees its own writes, and its writes become visible together on commit.
- **Change sets.** `Store::transaction_with_changes` and `Store::update` return the quads a
  transaction added and removed. The engine uses them to keep routing networks that don't read
  the changed predicates and to patch the spatial index instead of rebuilding it.
- **SPARQL Update.** `Store::update` runs a whole request as one transaction, with `WHERE`
  patterns evaluated by the caller's evaluator on the data as the earlier operations left it.
  Graphs exist as long as they hold quads: `CREATE GRAPH` records nothing, and `CLEAR`/`DROP` of
  an empty graph fails unless `SILENT`. `LOAD` is not supported (there is no HTTP client) and
  fails unless `SILENT`.
- **On disk.** `Store::open(dir)` keeps the data in a directory as a numbered checkpoint
  (dictionary plus the eight segments) and a write-ahead log in N-Quads.
  - Every commit is in the log and synced before it returns. The sync happens after the
    writer lock is let go (group commit, #709): commits that queued meanwhile append to the
    log too, and one sync covers them all. A commit is visible to readers once applied, up
    to one sync before it returns. After a failed sync the store refuses writes until it is
    reopened.
  - Bulk loads write a new checkpoint instead, except a load of at most 100,000 quads
    into a store that already holds data, which is logged like a transaction: posting a few
    triples to a large dataset does not rewrite it.
  - A checkpoint replaces the previous one by renaming a pointer file.
  - On open, a transaction cut short by a crash is dropped.
  - The log is folded into a new checkpoint once it reaches 256 MB
    (`Store::set_checkpoint_log_size`). A commit is complete once it is in the log: if that
    checkpoint fails (a full disk, say), the commit still succeeds, the log is kept, and the
    next commit tries again.
  - Locks are taken without poisoning: a panic inside a transaction (caught by the server)
    leaves its writes out and the store usable.
  - A write looks up its terms in the dictionary and checks its quads in the segments in
    parallel (`Transaction::insert_all`, which `INSERT DATA` and `INSERT … WHERE` use). On a
    store on disk this runs on a pool of more threads than cores (`TRIPLETRAIL_LOOKUP_THREADS`,
    by default 4 per core, 32 to 128): right after a store opens, each lookup can wait on a
    read of the disk, and the reads then wait side by side rather than one after another
    (#437).
  - When a store opens, a thread reads the dictionary's saved hash index into the page cache
    from front to back, so the first writes find most of their terms without a random read
    of the disk each (#437). This is skipped for an index larger than a quarter of the
    machine's memory (or of `--memory-limit`), and with `TRIPLETRAIL_PREFETCH=0`. The pages are
    shared and file-backed, so the kernel can drop them under memory pressure.
  - A checkpoint larger than 1 GiB is mapped without being read, and its files are checked
    against their sums in the background (#436). The check pauses while the store is being
    written, rests after each read as long as the read took, and tells the kernel to drop the
    pages it read, so that it does not push the store's own pages out of memory (#437).
- **Saved and read-only datasets.** `Store::save_to(dir)` writes a store to a new directory
  (a checkpoint and an empty log, written to a temporary directory first and renamed into
  place). `Store::open_read_only(dir)` memory-maps the checkpoint files instead of reading
  them: the dictionary, the segments and the geometries are used where they lie in the page
  cache, a log left by a read-write run is replayed in memory, and nothing on disk changes.
  Every write answers `StoreError::ReadOnly`. `Store::mapped_size()` says how many bytes are
  mapped. A read-write open maps the same files (#218), so the data of a checkpoint stays in
  the page cache, which the kernel can drop, instead of on the heap. Headers are checked against the file sizes
  before anything is allocated. Dictionary files are format 3 (`OXGSDIC3`): the keys are
  padded to 8 bytes so the end offsets can be mapped in place (from format 2 on), and common
  numbers and dates have inline ids instead of keys (#125); formats 1 and 2 still open. A
  piece that may hold WKT literals in binary form (`--render-wkt`, #370) is format 4
  (`OXGSDIC4`), otherwise as format 3, so that a binary from before it refuses the store
  rather than misread those keys (#428).

## Benchmark

In [tripletrail-bench](https://github.com/Matdata-eu/tripletrail-bench), `cargo run --release -p tripletrail-bench --bin store -- 45 45 30` generates the synthetic
ERA-shaped network and loads it into both stores, each in its own process. It then runs the
same queries through the same evaluator, with geo functions registered, and keeps the best of
three runs. Both stores return the same number of rows for every query.

Run on 4 cores, 2026-11-04. The data is 132,300 net elements and 148,410 net relations
(1,419,660 triples).

| | Oxigraph (memory) | TripleTrail store | speed-up |
|---|---:|---:|---:|
| bulk load (N-Triples) | 9,776 ms | 1,965 ms | 5.0x |
| memory after load (RSS) | 546 MB | 303 MB | 1.8x less |
| count all triples | 211 ms | 111 ms | 1.9x |
| ERA micro edge query (network build input, 280,620 rows) | 3,365 ms | 2,558 ms | 1.3x |
| element → geometry → WKT join | 611 ms | 210 ms | 2.9x |
| numeric FILTER on lengths | 64 ms | 30 ms | 2.1x |
| relations with navigability None | 45 ms | 10 ms | 4.3x |
| GROUP BY navigability | 58 ms | 15 ms | 3.9x |
| `geof:sfIntersects` over every WKT (no index) | 311 ms | 144 ms | 2.2x |
| 2,000 single-element lookup queries | 909 ms | 304 ms | 3.0x |

On the 203k-triple default grid (`20 20 20`), load is 2.3x faster and memory 1.7x lower, and
queries run 1.4–4.2x faster.

The ERA edge query gains least. Most of its time goes to `CONCAT`/`STR` building vertex names
row by row, which costs the same on both stores. Changing the profile to emit term ids, so the
network builder needs no strings, is part of switching the engine over.

### Cheaper lookups

A lookup with subject and predicate bound finds its block by binary search, then decodes the
block from its first quad up to the match. That decoding was most of a lookup. Three changes:
blocks of 16 quads instead of 32 (half the decoding per lookup), a decoder unrolled by the
column that changes, and scans that take one reference count instead of four and skip the
delta merge when there is no delta.

The 6-pattern star (`?r a era:NetRelation ; era:elementA ?a ; …`, 262,680 rows) on the 60x60
grid (2.5 M triples), in memory, best of five:

| | before | after |
|---|---:|---:|
| 1,313,400 subject+predicate lookups, by term id | 0.50 s | 0.22 s |
| star query through SPARQL | 1.11 s | 0.82 s |
| count all triples | 0.21 s | 0.16 s |
| quad indexes in memory | 132 MB | 155 MB |

On the `30 30 30` store benchmark (637k triples), the ERA edge query drops from 1,014 ms to
871 ms, the element → geometry → WKT join from 95 ms to 73 ms and counting all triples from
50 ms to 42 ms; memory after load goes from 120 MB to 125 MB.

## Real RINF data

In [tripletrail-bench](https://github.com/Matdata-eu/tripletrail-bench), `cargo run --release -p tripletrail-bench --bin rinf -- --backend tripletrail|oxigraph <files>` runs the
real-data benchmark (loading, routing on three ERA profiles, spatial queries) on either store,
both in memory, each in its own process. Every query line ends with a digest of its solutions
that does not depend on row order, so the two outputs can be compared line by line.

The data is the public `rinf-plus` repository of graph.data.era.europa.eu, exported on
2026-11-04 as N-Triples per named graph and predicate (61 graphs, 41,666,423 lines, 6.9 GB). Three
Lithuanian literals longer than 16 MB were dropped, because Oxigraph's parser refuses lines that
long. Run on 4 cores and 15 GB of RAM. Pairs are 100 (one country) or 200 (all of RINF) random
places. Route times are p50 over the pairs with a path, on a cached network.

| All of RINF (41.1 M triples) | Oxigraph (memory) | TripleTrail store | |
|---|---:|---:|---:|
| bulk load | 227.0 s | 121.6 s | 1.9x faster |
| memory after load (RSS, freed memory returned) | 12,762 MB | 3,619 MB | 3.5x less |
| peak memory during load | 12,988 MB | 11,580 MB | |
| count all triples | 6,911 ms | 3,158 ms | 2.2x |
| network build, micro topology (first routing query) | 802 ms | 362 ms | 2.2x |
| network build, micro with reversing | 897 ms | 554 ms | 1.6x |
| network build, sections of line (60 k vertices) | 1,198 ms | 431 ms | 2.8x |
| `pgr_dijkstra` query, micro with reversing | 2.9 ms | 2.2 ms | |
| `pgr_dijkstra` query, sections of line | 2.3 ms | 2.4 ms | |
| scan all 305 k `geo:asWKT` literals | 631 ms | 254 ms | 2.5x |
| parse all of them (`geof:isEmpty`) | 2,192 ms | 1,338 ms | 1.6x |
| first `sfIntersects` FILTER (builds the spatial index) | 2,688 ms | 2,223 ms | 1.2x |
| `SUM(geof:metricLength)` over 148 k line strings | 2,691 ms | 2,171 ms | 1.2x |
| `ST_LineLocatePoint` for 148 k line strings | 1,633 ms | 841 ms | 1.9x |
| `geof:distance` < 2 km FILTER | 166 ms | 65 ms | 2.6x |

The TripleTrail store holds 6.4 M terms (dictionary 2.1 GB) and 1.98 GB of quad indexes, and keeps
229 k parsed geometries.

| | Norway, Oxigraph | Norway, TripleTrail | Lithuania, Oxigraph | Lithuania, TripleTrail |
|---|---:|---:|---:|---:|
| triples | 1.59 M | 1.59 M | 0.42 M | 0.42 M |
| bulk load | 4.8 s | 2.1 s | 2.6 s | 9.7 s |
| memory after load | 664 MB | 245 MB | 714 MB | 1,567 MB |
| network build, micro with reversing | 416 ms | 282 ms | 247 ms | 209 ms |
| `pgr_dijkstra` query, micro with reversing | 2.7 ms | 2.6 ms | 1.2 ms | 0.8 ms |
| `ST_LineLocatePoint`, all line strings | 309 ms | 220 ms | 100 ms | 91 ms |

Lithuania is the TripleTrail store's weak spot. Its graph has 92 `era:digitalSchematicOverview`
literals of 6-16 MB each, schematic drawings as WKT (562 MB of the 670 MB file, 8.4 M line
strings). The TripleTrail store parses every geometry literal when it is loaded, so it keeps both
the text and about 0.9 GB of parsed geometry, and spends about 6 s parsing. Oxigraph's store only
keeps the text. No query of the benchmark touches these literals. (Measured before geometries
were parsed lazily; see below.)

### Same results

Both stores return the same rows for every query, with three differences that are expected:

- **Triple count.** All of RINF has 41,145,174 triples in the TripleTrail store and 41,145,150 in
  Oxigraph's. The 24 extra are `era:loadCapabilitySpeed` values written both as `"60"` and
  `"060"` (`xsd:integer`) on the same subject. They are different RDF terms, and Oxigraph merges
  them because it stores the value (see "Exact terms" above).
- **Equal-cost routes.** On sections of line, `pgr_dijkstra` and `pgr_bdDijkstra` pick another
  path for some pairs (all of RINF and Lithuania). Their costs are the same: the
  `pgr_dijkstraCost` digest and the rows per route match. With several shortest paths, the one
  found depends on the order the edge query returns edges, and that order differs between stores.
- **Float sums.** `SUM(geof:metricLength)` differs in the last digit (18848400.49426554 against
  18848400.494265575 m), because rows arrive in another order.

### Fixed while measuring

- Parsed WKT kept the vectors of the WKT parser, which are sized for coordinates with Z and M:
  a line string held about four times the memory its points need. Parsing now copies the
  coordinates into vectors of the right size. This also shrinks the parse cache used with
  Oxigraph's store. Lithuania on the TripleTrail store went from 3.6 GB to 1.6 GB.
- The TripleTrail store parsed geometries through the shared parse cache, which kept a second copy of
  every literal's text. It now parses without the cache, because it keeps the geometry itself.

### Decision

The TripleTrail store is the default (2026-11-04). On all of RINF it loads 1.9x faster, needs 3.5x less memory
once loaded, builds routing networks 1.6-2.8x faster, runs the spatial queries 1.2-2.6x faster,
and returns the same results. Route searches take the same time, since they run on the network,
not the store. Two things to fix next, neither of which blocks the switch:

- **Parse geometries lazily**, so data with large non-spatial WKT (the Lithuanian schematics)
  does not cost load time and memory up front. Done since: geometries are now parsed on first
  use (see below).
- **Stream the bulk load.** A load read its whole chunk and parsed every quad before
  interning them, so peak memory during a load (11.6 GB for all of RINF in 1 GB chunks) was three
  times what the store keeps afterwards. Done since: N-Triples and N-Quads are read and parsed
  64 MB at a time, with each block's terms looked up in the dictionary by the parsing threads
  (only new terms are added one by one), and the index permutations are built one after
  another in one buffer. Loading the generated 60x60 grid (2.5 M triples, 359 MB of N-Triples)
  with `tripletrail load` now peaks at 350 MB instead of 1,267 MB, and takes 3.7 s instead of 4.0 s.

### Streaming bulk load on all of RINF

All of RINF (61 files, 6.9 GB of N-Triples, 41,145,174 triples), TripleTrail store, 4 cores, one run
each. Every query's digest and row count is unchanged:

| | before, 1 GB chunks (8 loads) | after, 1 GB chunks | after, one load |
|---|---:|---:|---:|
| bulk load | 201.8 s | 143.0 s | 55.9 s |
| peak memory during load (RSS) | 10,979 MB | 5,354 MB | 3,962 MB |
| memory after load (RSS) | 2,600 MB | 2,614 MB | 2,616 MB |

Loading everything at once is 3.6x faster than before and peaks at 1.5x what the store keeps
afterwards, so the RINF benchmark now loads all files in one bulk load instead of 1 GB chunks.
The earlier "before" run on this machine took 122 s, so expect noise between runs.

### Lazy geometry parsing

Geometries are now parsed on first use. Same benchmark, TripleTrail store, before and after; every
query's digest and row count is unchanged:

| | Lithuania before | Lithuania after | Lithuania, Oxigraph | Norway before | Norway after |
|---|---:|---:|---:|---:|---:|
| bulk load | 9.7 s | 2.1 s | 2.6 s | 2.1 s | 2.0 s |
| memory after load (RSS) | 1,561 MB | 621 MB | 714 MB | 245 MB | 204 MB |
| first `sfIntersects` FILTER (builds the index) | 376 ms | 193 ms | 108 ms | 687 ms | 687 ms |

The first spatial query does not get slower: building the index already parsed every
`geo:asWKT` geometry. Lithuania now loads faster than on Oxigraph's store and needs less memory.

At the time of this measurement the TripleTrail store read its segments into memory on open.
Since #218 it memory-maps its checkpoint files, read-write stores included, and
`--memory-limit` loads data larger than memory.

### PSOG and GPSO (#168)

The two permutations sorted by predicate then subject. Measured 2026-11-08 on one cloud
container (4 cores), main and this change built and run back to back, Sparqloscope queries
over HTTP with the result cache off, best of 10 runs, all answers identical:

| | BSBM 1M (953k triples) | RINF 0076 (1.59M triples) |
|---|---:|---:|
| Sparqloscope geometric mean (102 queries) | 6.42 → 6.14 ms | 10.17 → 9.73 ms |
| join queries (10) | 3.27 → 3.09 ms | 7.61 → 5.93 ms |
| `join-3-star-largest-sum-of-join-sizes` | 11.2 → 9.2 ms | 35.6 → 18.7 ms |
| `join-2-largest-result` | 6.7 → 4.6 ms | 29.5 → 14.7 ms |
| EXISTS queries (10) | 8.74 → 7.06 ms | 29.9 → 22.8 ms |
| quad segments on disk | 43.1 → 57.6 MB (+34 %) | |
| store on disk (with dictionary) | 102.6 → 117.2 MB (+14 %) | 189.4 → 214.1 MB (+13 %) |
| bulk load | 1.17-1.24 → 1.22-1.26 s | 1.93-2.18 → 2.09-2.26 s |

The gain grows with the size of the joined predicates, since the sort it saves is n log n.
The OPTIONAL queries don't use the merge join and move within the noise of this container
(±30 % between runs).

## Not done yet

- The spatial index reads parsed geometries from the store, but the network builder still
  reads edges through SPARQL and strings. Building networks from term ids comes next.

