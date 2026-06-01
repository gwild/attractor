//! Linux-targeted Rust→Python shared-memory export for Attractor (#2243).
//!
//! Adapts audmon's proven pattern (`shared_memory_updater_loop`): the Rust
//! side writes a byte blob into an mmap'd `/dev/shm` region and publishes a
//! small **control file** with metadata (pid / generation / length / data
//! path) so a Python reader can mmap the same region and know what's valid —
//! audmon ships only the Rust *writer*, so Attractor's Python consumer is a
//! separate follow-up.
//!
//! ## Layering (the proto-split applied here)
//! - [`ShmControl`] — the Rust↔Python **control-file contract**. Pure `serde`,
//!   no OS deps → it compiles + attests identically on Linux-1.92 and
//!   Windows-native-1.92, same property as `attractor-core`/`attractor-proto`.
//! - [`shm_export`] — the actual mmap/`/dev/shm` writer. `cfg(unix)`-gated, so
//!   this crate still compiles on Windows (the module is simply absent) while
//!   the shared-memory path runs on Linux.
//!
//! ## Data-model-agnostic
//! This crate owns only the **transport**: callers serialize matrix
//! nodes/cells to bytes (the on-wire payload format is coordinated with
//! `attractor-core`/`attractor-store`) and hand the opaque blob to the
//! exporter. Keeping the payload opaque here means the export format can settle
//! independently without churning the transport.

mod control;
pub use control::ShmControl;

#[cfg(unix)]
pub mod shm_export;
