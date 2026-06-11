# Attractor

Content-addressed matrix substrate for the Milton fleet. A field where
information is added and convergence (the "attractor" state) is computed over
it — the Rust core of the matrix project.

## Workspace layout

- **`attractor-core`** — pure schema + content-addressing: the cell/frame data
  model, `blake3` content hashing, `serde`. **No `tokio`/`axum`/`qdrant`/OS
  deps** — this is the layer that compiles and is attested cross-platform.
  MSRV pinned to the fleet's lowest Rust (`1.92`) on purpose.
- **`attractor-store` / `attractor-api` / `attractor-projector`** — store,
  control verbs, and WS-3 projection. These sit above the portable core and run
  on the Linux/Homer container leg when they need store semantics.

## Cross-platform attestation (#2235 self-verify floor)

`attractor-core` is verified **at the MSRV floor**, not above it:

- Homer Linux, `cargo +1.92 test` (Ivy)
- Windows native, Rust 1.92 (Windy)

Both must be green before core changes merge. Verifying at the floor (not just
pinning `rust-version`, which is informational) is what keeps own-code drift
from passing on a newer toolchain.

## Design issues

#2209 (matrix ops + API) · #2212 (convergence plan) · #2213 (crate ground
rules + content-addressing) · #2215 (schema spec) · #2220/#2221 (redundancy
detector + cell-store scope)
