#!/usr/bin/env bash
# Folders and files the tests left in the temporary folders, after a CI job.
#
# Tests make their temporary folders with tripletrail_testdir::tempdir (crates/tripletrail-testdir),
# named tripletrail-*, and remove them when they are dropped, also when a test fails. A
# tripletrail-* entry still in the job's own temporary folder ($TMPDIR, under $RUNNER_TEMP) was
# left by this job: that is an error, so a test that leaks shows up in the pull request that adds
# it. The machine's /tmp is shared with older runs and other jobs: entries there are a warning,
# and those older than a day are removed so that they cannot pile up on a persistent runner.
set -u

prefix="tripletrail-"
status=0

list() {
  find "$1" -mindepth 1 -maxdepth 1 -name "$prefix*" "${@:2}" -printf '%TY-%Tm-%Td %TH:%TM  %p\n' 2>/dev/null | sort
}

own="${TMPDIR:-}"
if [ -n "$own" ] && [ "$own" != /tmp ] && [ -d "$own" ]; then
  left=$(list "$own")
  if [ -n "$left" ]; then
    n=$(printf '%s\n' "$left" | wc -l)
    echo "::error title=Temporary folders left behind::$n $prefix* entries left in $own by this job's tests (make them with tripletrail_testdir::tempdir and keep it until the server or process stops)"
    printf '%s\n' "$left" | head -n 50 || true
    status=1
  else
    echo "No $prefix* entries left in $own."
  fi
fi

if [ -d /tmp ] && [ "$own" != /tmp ]; then
  left=$(list /tmp -user "$(id -u)")
  if [ -n "$left" ]; then
    n=$(printf '%s\n' "$left" | wc -l)
    echo "::warning title=Temporary folders in /tmp::$n $prefix* entries in /tmp (from earlier runs or other jobs); removing those older than a day"
    printf '%s\n' "$left" | head -n 20 || true
    find /tmp -mindepth 1 -maxdepth 1 -name "$prefix*" -user "$(id -u)" -mmin +1440 -exec rm -rf {} + 2>/dev/null || true
  else
    echo "No $prefix* entries in /tmp."
  fi
fi

exit "$status"
