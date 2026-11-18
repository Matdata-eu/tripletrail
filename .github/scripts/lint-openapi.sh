#!/usr/bin/env bash
# Lints the OpenAPI definitions the REST API (#143) and OGC API Features (#138) make, with Spectral's OpenAPI rules
# (.github/spectral.yaml): for the user guide's sample network, and for data with embedded
# objects (blank nodes with a class), references and language strings. Fails on any warning.
#
#   .github/scripts/lint-openapi.sh path/to/tripletrail
set -euo pipefail
bin="${1:?usage: lint-openapi.sh TRIPLETRAIL_BINARY}"
here="$(cd "$(dirname "$0")/../.." && pwd)"
work="$(mktemp -d)"
port=$((20000 + RANDOM % 20000))
cleanup() { [ -n "${pid:-}" ] && kill "$pid" 2>/dev/null; rm -rf "$work"; }
trap cleanup EXIT
# A step that fails before any lint ran (the server not starting, a load refused) says which
# one, with the server's log, instead of ending without a word.
failed() {
  echo "lint-openapi.sh: line $1 failed: $2" >&2
  [ -f "$work/serve.log" ] && { echo "--- tripletrail serve log:" >&2; tail -n 50 "$work/serve.log" >&2; }
}
trap 'failed "$LINENO" "$BASH_COMMAND"' ERR

cat > "$work/embedded.ttl" <<'TTL'
@prefix ex: <http://example.com/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
ex:o1 a ex:Operator ; rdfs:label "Infrabel" ; ex:seat ex:t1 .
ex:t1 a ex:Town ; rdfs:label "Brussels"@en, "Bruxelles"@fr ; ex:population 1200000 .
ex:s1 a ex:Station ; rdfs:label "Station 1"@en ; ex:tracks 3 ; ex:open true ;
  ex:hasOperator ex:o1 ; ex:address [ a ex:Address ; ex:city "Gent" ; ex:near ex:t1 ] ;
  geo:hasGeometry [ geo:asWKT "POINT(4 50)"^^geo:wktLiteral ] .
TTL

"$bin" serve --bind "127.0.0.1:$port" --dataset network --dataset embedded > "$work/serve.log" 2>&1 &
pid=$!
for _ in $(seq 100); do
  curl -sf "http://127.0.0.1:$port/\$/ready" > /dev/null && break
  sleep 0.2
done
curl -sf "http://127.0.0.1:$port/\$/ready" > /dev/null
load() {
  curl -sf -X POST -H 'Content-Type: text/turtle' --data-binary "@$2" \
    "http://127.0.0.1:$port/$1/data" > /dev/null
}
load network "$here/docs/guide/data/network.ttl"
load embedded "$work/embedded.ttl"

status=0
lint() {
  npx --yes @stoplight/spectral-cli@6.17.0 lint --ruleset "$here/.github/spectral.yaml" \
    --fail-severity=warn "$1" || status=1
}
for ds in network embedded; do
  for ext in json yaml; do
    file="$work/$ds-openapi.$ext"
    curl -sf "http://127.0.0.1:$port/$ds/api/openapi.$ext" -o "$file"
    echo "== /$ds/api/openapi.$ext"
    lint "$file"
  done
  # The OGC API Features definition (#138), OpenAPI 3.0.
  file="$work/$ds-ogc.json"
  curl -sf -H 'Accept: application/json' "http://127.0.0.1:$port/$ds/ogc/api" -o "$file"
  echo "== /$ds/ogc/api"
  lint "$file"
done
exit $status
