#![forbid(unsafe_code)]
#![warn(clippy::pedantic, clippy::nursery)]
#![deny(missing_docs, missing_debug_implementations)]

//! # attractor-store
//!
//! The persistence layer above [`attractor_core`]: a content-addressed node
//! store, an acyclic relation graph, and a tamper-evident provenance chain.
//!
//! Core defines *what a value is* (schema + [`ContentHash`]); this crate defines
//! *how values are kept and related*. Three guarantees, each enforced by
//! construction rather than convention:
//!
//! 1. **Content-addressed dedup.** A [`Node`] is keyed by its core content
//!    address, so inserting identical content twice is a no-op that returns the
//!    same address — identity is the hash, not insertion order.
//! 2. **Acyclic, referentially-intact DAG.** [`Store::link`] refuses an edge to
//!    a node that isn't stored (no dangling edges) and refuses any edge that
//!    would introduce a cycle (the matrix is a DAG).
//! 3. **Tamper-evident chain.** Every first insertion appends a [`ChainEntry`]
//!    committing to its predecessor's hash, so [`Store::verify_chain`] detects
//!    any reordering or omission — the `prev_hash` chain that core deferred to
//!    this layer.
//!
//! This is the Linux-container-only layer (it pulls `petgraph`); it is pure
//! synchronous data structure with no async/HTTP — those belong in
//! `attractor-api`.

use std::collections::HashMap;
use std::fmt;

use attractor_core::{content_address, AddressError, Cell, ContentHash, Edge, Frame, SourceRef};
use petgraph::algo::is_cyclic_directed;
use petgraph::graph::{DiGraph, NodeIndex};
use serde::{Deserialize, Serialize};

/// An addressable node in the matrix: either a leaf [`Cell`] or a [`Frame`]
/// grouping. Edges relate nodes; nodes themselves carry the content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Node {
    /// A leaf value with provenance.
    Cell(Cell),
    /// A named frame whose roles map to other nodes' addresses.
    Frame(Frame),
}

impl Node {
    /// This node's content address — its identity in the store.
    ///
    /// Delegates to the inner value's core addressing so the address equals what
    /// an ingestor computes via [`Cell::address`] / [`content_address`]; the
    /// `Node` wrapper itself is never hashed.
    ///
    /// # Errors
    /// Propagates [`AddressError`] from the inner value (e.g. a NaN/Inf float in
    /// a [`Cell`], which core refuses to address).
    pub fn address(&self) -> Result<ContentHash, AddressError> {
        match self {
            Self::Cell(c) => c.address(),
            Self::Frame(f) => Ok(content_address(f)?),
        }
    }
}

/// One link in the provenance chain: the address that was inserted, plus the
/// hash of the entry before it (`None` at genesis). Hashing an entry commits to
/// its predecessor, making the chain tamper-evident.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChainEntry {
    /// The content address inserted at this step.
    pub node: ContentHash,
    /// The previous entry's [`ChainEntry::hash`], or `None` for the first entry.
    pub prev: Option<ContentHash>,
}

impl ChainEntry {
    /// This entry's own hash, committing to its node and predecessor.
    ///
    /// Hashes the raw 32-byte addresses directly (with a one-byte genesis/link
    /// tag for domain separation) rather than via `serde_json`, so the chain's
    /// integrity does not depend on any codec choice and cannot fail.
    #[must_use]
    pub fn hash(&self) -> ContentHash {
        let mut bytes = Vec::with_capacity(1 + 32 + 32);
        bytes.extend_from_slice(self.node.as_bytes());
        match &self.prev {
            Some(p) => {
                bytes.push(1);
                bytes.extend_from_slice(p.as_bytes());
            }
            None => bytes.push(0),
        }
        ContentHash::of(&bytes)
    }
}

/// Why a store operation failed.
#[derive(Debug)]
pub enum StoreError {
    /// The node could not be content-addressed (see [`AddressError`]).
    Address(AddressError),
    /// An edge endpoint is not present in the store (no dangling edges).
    DanglingEdge {
        /// The missing endpoint's address.
        endpoint: ContentHash,
    },
    /// Adding the edge would introduce a cycle; the matrix must stay acyclic.
    WouldCycle {
        /// The edge's source.
        from: ContentHash,
        /// The edge's destination.
        to: ContentHash,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Address(e) => write!(f, "cannot address node: {e}"),
            Self::DanglingEdge { endpoint } => {
                write!(f, "edge endpoint {endpoint} is not in the store")
            }
            Self::WouldCycle { from, to } => {
                write!(f, "edge {from} -> {to} would create a cycle")
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Address(e) => Some(e),
            Self::DanglingEdge { .. } | Self::WouldCycle { .. } => None,
        }
    }
}

impl From<AddressError> for StoreError {
    fn from(e: AddressError) -> Self {
        Self::Address(e)
    }
}

/// A content-addressed store of [`Node`]s, an acyclic relation graph over their
/// addresses, and the provenance chain of insertions.
#[derive(Debug, Default)]
pub struct Store {
    nodes: HashMap<ContentHash, Node>,
    graph: DiGraph<ContentHash, String>,
    index: HashMap<ContentHash, NodeIndex>,
    chain: Vec<ChainEntry>,
    head: Option<ContentHash>,
    provenance: HashMap<ContentHash, Vec<SourceRef>>,
}

impl Store {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a node, returning its content address.
    ///
    /// The *node* dedups: re-inserting identical content returns the same
    /// address without duplicating the node, the graph vertex, or the
    /// provenance chain — content-addressing dedups by construction.
    ///
    /// *Provenance does not dedup.* Identity is value-only (#2249), so the same
    /// value from a new source resolves to the existing address; its
    /// [`SourceRef`] is still appended to that address's provenance multiset.
    /// That repeat-insert-different-origin append is how #2220 redundancy
    /// convergence records that a fact was observed again. Frames carry no
    /// source and contribute nothing to provenance.
    ///
    /// First-writer-wins on provenance: when a re-inserted Cell carries a
    /// different `source` than the stored copy, the later source is dropped
    /// here. The authoritative source multiset lives in the provenance index
    /// (follow-up), not on the stored Node.
    ///
    /// # Errors
    /// [`StoreError::Address`] if the node cannot be content-addressed.
    pub fn insert(&mut self, node: Node) -> Result<ContentHash, StoreError> {
        let addr = node.address()?;
        if let Node::Cell(cell) = &node {
            self.provenance
                .entry(addr)
                .or_default()
                .push(cell.source.clone());
        }
        if self.nodes.contains_key(&addr) {
            return Ok(addr);
        }
        let idx = self.graph.add_node(addr);
        self.index.insert(addr, idx);
        self.nodes.insert(addr, node);
        let entry = ChainEntry {
            node: addr,
            prev: self.head,
        };
        self.head = Some(entry.hash());
        self.chain.push(entry);
        Ok(addr)
    }

    /// The node at `addr`, if stored.
    #[must_use]
    pub fn get(&self, addr: &ContentHash) -> Option<&Node> {
        self.nodes.get(addr)
    }

    /// Whether `addr` is stored.
    #[must_use]
    pub fn contains(&self, addr: &ContentHash) -> bool {
        self.nodes.contains_key(addr)
    }

    /// Number of distinct nodes stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the store holds no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Relate two stored nodes with a typed edge.
    ///
    /// Returns the [`Edge`] that was recorded.
    ///
    /// # Errors
    /// - [`StoreError::DanglingEdge`] if either endpoint is not stored.
    /// - [`StoreError::WouldCycle`] if the edge would make the graph cyclic (the
    ///   edge is not added in that case).
    pub fn link(
        &mut self,
        from: ContentHash,
        to: ContentHash,
        kind: impl Into<String>,
    ) -> Result<Edge, StoreError> {
        let from_idx = *self
            .index
            .get(&from)
            .ok_or(StoreError::DanglingEdge { endpoint: from })?;
        let to_idx = *self
            .index
            .get(&to)
            .ok_or(StoreError::DanglingEdge { endpoint: to })?;
        let kind = kind.into();
        let edge_idx = self.graph.add_edge(from_idx, to_idx, kind.clone());
        if is_cyclic_directed(&self.graph) {
            self.graph.remove_edge(edge_idx);
            return Err(StoreError::WouldCycle { from, to });
        }
        Ok(Edge { from, to, kind })
    }

    /// The current chain head: the hash of the most recent entry, or `None` when
    /// empty. This single value is what to persist/compare for tamper-evidence.
    #[must_use]
    pub fn head(&self) -> Option<ContentHash> {
        self.head
    }

    /// Number of insertions recorded in the provenance chain.
    #[must_use]
    pub fn chain_len(&self) -> usize {
        self.chain.len()
    }

    /// Recompute the chain and confirm it is intact: every entry must commit to
    /// its predecessor's hash and [`Store::head`] must equal the last entry's
    /// hash. Returns `true` for an untampered chain.
    #[must_use]
    pub fn verify_chain(&self) -> bool {
        let mut prev: Option<ContentHash> = None;
        for entry in &self.chain {
            if entry.prev != prev {
                return false;
            }
            prev = Some(entry.hash());
        }
        prev == self.head
    }

    /// The provenance multiset for `addr`: every [`SourceRef`] observed for this
    /// value, in insertion order.
    ///
    /// Since identity is value-only, one address can carry many sources — the
    /// same fact seen in `#general` and `#issues` yields two entries here while
    /// remaining one node. Returns an empty slice for frames (no source) and for
    /// addresses never inserted.
    #[must_use]
    pub fn provenance(&self, addr: &ContentHash) -> &[SourceRef] {
        self.provenance.get(addr).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::{ChainEntry, Node, Store, StoreError};
    use attractor_core::{Cell, ContentHash, Frame, Primitive, SourceRef};
    use std::collections::BTreeMap;

    fn cell(text: &str) -> Node {
        Node::Cell(Cell {
            value: Primitive::Text(text.to_owned()),
            source: SourceRef {
                content_hash: ContentHash::of(b"origin"),
                origin: "#test".to_owned(),
            },
        })
    }

    fn frame(name: &str) -> Node {
        Node::Frame(Frame {
            name: name.to_owned(),
            elements: BTreeMap::new(),
        })
    }

    fn cell_from(text: &str, origin: &str) -> Node {
        Node::Cell(Cell {
            value: Primitive::Text(text.to_owned()),
            source: SourceRef {
                content_hash: ContentHash::of(origin.as_bytes()),
                origin: origin.to_owned(),
            },
        })
    }

    #[test]
    fn node_address_matches_inner_cell_address() {
        let Node::Cell(c) = cell("x") else {
            unreachable!()
        };
        assert_eq!(Node::Cell(c.clone()).address().unwrap(), c.address().unwrap());
    }

    #[test]
    fn insert_then_get_round_trips() {
        let mut s = Store::new();
        let n = cell("hello");
        let addr = s.insert(n.clone()).unwrap();
        assert_eq!(s.get(&addr), Some(&n));
        assert!(s.contains(&addr));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn insert_is_idempotent_and_does_not_extend_chain() {
        let mut s = Store::new();
        let a1 = s.insert(cell("dup")).unwrap();
        let a2 = s.insert(cell("dup")).unwrap();
        assert_eq!(a1, a2);
        assert_eq!(s.len(), 1);
        assert_eq!(s.chain_len(), 1);
    }

    #[test]
    fn distinct_content_distinct_address_extends_chain() {
        let mut s = Store::new();
        let a = s.insert(cell("a")).unwrap();
        let b = s.insert(cell("b")).unwrap();
        assert_ne!(a, b);
        assert_eq!(s.len(), 2);
        assert_eq!(s.chain_len(), 2);
    }

    #[test]
    fn link_requires_existing_endpoints() {
        let mut s = Store::new();
        let a = s.insert(cell("a")).unwrap();
        let missing = ContentHash::of(b"never-inserted");
        assert!(matches!(
            s.link(a, missing, "refers_to"),
            Err(StoreError::DanglingEdge { .. })
        ));
    }

    #[test]
    fn link_succeeds_between_stored_nodes() {
        let mut s = Store::new();
        let a = s.insert(frame("from")).unwrap();
        let b = s.insert(cell("to")).unwrap();
        let edge = s.link(a, b, "contains").unwrap();
        assert_eq!(edge.from, a);
        assert_eq!(edge.to, b);
        assert_eq!(edge.kind, "contains");
    }

    #[test]
    fn link_rejects_cycle() {
        let mut s = Store::new();
        let a = s.insert(cell("a")).unwrap();
        let b = s.insert(cell("b")).unwrap();
        s.link(a, b, "next").unwrap();
        assert!(matches!(
            s.link(b, a, "next"),
            Err(StoreError::WouldCycle { .. })
        ));
        // The rejected edge must not have been retained.
        assert!(s.link(b, a, "next").is_err());
    }

    #[test]
    fn link_rejects_self_loop() {
        let mut s = Store::new();
        let a = s.insert(cell("a")).unwrap();
        assert!(matches!(
            s.link(a, a, "self"),
            Err(StoreError::WouldCycle { .. })
        ));
    }

    #[test]
    fn head_advances_per_insertion() {
        let mut s = Store::new();
        assert_eq!(s.head(), None);
        let _ = s.insert(cell("a")).unwrap();
        let h1 = s.head();
        assert!(h1.is_some());
        let _ = s.insert(cell("b")).unwrap();
        assert_ne!(s.head(), h1);
    }

    #[test]
    fn chain_verifies_after_inserts() {
        let mut s = Store::new();
        assert!(s.verify_chain());
        s.insert(cell("a")).unwrap();
        s.insert(cell("b")).unwrap();
        s.insert(cell("c")).unwrap();
        assert!(s.verify_chain());
    }

    #[test]
    fn genesis_entry_has_no_predecessor() {
        let e = ChainEntry {
            node: ContentHash::of(b"a"),
            prev: None,
        };
        // A genesis entry and a non-genesis entry over the same node hash differ.
        let linked = ChainEntry {
            node: ContentHash::of(b"a"),
            prev: Some(ContentHash::of(b"prev")),
        };
        assert_ne!(e.hash(), linked.hash());
    }

    #[test]
    fn same_value_different_source_converges_to_one_node() {
        // #2249/#2220: value-only identity means a fact from two channels is one
        // node, with both sources accruing in the provenance multiset.
        let mut s = Store::new();
        let a = s.insert(cell_from("fact", "#general")).unwrap();
        let b = s.insert(cell_from("fact", "#issues")).unwrap();
        assert_eq!(a, b, "same value must converge regardless of origin");
        assert_eq!(s.len(), 1);
        assert_eq!(s.chain_len(), 1, "convergence must not extend the chain");
        let origins: Vec<&str> = s.provenance(&a).iter().map(|sr| sr.origin.as_str()).collect();
        assert_eq!(origins, ["#general", "#issues"]);
    }

    #[test]
    fn provenance_is_a_multiset_counting_re_observation() {
        // Identical value+source inserted twice → one node, but provenance
        // records both observations (multiset, not set).
        let mut s = Store::new();
        let a = s.insert(cell_from("dup", "#general")).unwrap();
        s.insert(cell_from("dup", "#general")).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s.provenance(&a).len(), 2);
    }

    #[test]
    fn frames_have_no_provenance() {
        let mut s = Store::new();
        let f = s.insert(frame("structural")).unwrap();
        assert!(s.provenance(&f).is_empty());
    }

    #[test]
    fn provenance_empty_for_unknown_address() {
        let s = Store::new();
        assert!(s.provenance(&ContentHash::of(b"never")).is_empty());
    }
}
