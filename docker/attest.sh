#!/usr/bin/env bash
# attest.sh — #2288 promotion gate: run the Homer Linux Docker attestation matrix.
#
# Portable leg (Rust 1.92): core, proto, config, api, io
# Linux-container leg (Rust 1.96): store
#
# Exits non-zero on first failure. Prints a receipt block for chat/fleet logs.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOST="$(hostname -s)"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
COMMIT="$(git -C "$REPO_ROOT" rev-parse --short HEAD)"

run_leg() {
  local leg="$1"
  shift
  echo "== leg: $leg =="
  "$SCRIPT_DIR/run.sh" "$@"
}

echo "attractor docker attestation host=$HOST ts=$TS commit=$COMMIT"

run_leg "portable-1.92" test \
  -p attractor-core \
  -p attractor-proto \
  -p attractor-config \
  -p attractor-api \
  -p attractor-io

run_leg "store-linux-1.96" --store test -p attractor-store -p attractor-api -p attractor-projector -p attractor-resolver -p attractor-assembly --features attractor-api/store

echo "RECEIPT host=$HOST ts=$TS commit=$COMMIT legs=portable-1.92,store-linux-1.96 status=PASS"
