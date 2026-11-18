# Fuzz targets

Coverage-guided fuzzing with [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer).
This crate is outside the main workspace because it needs nightly Rust.

| Target | What it checks |
|---|---|
| `geo_functions` | Every registered geometry function, with arguments built from the input (empty, degenerate, NaN, infinite and CRS-tagged cases), never panics in TripleTrail's own code, hangs or blows up in memory. |
| `geo_literals` | WKT, GeoJSON, GML, KML and H3 literals never make the parser panic, and what parses writes WKT that parses again. |
| `store_files` | A store directory with one file (`CURRENT`, a log, `dict.bin` or a `.seg`) replaced or patched by the input opens with an error or with its quads, never a panic or an abort. |
| `graph_grants` | A caller limited to some named graphs (#208) gets, for a query built from the input, the answers of a store holding only those graphs; updates under read and write grants change only what they would change there, and a refused update changes nothing. Row filters, and row rules judged from a random policy (`sg:visibleIf` and `sg:hideProperties` with class, node, subjects-of and objects-of targets or none, and `minCount`, `maxCount`, `hasValue`, `in`, `minInclusive`, `datatype`, `nodeKind` and `class` constraints), answer as a store without what a small evaluator in the test says they hide, also after random inserts and deletes. The same checks run on 400 seeded inputs each in `crates/tripletrail/tests/graph_access_diff.rs` and `crates/tripletrail/tests/row_filter.rs`. |
| `routing` | On graphs built from the input, contraction hierarchies (static and customizable) find the same costs as Dijkstra; saved routing graphs read from the input never panic. |

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run geo_functions -- -max_total_time=60
```

A crash is saved under `fuzz/artifacts/<target>/`. Replay it with
`cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<file>`, and minimise it with
`cargo +nightly fuzz tmin <target> <file>`.

The function table turns any panic into a SPARQL error, so `geo_functions` only fails on panics
raised in `crates/tripletrail-*`. Panics inside third-party crates (`i_overlay` behind `geo`'s boolean
operations, the `wkt` writer) are upstream bugs; guard against their inputs in our code where
the result matters.

CI (`.github/workflows/fuzz.yml`) runs each target for 60 s on every push to
`main`, and for 30 minutes weekly. The corpus is kept in the Actions cache between runs.

The crash-recovery test for the store (kill -9 at random moments) is an ordinary test,
`crates/tripletrail-store/tests/crash.rs`, and runs with `cargo test`.
