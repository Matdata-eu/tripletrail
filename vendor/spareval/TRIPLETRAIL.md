# spareval, patched for TripleTrail

This is [spareval](https://crates.io/crates/spareval) 0.2.9, Oxigraph's SPARQL evaluator
(MIT OR Apache-2.0, by Tpt; used here under the MIT license, see `LICENSE-MIT`), used through
`[patch.crates-io]` in the workspace, `python/Cargo.toml` and `fuzz/Cargo.toml`. Every change
is marked with a `tripletrail:` comment. Besides those, `Cargo.toml` asks for `thiserror` 2 (the
version the rest of the workspace uses).

1. **Literals keep their datatype and lexical form** (issue #379, `src/dataset.rs`,
   `src/expression.rs`, `src/eval.rs`, `src/lib.rs`). spareval turned every literal into a
   typed value: `"5"^^xsd:long` became the integer 5, `"01"^^xsd:integer` the integer 1, and
   `xsd:dateTimeStamp` a `dateTime`. A literal computed from one (`STRDT`, a literal in the
   query, a custom function's result such as `xsd:long(...)`) was written back as
   `"5"^^xsd:integer`, and `STR`, `LANG` and `DATATYPE` of a stored literal gave the base type
   and the canonical form.
   - `ExpressionTerm::from_literal_as_written` keeps such a literal as an `OtherTypedLiteral`
     with its own datatype and lexical form. `STRDT`, literals in expressions and custom
     function results use it.
   - `STR`, `LANG` and `DATATYPE` of a variable read the stored literal that way, through the
     new context method `build_externalize_written_term`. Other expressions read variables by
     their value, as before, so sums, filters and sorts on stored `xsd:int` cost what they did.
   - `ExpressionTerm::value` gives the typed value where a value is needed: comparisons,
     arithmetic, the effective boolean value, casts and every function argument except those
     of `STR`, `LANG` and `DATATYPE` (`build_value_evaluator`, which parses a literal constant
     once).
   Fixes W3C dawg-str-1 and dawg-str-2.
2. **`MINUS` inside `GRAPH ?g` does not share the graph variable** (issue #427,
   `src/eval.rs`): sparopt's `Minus::graph_variable` (a TripleTrail change too) names it, and
   `are_compatible_and_not_disjointed_except` requires both rows to agree on it without
   counting it as shared. Before, `GRAPH ?g { ?a :p :o MINUS { ?b :p :o OPTIONAL { ?a :q ?z } } }`
   removed every row, because each pair of rows had the bound `?g` in common.
3. **`GROUP_CONCAT` gives a simple literal** (`src/eval.rs`), also when every value has the
   same language tag, as the W3C tests agg-groupconcat-04 and 06 and Jena have it. Upstream
   kept the common tag.
4. **`=` and `!=` on dates and times follow their order** (`src/expression.rs`,
   `temporal_equals`). A value with a timezone and one without that are less than 14 hours
   apart compare as indeterminate, so `=` and `!=` are errors (W3C open-world date-2).
   Upstream's `==` gave `false`, so `!=` was true.
5. **A `+` or `*` path keeps its ends per start term** (`src/eval.rs`, `PathMemo`). A path
   joined to the rows of another pattern is evaluated once per row: `?s rdf:type/rdfs:subClassOf+
   ?o` walked the class hierarchy once per `rdf:type` triple (95,522 times for 104 classes on
   BSBM 1M). A path with a closure now keeps the ends it found per start (or, bound the other
   way, per end) term and graph for the rest of that evaluation, up to 2^20 terms in all, so it
   walks once per class. An evaluation that gives an error, or more ends than may still be
   kept, streams as before and is not kept. Where starts rarely repeat (a path in `EXISTS` per
   subject), keeping them only costs: after 1,024 lookups the memo is given up on unless a
   quarter of them found their start kept.
6. **Cross joins stop when the query is cancelled** (issue #467, `src/eval.rs`,
   `CartesianProductJoinIterator`, `HashJoinIterator`). The cancellation token was only
   checked when a quad is read, and a cross join reads none while it combines its sides, so a
   timeout did not stop `?a ?b ?c . ?d ?e ?f . ?g ?h ?i`. The cross join now makes the rows
   of a probe tuple one at a time (in upstream's order) instead of buffering all of them,
   checks the token every 4,096 rows, and the hash join checks it per probe tuple. Once
   cancelled, both give the error once and then end, as consumers that collect errors (a
   `COUNT`, a join's build side) keep pulling.

7. **Custom functions are shared, and read their query's cancellation from the call** (#186,
   `src/lib.rs`, `src/eval.rs`, `src/expression.rs`). The function registry of an evaluator
   is behind an `Arc`, so cloning an evaluator, and each execution, no longer copies a
   thousand functions. A custom function runs with its query's cancellation token set for
   the call: [`custom_function_call_cancelled`] says whether it was cancelled, so functions
   registered once for every query (TripleTrail's geo functions) still stop with a timeout.

8. **An expression left without a value by a cancellation fails the query** (#235,
   `src/eval.rs`, `Filter` and `Extend`). A custom function whose own work was cancelled (a
   user-defined function whose body hit the timeout) can only return no value. When a `BIND`
   or `FILTER` expression has no value and the query's token is cancelled, the query now
   ends with the cancellation error instead of leaving the variable unbound or dropping the
   row.

9. **Bounded property paths** (#234, `src/eval.rs`, `PropertyPath::Bounded`, `bounded_walk`):
   the ends of the walks of `min` to `max` steps, each end once. The walk keeps every node of
   each level up to `min` steps (a node may be an end at several lengths), then goes on
   breadth first with a visited set up to `max`; `{0,m}` matches zero steps as `p*` does.

10. **A default graph that merges several graphs holds each triple once** (#778, #799,
   `src/eval.rs`, `internal_quads_for_pattern`): the union of all graphs and `FROM` with
   several graphs read each graph, so a triple stored in two of them was matched twice, and
   counts, joins and paths over it were too high on Oxigraph's store (`--store oxigraph`).
   A triple is now kept from one graph only, checked with a lookup and no set of the triples
   seen: for the union, the graph a lookup of the triple reports first (no lookup when there
   are no named graphs); for `FROM`, the first listed graph that holds it. The TripleTrail store
   merges the graphs itself (#376) and reads them as one default graph, so it does not take
   this branch.
11. **A zero-length path matches a term that is in no triple** (#814, `src/eval.rs`,
   `zero_length_or`): `<x> :p* ?o` gave no row when `<x>` is no subject or object of the
   graph; it now gives `?o = <x>`, as SPARQL's zero-length path, Jena and the TripleTrail store
   (which rewrites such patterns itself, `crates/tripletrail/src/zero_length.rs`) do, and
   `<x> :p* <x>` and `<x> :p? <x>` hold. Only for a constant of the pattern
   (`PathEvaluator::constant_end`): a value bound by the rest of the query is matched only
   when it is a node, as W3C `values_and_path` asks. Paths of one step or more from such a
   term still match nothing. Inside `GRAPH ?g` it is unchanged: the graphs enumerated are those that
   hold the term.

To move to a newer spareval: copy the new release over `src/`, apply the `tripletrail:` changes
again, and run `cargo test -p tripletrail --test derived_datatypes`.
