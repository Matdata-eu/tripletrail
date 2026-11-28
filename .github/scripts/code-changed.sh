#!/usr/bin/env bash
# Reads changed file names, one a line, and prints `code=true` when one of them is not
# documentation, else `code=false` (#408). The documentation paths are those the push trigger of
# ci.yml ignores: Markdown at the top level, docs/, bench/, LICENSE and NOTICE.
#
#   git diff --name-only HEAD^1 HEAD | .github/scripts/code-changed.sh >> "$GITHUB_OUTPUT"
set -euo pipefail
code=false
while IFS= read -r file; do
  case "$file" in
    "" | docs/* | bench/* | LICENSE | NOTICE) ;;
    */*) code=true ;;
    *.md) ;;
    *) code=true ;;
  esac
done
echo "code=$code"
