# sparopt, patched for TripleTrail

This is [sparopt](https://crates.io/crates/sparopt) 0.3.9, Oxigraph's SPARQL optimizer
(MIT OR Apache-2.0, by Tpt; used here under the MIT license, see `LICENSE-MIT`), used through `[patch.crates-io]` in the workspace and in
`python/Cargo.toml`. Every change is marked with an `tripletrail:` comment in `src/optimizer.rs`
(and `src/type_inference.rs`).

They are about the right side of a `LATERAL` join (a for-loop join), which is evaluated once
per row of the left side, with that row's variables bound:

1. **Planned with the left side's variables.** The optimizer turns a join into a `LATERAL`
   join when the right operand allows it (after a `BIND`, an `OPTIONAL`, a `UNION`...), but
   that operand had already been planned on its own, without those variables: for every left
   row, its joins started from a full scan (all tracks, all positioning systems...). It is
   now planned again knowing them, as an explicit `LATERAL` is.
2. **Parts linked only through bound variables** (two triple patterns on `?l`, bound by the
   left side) were planned as a cartesian product, a hash join that cannot be a `LATERAL`
   right side, so the unplanned version was kept. They are now chained as `LATERAL` joins.
3. **`OPTIONAL` in a right side** is looked up per row, rather than a hash join that builds
   its right side again for every left row.
4. **A triple pattern whose object is a bound variable** (`?aps :coord ?g`, `?g` bound) is
   estimated as more selective than one with a constant object (`?aps :refersTo :gps01`),
   which is often shared by a whole class, so the lookup starts from the bound value.

Others are about where a plan starts and what was not planned (issue #61, SHACL rules):

5. **Parts linked by a variable a `BIND` or an aggregate binds** (`{ SELECT ?t (SAMPLE(?a)
   AS ?s) ... } ?s :on ?ne`) share no variable bound in every row, so they were joined as a
   cartesian product. They are now planned with that variable bound and joined as `LATERAL`
   joins after the part that binds it (`join_components`).
6. **`VALUES` counts as a tenth of its rows**: they are known, while the 1,000 guessed for a
   triple pattern with a constant object is often a whole class. The focus nodes of a SHACL
   rule then drive the plan.
7. **The first operand of a join** is the one with the smallest first step when it is
   planned as nested loops (`entry_size`), not the smallest product of guesses.
8. **`EXISTS` and `NOT EXISTS` patterns** were left as written (hash joins, built again for
   every row); they are now planned with the row's variables bound (`plan_exists`).

Others use the data (issue #153):

9. **Estimates from the store** (`estimate`): a store can install an `Estimator` for the
   thread that plans a query (`estimate::with_estimator`). Triple patterns with a constant
   predicate are then sized from the data (the tripletrail store answers from its block index,
   and samples how many quads one value of a bound position matches), not from fixed guesses.
10. **With estimates, the start counts its first lookup**: the plan starts from the operand
   whose first step times its cheapest second step is smallest, so a one-row `VALUES`
   (`?ps` = `ex:lps02`) whose lookup gives half of the coordinates is no longer the start.

And one about large `VALUES` blocks (issue #110):

11. **A `VALUES` block of more than 64 rows is not the right side of a for-loop join**: that
   join reads every row of the block for each row of the left side. A semi-naive rule pass
   joins the triples inferred in the last pass (a million on the rail KG) as a `VALUES`
   block with each batch of 4,096 focus nodes; it is now a hash join.

And one about parts that share no variable:

12. **One-row parts drive the rest** (issue #211): parts sharing no variable with the others
   are joined in a cartesian product, the smallest first. When the first ones are one row
   at most by the estimates (`ex:p1 ex:num1 ?o1`, BSBM explore Q5), the others are joined
   as `LATERAL` right sides instead of by a hash join on no key: each is still evaluated
   once, but with the row's variables bound, so a `FILTER` comparing them with the other
   part (`?s1 < ?o1 + 120`) is pushed into its chain, after the triple pattern binding
   `?s1`, instead of running on every row at the end. Q5 through the evaluator (2,785
   products): 2.0 → 1.55 ms.
14. **A part that binds a link variable only in some rows drives the part that always binds
   it** (issue #374): a variable a `BIND` or an aggregate makes (`BIND(IRI(...) AS ?c)`) is no
   hash key, so `join_components` joins its part with the part looking it up
   (`?c a ex:C`) as a `LATERAL` lookup. Both used to count as "driven", and when the
   looked-up part was estimated smaller, the plan started from it, found no `LATERAL` plan for
   the `BIND` side, and joined the two by a hash join on no key: every row with every row.
   Now only the side that always binds the variable is driven. Rail-KG-shaped query, 80,000 rows:
   75.6 s → 0.24 s.

And one about what `GRAPH` means (in `src/algebra.rs`):

13. **`GRAPH` as the SPARQL algebra has it** (issue #364): sparopt pushes the graph name into
   every quad pattern inside `GRAPH`. That stays when every row of the inside matches a quad
   of the active graph, the inside does not use the graph variable and has no sub-select
   (`GRAPH ?g { ?s ?p ?o ... }`). Otherwise `GRAPH <g> { ... }` is joined with the check that
   `<g>` is a graph of the dataset, and `GRAPH ?g { ... }` is the right side of a `LATERAL`
   join over the dataset's named graphs, bound to a fresh variable: the inside is evaluated
   once per graph (a sub-select's `LIMIT` and `COUNT` apply per graph, the variable goes
   through its projection), `?g` is not in scope inside, and a row binding it inside is kept
   only when it is that graph's name. Fixes 12 W3C tests (graph-variable-scope,
   graph-optional, graph-nested-2/3, agg-empty-group-count-graph, the `VALUES` in `GRAPH`
   ones, subquery15/16).

15. **A `MINUS` whose sides share no variable is dropped** (in `src/algebra.rs`): it removes
   no row. Inside `GRAPH ?g`, both sides bound the graph variable once it was pushed into
   their quad patterns, so they shared it and `MINUS` removed rows (W3C test graph-minus).
16. **`MINUS` inside `GRAPH ?g` names the graph variable** (in `src/algebra.rs` and
   `src/optimizer.rs`): the new field `GraphPattern::Minus::graph_variable` holds it. spareval
   (vendored too) requires a left and a right row to agree on it, but does not count it as a
   variable they share, so a right row whose other variables are unbound removes nothing
   (issue #427).
17. **The greedy join order keeps the size of the join it grows** (in `src/optimizer.rs`,
   `reorder_joins`): it estimated the whole join again for every candidate at every step, and a
   chain of LATERAL joins infers its types again at each level, so a join of n parts took
   about n⁴ steps to plan: 900 triple patterns took minutes (issue #483). The size is now
   carried along with the same formulas, so the plans do not change.
18. **A variable made by `BIND` or an aggregate drives lookups** (in `src/optimizer.rs`,
   `bound_by_expressions`, `driving_types`; issues #484, #486, #494): it is unbound only where
   its expression fails, so it is no hash key, and a join or `OPTIONAL` on it was a hash join on
   a weak key (`?to_measure`, three quarters `"0.0"`) or on no key at all. The greedy join order
   and the `OPTIONAL` planning now take such variables as bound when they decide whether the
   other side can be looked up per row, and plan it with them bound. A row where the variable
   is unbound still gets the right answer: the lookup then binds it freely, as unbound is
   compatible with any value. Rail-KG-shaped queries: 11.6 s → 0.06 s (#486), 9.7 s → 0.01 s (#484).
19. **Each condition of an `OPTIONAL` that reads only its own side goes into it** (in
   `src/optimizer.rs`, `push_filters`; issue #485): the conditions were one conjunction, kept in
   the left join's expression as soon as one of them read the left side, so the optional side
   was built in full (a cross product of its parts) and only then filtered. 3.9 s → 1.6 s on
   the rail-KG-shaped query (Fuseki 2.2 s).

20. **Chains of `LATERAL` joins are walked from their start** (in `src/optimizer.rs`,
   `push_filters`, `plan_exists` and `lateral_spine`, #509). Both recursed into the left side
   of each `LATERAL` and inferred its types again at every level, holding one set of types per
   level: a chain of 8,000 triple patterns (`?x0 :p ?x1 . ?x1 :p ?x2 ...`) took 25 s and 2.7 GB
   to plan, past a 5 s timeout. They now take the chain's start and right sides in order,
   keeping the types of the part done so far; each filter still goes where the recursion put
   it. The chain now ends with the timeout at 5.1 s and 0.5 GB.

21. **An equality with a computed value is a join key** (in `src/optimizer.rs`, `push_filters`,
   `equality_key` and `computed_key`, #531). In `?kept :parallel ?oid . ?this :object ?object .
   FILTER(?oid = STRAFTER(STR(?object), STR(src:)))` the two parts share no variable: they
   were joined on no key, every row with every row, before the filter. When one side binds
   `?oid` in every row, the other never does, and the value reads only the other side's
   variables, the value is now computed on that side (`BIND`, rows without a value dropped)
   and the join is on `?oid`; in a chain of `LATERAL` joins it is computed on the part done,
   and the next right side looks it up. With `=` the value must be a simple literal or an
   IRI (`STR`, `IRI`, `STRAFTER`/`STRBEFORE`/`SUBSTR`/`UCASE`/`LCASE` of one, `CONCAT` of
   them, hashes, `ENCODE_FOR_URI`), for which `=` is `sameTerm`; `sameTerm` takes any value.
   The rail KG `area_locations` rule on rail-KG-shaped data (133,000 focus nodes): 131.8 s → 29.4 s.

On the RINF rules, `tripletrail infer` on a 1.86 GB rail knowledge graph now reaches its fixpoint
in 6.6 min; it ran out of memory before.

On the post-processing UPDATEs of a rail knowledge graph (12.5M triples, issue #57), the WHERE clauses of
the 13 that timed out after 300 s now take 0.3 to 37 s.

22. **Bounded property paths are estimated as their unbounded forms** (#234, in
    `src/optimizer.rs`, `estimate_path_size`): a thousand times a step with an end bound or a
    minimum of at least 1, else a billion, as for `+` and `*` paths. `Cargo.toml` takes spargebra from
    `../spargebra`, so the variant is there when sparopt's tests are built alone.

The tests are in `src/optimizer.rs` (`tripletrail_tests`); run them with
`cargo test --manifest-path vendor/sparopt/Cargo.toml --features sep-0006 --lib`.

To move to a newer sparopt: copy the new release over `src/`, apply the `tripletrail:` changes
again, and check the tests.
