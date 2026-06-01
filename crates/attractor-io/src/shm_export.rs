//! mmap + control-file writer for the Rust→Python shared-memory export.
//!
//! Unix-only (`/dev/shm` + mmap), gated at the crate root with `#[cfg(unix)]`.
//! Adapts audmon's `shared_memory_updater_loop`: write the byte blob into an
//! mmap'd region, then publish the [`ShmControl`] sidecar so a Python reader
//! can mmap the same file and read exactly `len` valid bytes.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process;

use memmap2::MmapMut;

use crate::ShmControl;

/// Writes opaque byte blobs into an mmap'd region and publishes a control file
/// describing each write. The payload format is the caller's concern (the
/// transport is data-model-agnostic).
pub struct ShmExporter {
    data_path: PathBuf,
    control_path: PathBuf,
    mmap: MmapMut,
    capacity: usize,
    generation: u64,
}

impl ShmExporter {
    /// Create (or truncate) the data file to `capacity` bytes, mmap it, and
    /// remember where the control file goes. `data_path` typically lives under
    /// `/dev/shm` so the region is RAM-backed.
    pub fn new(
        data_path: impl AsRef<Path>,
        control_path: impl AsRef<Path>,
        capacity: usize,
    ) -> std::io::Result<Self> {
        let data_path = data_path.as_ref().to_path_buf();
        let control_path = control_path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&data_path)?;
        file.set_len(capacity as u64)?;
        // SAFETY: we own this freshly-created/truncated file for the exporter's
        // lifetime; no other writer maps it.
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        Ok(Self { data_path, control_path, mmap, capacity, generation: 0 })
    }

    /// Copy `bytes` into the region and publish an updated control file
    /// (atomically, via temp + rename, so a reader never sees a half-written
    /// control). Returns the new generation. Errors if `bytes` exceeds capacity.
    pub fn publish(&mut self, bytes: &[u8]) -> std::io::Result<u64> {
        if bytes.len() > self.capacity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("payload {} bytes exceeds shm capacity {}", bytes.len(), self.capacity),
            ));
        }
        self.mmap[..bytes.len()].copy_from_slice(bytes);
        self.mmap.flush()?;
        self.generation += 1;
        let ctrl = ShmControl::new(
            process::id(),
            self.generation,
            bytes.len() as u64,
            self.capacity as u64,
            self.data_path.to_string_lossy().into_owned(),
        );
        let tmp = self.control_path.with_extension("ctrl.tmp");
        std::fs::write(&tmp, ctrl.to_json())?;
        std::fs::rename(&tmp, &self.control_path)?;
        Ok(self.generation)
    }

    /// Current generation (number of publishes so far).
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir();
        (dir.join(format!("attractor_io_{tag}.data")), dir.join(format!("attractor_io_{tag}.ctrl")))
    }

    #[test]
    fn publish_writes_data_and_control() {
        let (data, control) = paths("pub");
        let mut ex = ShmExporter::new(&data, &control, 64).unwrap();
        let g = ex.publish(b"hello").unwrap();
        assert_eq!(g, 1);

        let c = ShmControl::from_json(&std::fs::read_to_string(&control).unwrap()).unwrap();
        assert_eq!(c.len, 5);
        assert_eq!(c.generation, 1);
        assert_eq!(c.capacity, 64);

        let raw = std::fs::read(&data).unwrap();
        assert_eq!(&raw[..5], b"hello");

        let _ = std::fs::remove_file(&data);
        let _ = std::fs::remove_file(&control);
    }

    #[test]
    fn generation_increments_per_publish() {
        let (data, control) = paths("gen");
        let mut ex = ShmExporter::new(&data, &control, 32).unwrap();
        assert_eq!(ex.publish(b"a").unwrap(), 1);
        assert_eq!(ex.publish(b"bb").unwrap(), 2);
        assert_eq!(ex.generation(), 2);
        let _ = std::fs::remove_file(&data);
        let _ = std::fs::remove_file(&control);
    }

    #[test]
    fn oversized_payload_errors_not_panics() {
        let (data, control) = paths("over");
        let mut ex = ShmExporter::new(&data, &control, 4).unwrap();
        assert!(ex.publish(b"toolong").is_err());
        let _ = std::fs::remove_file(&data);
        let _ = std::fs::remove_file(&control);
    }
}
