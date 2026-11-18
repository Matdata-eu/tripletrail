#!/bin/sh
# Rebuilds ../codemirror.min.js.gz from the versions pinned in package-lock.json.
# Needs Node.js and network access; the result is committed, so builds of TripleTrail need neither.
set -eu
cd "$(dirname "$0")"
npm ci --no-audit --no-fund
npx esbuild entry.js --bundle --minify --format=iife --global-name=CM --legal-comments=none --outfile=codemirror.min.js
gzip -9n < codemirror.min.js > ../codemirror.min.js.gz
rm codemirror.min.js
# Every bundled package's license, for ../LICENSES.txt.
node licenses.js > ../LICENSES.txt
