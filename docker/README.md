# Attractor Docker sandbox (#2288 / WS-5)

Pinned Linux containers for build/test on Homer. Two planes per
`docs/attractor-working-plan.md`:

- **Portable leg (Rust 1.92):** `attractor-core`, `-proto`, `-config`, `-api`, `-io`
- **Linux-container leg (Rust 1.96):** `attractor-store`

No multi-agent shared store write from per-agent containers until the
projector trust boundary lands.

## Quick start

```bash
# Portable leg (default)
./docker/run.sh test -p attractor-core

# Store leg
./docker/run.sh --store test -p attractor-store

# Full #2288 attestation matrix
./docker/attest.sh
```

## Images

| Tag | Dockerfile | Rust | Crates |
| --- | --- | --- | --- |
| `attractor-build:1.92` | `Dockerfile` | 1.92 | portable workspace members |
| `attractor-build:1.96` | `Dockerfile.store` | 1.96 | `attractor-store` |

Named volumes `attractor-cargo-registry` and `attractor-cargo-git` cache deps
between runs.

## Gate

`docker/attest.sh` is the Homer Linux Docker verification leg for #2288.
Windows native attestation stays native (not this container).
