# 1. Install TripleTrail

TripleTrail is one program, `tripletrail`. It runs the server, and it also has commands to load,
validate and transform data without one. Pick the way to get it that suits you.

## With Docker (no build needed)

If you have [Docker](https://docs.docker.com/get-docker/), the image has everything. Until
the first release is published, build the image once from the source:

```sh
git clone https://github.com/Matdata-eu/tripletrail
cd tripletrail
docker build -t tripletrail .
```

Then start it, keeping its data in a Docker volume:

```sh
docker run -p 7878:7878 -v tripletrail-data:/data tripletrail
```

Once releases are out, `docker run -p 7878:7878 -v tripletrail-data:/data
ghcr.io/matdata-eu/tripletrail` does the same without the build.

To run the other commands of this guide in the container on the files of the current folder,
put `docker run --rm -u "$(id -u):$(id -g)" -v "$PWD:/work" -w /work tripletrail` where the guide
writes `tripletrail`. For example:

```sh
docker run --rm -u "$(id -u):$(id -g)" -v "$PWD:/work" -w /work tripletrail validate --data network.ttl --shapes shapes.ttl
```

## From source

You need [Rust](https://rustup.rs) 1.95 or later. Building takes a few minutes the first
time:

```sh
git clone https://github.com/Matdata-eu/tripletrail
cd tripletrail
cargo install --locked --path crates/tripletrail-server
```

This puts the `tripletrail` command in `~/.cargo/bin`, which `rustup` adds to your `PATH`. Check
that it works:

<!-- test: run -->
```sh
tripletrail --help
```

<!-- test: output contains -->
```text
Usage: tripletrail <COMMAND>
```

## From a release

Releases publish ready-made binaries for Linux, macOS and Windows on the
[releases page](https://github.com/Matdata-eu/tripletrail/releases). Unpack the archive and put
`tripletrail` (`tripletrail.exe` on Windows) somewhere on your `PATH`.

## Python

TripleTrail is also a Python package that runs the engine inside your Python process, with the
same functions and no server. For Python 3.9 and later, build it from a checkout of the
repository (it needs Rust):

```sh
pip install ./python
```

It is not on PyPI yet: v0.1.0 came out before releases published it there. A later release
puts it on PyPI, with ready-made wheels for Linux (x86_64, aarch64), macOS (Intel and Apple
silicon) and Windows (x86_64), and `pip install tripletrail` then needs no Rust.

See [the Python package](../../python/README.md) for what it offers.

## What you have now

| Command | What it does | Guide |
|---|---|---|
| `tripletrail serve` | Runs the server: SPARQL endpoints, web UI, map, APIs | [2](02-first-steps.md) |
| `tripletrail load` | Loads big files into a dataset on disk, fast | [3](03-data.md) |
| `tripletrail lift` | Turns CSV, JSON, XML, spreadsheets and other files into RDF | [7](07-lifting.md) |
| `tripletrail validate`, `infer`, `entail` | SHACL validation, rules and reasoning | [6](06-validation-and-reasoning.md) |
| `tripletrail pipeline run` | Runs a whole ETL described in a `pipeline.yaml` | [8](08-pipelines.md) |

`tripletrail <command> --help` lists every option of a command.

Next: [your first dataset and query](02-first-steps.md).
