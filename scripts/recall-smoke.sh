#!/usr/bin/env bash
# recall-smoke.sh — #2510 / WS-7 consumer recall harness (arm64-native or Homer Docker).
#
# Runs the assembly Relations + receipts() multiset smoke test and prints a
# receipt block for fleet logs. No Docker required on sd-3; Homer can also run
# this inside the store-linux leg via docker/run.sh.
#
# Usage:
#   ./scripts/recall-smoke.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOST="$(hostname -s)"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
COMMIT="$(git -C "$REPO_ROOT" rev-parse --short HEAD)"
ARCH="$(uname -m)"

echo "attractor recall-smoke host=$HOST arch=$ARCH ts=$TS commit=$COMMIT"

cd "$REPO_ROOT"
cargo test -p attractor-assembly recall_smoke_ws7_receipts_multiset_on_converged_target -- --nocapture

echo "RECEIPT host=$HOST arch=$ARCH ts=$TS commit=$COMMIT test=recall_smoke_ws7 status=PASS receipts=multiset+idempotent relations=ok"
