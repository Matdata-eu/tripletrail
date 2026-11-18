# Canonicalization and diff

TripleTrail compares two releases of a dataset (a rail knowledge graph, RINF) without custom scripts.

## `tripletrail canon`

```sh
tripletrail canon data.ttl more.nq > canonical.nq     # canonical N-Quads; SHA-256 on stderr
tripletrail canon --digest /data/datasets/rinf         # only sha256:<hex>, for an on-disk dataset
```

The inputs are files (format from the extension) or directories of on-disk datasets
(`<location>/<name>`, `--store` as for `load`), read together as one dataset. The output is
the dataset in canonical N-Quads as RDF Dataset Canonicalization (RDFC-1.0, W3C
Recommendation) defines it: blank nodes relabelled `_:c14n0`, `_:c14n1`, …, one quad per
line, the lines sorted, with literals escaped one way only. Two isomorphic datasets (the same
graph up to blank node labels) give the same bytes, so the SHA-256 of the output is a digest
of the dataset that does not depend on how it was written.

Literals keep their lexical form: `"1.0"^^xsd:decimal` and `"1.00"^^xsd:decimal` differ.

## `tripletrail diff`

```sh
tripletrail diff release-2026-10.ttl release-2026-11.ttl            # JSON summary
tripletrail diff --format rdf-patch old.nq new.nq > changes.rdfp    # D and A lines
tripletrail diff old.nq new.nq --added added.nq --removed removed.nq
```

Both sides are canonicalized and their quads compared: a quad only in the second is added,
only in the first removed. Blank nodes are matched through their canonical labels, so a
blank node that did not change keeps its label; when any blank node changes, the labels of
others may shift and their quads show as removed and added.

The JSON summary gives the digest and size of each side, the totals, and the added and
removed quads per graph (`""` is the default graph), per predicate and per class of the
subject (its `rdf:type` in either release; `""` for subjects without one). `--format
rdf-patch` writes an RDF Patch: a `D` line per removed quad, then an `A` line per added one.
The command exits with status 1 when the datasets differ, as `diff` does.

## Limits

Both commands hold the dataset in memory as canonical N-Quads lines (about the size of
the N-Quads file, besides the quads read).

RDFC-1.0 labels blank nodes by the shape of their neighbourhood. When many blank nodes look
alike (a blank node with eight identical blank node children, or a clique of blank nodes all
linked to each other) the algorithm tries the orders of them, which grows factorially; this
is the "dataset poisoning" the specification warns about. Long chains of alike blank nodes
(an RDF list of one value repeated hundreds of times) make it search the whole chain from
every cell. TripleTrail refuses the dataset after 20,000 steps for one blank node, 5 million
units of work for the whole run (copies of the labelling state count by their size), or a
search 200 levels deep, so a bad dataset fails within seconds instead of exhausting the
server's memory. The W3C test suite passes (its "evil" poison
graphs take under 5,000 steps; the 10-clique is refused) except the SHA-384 test, as only
SHA-256 is offered. Blank nodes inside RDF 1.2 triple terms are outside RDFC-1.0 and are
refused.

## On the server

`POST /$/digest/{name}` computes a dataset's digest in the background (a task in `/$/tasks`,
with `sha256` once finished), and `POST /$/diff/{name}?to={other}` compares two datasets (the
task's `diff` is the JSON summary above), for the admin token or the datasets' admins. To
compare with an older release, restore its backup as a dataset first. See
[server](server.md).

## In Python

`Engine.canonical_digest()`, `Engine.canonicalize()` (bytes) and `tripletrail.diff(a, b)` (a dict
with `added`, `removed`, `sha256` and counts per `graphs`, `predicates` and `classes`).

## In Rust

```rust
let a = tripletrail::canon::canonicalize(quads_a)?;
let b = tripletrail::canon::canonicalize(quads_b)?;
println!("{}", a.digest());
let diff = tripletrail::canon::diff(&a, &b);
let summary = diff.summary(&a, &b);
```
