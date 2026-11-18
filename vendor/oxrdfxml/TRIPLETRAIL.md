# oxrdfxml, patched for TripleTrail

This is [oxrdfxml](https://crates.io/crates/oxrdfxml) 0.2.5, Oxigraph's RDF/XML parser and
serializer (MIT OR Apache-2.0, by Tpt; used here under the MIT license, see `LICENSE-MIT`),
used through `[patch.crates-io]` in the workspace, `python/Cargo.toml` and `fuzz/Cargo.toml`.
Every change is marked with a `tripletrail:` comment.

It is vendored to move it from quick-xml 0.37 to 0.41 (#133): 0.37 has two denial-of-service
advisories, RUSTSEC-2026-0194 (quadratic duplicate-attribute check) and RUSTSEC-2026-0195
(unbounded namespace declarations), and no oxrdfxml release has moved yet.

1. **quick-xml 0.41** (`Cargo.toml`, `src/parser.rs`): namespaces are resolved through
   `NsReader::resolver()`, attribute values through `decoded_and_normalized_value_with`.
2. **Text is gathered across events** (`src/parser.rs`, `parse_event`): quick-xml 0.38 and
   later report `&amp;`, `&#65;` and DOCTYPE entities as events of their own. The text, CDATA
   and entity reference events between two tags are joined before they are read, so
   `a &amp; b` stays one literal.
3. **DOCTYPE entities are capped at 1 MiB** (`MAX_ENTITY_SIZE`): an entity declared from
   others grew with each level, so a few lines of DOCTYPE could take all the memory.

`Cargo.toml` also asks for `thiserror` 2 (the version the rest of the workspace uses).

To move to a newer oxrdfxml: if its release is on quick-xml 0.38 or later, drop this copy and
the patch; otherwise copy it over `src/`, apply the `tripletrail:` changes again, and run
`cargo test -p tripletrail --test xml_parsers` and the W3C RDF/XML suites
(`tripletrail-testsuite --suite rdf11-xml --suite rdf12-xml`).
