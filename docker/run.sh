#!/usr/bin/env bash
# run.sh — build/test Attractor inside the pinned Docker sandbox (#2288 / WS-5).
#
# Usage:
#   ./docker/run.sh [--store] [cargo args...]
#
# Default (portable leg, Rust 1.92): cargo test --workspace
#   excluding attractor-store unless --store is passed.
# With --store (Linux-container leg, Rust 1.96): cargo test -p attractor-store
#
# Examples:
#   ./docker/run.sh test -p attractor-core
#   ./docker/run.sh --store test -p attractor-store
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

USE_STORE=0
if [[ "${1:-}" == "--store" ]]; then
  USE_STORE=1
  shift
fi

if [[ "$USE_STORE" -eq 1 ]]; then
  IMAGE="attractor-build:1.96"
  DOCKERFILE="$SCRIPT_DIR/Dockerfile.store"
  DEFAULT_ARGS=(test -p attractor-store)
else
  IMAGE="attractor-build:1.92"
  DOCKERFILE="$SCRIPT_DIR/Dockerfile"
  DEFAULT_ARGS=(
    test
    -p attractor-core
    -p attractor-proto
    -p attractor-config
    -p attractor-api
    -p attractor-io
  )
fi

CARGO_ARGS=("$@")
if [[ ${#CARGO_ARGS[@]} -eq 0 ]]; then
  CARGO_ARGS=("${DEFAULT_ARGS[@]}")
fi

if ! docker image ls --format '{{.Repository}}:{{.Tag}}' | grep -qx "$IMAGE"; then
  echo ">> building $IMAGE from $DOCKERFILE ..." >&2
  docker build -f "$DOCKERFILE" -t "$IMAGE" "$SCRIPT_DIR"
fi

TTY_FLAGS=(-i)
if [[ -t 0 ]]; then
  TTY_FLAGS=(-it)
fi

exec docker run --rm "${TTY_FLAGS[@]}" \
  -v "$REPO_ROOT":/work \
  -v attractor-cargo-registry:/home/app/.cargo/registry \
  -v attractor-cargo-git:/home/app/.cargo/git \
  -w /work \
  "$IMAGE" \
  cargo "${CARGO_ARGS[@]}"
