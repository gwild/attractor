#![forbid(unsafe_code)]
#![warn(clippy::pedantic, clippy::nursery)]
#![deny(missing_docs, missing_debug_implementations)]

//! # attractor-core
//!
//! Pure schema + content-addressing for the Attractor matrix.
//!
//! This crate is deliberately dependency-light — `serde`, `serde_json`, and
//! `blake3`, with **no** async runtime, HTTP, vector store, or OS-specific
//! deps. That is what lets it compile and be attested identically on Linux
//! (Homer, `cargo +1.92`) and Windows-native (1.92). Higher layers
//! (`attractor-store`, `attractor-api`) add `petgraph`/`axum`/`pyo3` and floor
//! their MSRV upward; they do not belong here.
//!
//! ## Content addressing
//!
//! Every value's identity is the [`ContentHash`] of its canonical bytes
//! (BLAKE3). Naming-by-hash makes the store tamper-evident by construction:
//! any mutation yields a different hash, so a "tagged" value is no longer
//! reachable from the same address (git's object-model property). Embeddings
//! and indexes key on the hash, so changed content invalidates by construction
//! rather than silently drifting.
//!
//! ## v0 status
//!
//! The type *shapes* below (`Cell`/`Domain`/`Primitive`/`Frame`/`Edge`/
//! `SourceRef`) are a reviewable v0 grounded in the #2213 content-addressing
//! model and the frame-semantics discussion — not a frozen spec. Two things
//! are explicitly deferred and noted at their call sites: a canonical codec
//! (v0 hashes `serde_json` bytes, deterministic for these types but not a
//! formal canonical form), and the Ed25519 signing/`prev_hash` chain (a
//! `-store`-layer concern, not core schema).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// A BLAKE3 content address: the 32-byte hash that *is* a value's identity.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// Hash raw bytes into a content address.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).into())
    }

    /// The raw 32-byte digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase-hex rendering of the digest.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            // 1.92-safe: no format-args capture beyond what's stable here.
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

// Hex Debug/Display keeps logs readable and satisfies the crate-wide
// `missing_debug_implementations` deny without dumping a 32-element array.
impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({})", self.to_hex())
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Content-address any serializable value.
///
/// v0: hashes the value's `serde_json` encoding, which is deterministic for the
/// types in this crate (structs serialize in field order; maps are
/// [`BTreeMap`], i.e. key-sorted). A formal canonical codec is a fast-follow;
/// until then do not rely on cross-version byte-stability of the encoding.
///
/// # Errors
/// Returns [`serde_json::Error`] if `value` fails to serialize.
pub fn content_address<T: Serialize>(value: &T) -> Result<ContentHash, serde_json::Error> {
    Ok(ContentHash::of(&serde_json::to_vec(value)?))
}

/// A typed leaf value — the smallest semantic unit stored in a [`Cell`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Primitive {
    /// UTF-8 text.
    Text(String),
    /// Signed 64-bit integer.
    Int(i64),
    /// 64-bit float. (Excludes `Primitive` from `Eq`/`Hash` by design.)
    Float(f64),
    /// Boolean.
    Bool(bool),
    /// A reference to another content-addressed value.
    Ref(ContentHash),
}

/// Provenance: where a value came from, anchored to the source's content hash.
///
/// The chain/signature fields from the #2213 model (`prev_hash`, Ed25519
/// `signer`/`sig`) live in the `-store` layer; core keeps provenance to the
/// content anchor plus a free-form origin label (e.g. a chat channel).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    /// Content address of the source payload this value was derived from.
    pub content_hash: ContentHash,
    /// Human/origin label (e.g. `"#general"`, `"pr:2232"`); not load-bearing.
    pub origin: String,
}

/// The atomic unit of the matrix: a typed value plus its provenance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    /// The value this cell holds.
    pub value: Primitive,
    /// Where the value came from.
    pub source: SourceRef,
}

impl Cell {
    /// This cell's content address — its identity in the store.
    ///
    /// # Errors
    /// Propagates serialization failure from [`content_address`].
    pub fn address(&self) -> Result<ContentHash, serde_json::Error> {
        content_address(self)
    }
}

/// A named grouping/namespace of cells (one axis of the matrix).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Domain {
    /// Domain name (unique within the matrix).
    pub name: String,
    /// Optional human description.
    pub description: String,
}

/// A frame-semantics structure: a named frame whose roles map to content
/// addresses of the cells (or other frames) filling them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    /// Frame name (e.g. a frame-semantic type).
    pub name: String,
    /// Role name -> content address of the filler. `BTreeMap` keeps the
    /// encoding deterministic for content addressing.
    pub elements: BTreeMap<String, ContentHash>,
}

/// A typed, directed relation between two nodes (cells/frames) in the matrix
/// DAG, addressed by their content hashes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// Source node.
    pub from: ContentHash,
    /// Destination node.
    pub to: ContentHash,
    /// Relation kind (e.g. `"derives"`, `"refers_to"`).
    pub kind: String,
}

#[cfg(test)]
mod tests {
    use super::{content_address, Cell, ContentHash, Frame, Primitive, SourceRef};
    use std::collections::BTreeMap;

    fn sample_cell() -> Cell {
        Cell {
            value: Primitive::Text("hello".to_owned()),
            source: SourceRef {
                content_hash: ContentHash::of(b"origin-payload"),
                origin: "#general".to_owned(),
            },
        }
    }

    #[test]
    fn hash_is_deterministic_and_distinguishing() {
        assert_eq!(ContentHash::of(b"abc"), ContentHash::of(b"abc"));
        assert_ne!(ContentHash::of(b"abc"), ContentHash::of(b"abd"));
    }

    #[test]
    fn hex_round_trips_length() {
        assert_eq!(ContentHash::of(b"x").to_hex().len(), 64);
    }

    #[test]
    fn cell_address_is_stable_across_clones() {
        let c = sample_cell();
        assert_eq!(c.address().unwrap(), c.clone().address().unwrap());
    }

    #[test]
    fn cell_address_changes_with_content() {
        let mut other = sample_cell();
        other.value = Primitive::Text("world".to_owned());
        assert_ne!(
            sample_cell().address().unwrap(),
            other.address().unwrap()
        );
    }

    #[test]
    fn serde_round_trip_preserves_value() {
        let c = sample_cell();
        let json = serde_json::to_string(&c).unwrap();
        let back: Cell = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn frame_elements_are_order_independent_for_addressing() {
        let h1 = ContentHash::of(b"1");
        let h2 = ContentHash::of(b"2");
        let mut a = BTreeMap::new();
        a.insert("agent".to_owned(), h1);
        a.insert("theme".to_owned(), h2);
        let mut b = BTreeMap::new();
        b.insert("theme".to_owned(), h2);
        b.insert("agent".to_owned(), h1);
        let fa = Frame { name: "post".to_owned(), elements: a };
        let fb = Frame { name: "post".to_owned(), elements: b };
        assert_eq!(content_address(&fa).unwrap(), content_address(&fb).unwrap());
    }
}
