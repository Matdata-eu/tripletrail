# sparesults, patched for TripleTrail

This is [sparesults](https://crates.io/crates/sparesults) 0.3.5, Oxigraph's parser and
serializer of SPARQL query results (MIT OR Apache-2.0, by Tpt; used here under the MIT
license, see `LICENSE-MIT`), used through `[patch.crates-io]` in the workspace,
`python/Cargo.toml` and `fuzz/Cargo.toml`. Every change is marked with a `tripletrail:` comment.

It is vendored to move its XML results format from quick-xml 0.37 to 0.41 (#133), for the
same advisories as oxrdfxml (see `vendor/oxrdfxml/TRIPLETRAIL.md`). It parses the answers of
remote endpoints in `SERVICE`.

1. **Text is gathered across events** (`src/xml.rs`, `read_event` and `Text`): quick-xml 0.38
   and later report `&amp;` and character references as events of their own, so the text of
   a `<literal>`, `<uri>` or `<bnode>` is joined from its events before it is read. The
   `trim_text` option is gone; whitespace at the ends of the text is trimmed as it was, but
   not whitespace written as a character reference (`&#32;`).

`Cargo.toml` also asks for `thiserror` 2 (the version the rest of the workspace uses).

To move to a newer sparesults: as for oxrdfxml, and run the SPARQL query suites
(`tripletrail-testsuite --suite sparql10 --suite sparql11-query`), whose expected results are
XML.
