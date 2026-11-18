#!/usr/bin/env bash
# Release notes for version $1 (e.g. 1.2.0), on standard output: the CHANGELOG.md section of
# that version, then the commits since the previous tag grouped by their Conventional
# Commits type. With --check, only checks that the release is consistent:
#   - the workspace version in Cargo.toml is $1;
#   - CHANGELOG.md has a "## $1" (or "## [$1]") section;
#   - the newest entry of crates/tripletrail-server/ui/whats-new.json is for $1.
set -euo pipefail

check=false
if [ "${1:-}" = --check ]; then
  check=true
  shift
fi
version=${1:?usage: release-notes.sh [--check] <version>}
version=${version#v}
root=$(git rev-parse --show-toplevel)
cd "$root"

section() {
  awk -v v="$version" '
    /^## / {
      if (found) exit
      h = $0; sub(/^## \[?/, "", h); sub(/[] ].*$/, "", h)
      if (h == v) { found = 1; next }
    }
    found { print }
  ' CHANGELOG.md
}

if $check; then
  status=0
  cargo_version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
  if [ "$cargo_version" != "$version" ]; then
    echo "Cargo.toml has version $cargo_version, not $version" >&2
    status=1
  fi
  if [ -z "$(section | tr -d '[:space:]')" ]; then
    echo "CHANGELOG.md has no section \"## $version\" (rename \"## [Unreleased]\")" >&2
    status=1
  fi
  news=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[0]["version"])' \
    crates/tripletrail-server/ui/whats-new.json)
  if [ "$news" != "$version" ]; then
    echo "whats-new.json starts with $news, not $version" >&2
    status=1
  fi
  exit $status
fi

section
previous=$(git describe --tags --abbrev=0 "v$version^" 2>/dev/null || true)
range=${previous:+$previous..}HEAD
if git rev-parse -q --verify "refs/tags/v$version" >/dev/null; then
  range=${previous:+$previous..}v$version
fi
echo
echo "## Commits${previous:+ since $previous}"
for group in "feat:Features" "fix:Fixes" "perf:Performance" "other:Other changes"; do
  type=${group%%:*}
  title=${group#*:}
  if [ "$type" = other ]; then
    lines=$(git log --first-parent --no-merges --format='%s (%h)' "$range" | grep -Ev '^(feat|fix|perf)(\(|!|:)' || true)
  else
    lines=$(git log --first-parent --no-merges --format='%s (%h)' "$range" | grep -E "^$type(\\(|!|:)" || true)
  fi
  if [ -n "$lines" ]; then
    echo
    echo "### $title"
    echo
    echo "$lines" | sed 's/^/- /'
  fi
done
