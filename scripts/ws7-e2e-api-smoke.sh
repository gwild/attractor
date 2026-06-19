#!/usr/bin/env bash
# ws7-e2e-api-smoke.sh — WS-7 / #2346 API-level e2e acceptance (assemble → Relations + Get).
#
# Runs the assembly + dispatch_with_store integration test and prints a receipt
# block for fleet logs. Requires store-linux Docker leg on hosts without Rust 1.96.
#
# Usage:
#   ./scripts/ws7-e2e-api-smoke.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOST="$(hostname -s)"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
COMMIT="$(git -C "$REPO_ROOT" rev-parse --short HEAD)"
ARCH="$(uname -m)"

echo "attractor ws7-e2e-api-smoke host=$HOST arch=$ARCH ts=$TS commit=$COMMIT"

cd "$REPO_ROOT"
if [ ! -x "$REPO_ROOT/docker/run.sh" ]; then
  echo "ERROR: missing $REPO_ROOT/docker/run.sh" >&2
  exit 1
fi
"$REPO_ROOT/docker/run.sh" --store test \
  -p attractor-assembly \
  ws7_e2e_api_relations_and_get_after_assembly \
  -- --nocapture

echo "RECEIPT host=$HOST arch=$ARCH ts=$TS commit=$COMMIT test=ws7_e2e_api status=PASS relations=ok get=ok receipts=multiset"
