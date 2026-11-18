# spargebra, patched for TripleTrail

This is [spargebra](https://crates.io/crates/spargebra) 0.4.8, Oxigraph's SPARQL parser
(MIT OR Apache-2.0, by Tpt; used here under the MIT license, see `LICENSE-MIT`), used through
`[patch.crates-io]` in the workspace, `python/Cargo.toml` and `fuzz/Cargo.toml`, so that
sparopt and spareval (vendored too) use it as well. Every change is marked with a `tripletrail:`
comment. Besides those, `Cargo.toml` asks for `thiserror` 2 (the version the rest of the
workspace uses).

1. **`SELECT *` keeps the order of the pattern** (`src/parser.rs`, `src/algebra.rs`): the
   variables were sorted by name, so `SELECT * { ?s ?p ?o }` gave the columns `o, p, s`. They
   now come in the order they appear in the pattern, a `BIND` variable where the `BIND` is
   written, as Jena gives them (W3C csv01 to csv04).
2. **`TRUE` and `False` are booleans** (`src/parser.rs`, `BooleanLiteral`): keywords are
   case-insensitive, the boolean literals too (W3C case-insensitive-booleans).
3. **Longest token before `<`** (`src/parser.rs`, `RelationalExpression_inner`, `IRIREF_token`):
   `?x<?a&&?b>?y` holds the IRI `<?a&&?b>`, so it is rejected and no longer read as two
   comparisons (W3C syn-bad-26).
4. **A SELECT expression may use an earlier one** (`src/parser.rs`, `build_select`):
   `(COUNT(?v) AS ?n) (?n + 1 AS ?m)` (W3C sparql12 select-variable-reuse).
5. **CONSTRUCT, ASK and DESCRIBE may have GROUP BY** (`src/parser.rs`,
   `SelectionVariables::Implicit`): their implicit `*` gave the "SELECT * is not authorized
   with GROUP BY" error (W3C values-construct-group-by).
6. **Invalid queries are rejected** (`src/parser.rs`): nested aggregates such as
   `COUNT(COUNT(*))` (`new_aggregation`, W3C nested-aggregate-functions), and a literal or a
   triple term as the subject of a triple term in an expression (`ExprTripleTermSubject`, W3C
   tripleterm-subject-03 and -06).

7. **Constant arguments of custom aggregates** (`src/parser.rs`, `Aggregate`,
   `with_aggregate_arguments`): with a function and the aggregates that take arguments
   set, `ex:percentile(?x, 0.95)` parses as
   `ex:percentile(<function>(?x, 0.95))`, for TripleTrail's `tt:percentile` and `tt:topK`.
8. **A custom aggregate with `DISTINCT`** (`src/parser.rs`, `iriOrFunction`):
   `ex:agg(DISTINCT ?x)` was read as the IRI `ex:agg` followed by a syntax error.

9. **Bounded property paths** (#234; `src/algebra.rs`, `PropertyPathExpression::Bounded`;
   `src/parser.rs`, `PathElt`): `p{n}`, `p{n,m}`, `p{n,}` and `p{,m}`, as Jena reads them (from
   the SPARQL 1.1 drafts). `{0,1}`, `{0,}`, `{1,}` and `{1}` become `p?`, `p*`, `p+` and `p`;
   a maximum below the minimum is a syntax error. The suffix is one rule with the `?`, `*` and
   `+` operators, so a PEG choice between them does not backtrack.

10. **COPY, MOVE and ADD can be told apart** (#782; `src/parser.rs`, `copy_graph`;
    `src/lib.rs`, `COPY_VARIABLES`): the `INSERT … WHERE { ?s ?p ?o }` they become uses the
    variables `copy-s`, `copy-p` and `copy-o`, which no query can name, so TripleTrail can give
    these operations the stored default graph on a store whose default graph is the union, as
    Fuseki does, without changing an update a user wrote the same way.

To move to a newer spargebra: copy the new release over `src/`, apply the `tripletrail:` changes
again, and run `cargo test -p tripletrail --test select_star --test sparql_syntax`.
