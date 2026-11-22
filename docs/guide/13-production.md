# 13. Running TripleTrail in production

This chapter puts the pieces together for a server others depend on: data on disk, backups,
new releases of the data without downtime, limits, monitoring, HTTPS and containers. Each
section is a starting point; the [server reference](../server.md) has every option.

## A checklist

- Data on disk: `--location /data` (the Docker image's default).
- Access control on ([chapter 12](12-access-control.md)), and `--bind 0.0.0.0:7878` only then.
- HTTPS, from a reverse proxy or the server itself.
- `--timeout` so that no query holds the server for long, and limits per dataset or caller
  if many people share it.
- Backups on a schedule, kept somewhere else.
- `/$/ready` for the load balancer, `/$/metrics` for Prometheus.

## Backups

Start a server on a dataset on disk, with an admin token:

<!-- test: run -->
```sh
tripletrail load --location data --dataset rail_v1 network.ttl
```

<!-- test: serve -->
```sh
tripletrail serve --location data --dataset rail_v1 --alias rail=rail_v1 --admin-token adm1n
```

A backup runs in the background and writes gzipped N-Quads, which any RDF tool reads, to
`data/backups`:

<!-- test: run -->
```sh
curl -s -X POST -H 'Authorization: Bearer adm1n' 'http://localhost:7878/$/backup/rail_v1'
```

<!-- test: output contains -->
```text
"file":"rail_v1_
.nq.gz"
"taskId":
```

- `kind=store` writes a copy of the store instead, which restores without parsing: faster for
  large datasets.
- `GET /$/backups-list` lists the backups, and `POST /$/backups/{file}/verify` checks one.
- `--backup-keep 7` keeps the newest seven backups of each dataset.
- `POST /$/datasets?name=rail_restored&type=disk&from={file}` restores one as a new dataset,
  and with `--wal-archive` a store backup restores to any moment after it (`until=`).

Run backups from cron or your scheduler with `curl`, and copy `data/backups` off the machine.
The **Tasks & backups** page of the web UI does the same by hand. See
[backups](../server.md#backups).

## New data without downtime

Clients query the stable name `rail`, an alias that points at a version, `rail_v1`. Build the
next version offline, next to the running server, here with one more station:

<!-- test: file extra.ttl -->
```turtle
@prefix ex: <https://example.org/rail#> .
ex:BRG a ex:Station .
```

<!-- test: run -->
```sh
tripletrail load --location data --dataset rail_v2 network.ttl extra.ttl
```

Attach it read-only, then point the alias at it. Requests already running finish on the old
version; new ones see the new one at once:

<!-- test: run -->
```sh
T='Authorization: Bearer adm1n'
curl -s -o /dev/null -X POST -H "$T" 'http://localhost:7878/$/datasets?name=rail_v2&location=data/rail_v2&mode=read-only'
curl -s -o /dev/null -X POST -H "$T" 'http://localhost:7878/$/datasets/rail/swap?to=rail_v2'
curl -s http://localhost:7878/rail/query -H 'Accept: text/csv' \
  --data-urlencode 'query=SELECT (COUNT(?s) AS ?stations) WHERE { ?s a <https://example.org/rail#Station> }'
```

<!-- test: output -->
```text
stations
11
```

To roll back, swap to `rail_v1` again. `release=true` on the swap detaches the old version
once it is idle. A read-only dataset is memory-mapped, so it opens at once and shares memory
with every process that maps it. Pipelines can write such a folder for you with a `publish`
step ([chapter 8](08-pipelines.md)). See
[releasing a new version](../server.md#releasing-a-new-version-without-downtime).

## Disk space

`tripletrail disk-usage` shows what a dataset on disk takes: its files by kind, and the
dictionary (`dict.bin`, the text of every IRI and literal) by kind of term. It reads the
dataset without changing it, so run it on a dataset no server has open, here a fresh one:

<!-- test: run -->
```sh
tripletrail load --location data --dataset sized network.ttl
tripletrail disk-usage --location data --dataset sized
```

<!-- test: output contains -->
```text
sized:
dict.bin
geoms.bin
dictionary keys:
  kind                 keys      bytes      %   datatype  namespace
  iri                    35
  wkt                    22
```

The `datatype` column is what the datatype IRIs repeated in every typed literal take, and
`namespace` what the IRIs' namespaces (up to the last `/` or `#`) take. `--json` gives the
same as JSON. Keys written since the last checkpoint are counted with the dictionary but are
still in `wal.log`; the output says so when there are any.

On spatial data the WKT text is often most of the store, kept once as text in `dict.bin` and
once as numbers in `geoms.bin`. `--render-wkt` keeps a WKT literal only in binary form when
its text can be written back from it exactly, as it usually can for data written by a tool
with a fixed number of decimals:

<!-- test: run -->
```sh
tripletrail load --location data --dataset compact --render-wkt network.ttl
tripletrail disk-usage --location data --dataset compact
```

<!-- test: output contains -->
```text
  wktBinary              22
```

Queries give the same text. A query that outputs many WKT literals is slower, since each is
written back (on 290,000 OpenStreetMap geometries, 1.9 s instead of 1.3 s for all of them);
spatial filters and joins are not. On that OpenStreetMap extract of Norway the store went
from 731 MB to 364 MB. The setting stays with the dataset. See [the store](../store.md).

## Read replicas

For more query capacity, or a copy in another place, a replica follows a leader's dataset and
applies each of its transactions, in order:

```sh
tripletrail serve --location /replica --follow rail=http://leader:7878/rail --follow-token "$ADMIN_TOKEN"
```

A replica answers queries only. `/$/status` shows how far behind it is.

A replica lags its leader by a moment, a few milliseconds even while the leader takes thousands of writes a second. To read your own write from a replica, take the
`TripleTrail-Version` header from the leader's answer to the write and send it with the read. The
replica waits until it has that write, for up to 10 seconds:

```sh
version=$(curl -s -D - -o /dev/null --data-urlencode 'update=INSERT DATA { <x:a> <x:b> <x:c> }' \
  http://leader:7878/rail/update | grep -i '^tripletrail-version:' | cut -d' ' -f2 | tr -d '\r')
curl -H "TripleTrail-Min-Version: $version" --data-urlencode 'query=ASK { <x:a> <x:b> <x:c> }' \
  http://replica:7878/rail/query
```

To lose no answered write when the leader's machine dies, start the leader with
`--sync-replicas 1`: each write then answers only once a replica holds it too (a millisecond or
two more per write), and its `TripleTrail-Replicas` header says how many do. If no replica answers
within 10 seconds (`--sync-timeout`), the write answers anyway and the leader logs a warning;
the writes after it answer at once (with `TripleTrail-Replicas: 0`) instead of each waiting 10
seconds, until a replica has caught up, when writes wait for it again. `GET
/$/replication/rail` with the admin token shows whether writes wait (`sync.waiting`) and where
each replica stands.

When the leader's machine is lost, promote a replica: it stops following, takes writes, and
notes in its history where on the old leader's timeline it took over. Promote the replica that
is furthest along: `offset` and `generation` under `replica` in its `/$/status`. With
`--sync-replicas 1` and two replicas, only one of them may hold the last answered write.
Stop the old leader first, if it still runs, so that only one server takes writes:

```sh
curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" 'http://standby:7878/$/replication/rail/promote'
```

Give the other replicas every server that may become the leader. A replica reads from the
first that answers. When that one is down, or on another timeline, it moves to the server
that took over at the replica's position, and goes on from there without copying the data
again. A replica that was behind (or ahead of) the one promoted copies a new base from it:

```sh
tripletrail serve --location /replica --follow-token "$ADMIN_TOKEN" \
  --follow rail=http://leader:7878/rail,http://standby:7878/rail
```

A promoted replica stays the leader when it restarts, with or without its `--follow` on the
command line, and with no `--dataset`. To make it a replica again, or the old leader a
replica of the new one, start it with a fresh `--location` and a `--follow` to the leader. A load balancer sends writes to whichever server answers 200 on
`/$/role/{name}`. The leader answers `leader`. A replica answers 503 with `follower`, and a
read-only dataset answers 503 with `read-only`:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/$/role/rail_v1'
curl -s 'http://localhost:7878/$/role/rail'
```

<!-- test: output -->
```text
leader
read-only
```

See [read replicas](../server.md#read-replicas).

## Limits

- `--timeout 120` stops queries and updates after 120 seconds (a request may ask for less).
- `--max-rows 100000` caps the rows of one answer.
- `--query-memory 2G` bounds the memory of one query. `queryMemory` in the limits file sets
  a lower limit for one dataset, and a request may ask for less with `query-memory`. A query
  whose joins, sorts and groups together would hold more fails with 503 instead of taking
  the server's memory:

<!-- test: run -->
```sh
q='SELECT (COUNT(*) AS ?n) { { SELECT ?s ?p { ?s ?p ?o } } { SELECT ?p ?x { ?x ?p ?y } } }'
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:7878/rail_v1/query \
  --data-urlencode "query=$q" --data-urlencode 'query-memory=16KB'
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:7878/rail_v1/query \
  --data-urlencode "query=$q"
```

<!-- test: output -->
```text
503
200
```

- `--limits limits.json` sets, per dataset, how many requests run at once, a queue, timeouts
  and disk and memory quotas, and per caller a request rate. See
  [operations](../server.md#operations).
- A query can carry its own hints in `#pragma tt.…` comment lines: `#pragma tt.timeout 5s`
  stops it sooner than the server's timeout. When the planner gets a query wrong,
  `#pragma tt.optimizer off` keeps the written join order. `/explain` shows the hints it
  ran with:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/rail_v1/explain' --data-urlencode 'query=#pragma tt.optimizer off
SELECT * WHERE { ?s ?p ?o } LIMIT 1' | grep -o '"hints":\[[^]]*\]'
```

<!-- test: output -->
```text
"hints":["#pragma tt.optimizer off"]
```

  Queries written for Blazegraph (Wikidata's query service) keep working: their `hint:`
  triples are taken out before the query runs, and `hint:optimizer "None"` keeps the written
  join order too:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/rail_v1/explain' --data-urlencode 'query=PREFIX hint: <http://www.bigdata.com/queryHints#>
SELECT * WHERE { hint:Query hint:optimizer "None" . ?s ?p ?o } LIMIT 1' | grep -o '"hints":\[[^]]*\]'
```

<!-- test: output -->
```text
"hints":["#pragma tt.optimizer off"]
```

- `--executor ids` answers a `SELECT` whose pattern runs on term ids (joins, `OPTIONAL`,
  `MINUS`, filters, `ORDER BY`, `DISTINCT`) straight from the id tables, without the term
  evaluator: a query that returns many rows comes back up to twice as fast, and an `OPTIONAL`
  several times. The answers are the same. It is off by default for now; try it on one query
  with `#pragma tt.executor ids`, and `/explain` says which executor answered:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/rail_v1/explain' --data-urlencode 'query=#pragma tt.executor ids
SELECT ?s ?type WHERE { ?s a ?type }' | grep -o '"executor":"[a-z]*"'
```

<!-- test: output -->
```text
"executor":"ids"
```

  All hints are listed under [query hints](../server.md#query-hints).

## Monitoring

- `/$/ping` answers while the process lives (liveness); `/$/ready` answers `200` once every
  dataset is open and indexed (readiness). The Docker image's health check calls `tripletrail
  health`.
- `/$/metrics` is in the Prometheus format: requests, durations, timeouts, triples, cache
  hits and more:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/$/metrics' | grep '^tripletrail_triples'
```

<!-- test: output unordered -->
```text
tripletrail_triples{dataset="rail_v1"} 158
tripletrail_triples{dataset="rail_v2"} 159
```

- For datasets on disk it also has the write-ahead log, commits and checkpoints with their
  times, runs waiting to be merged, and the check of the files; for read replicas their lag,
  and on a leader the replicas it hears from. `rail_v1` has taken no writes since this server
  started (`rail_v2`, read-only, has no commits to count):

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/$/metrics' | grep '^tripletrail_commits_total'
```

<!-- test: output unordered -->
```text
tripletrail_commits_total{dataset="rail_v1"} 0
```

- `tripletrail_dataset_memory_bytes` is the heap a dataset holds, by part: what a `maxMemory`
  quota counts. An index counts once a query has built it, so a search or a spatial query
  can take a dataset over its quota and stop its writes: give `maxMemory` room for about
  twice the data when the spatial, full-text, vector and range indexes are all in use:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/$/metrics' | grep '^tripletrail_dataset_memory_bytes{dataset="rail_v1"' | cut -d' ' -f1
```

<!-- test: output unordered -->
```text
tripletrail_dataset_memory_bytes{dataset="rail_v1",part="dictionary"}
tripletrail_dataset_memory_bytes{dataset="rail_v1",part="quads"}
tripletrail_dataset_memory_bytes{dataset="rail_v1",part="geometries"}
tripletrail_dataset_memory_bytes{dataset="rail_v1",part="spatialIndex"}
tripletrail_dataset_memory_bytes{dataset="rail_v1",part="textIndex"}
tripletrail_dataset_memory_bytes{dataset="rail_v1",part="vectorIndexes"}
tripletrail_dataset_memory_bytes{dataset="rail_v1",part="rangeIndexes"}
tripletrail_dataset_memory_bytes{dataset="rail_v1",part="resultCache"}
tripletrail_dataset_memory_bytes{dataset="rail_v1",part="routingNetworks"}
tripletrail_dataset_memory_bytes{dataset="rail_v1",part="unionCopies"}
```

  `/$/stats/rail_v1` has the same parts under `memory`.

- `tripletrail_budget_trips_total` counts, per dataset, the queries that ran into a budget:
  stopped by its query memory (`memory`; the 503 above is one), refused for more rows than `max-rows` (`rows`), or
  cut at `max-rows` with `partial=true` (`partial`). Timeouts have their own
  `tripletrail_timeouts_total`. A query asking for at most one row of a larger result is
  refused, and counted:

<!-- test: run -->
```sh
curl -s -G -o /dev/null -w '%{http_code}\n' 'http://localhost:7878/rail_v1/query' \
  --data-urlencode 'query=SELECT * WHERE { ?s ?p ?o }' --data-urlencode 'max-rows=1'
curl -s 'http://localhost:7878/$/metrics' | grep '^tripletrail_budget_trips_total{dataset="rail_v1"'
```

<!-- test: output unordered -->
```text
413
tripletrail_budget_trips_total{dataset="rail_v1",budget="memory"} 1
tripletrail_budget_trips_total{dataset="rail_v1",budget="rows"} 1
tripletrail_budget_trips_total{dataset="rail_v1",budget="partial"} 0
```

- For Grafana, import [`deploy/grafana/tripletrail.json`](../../deploy/grafana/tripletrail.json)
  (Dashboards → New → Import) and pick your Prometheus data source: requests, errors,
  latency, timeouts, limits and memory per dataset, commits, checkpoints and the
  write-ahead log, and replica lag.
- `--log-requests` writes a JSON line per request, and `--slow-query-ms 2000` logs the full
  text of slow queries.
- `--otlp-endpoint http://collector:4318` (or `OTEL_EXPORTER_OTLP_ENDPOINT`) sends an
  OpenTelemetry trace of every request to an OTLP collector (Jaeger, Grafana Tempo, the
  OpenTelemetry Collector): a span per request, with its steps (`sparql.prepare`,
  `sparql.evaluate`, `http.send`) as children. A client that sends a `traceparent` header
  sees TripleTrail's spans inside its own trace. `--otlp-sample 0.1` traces one request in ten.
  See [operations](../server.md#operations).

## HTTPS

Behind a reverse proxy (nginx, Traefik, an Ingress) that ends TLS, nothing changes in
TripleTrail. To serve HTTPS itself, give it the certificate and key; a renewed certificate is
picked up without a restart:

```sh
tripletrail serve --location /data --dataset rail \
  --tls-cert /etc/letsencrypt/live/kg.example.org/fullchain.pem \
  --tls-key /etc/letsencrypt/live/kg.example.org/privkey.pem
```

## Upgrading TripleTrail

A new TripleTrail opens the dataset directories of an older one as they are: install it, restart
the server, and the next checkpoint writes the files in the new format. Each store keeps its
format in a `FORMAT` file:

<!-- test: run -->
```sh
cat data/rail_v1/FORMAT
```

<!-- test: output -->
```text
1
```

Going back is not always possible. An older TripleTrail refuses a store a newer one wrote, before
it reads or changes anything in it, and says so (here a copy marked as format 2):

<!-- test: run fails -->
```sh
mkdir -p old && cp -r data/rail_v1 old/rail_v1 && echo 2 > old/rail_v1/FORMAT
tripletrail load --location old --dataset rail_v1 network.ttl
```

<!-- test: output contains -->
```text
is in format 2, written by a newer TripleTrail
```

So take a [store backup](#backups) before an upgrade, and keep the old binary until the new
one has run for a while. With [read replicas](#read-replicas), upgrade the replicas first and
the leader last. See [dataset directories](../server.md#dataset-directories).

## Containers

Almost every option is also an environment variable (not `--load` and the `--fx-*` options):
`--admin-token` is `TRIPLETRAIL_ADMIN_TOKEN`,
`--dataset` is `TRIPLETRAIL_DATASETS`, and so on. A Docker Compose service:

```yaml
services:
  tripletrail:
    image: ghcr.io/matdata-eu/tripletrail
    ports: ["7878:7878"]
    volumes: ["tripletrail-data:/data"]
    environment:
      TRIPLETRAIL_DATASETS: rail
      TRIPLETRAIL_ADMIN_TOKEN: ${TRIPLETRAIL_ADMIN_TOKEN}
      TRIPLETRAIL_TIMEOUT: "120"
volumes:
  tripletrail-data:
```

On Kubernetes, the Helm chart in [`deploy/helm/tripletrail`](../../deploy/helm/tripletrail/README.md)
sets up the volume, secrets, probes, an Ingress and a ServiceMonitor. See
[Kubernetes](../server.md#kubernetes).

### Ship the command in another image

Another image can carry the `tripletrail` command, to validate or convert data in a worker,
by copying it out of the published image. The binary is `/usr/local/bin/tripletrail`, and
`/usr/share/doc/tripletrail` holds `LICENSE`, `NOTICE` and `THIRD_PARTY_LICENSES.md`, which
go with it:

```dockerfile
FROM debian:bookworm-slim
COPY --from=ghcr.io/matdata-eu/tripletrail:0.1.0 /usr/local/bin/tripletrail /usr/local/bin/tripletrail
COPY --from=ghcr.io/matdata-eu/tripletrail:0.1.0 /usr/share/doc/tripletrail /usr/share/doc/tripletrail
```

Pin a version tag rather than `latest`, so a rebuild gets the same command. The image is
published for linux/amd64 and linux/arm64, and `COPY --from` takes the one for the platform
being built, so a multi-platform build needs no emulation for this step. The binary is built
on Debian 12 and needs glibc 2.36 or newer (Debian 12, Ubuntu 24.04 and later, not Alpine).
Each release also has the binaries as archives on its GitHub release page.

## The enterprise edition

TripleTrail Enterprise is the same server with a few features for larger organisations, sold
with a subscription that includes updates and support. It is a separate build: the Apache-2.0
`tripletrail` you have used in this guide never contains enterprise code. `/$/server` says which
edition a server is:

<!-- test: run -->
```sh
curl -s 'http://localhost:7878/$/server' | grep -o '"edition":"[a-z]*"'
```

<!-- test: open output -->
```text
"edition":"open"
```

The enterprise build answers `"edition":"enterprise"`, and `tripletrail --version` ends in
`(enterprise)`. It takes the licence Matdata sends you, one line of text starting with
`ttlic1.`, from a file or a variable:

```sh
tripletrail serve --license-file /etc/tripletrail/license.txt --location /data
# or, in a container, a mounted secret:
TRIPLETRAIL_LICENSE_FILE=/run/secrets/tripletrail-license tripletrail serve
```

The licence is checked inside the server, with no network access, so it works on closed
networks. To see what a licence covers:

```sh
tripletrail license inspect /etc/tripletrail/license.txt
```

```text
Licence:   2026-001
Customer:  Example Rail AS
Issued:    2026-11-14
Expires:   2027-11-13
Features:  audit, sso
Key:       95322a17
Status:    valid
```

It exits with status 0 while the licence unlocks the enterprise features, so a monitoring
check can run it; `--json` prints the same as `GET /$/license` on the running server.

A licence never stops the server. In its last 30 days the log warns at start and once a day;
after it expires, everything keeps working for 30 more days with warnings; after that only the
enterprise features turn off and the open features keep working. Updates come from Matdata's
private registry with your subscription's token. [TripleTrail Enterprise](../enterprise.md) has
the details.

### Who changed what: the audit log

An enterprise server can keep an audit log: one line for each change and admin call (updates,
uploads, loads, new datasets, access changes) and for each refused attempt at one, saying who
sent it, when, to which dataset, what it did to each graph, and how it ended. Queries are not
logged unless you ask for them with `--audit-reads`, nor is the text of an update (only its
hash).

<!-- test: enterprise serve -->
```sh
tripletrail serve --load network.ttl --update-token s3cret --audit-log audit.jsonl
```

<!-- test: enterprise run -->
```sh
curl -s -o /dev/null -H 'Authorization: Bearer s3cret' http://localhost:7878/ds/update \
  --data-urlencode 'update=INSERT DATA { <https://example.org/rail#S01> <https://example.org/rail#checked> true }'
tail -n 1 audit.jsonl
```

<!-- test: enterprise output contains -->
```text
"event":"update"
"outcome":"done"
"path":"/ds/update"
"graphs":[{"added":1,"graph":null,"removed":0}]
```

`graphs` says how many quads the update added to and removed from each graph (`null` is the
default graph).

Each line carries the hash of the line before it, so a line that is removed, changed or moved
breaks the chain, and `tripletrail audit verify` finds it:

<!-- test: enterprise run -->
```sh
tripletrail audit verify audit.jsonl
```

<!-- test: enterprise output contains -->
```text
The chain is whole.
```

Keep the last hash it prints somewhere else (a ticket, a log server) now and then: then lines
cut from the end show too. The log is written whatever the licence says, so a lapsed licence
never leaves a gap in it. To have the lines in your SIEM as well, add
`--audit-syslog udp://logs.example.org:514` (or `tcp://…`): each line also goes to that
syslog collector as it is written.

To answer "who changed what last week?", `tripletrail audit report` reads the log and sums it up
by agent and dataset: the changes done and refused, the quads added and removed in each
graph, how each agent signed in and from where. `--since`, `--until`, `--agent` and
`--dataset` narrow it, and `--json` gives it as JSON:

<!-- test: enterprise run -->
```sh
tripletrail audit report audit.jsonl
```

<!-- test: enterprise output contains -->
```text
  ds: 1 changes; +1 -0 quads
    default: +1 -0
```

## Publish to a data portal

Open data portals such as [data.europa.eu](https://data.europa.eu/) list datasets from a
[DCAT-AP](https://semiceu.github.io/DCAT-AP/) catalog that they harvest. TripleTrail serves one
at `/.well-known/dcat`, with each dataset, its SPARQL endpoint and its dump. The titles, the
publisher and the licence are for you to say, in a Turtle file. Its relative IRIs are the
server's own: `<#catalog>` is the catalog, and `<rail/void#dataset>` is the dataset `rail`.
Save this as `catalog.ttl`:

<!-- test: file catalog.ttl -->
```turtle
@prefix dct:  <http://purl.org/dc/terms/> .
@prefix dcat: <http://www.w3.org/ns/dcat#> .

<#catalog> dct:title "Belgian rail network" ;
  dct:publisher <https://example.org/infrabel> .
<https://example.org/infrabel> dct:title "Example rail infrastructure manager" .
<rail/void#dataset> dct:title "Rail network" ;
  dct:description "Stations and sections of the network, with their geometry." ;
  dcat:keyword "railway" ;
  dct:license <http://creativecommons.org/licenses/by/4.0/> .
```

and name it with `--catalog`:

<!-- test: serve -->
```sh
tripletrail serve --location data --dataset rail_v1 --alias rail=rail_v1 --catalog catalog.ttl
```

<!-- test: run -->
```sh
curl -s http://localhost:7878/.well-known/dcat -H 'Accept: application/n-triples' \
  -H 'X-Forwarded-Host: data.example.org' -H 'X-Forwarded-Proto: https' \
  | grep -E 'title|endpointURL' | sort
```

<!-- test: output -->
```text
<https://data.example.org/#catalog> <http://purl.org/dc/terms/title> "Belgian rail network" .
<https://data.example.org/rail_v1/sparql> <http://purl.org/dc/terms/title> "SPARQL endpoint of rail_v1" .
<https://data.example.org/rail_v1/sparql> <http://www.w3.org/ns/dcat#endpointURL> <https://data.example.org/rail_v1/sparql> .
<https://data.example.org/rail_v1/void#dataset> <http://purl.org/dc/terms/title> "Rail network" .
<https://data.example.org/rail_v1/void#dump> <http://purl.org/dc/terms/title> "Dump (N-Quads)" .
<https://data.example.org/rail_v1/void#sparql> <http://purl.org/dc/terms/title> "SPARQL endpoint" .
<https://example.org/infrabel> <http://purl.org/dc/terms/title> "Example rail infrastructure manager" .
```

The catalog lists the datasets themselves (`rail_v1`), and what the file says about the
alias `rail` goes to the dataset it names, also after a swap. A dataset with no title or
description in the file gets its name and a short sentence, since DCAT-AP requires both. The
two `X-Forwarded-` headers are what a reverse proxy sends: the IRIs are then the public
ones, here `https://data.example.org/`. Each caller sees only the datasets it may list.

## What you learnt

- Backups are one admin request, run on a schedule; store backups restore fastest.
- An alias and a swap publish a new version of the data with no downtime and an easy rollback.
- `tripletrail disk-usage` shows where a store's bytes go, and `--render-wkt` keeps WKT
  literals once, in binary form.
- Replicas, limits, metrics and HTTPS are options of the same `tripletrail serve`.
- `/.well-known/dcat` is a DCAT-AP catalog for data portals; `--catalog` adds titles,
  publisher and licence.
- The enterprise edition is a separate build that reads an offline licence and never stops
  the server when it expires; its audit log records who changed what, in a chain that shows
  tampering.

That is the end of the guide. For everything else, the [reference
documentation](../../README.md#documentation) has every option, and the
[benchmarks](https://github.com/Matdata-eu/tripletrail-bench) show how TripleTrail compares with PostGIS, QLever, Jena and
others.
