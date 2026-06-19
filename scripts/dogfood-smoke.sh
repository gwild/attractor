#!/usr/bin/env bash
# dogfood-smoke.sh — #2653 dogfood ingest/query acceptance (attractor#38 + milton#2827).
#
# Runs the idempotent ingest + Secretary-equivalent query integration test.
#
# Usage:
#   ./scripts/dogfood-smoke.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOST="$(hostname -s)"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
COMMIT="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
ARCH="$(uname -m)"

echo "attractor dogfood-smoke host=$HOST arch=$ARCH ts=$TS commit=$COMMIT"

cd "$REPO_ROOT"
if [ ! -x "$REPO_ROOT/docker/run.sh" ]; then
  echo "ERROR: missing $REPO_ROOT/docker/run.sh" >&2
  exit 1
fi
"$REPO_ROOT/docker/run.sh" --store test \
  -p attractor-assembly \
  ws3_dogfood_2653_milton_pr_evidence \
  -- --nocapture

echo "RECEIPT host=$HOST arch=$ARCH ts=$TS commit=$COMMIT test=ws3_dogfood_2653 status=PASS idempotent=ok secretary_query=ok relations=ok"
