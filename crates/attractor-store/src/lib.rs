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
use attractor_proto::E2eReceipt;
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

/// One link in the receipt audit chain: the stored node address, the attached
/// receipt's content hash, and the previous receipt entry hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReceiptChainEntry {
    /// The stored node whose receipt was attached.
    pub node: ContentHash,
    /// Hash of the attached receipt payload.
    pub receipt: ContentHash,
    /// The previous [`ReceiptChainEntry::hash`], or `None` for the first receipt.
    pub prev: Option<ContentHash>,
}

impl ReceiptChainEntry {
    /// This entry's own hash, committing to its node, receipt payload, and
    /// predecessor.
    #[must_use]
    pub fn hash(&self) -> ContentHash {
        let mut bytes = Vec::with_capacity(1 + 32 + 32 + 32);
        bytes.extend_from_slice(self.node.as_bytes());
        bytes.extend_from_slice(self.receipt.as_bytes());
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
    /// A receipt was attached for an address that is not in the store.
    ReceiptForUnknown {
        /// The unknown address.
        addr: ContentHash,
    },
    /// The receipt payload could not be content-addressed.
    ReceiptAddress(serde_json::Error),
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
            Self::ReceiptForUnknown { addr } => {
                write!(f, "receipt for {addr} which is not in the store")
            }
            Self::ReceiptAddress(e) => write!(f, "cannot address receipt: {e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Address(e) => Some(e),
            Self::ReceiptAddress(e) => Some(e),
            Self::DanglingEdge { .. }
            | Self::WouldCycle { .. }
            | Self::ReceiptForUnknown { .. } => None,
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
    receipts: HashMap<ContentHash, Vec<E2eReceipt>>,
    receipt_chain: Vec<ReceiptChainEntry>,
    receipt_head: Option<ContentHash>,
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
    pub const fn head(&self) -> Option<ContentHash> {
        self.head
    }

    /// Number of insertions recorded in the provenance chain.
    #[must_use]
    pub const fn chain_len(&self) -> usize {
        self.chain.len()
    }

    /// The current receipt-chain head, or `None` when no receipts are attached.
    #[must_use]
    pub const fn receipt_head(&self) -> Option<ContentHash> {
        self.receipt_head
    }

    /// Number of receipt attachments recorded in the receipt audit chain.
    #[must_use]
    pub const fn receipt_chain_len(&self) -> usize {
        self.receipt_chain.len()
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

    /// Attach a transform receipt that produced `addr`.
    ///
    /// Receipts are out-of-band metadata: they make a stored derived node
    /// auditable without changing its content address. Because identity is
    /// value-only (#2249), one address can be derived by several transforms — a
    /// note's title and another note's `[[reference]]` converge to the same node
    /// — so receipts **accumulate per address** as a multiset, mirroring the
    /// provenance index rather than overwriting. Re-attaching an *identical*
    /// receipt is an idempotent no-op (no second chain entry); a *distinct*
    /// receipt is appended. Reading: [`Store::receipts`] returns every
    /// derivation's receipt; [`Store::receipt`] returns the first for callers
    /// that still want a single one.
    ///
    /// # Errors
    /// [`StoreError::ReceiptForUnknown`] if `addr` is not in the store;
    /// [`StoreError::ReceiptAddress`] if the receipt cannot be content-addressed.
    pub fn attach_receipt(
        &mut self,
        addr: ContentHash,
        receipt: E2eReceipt,
    ) -> Result<(), StoreError> {
        if !self.nodes.contains_key(&addr) {
            return Err(StoreError::ReceiptForUnknown { addr });
        }
        // Idempotent on an exact duplicate: no second chain entry, no dup in the
        // multiset.
        if self
            .receipts
            .get(&addr)
            .is_some_and(|bucket| bucket.contains(&receipt))
        {
            return Ok(());
        }
        let receipt_hash = content_address(&receipt).map_err(StoreError::ReceiptAddress)?;
        let entry = ReceiptChainEntry {
            node: addr,
            receipt: receipt_hash,
            prev: self.receipt_head,
        };
        self.receipt_head = Some(entry.hash());
        self.receipt_chain.push(entry);
        self.receipts.entry(addr).or_default().push(receipt);
        Ok(())
    }

    /// The first transform receipt attached to `addr`, if any.
    ///
    /// Compatibility accessor for callers that want a single receipt; a
    /// converged node may carry several, so multiset-aware code (e.g. the
    /// provenance/multiset gate) should read [`Store::receipts`] instead.
    #[must_use]
    pub fn receipt(&self, addr: &ContentHash) -> Option<&E2eReceipt> {
        self.receipts.get(addr).and_then(|bucket| bucket.first())
    }

    /// Every transform receipt attached to `addr`, in attach order — the
    /// derivation multiset for a (possibly value-converged) node. Empty slice if
    /// none were attached.
    #[must_use]
    pub fn receipts(&self, addr: &ContentHash) -> &[E2eReceipt] {
        self.receipts.get(addr).map_or(&[], Vec::as_slice)
    }

    /// Recompute the receipt chain and confirm it is intact.
    ///
    /// Each entry commits to the stored node address, the attached receipt's
    /// content hash, and the previous receipt entry. Returns `true` when the
    /// receipt audit chain is untampered.
    #[must_use]
    pub fn verify_receipt_chain(&self) -> bool {
        let mut prev: Option<ContentHash> = None;
        for entry in &self.receipt_chain {
            if entry.prev != prev {
                return false;
            }
            prev = Some(entry.hash());
        }
        prev == self.receipt_head
    }

    /// Confirm every attached receipt is internally consistent with the store:
    /// its key resolves to a stored node and matches the receipt's own
    /// `graph_address`. Catches a receipt swapped onto the wrong address or
    /// left dangling by a corrupted persistence layer.
    ///
    /// Also verifies the receipt audit chain and confirms each current receipt
    /// still hashes to the chain entry recorded at attach time, so forged
    /// receipt contents cannot pass merely by preserving `graph_address`.
    #[must_use]
    pub fn verify_receipts(&self) -> bool {
        let total_receipts: usize = self.receipts.values().map(Vec::len).sum();
        self.verify_receipt_chain()
            && total_receipts == self.receipt_chain.len()
            && self.receipt_chain.iter().all(|entry| {
                self.nodes.contains_key(&entry.node)
                    && self.receipts.get(&entry.node).is_some_and(|bucket| {
                        bucket.iter().any(|receipt| {
                            entry.node.to_hex() == receipt.graph_address.as_str()
                                && content_address(receipt).is_ok_and(|hash| hash == entry.receipt)
                        })
                    })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{ChainEntry, Node, Store, StoreError};
    use attractor_core::{Cell, ContentHash, Frame, Primitive, SourceRef};
    use attractor_proto::{ContentHashHex, E2eReceipt};
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

    // Tamper-evidence negative paths. The chain's guarantee is that any
    // mutation of recorded history is caught by `verify_chain` as long as the
    // externally persisted head is honest — these tests mutate the private
    // fields directly, standing in for a corrupted or hostile persistence
    // layer underneath an honest head.

    /// Three inserts, returning the store with chain entries to tamper with.
    fn tampered_base() -> Store {
        let mut s = Store::new();
        s.insert(cell("a")).unwrap();
        s.insert(cell("b")).unwrap();
        s.insert(cell("c")).unwrap();
        assert!(s.verify_chain(), "base chain must verify before tampering");
        s
    }

    #[test]
    fn tampered_middle_entry_node_is_detected() {
        let mut s = tampered_base();
        s.chain[1].node = ContentHash::of(b"forged");
        assert!(!s.verify_chain(), "rewriting a middle entry's node must fail");
    }

    #[test]
    fn tampered_last_entry_node_is_detected() {
        // The last entry has no successor committing to it; only the head
        // comparison catches this one.
        let mut s = tampered_base();
        s.chain[2].node = ContentHash::of(b"forged");
        assert!(!s.verify_chain(), "rewriting the last entry's node must fail");
    }

    #[test]
    fn tampered_prev_link_is_detected() {
        let mut s = tampered_base();
        s.chain[1].prev = Some(ContentHash::of(b"forged"));
        assert!(!s.verify_chain(), "breaking a prev link must fail");
    }

    #[test]
    fn truncated_chain_is_detected() {
        let mut s = tampered_base();
        s.chain.pop();
        assert!(!s.verify_chain(), "dropping the last entry must fail against the head");
    }

    #[test]
    fn reordered_entries_are_detected() {
        let mut s = tampered_base();
        s.chain.swap(0, 1);
        assert!(!s.verify_chain(), "reordering history must fail");
    }

    #[test]
    fn appended_forgery_with_stale_head_is_detected() {
        // An attacker who can write the chain but not the persisted head:
        // append an entry whose prev correctly commits to the real last entry.
        let mut s = tampered_base();
        let real_last = s.chain.last().unwrap().hash();
        s.chain.push(ChainEntry {
            node: ContentHash::of(b"forged"),
            prev: Some(real_last),
        });
        assert!(!s.verify_chain(), "a well-linked appended entry must still fail the head check");
    }

    #[test]
    fn forged_head_is_detected() {
        let mut s = tampered_base();
        s.head = Some(ContentHash::of(b"forged"));
        assert!(!s.verify_chain(), "a head not matching the last entry must fail");
    }

    #[test]
    fn nonempty_head_over_empty_chain_is_detected() {
        let mut s = Store::new();
        s.head = Some(ContentHash::of(b"forged"));
        assert!(!s.verify_chain(), "a head with no chain behind it must fail");
    }

    #[test]
    fn genesis_marker_tamper_is_detected() {
        // Rewriting the genesis entry to claim a predecessor must fail even
        // when every later link is internally consistent.
        let mut s = tampered_base();
        s.chain[0].prev = Some(ContentHash::of(b"pre-genesis"));
        assert!(!s.verify_chain(), "genesis must have no predecessor");
    }

    // Receipt surface: write-once attach + integrity verification. Receipts
    // have their own audit chain because they are attached after node insert.

    /// A minimal receipt whose graph_address matches `addr`.
    fn receipt_for(addr: ContentHash, transform: &str) -> E2eReceipt {
        E2eReceipt {
            graph_address: ContentHashHex::new(addr.to_hex()).unwrap(),
            source_spans: vec![],
            transform_id: transform.to_owned(),
            confidence: None,
            provenance_refs: vec![],
        }
    }

    #[test]
    fn attach_receipt_rejects_unknown_address() {
        let mut s = Store::new();
        let addr = ContentHash::of(b"never stored");
        let err = s.attach_receipt(addr, receipt_for(addr, "t.v0")).unwrap_err();
        assert!(matches!(err, StoreError::ReceiptForUnknown { .. }));
    }

    #[test]
    fn attach_receipt_is_idempotent_for_identical_receipt() {
        let mut s = Store::new();
        let addr = s.insert(cell("derived")).unwrap();
        s.attach_receipt(addr, receipt_for(addr, "t.v0")).unwrap();
        s.attach_receipt(addr, receipt_for(addr, "t.v0")).unwrap();
        assert_eq!(s.receipt(&addr).unwrap().transform_id, "t.v0");
        assert_eq!(
            s.receipt_chain_len(),
            1,
            "idempotent re-attach must not extend the audit chain"
        );
        assert!(s.receipt_head().is_some());
    }

    #[test]
    fn attach_receipt_accumulates_distinct_receipts_per_address() {
        // Value-only identity (#2249) converges derivations onto one address, so
        // distinct receipts accumulate (multiset) instead of conflicting.
        let mut s = Store::new();
        let addr = s.insert(cell("derived")).unwrap();
        s.attach_receipt(addr, receipt_for(addr, "t.v0")).unwrap();
        s.attach_receipt(addr, receipt_for(addr, "t.v1")).unwrap();

        let kinds: Vec<&str> = s
            .receipts(&addr)
            .iter()
            .map(|r| r.transform_id.as_str())
            .collect();
        assert_eq!(kinds, ["t.v0", "t.v1"], "both derivations recorded");
        assert_eq!(
            s.receipt(&addr).unwrap().transform_id,
            "t.v0",
            "compat accessor returns the first"
        );
        assert_eq!(
            s.receipt_chain_len(),
            2,
            "each distinct receipt is one chain entry"
        );
        assert!(s.verify_receipts(), "multiset receipts verify");
    }

    #[test]
    fn verify_receipts_true_on_honest_store() {
        let mut s = Store::new();
        let a = s.insert(cell("a")).unwrap();
        let b = s.insert(cell("b")).unwrap();
        s.attach_receipt(a, receipt_for(a, "t.v0")).unwrap();
        s.attach_receipt(b, receipt_for(b, "t.v0")).unwrap();
        assert!(s.verify_receipt_chain());
        assert!(s.verify_receipts());
    }

    #[test]
    fn swapped_receipts_are_detected() {
        // A corrupted persistence layer swaps the audit records of two nodes.
        let mut s = Store::new();
        let a = s.insert(cell("a")).unwrap();
        let b = s.insert(cell("b")).unwrap();
        s.attach_receipt(a, receipt_for(a, "t.v0")).unwrap();
        s.attach_receipt(b, receipt_for(b, "t.v0")).unwrap();
        let ra = s.receipts[&a].clone();
        let rb = s.receipts[&b].clone();
        s.receipts.insert(a, rb);
        s.receipts.insert(b, ra);
        assert!(!s.verify_receipts(), "cross-key receipt swap must be caught");
    }

    #[test]
    fn dangling_receipt_is_detected() {
        // Written underneath the API: a receipt keyed by an address that was
        // never stored (attach_receipt would have refused it).
        let mut s = Store::new();
        let a = s.insert(cell("a")).unwrap();
        let ghost = ContentHash::of(b"ghost");
        let r = s.receipts.get(&a).cloned();
        assert!(r.is_none());
        s.receipts.insert(ghost, vec![receipt_for(ghost, "t.v0")]);
        assert!(!s.verify_receipts(), "receipt without a stored node must be caught");
    }

    #[test]
    fn forged_receipt_contents_are_detected() {
        let mut s = Store::new();
        let a = s.insert(cell("a")).unwrap();
        s.attach_receipt(a, receipt_for(a, "t.v0")).unwrap();
        s.receipts.insert(a, vec![receipt_for(a, "forged.v1")]);
        assert!(
            !s.verify_receipts(),
            "receipt content changes must fail even when graph_address still matches"
        );
    }

    #[test]
    fn receipt_chain_tampering_is_detected() {
        let mut s = Store::new();
        let a = s.insert(cell("a")).unwrap();
        let b = s.insert(cell("b")).unwrap();
        s.attach_receipt(a, receipt_for(a, "t.v0")).unwrap();
        s.attach_receipt(b, receipt_for(b, "t.v0")).unwrap();
        s.receipt_chain[1].prev = Some(ContentHash::of(b"forged"));
        assert!(!s.verify_receipt_chain(), "breaking a receipt prev link must fail");
        assert!(!s.verify_receipts());
    }
}
