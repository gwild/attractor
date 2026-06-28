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

pub mod sanitize;
pub use sanitize::Sanitized;

pub mod ingest;

pub mod source_map;
pub use source_map::{ByteRange, LineRange, MarkdownSourceSpan, SyntaxKind, TypedEvidence};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

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

    /// Construct a content address from an existing 32-byte digest (not a hash-of-bytes).
    #[must_use]
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    /// Lowercase-hex rendering of the digest.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            // 1.92-safe: no format-args capture beyond what's stable here.
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    /// Parse a 64-digit lowercase-hex digest back into a content address — the
    /// inverse of [`ContentHash::to_hex`].
    ///
    /// # Errors
    /// [`ParseHashError::WrongLength`] if `hex` is not exactly 64 characters, or
    /// [`ParseHashError::NotHex`] if any character is not a hex digit.
    pub fn from_hex(hex: &str) -> Result<Self, ParseHashError> {
        let bytes = hex.as_bytes();
        if bytes.len() != 64 {
            return Err(ParseHashError::WrongLength { len: bytes.len() });
        }
        let mut digest = [0u8; 32];
        for (i, chunk) in bytes.chunks_exact(2).enumerate() {
            let pair = std::str::from_utf8(chunk).map_err(|_| ParseHashError::NotHex)?;
            digest[i] = u8::from_str_radix(pair, 16).map_err(|_| ParseHashError::NotHex)?;
        }
        Ok(Self(digest))
    }
}

/// Why a hex string could not be parsed into a [`ContentHash`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseHashError {
    /// The string was not exactly 64 hex characters.
    WrongLength {
        /// The byte length actually received.
        len: usize,
    },
    /// The string contained a character that is not a hex digit.
    NotHex,
}

impl fmt::Display for ParseHashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength { len } => {
                write!(f, "content hash hex must be 64 chars, got {len}")
            }
            Self::NotHex => f.write_str("content hash hex contains a non-hex character"),
        }
    }
}

impl std::error::Error for ParseHashError {}

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
/// **Non-identity metadata (#2249).** A `SourceRef` does NOT participate in a
/// cell's content address — the same value from `#general` and `#issues` is
/// *one* node, not two. `attractor-store` lifts provenance into a
/// `node → multiset<SourceRef>` index (one fact, the set of places it came
/// from), so origin can't fork the address and #2220 redundancy can converge.
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
///
/// **Identity is value-only (#2249):** [`Cell::address`] hashes `value`, *not*
/// `source`. Two cells with equal `value` and different `source` share one
/// address (convergence); `source` rides along as non-identity provenance that
/// `attractor-store` indexes per address.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    /// The value this cell holds — the addressable unit.
    pub value: Primitive,
    /// Where the value came from. **Non-identity** — not part of the address.
    pub source: SourceRef,
}

/// Why a [`Cell`] can't be content-addressed.
#[derive(Debug)]
pub enum AddressError {
    /// A non-finite float (NaN / ±Inf). `serde_json` encodes these as `null`,
    /// which would collapse distinct values to one address — so we reject them
    /// loudly instead of minting an ambiguous identity (#2215 review, Rebecca).
    NonFiniteFloat,
    /// Underlying JSON serialization failure.
    Serde(serde_json::Error),
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteFloat => {
                f.write_str("non-finite float (NaN/Inf) cannot be content-addressed")
            }
            Self::Serde(e) => write!(f, "serialization failed: {e}"),
        }
    }
}

impl std::error::Error for AddressError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serde(e) => Some(e),
            Self::NonFiniteFloat => None,
        }
    }
}

impl From<serde_json::Error> for AddressError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serde(e)
    }
}

impl Cell {
    /// This cell's content address — its identity in the store.
    ///
    /// **Value-only (#2249):** hashes `self.value`, not `self.source`. Equal
    /// values address identically regardless of where they came from, so the
    /// same fact from two channels converges to one node; provenance lives in
    /// `attractor-store`'s `node → multiset<SourceRef>` index, not the address.
    ///
    /// # Errors
    /// [`AddressError::NonFiniteFloat`] if the value is a NaN/Inf float
    /// (would serialize to `null` and alias distinct values); otherwise
    /// [`AddressError::Serde`] on serialization failure.
    pub fn address(&self) -> Result<ContentHash, AddressError> {
        if let Primitive::Float(f) = &self.value {
            if !f.is_finite() {
                return Err(AddressError::NonFiniteFloat);
            }
        }
        Ok(content_address(&self.value)?)
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
    use super::{
        content_address, AddressError, Cell, ContentHash, Frame, Primitive, SourceRef,
    };
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
    fn from_hex_inverts_to_hex() {
        let h = ContentHash::of(b"some value");
        assert_eq!(ContentHash::from_hex(&h.to_hex()).unwrap(), h);
    }

    #[test]
    fn from_hex_rejects_wrong_length_and_non_hex() {
        assert_eq!(
            ContentHash::from_hex("abc"),
            Err(super::ParseHashError::WrongLength { len: 3 })
        );
        let bad = format!("{}z", "a".repeat(63));
        assert_eq!(ContentHash::from_hex(&bad), Err(super::ParseHashError::NotHex));
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
    fn cell_address_is_value_only_ignores_source() {
        // #2249: same value from different sources → ONE address (convergence).
        let mut other = sample_cell();
        other.source = SourceRef {
            content_hash: ContentHash::of(b"different-payload"),
            origin: "#issues".to_owned(),
        };
        assert_eq!(
            sample_cell().address().unwrap(),
            other.address().unwrap()
        );
    }

    #[test]
    fn cell_address_equals_bare_value_address() {
        // The address IS the value's content address — source contributes nothing.
        let c = sample_cell();
        assert_eq!(c.address().unwrap(), content_address(&c.value).unwrap());
    }

    #[test]
    fn cell_address_rejects_non_finite_float() {
        // serde_json encodes NaN/Inf as `null` → distinct values would alias to
        // one address. Reject loudly instead (#2215 review).
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut c = sample_cell();
            c.value = Primitive::Float(bad);
            assert!(matches!(c.address(), Err(AddressError::NonFiniteFloat)));
        }
    }

    #[test]
    fn cell_address_allows_finite_float() {
        let mut c = sample_cell();
        c.value = Primitive::Float(3.5);
        assert!(c.address().is_ok());
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
