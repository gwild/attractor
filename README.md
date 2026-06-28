# Attractor

Content-addressed matrix substrate for the Milton fleet. A field where
information is added and convergence (the "attractor" state) is computed over
it — the Rust core of the matrix project. Built and run in a Linux Docker on
Homer (#2288); every agent has repo access, per-agent Docker access for
traceability, secrets in `.env` in local clones.

## What it guarantees

The matrix is not a bag of records — the store enforces its invariants by
construction, not convention:

1. **Content-addressed identity.** A node is keyed by the `blake3` hash of its
   content. Inserting the same value twice — from any origin, over any
   transport — converges to one node; identity is the hash, not insertion
   order. Each observation is kept in a **provenance multiset** (#2249), so
   convergence never loses *who saw it where*.
2. **Acyclic, referentially-intact DAG.** `Store::link` refuses dangling edges
   and any edge that would introduce a cycle.
3. **Tamper-evident history.** Every first insertion appends a chain entry
   committing to its predecessor's hash; `Store::verify_chain` detects
   rewriting, reordering, truncation, or appended forgeries. Trust boundary:
   the guarantee holds as long as the externally persisted head is honest —
   a consistent rewrite of chain *and* head together is out of scope by design.
4. **Typed promotion boundary.** Raw authored input (markdown) never lands in
   the store. `attractor-projector` (WS-3) derives a typed node set and returns
   a receipt with populated `source_spans` (#2517) — the caller inserts the
   derived nodes, not the prose.
5. **Fail-loud configuration.** No silent defaults anywhere (RULES.md NO
   FALLBACKS): `attractor-config` loads typed sections from `config.yaml` and
   required secrets from `.env`, and errors loudly when something is missing.

## Workspace layout

Two MSRV legs, attested separately (see below):

**Portable contract layer — Rust 1.92 (fleet floor), no async/web/OS deps:**

- **`attractor-core`** — pure schema + content-addressing: the cell/frame data
  model, `blake3` hashing, `serde`. Compiles identically on Linux and
  Windows-native; this is the cross-platform-attested layer.
- **`attractor-proto`** — wire types for the control plane (#2243 proto-split):
  the request/response envelope both server and clients depend on. Pure
  `serde`.
- **`attractor-config`** — shared `config.yaml` + `.env` loader: generic typed
  section load, required-secret access, relative-path resolution against the
  config file's directory. Mechanism here, schema at each consumer.
- **`attractor-api`** — control server + `atctl` client: JSON-lines
  request/response over a Unix-domain socket (local, `serve_unix`) and TCP
  (remote, `serve_tcp`), one line per connection; per-connection errors are
  logged, never fatal to the accept loop. Build with the `store` feature for
  the WS-1 matrix verbs (`Get`/`Insert`/`Link`/`Snapshot`).
- **`attractor-io`** — Rust→Python shared-memory export (audmon `/dev/shm` +
  control-file pattern): portable `ShmControl` contract, `cfg(unix)` mmap
  writer.

**Linux-container leg — Rust 1.96 (store semantics, `petgraph`):**

- **`attractor-store`** — the persistence layer: content-addressed node store,
  acyclic relation graph, tamper-evident provenance chain (guarantees 1–3
  above).
- **`attractor-projector`** — WS-3 markdown projection boundary (guarantee 4).
- **`attractor-wiki`** — issue-wiki local materialization, batch manifest,
  gated publish boundary, and explicit-hash batch driver; it consumes the
  projector, so it follows this leg.

## Build, test, attest

All builds run in the pinned Docker sandbox (WS-5, #2288) — no host toolchain
drift:

```sh
./docker/run.sh test -p attractor-core            # portable leg, Rust 1.92
./docker/run.sh --store test -p attractor-store   # store leg, Rust 1.96
./docker/attest.sh                                # full #2288 gate: both legs
```

`docker/attest.sh` is the promotion gate: portable leg (core, proto, config,
api, io) at 1.92 plus store leg (store, api+store feature, projector, wiki) at 1.96,
emitting a `RECEIPT host=… commit=… status=PASS` line for fleet logs. The
Milton-side hook is `milton/scripts/attractor-docker-attest.sh` (gate runs on
Homer).

### Cross-platform floor (#2235)

`attractor-core` (and the other 1.92 crates) are verified **at the MSRV
floor**, not above it:

- Homer Linux, Rust 1.92 in the Docker sandbox (Ivy)
- Windows native, Rust 1.92 (Windy)

Both must be green before contract-layer changes merge. Verifying at the floor
(not just pinning `rust-version`, which is informational) is what keeps
own-code drift from passing on a newer toolchain.

## Configuration

Copy `config.example.yaml` → `config.yaml`; each crate reads its **own**
section (api endpoints, store data dir, io shm paths). Paths are relative and
resolved against the config file's directory, so deploys can move. Secrets go
in `.env` only — read via `attractor-config::secret`, never committed.

## Design issues

#2209 (matrix ops + API) · #2212 (convergence plan) · #2213 (crate ground
rules + content-addressing) · #2215 (schema spec) · #2220/#2221 (redundancy
detector + cell-store scope) · #2243 (proto split) · #2249 (provenance
multiset) · #2288 (Docker deployment + gate) · #2517 (projection receipts /
source spans)
