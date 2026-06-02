//! Ingestion mapping: audmon partials → attractor-core schema (#2243).
//!
//! Pure and **dep-free of audmon** — it takes the *plain* partials payload
//! (`&[Vec<(f32, f32)>]` = per-channel `(frequency, amplitude)` pairs) plus a
//! little [`PartialsMeta`], not audmon's crate type. So the chain stays
//! decoupled: `attractor-io` reads audmon's `/dev/shm` → plain data → this
//! mapping → shipped [`Cell`]/[`Frame`]/[`SourceRef`] → `attractor-store`
//! inserts them.
//!
//! Codec/identity is locked per the #2243 thread: identity is serde_json
//! content-addressing (audmon's bincode is transport only), and `f32` widens to
//! `f64` exactly (no new `Primitive` variant). Float cells go through
//! [`Cell::address`], so a NaN/Inf in the audio stream is rejected loudly
//! ([`AddressError::NonFiniteFloat`]) rather than aliased to a `null` hash.

use crate::{AddressError, Cell, ContentHash, Frame, Primitive, SourceRef};
use std::collections::BTreeMap;

/// Minimal analysis metadata carried alongside a partials frame. Kept small on
/// purpose (v0); add scalars here as ingestion needs them — each becomes a
/// config [`Cell`] on the root frame.
#[derive(Clone, Debug, PartialEq)]
pub struct PartialsMeta {
    /// Configured number of partials per channel.
    pub num_partials: usize,
    /// Signal gain applied during analysis.
    pub gain: f64,
}

/// The result of ingesting one partials payload: every produced node plus the
/// content address of the top `audmon.partials_frame` (the query entry point).
/// All `cells` and `frames` are content-addressed and ready for
/// `attractor-store` to insert; nothing here is audmon-specific.
#[derive(Clone, Debug, PartialEq)]
pub struct Ingested {
    /// Leaf value cells (each freq, each amp, config scalars).
    pub cells: Vec<Cell>,
    /// Structural frames (per partial, per channel, and the root).
    pub frames: Vec<Frame>,
    /// Address of the root `audmon.partials_frame`.
    pub root: ContentHash,
}

/// Map a plain partials payload into attractor-core nodes.
///
/// `channels[c]` is channel `c`'s list of `(freq, amp)` partials. `origin` is a
/// provenance label (e.g. `"audmon:shm0"`); `payload` is the raw source bytes
/// the frame was derived from — its hash anchors every node's [`SourceRef`].
///
/// # Errors
/// [`AddressError::NonFiniteFloat`] if any freq/amp is NaN/Inf (rejected, not
/// aliased), else [`AddressError::Serde`] on a serialization failure.
pub fn ingest_partials(
    channels: &[Vec<(f32, f32)>],
    meta: &PartialsMeta,
    origin: &str,
    payload: &[u8],
) -> Result<Ingested, AddressError> {
    let source = SourceRef {
        content_hash: ContentHash::of(payload),
        origin: origin.to_owned(),
    };

    let mut cells: Vec<Cell> = Vec::new();
    let mut frames: Vec<Frame> = Vec::new();

    // Helper: build a float cell, record it, return its address.
    let push_float = |value: f64,
                      cells: &mut Vec<Cell>|
     -> Result<ContentHash, AddressError> {
        let cell = Cell {
            value: Primitive::Float(value),
            source: source.clone(),
        };
        let addr = cell.address()?;
        cells.push(cell);
        Ok(addr)
    };

    let mut channel_addrs: Vec<ContentHash> = Vec::with_capacity(channels.len());
    for partials in channels {
        let mut partial_addrs: Vec<ContentHash> = Vec::with_capacity(partials.len());
        for &(freq, amp) in partials {
            let freq_addr = push_float(f64::from(freq), &mut cells)?;
            let amp_addr = push_float(f64::from(amp), &mut cells)?;
            let mut elements = BTreeMap::new();
            elements.insert("freq".to_owned(), freq_addr);
            elements.insert("amp".to_owned(), amp_addr);
            let partial = Frame {
                name: "audmon.partial".to_owned(),
                elements,
            };
            let addr = crate::content_address(&partial)?;
            partial_addrs.push(addr);
            frames.push(partial);
        }
        let channel = Frame {
            name: "audmon.channel".to_owned(),
            elements: index_roles("partial", &partial_addrs),
        };
        let addr = crate::content_address(&channel)?;
        channel_addrs.push(addr);
        frames.push(channel);
    }

    // Config scalars as cells on the root frame.
    let num_partials_cell = Cell {
        value: Primitive::Int(i64::try_from(meta.num_partials).unwrap_or(i64::MAX)),
        source: source.clone(),
    };
    let num_partials_addr = num_partials_cell.address()?;
    cells.push(num_partials_cell);
    let gain_addr = push_float(meta.gain, &mut cells)?;

    let mut root_elements = index_roles("channel", &channel_addrs);
    root_elements.insert("num_partials".to_owned(), num_partials_addr);
    root_elements.insert("gain".to_owned(), gain_addr);
    let root_frame = Frame {
        name: "audmon.partials_frame".to_owned(),
        elements: root_elements,
    };
    let root = crate::content_address(&root_frame)?;
    frames.push(root_frame);

    Ok(Ingested { cells, frames, root })
}

/// Build `{prefix}.{i}` → addr roles for an ordered list (v0 ordering; switch
/// to explicit `Edge` links if a richer order model is needed later).
fn index_roles(prefix: &str, addrs: &[ContentHash]) -> BTreeMap<String, ContentHash> {
    addrs
        .iter()
        .enumerate()
        .map(|(i, a)| (format!("{prefix}.{i}"), *a))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{ingest_partials, PartialsMeta};
    use crate::AddressError;

    fn meta() -> PartialsMeta {
        PartialsMeta { num_partials: 4, gain: 1.0 }
    }

    #[test]
    fn maps_partials_to_nodes_with_root() {
        let channels = vec![vec![(440.0_f32, 0.8_f32), (880.0, 0.3)]];
        let out = ingest_partials(&channels, &meta(), "audmon:shm0", b"payload").unwrap();
        // 2 partials → 4 float cells + 2 config cells; 2 partial frames + 1
        // channel + 1 root.
        assert_eq!(out.cells.len(), 6);
        assert_eq!(out.frames.len(), 4);
        assert!(out.frames.iter().any(|f| f.name == "audmon.partials_frame"));
    }

    #[test]
    fn deterministic_same_input_same_root() {
        let channels = vec![vec![(440.0_f32, 0.8_f32)]];
        let a = ingest_partials(&channels, &meta(), "o", b"p").unwrap();
        let b = ingest_partials(&channels, &meta(), "o", b"p").unwrap();
        assert_eq!(a.root, b.root);
    }

    #[test]
    fn changed_payload_changes_provenance_not_structure_root() {
        // Same partials + meta but different source payload. Addressing is
        // value-only (#8/#2249): a cell's address hashes its value, never its
        // `source`, so identical audio structure from two different sources
        // converges to the SAME root. The differing payload still lives on each
        // cell's `SourceRef` as non-identity provenance — it just doesn't move
        // the address. This is the convergence the matrix is built on, proven at
        // the ingest layer.
        let channels = vec![vec![(440.0_f32, 0.8_f32)]];
        let a = ingest_partials(&channels, &meta(), "o", b"p1").unwrap();
        let b = ingest_partials(&channels, &meta(), "o", b"p2").unwrap();
        // Convergence: same structure → same root, regardless of source.
        assert_eq!(a.root, b.root);
        // ...but the payload difference is NOT lost — it's preserved as
        // non-identity provenance on each cell's `SourceRef`. (Accumulating both
        // sources into a multiset is `-store`'s job; core only proves the source
        // is recorded and distinct, keeping #7 dep-light for the 1.92 legs.)
        assert_ne!(
            a.cells[0].source.content_hash,
            b.cells[0].source.content_hash
        );
    }

    #[test]
    fn non_finite_partial_is_rejected() {
        let channels = vec![vec![(f32::NAN, 0.5_f32)]];
        assert!(matches!(
            ingest_partials(&channels, &meta(), "o", b"p"),
            Err(AddressError::NonFiniteFloat)
        ));
    }

    #[test]
    fn empty_channels_yields_just_config_and_root() {
        let out = ingest_partials(&[], &meta(), "o", b"p").unwrap();
        assert_eq!(out.cells.len(), 2); // num_partials + gain
        assert_eq!(out.frames.len(), 1); // root only
    }
}
