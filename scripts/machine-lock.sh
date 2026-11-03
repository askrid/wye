#!/usr/bin/env bash
# Runs a command while holding the machine lock, so heavy jobs on this
# machine (a gate's builds and benchmark, an A/B run, a site build with
# screenshots) run one at a time and none skews another's measurements.
#
#   scripts/machine-lock.sh <command> [arguments...]
#
# The lock is one file in the git common directory, so every worktree of the
# repository shares it. A job already holding it (WYE_MACHINE_LOCKED is set)
# runs nested jobs directly. CI runs one job per runner and has no lockf
# (Linux): both run the command without a lock.
set -euo pipefail

if (( $# == 0 )); then
  echo "usage: scripts/machine-lock.sh <command> [arguments...]" >&2
  exit 2
fi
if [[ -n ${WYE_MACHINE_LOCKED:-} || -n ${CI:-} || ! -x /usr/bin/lockf ]]; then
  exec "$@"
fi

lock="$(git rev-parse --path-format=absolute --git-common-dir)/wye-machine.lock"
if ! /usr/bin/lockf -k -s -t 0 "$lock" true; then
  echo "machine-lock: another heavy job holds $lock; waiting" >&2
fi
exec /usr/bin/lockf -k "$lock" env WYE_MACHINE_LOCKED=1 "$@"
