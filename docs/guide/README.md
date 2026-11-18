# TripleTrail user guide

This guide walks you through TripleTrail one task at a time, from installing it to running it in
production. Every chapter builds on a small sample network of Belgian railway stations and
sections, so you can follow along on your own machine and see the same answers.

You need no SPARQL or RDF experience for the first chapters. When you want every option of
something, each chapter links to the [reference documentation](../../README.md#documentation).

## Chapters

**Getting started**

1. [Install TripleTrail](01-install.md)
2. [Your first dataset and query](02-first-steps.md)
3. [Load, change and keep data](03-data.md)

**Working with the data**

4. [Spatial queries](04-spatial.md)
5. [Routing over a network](05-routing.md)
6. [Check and enrich data: validation and reasoning](06-validation-and-reasoning.md)
7. [Turn files into RDF](07-lifting.md)
8. [Pipelines and tests](08-pipelines.md)
9. [The Workbench](09-workbench.md)

**Sharing the data**

10. [Maps and APIs](10-maps-and-apis.md)
11. [Coming from Apache Jena Fuseki](11-fuseki.md)
12. [Access control](12-access-control.md)
13. [Running TripleTrail in production](13-production.md)

## The sample data

[`data/network.ttl`](data/network.ttl) holds ten stations and twelve sections between them.
The station positions are real; the lengths, speeds and electrification are rounded or made
up. Each station has a name, a code and a point; each section has the stations at its ends,
a length in kilometres, a maximum speed, whether it is electrified, and a line.

```text
                     Antwerp-Central
                    /       |
  Ghent-Sint-Pieters     Mechelen
          \              /     \
           \   Brussels-North -- Leuven
            \        |               \
             \  Brussels-Central      \
              \      |                 \
             Brussels-South        Liège-Guillemins
                     |                  /
                 Ottignies -- Namur ---
```

## Conventions

- Commands are for a Unix shell (Linux, macOS, or WSL and Git Bash on Windows). In Windows
  PowerShell, write `curl.exe` instead of `curl`, and put a command on one line.
- The server runs on your own machine at `http://localhost:7878`, the default.
- Prefixes such as `ex:` and `geo:` are written out in every query, so each one can be
  copied and run on its own.

## For contributors: keeping the guide true

The guide is tested. `cargo test -p tripletrail-server --test guide` runs the examples of every
chapter against the `tripletrail` built from the same commit, and fails when an answer no longer
matches what the guide shows. A code block runs when an HTML comment right before it says how:

| Comment | The block |
|---|---|
| `<!-- test: file PATH -->` | is written to `PATH` in the chapter's folder (and must match `data/PATH` when the sample data has it) |
| `<!-- test: run -->` | is run with bash, with `tripletrail` on the `PATH`; `run fails` expects an error |
| `<!-- test: serve -->` | starts a server in the background, until the next one or the chapter's end |
| `<!-- test: query [DATASET] -->`, `<!-- test: update [DATASET] -->` | is sent to the running server (dataset `ds` by default) |
| `<!-- test: output [contains\|unordered] -->` | is what the block before it gives: a query its CSV results |
| `<!-- test: enterprise DIRECTIVE -->` | any directive above, for an example of an enterprise feature: run only by `cargo test -p tripletrail-server --features enterprise --test guide` |
| `<!-- test: open DIRECTIVE -->` | any directive above, for what only the open build answers (such as its edition): skipped by the enterprise build |

Each chapter starts in an empty folder holding a copy of `data/`. `GUIDE_CHAPTER=04 cargo test
-p tripletrail-server --test guide` runs one chapter. See
[`crates/tripletrail-server/tests/guide.rs`](../../crates/tripletrail-server/tests/guide.rs).

A pull request that changes what a user sees (a command, an option, an endpoint, a default, an
answer) updates the chapter that shows it, or adds one.
