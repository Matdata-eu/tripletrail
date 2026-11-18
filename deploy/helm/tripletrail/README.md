# TripleTrail Helm chart

Runs `tripletrail serve` with its datasets on a persistent volume.

```sh
helm install mg deploy/helm/tripletrail --set adminToken.value="$(openssl rand -hex 32)"
helm test mg
kubectl port-forward svc/mg-tripletrail 7878:7878   # web UI at http://localhost:7878/
```

The chart is also published as `oci://ghcr.io/matdata-eu/charts/tripletrail`. It will be
signed with cosign once the repository is public; the `cosign verify` below is for then:

```sh
helm install mg oci://ghcr.io/matdata-eu/charts/tripletrail --version 0.1.0 \
  --set adminToken.value="$(openssl rand -hex 32)"
cosign verify ghcr.io/matdata-eu/charts/tripletrail:0.1.0 \
  --certificate-identity-regexp '^https://github.com/Matdata-eu/tripletrail/.github/workflows/helm.yml@' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The image is private while the repository is: add a pull secret for ghcr.io
(`imagePullSecrets: [{name: ghcr}]`).

## What it creates

- A Deployment with one replica and the `Recreate` strategy. A dataset directory has one
  writer, so the old pod stops before the new one opens the volume.
- A PersistentVolumeClaim mounted at `/data` (`persistence.*`, or `persistence.existingClaim`).
  The datasets and the backups (`/data/backups`) live there.
- A Service on port 7878. Optionally, an Ingress (`ingress.*`) and a Prometheus Operator
  ServiceMonitor for `/$/metrics` (`metrics.serviceMonitor.*`); a Grafana dashboard for it is
  in `deploy/grafana/tripletrail.json`.
- Probes:
  - startup on `/$/ping`, so a large dataset may take minutes to open;
  - readiness on `/$/ready`;
  - liveness on `/$/ping`.
- A `helm test` pod that runs `tripletrail health` against the Service.
- Pod security: a non-root user, a read-only root filesystem, no capabilities and an
  emptyDir at `/tmp`.
- Volume size: a bulk load past `TRIPLETRAIL_MEMORY_LIMIT` writes its sort runs under the
  dataset's directory (`<dataset>/runs`), so the volume needs room for them during the load,
  besides the datasets and backups.

## Datasets

```yaml
datasets: [kg]                              # created empty under /data on first start
attach: ["network_20261107=/data/network_20261107:ro"]  # directories already on the volume
aliases: ["network=network_20261107"]               # a stable name the admin API can swap
```

Load data over the Graph Store Protocol (`PUT /kg/data`) or by placing a directory written
by `tripletrail load` on the volume and attaching it.

## Access

- `adminToken`, `updateToken`: a value (the chart makes a Secret) or `existingSecret` with
  its `key`.
- `authConfig`: a sparql-guard configuration (API key hashes from `tripletrail hash-key`,
  trusted proxy headers, anonymous access), inline under `config` or from an existing Secret.

```yaml
authConfig:
  enabled: true
  config:
    auth:
      anonymous: true
      apiKeys:
        - { sha256: "<hash>", agent: "urn:ops", groups: ["tripletrail:admin"] }
```

## Limits

`limits.config` is the `--limits` file: concurrency, timeouts, rows, uploads, disk and
memory quotas per dataset, and rates per caller (see docs/server.md). It is mounted from a
ConfigMap. That copy is read-only, so `PUT /$/limits` fails: change the values and run
`helm upgrade` instead. The server reads the file again when Kubernetes updates the mounted
copy.

```yaml
limits:
  enabled: true
  config:
    default: { maxConcurrent: 4, maxQueued: 8 }
    datasets:
      kg: { timeout: 30, maxTimeout: 300, maxDisk: "50GB" }
```

## Other settings

Any `tripletrail serve` option can be set through `env` (for example `TRIPLETRAIL_TIMEOUT: "60"`,
`TRIPLETRAIL_THREADS: "16"`), or through `extraEnv` in Kubernetes form. Give the pod memory for
the datasets it holds: the dictionary and the indexes are kept in memory, and checkpoint
files are memory-mapped.

## Ingress

Uploads and long queries need a proxy that allows them. For ingress-nginx:

```yaml
ingress:
  enabled: true
  className: nginx
  annotations:
    nginx.ingress.kubernetes.io/proxy-body-size: "0"
    nginx.ingress.kubernetes.io/proxy-read-timeout: "600"
  hosts: [{ host: tripletrail.example.org, paths: [{ path: /, pathType: Prefix }] }]
  tls: [{ secretName: tripletrail-tls, hosts: [tripletrail.example.org] }]
```

With `--base-iri` (Linked Data), the proxy must forward `Host` or `X-Forwarded-Host`.

The chart expects TLS to end at the Ingress. Its probes and `helm test` speak plain HTTP to
the pod, so they fail if the server itself is given `--tls-cert`/`--tls-key`
(`TRIPLETRAIL_TLS_CERT`/`TRIPLETRAIL_TLS_KEY` in `extraEnv`). A chart option for TLS in the pod,
with HTTPS probes, is still to come.
