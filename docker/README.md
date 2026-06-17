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
| `attractor-run:latest` | `Dockerfile.run` | 1.96 build / slim runtime | `attractord` + `atctl`, store feature |

Named volumes `attractor-cargo-registry` and `attractor-cargo-git` cache deps
between runs.

## Gate

`docker/attest.sh` is the Homer Linux Docker verification leg for #2288.
Windows native attestation stays native (not this container).

## Deploy (Homer, post-gate)

`attractord` (in `attractor-api`) is the daemon: one transport per process —
unix socket by default, `--tcp HOST:PORT` opt-in with an explicit address
(never an implicit `0.0.0.0`). With the `store` feature each serving loop owns
its in-process store, which is why one transport per process is enforced until
a shared store lands.

```bash
docker build -f docker/Dockerfile.run -t attractor-run .
mkdir -p ~/attractor-run ~/attractor-data ~/.config/systemd/user
cp docker/attractor.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now attractor

# Smoke test from the host:
ATTRACTOR_SOCKET_PATH=~/attractor-run/attractor_api.sock atctl ping
```

Trust boundary: the socket dir (`~/attractor-run`) is the access-control
surface — host filesystem permissions decide who can speak matrix verbs. No
network listener exists unless an operator adds `--tcp` to the unit, which
should ride its own reviewed change. Store state persists under
`~/attractor-data` (container path `/var/lib/attractor/store`); `docker restart`
reloads the snapshot when that volume is mounted.
