//! The Rust→Python shared-memory **control-file contract**.
//!
//! Portable (pure `serde`, no OS deps) so it attests on both the Linux-1.92 and
//! Windows-native-1.92 legs. The writer ([`crate::shm_export`]) emits this
//! alongside the mmap region; the Python reader parses it to learn where the
//! data lives, how many bytes are valid, and whether the contents changed.

use serde::{Deserialize, Serialize};

/// Sidecar metadata describing the current shared-memory contents.
///
/// Written as JSON next to the mmap'd data file. A reader polls/loads this,
/// then mmaps `data_path` and reads `len` bytes; `generation` lets it detect a
/// new publish without re-reading unchanged data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShmControl {
    /// PID of the writing process (so a reader can detect a dead/stale writer).
    pub pid: u32,
    /// Monotonic counter, bumped on every publish. Reader sees a new value =
    /// fresh data.
    pub generation: u64,
    /// Number of valid bytes at the start of the mmap region (`<= capacity`).
    pub len: u64,
    /// Total capacity of the mmap region in bytes.
    pub capacity: u64,
    /// Filesystem path of the mmap'd data file the reader should open
    /// (typically under `/dev/shm`).
    pub data_path: String,
}

impl ShmControl {
    /// Construct a control record for a freshly written region.
    pub fn new(pid: u32, generation: u64, len: u64, capacity: u64, data_path: impl Into<String>) -> Self {
        Self { pid, generation, len, capacity, data_path: data_path.into() }
    }

    /// Serialize to the JSON written to the control file.
    pub fn to_json(&self) -> String {
        // A flat struct of primitives + one String never fails to serialize.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parse a control file's JSON.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let c = ShmControl::new(4321, 7, 128, 4096, "/dev/shm/attractor.0");
        let back = ShmControl::from_json(&c.to_json()).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn generation_distinguishes_publishes() {
        let a = ShmControl::new(1, 1, 10, 4096, "/dev/shm/x");
        let b = ShmControl { generation: 2, ..a.clone() };
        assert_ne!(a, b);
        assert!(b.generation > a.generation);
    }

    #[test]
    fn len_within_capacity_is_the_writer_invariant() {
        // The contract a reader relies on: never read past `len`, never past
        // `capacity`. (Enforcement lives in the writer; documented here.)
        let c = ShmControl::new(1, 1, 100, 4096, "/dev/shm/x");
        assert!(c.len <= c.capacity);
    }

    #[test]
    fn json_shape_is_stable_for_the_python_reader() {
        let c = ShmControl::new(9, 3, 64, 256, "/dev/shm/a");
        let j = c.to_json();
        for key in ["pid", "generation", "len", "capacity", "data_path"] {
            assert!(j.contains(key), "control JSON must carry `{key}` for the Python reader: {j}");
        }
    }
}
