# Vendored pyoxigraph classes

`model.rs`, `io.rs`, `dataset.rs` and `sparql.rs` are pyoxigraph 0.5.13's
(`python/src` of https://github.com/oxigraph/oxigraph), licensed MIT OR Apache-2.0
(Copyright (c) Oxigraph contributors), used here under the MIT license (`LICENSE-MIT`). `errors.rs` holds the error mappings of its
`store.rs`. Changes: the classes are registered in the `tripletrail` module, the
`crate::` paths point here, pyo3 is the version of this crate, RDF 1.2 is always on (no `rdf-12` cfg: the
tripletrail store has triple terms), and the query preparation
used only by pyoxigraph's `Store` is not used. tripletrail additions, marked `tripletrail:` in
the code: query results keep the query's timeout alive (`KeepAlive`), a cancelled query
raises `TimeoutError`, and `QuerySolution.get(key, default)`.

pyoxigraph's own tests run against these classes in `python/tests/compat`.

To update: copy the files of the new pyoxigraph release over these and re-apply the
changes (the header comment marks each file).
