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
use std::path::Path;

use attractor_core::{content_address, AddressError, Cell, ContentHash, Edge, Frame, SourceRef};
use attractor_proto::E2eReceipt;
use petgraph::algo::is_cyclic_directed;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use petgraph::Direction;
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

/// Connection-derived heat for one graph node.
///
/// Heat is a read model, not stored mutable state: `degree / max_degree` over
/// the current relation graph, counting incoming and outgoing edges. An
/// isolated node has heat `0.0`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeHeat {
    /// Node address whose connection heat was measured.
    pub address: ContentHash,
    /// Incident edge count for this node.
    pub degree: usize,
    /// Maximum incident edge count among all nodes in the store.
    pub max_degree: usize,
    /// Normalized heat in `[0.0, 1.0]`.
    pub heat: f64,
}

/// An incident edge plus connection-derived endpoint weights.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeightedRelation {
    /// The relation edge.
    pub edge: Edge,
    /// Heat for the edge source node.
    pub from_heat: NodeHeat,
    /// Heat for the edge target node.
    pub to_heat: NodeHeat,
    /// Edge weight derived from its endpoint heat.
    pub weight: f64,
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

#[derive(Debug, Serialize, Deserialize)]
struct StoreSnapshot {
    version: u32,
    nodes: Vec<Node>,
    provenance: Vec<(ContentHash, Vec<SourceRef>)>,
    edges: Vec<Edge>,
    receipts: Vec<(ContentHash, E2eReceipt)>,
}

impl Store {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load a durable store snapshot from `path`, returning an empty store when
    /// the file does not exist.
    ///
    /// # Errors
    /// Returns an I/O error for unreadable files, malformed snapshots, or
    /// snapshots that cannot be replayed into a valid store.
    pub fn load_snapshot(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        match std::fs::read(path) {
            Ok(bytes) => {
                let snapshot: StoreSnapshot =
                    serde_json::from_slice(&bytes).map_err(invalid_data)?;
                Self::from_snapshot(snapshot)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(e),
        }
    }

    /// Atomically save a durable store snapshot to `path`.
    ///
    /// # Errors
    /// Returns an I/O error if the parent directory cannot be created, the
    /// snapshot cannot be serialized, or the write/rename fails.
    pub fn save_snapshot(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&self.to_snapshot()).map_err(invalid_data)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(tmp, path)
    }

    fn to_snapshot(&self) -> StoreSnapshot {
        let nodes = self
            .chain
            .iter()
            .filter_map(|entry| self.nodes.get(&entry.node).cloned())
            .collect();
        let mut provenance: Vec<(ContentHash, Vec<SourceRef>)> = self
            .provenance
            .iter()
            .map(|(addr, sources)| (*addr, sources.clone()))
            .collect();
        provenance.sort_by_key(|(addr, _)| addr.to_hex());
        let mut edges: Vec<Edge> = self
            .graph
            .edge_references()
            .map(|e| Edge {
                from: self.graph[e.source()],
                to: self.graph[e.target()],
                kind: e.weight().clone(),
            })
            .collect();
        edges.sort_by(|a, b| {
            (a.from.to_hex(), a.to.to_hex(), &a.kind).cmp(&(
                b.from.to_hex(),
                b.to.to_hex(),
                &b.kind,
            ))
        });
        let receipts = self
            .receipt_chain
            .iter()
            .filter_map(|entry| {
                self.receipts.get(&entry.node).and_then(|bucket| {
                    bucket
                        .iter()
                        .find(|receipt| {
                            content_address(*receipt).is_ok_and(|hash| hash == entry.receipt)
                        })
                        .cloned()
                        .map(|receipt| (entry.node, receipt))
                })
            })
            .collect();
        StoreSnapshot {
            version: 1,
            nodes,
            provenance,
            edges,
            receipts,
        }
    }

    fn from_snapshot(snapshot: StoreSnapshot) -> std::io::Result<Self> {
        if snapshot.version != 1 {
            return Err(invalid_data(format!(
                "unsupported store snapshot version {}",
                snapshot.version
            )));
        }
        let mut store = Self::new();
        for node in snapshot.nodes {
            store.insert(node).map_err(invalid_data)?;
        }
        store.provenance = snapshot.provenance.into_iter().collect();
        for edge in snapshot.edges {
            store
                .link(edge.from, edge.to, edge.kind)
                .map_err(invalid_data)?;
        }
        for (addr, receipt) in snapshot.receipts {
            store.attach_receipt(addr, receipt).map_err(invalid_data)?;
        }
        Ok(store)
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
    /// Returns the [`Edge`] that was recorded. Re-linking the same `from`,
    /// `to`, and `kind` is idempotent: the existing logical edge is returned
    /// without adding a duplicate graph edge.
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
        if self
            .graph
            .edges_directed(from_idx, Direction::Outgoing)
            .any(|edge| edge.target() == to_idx && edge.weight() == &kind)
        {
            return Ok(Edge { from, to, kind });
        }

        let edge_idx = self.graph.add_edge(from_idx, to_idx, kind.clone());
        if is_cyclic_directed(&self.graph) {
            self.graph.remove_edge(edge_idx);
            return Err(StoreError::WouldCycle { from, to });
        }
        Ok(Edge { from, to, kind })
    }

    /// Every edge incident to `addr` — the relation-graph read companion to
    /// [`Store::link`], and the traversal primitive a recall query walks.
    ///
    /// Returns outgoing edges (`from == addr`) followed by incoming edges
    /// (`to == addr`); the caller filters by `kind` (e.g. `references` vs
    /// `derived_from`) and reads direction off `from`/`to`. An address not in the
    /// store has no relations and yields an empty vec (not an error) — recall
    /// over a miss is empty, not a failure.
    #[must_use]
    pub fn relations(&self, addr: &ContentHash) -> Vec<Edge> {
        let Some(&idx) = self.index.get(addr) else {
            return Vec::new();
        };
        let mut edges = Vec::new();
        for e in self.graph.edges_directed(idx, Direction::Outgoing) {
            edges.push(Edge {
                from: *addr,
                to: self.graph[e.target()],
                kind: e.weight().clone(),
            });
        }
        for e in self.graph.edges_directed(idx, Direction::Incoming) {
            edges.push(Edge {
                from: self.graph[e.source()],
                to: *addr,
                kind: e.weight().clone(),
            });
        }
        edges
    }

    /// Connection-derived heat for a stored node.
    ///
    /// Returns `None` when `addr` is not in the store. Heat is computed from
    /// graph structure each time, so optimization consumers observe the same
    /// tunable substrate as the relation graph without a parallel weight table.
    #[must_use]
    pub fn node_heat(&self, addr: &ContentHash) -> Option<NodeHeat> {
        let idx = *self.index.get(addr)?;
        let max_degree = self.max_degree();
        let degree = self.degree(idx);
        Some(NodeHeat {
            address: *addr,
            degree,
            max_degree,
            heat: normalize_heat(degree, max_degree),
        })
    }

    /// Incident relations annotated with endpoint heat and derived edge weight.
    ///
    /// The edge weight is the mean of the source and target node heats. Callers
    /// that care about direction can still read `from_heat` and `to_heat`
    /// separately; the scalar is a stable ranking convenience for convergence
    /// and classifier scaffolding.
    #[must_use]
    pub fn weighted_relations(&self, addr: &ContentHash) -> Vec<WeightedRelation> {
        let Some(&idx) = self.index.get(addr) else {
            return Vec::new();
        };
        let max_degree = self.max_degree();
        let mut edges = Vec::new();
        for e in self.graph.edges_directed(idx, Direction::Outgoing) {
            let from_heat = self.heat_for_index(idx, max_degree);
            let to_idx = e.target();
            let to_heat = self.heat_for_index(to_idx, max_degree);
            edges.push(WeightedRelation {
                edge: Edge {
                    from: *addr,
                    to: self.graph[to_idx],
                    kind: e.weight().clone(),
                },
                weight: mean_heat(&from_heat, &to_heat),
                from_heat,
                to_heat,
            });
        }
        for e in self.graph.edges_directed(idx, Direction::Incoming) {
            let from_idx = e.source();
            let from_heat = self.heat_for_index(from_idx, max_degree);
            let to_heat = self.heat_for_index(idx, max_degree);
            edges.push(WeightedRelation {
                edge: Edge {
                    from: self.graph[from_idx],
                    to: *addr,
                    kind: e.weight().clone(),
                },
                weight: mean_heat(&from_heat, &to_heat),
                from_heat,
                to_heat,
            });
        }
        edges
    }

    fn degree(&self, idx: NodeIndex) -> usize {
        self.graph.edges_directed(idx, Direction::Incoming).count()
            + self.graph.edges_directed(idx, Direction::Outgoing).count()
    }

    fn max_degree(&self) -> usize {
        match self
            .graph
            .node_indices()
            .map(|idx| self.degree(idx))
            .max()
        {
            Some(max) => max,
            None => 0,
        }
    }

    fn heat_for_index(&self, idx: NodeIndex, max_degree: usize) -> NodeHeat {
        let degree = self.degree(idx);
        NodeHeat {
            address: self.graph[idx],
            degree,
            max_degree,
            heat: normalize_heat(degree, max_degree),
        }
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

fn normalize_heat(degree: usize, max_degree: usize) -> f64 {
    if max_degree == 0 {
        0.0
    } else {
        degree as f64 / max_degree as f64
    }
}

fn mean_heat(from: &NodeHeat, to: &NodeHeat) -> f64 {
    (from.heat + to.heat) / 2.0
}

fn invalid_data(e: impl fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
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
        assert_eq!(
            Node::Cell(c.clone()).address().unwrap(),
            c.address().unwrap()
        );
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
    fn link_dedupes_identical_edge() {
        let mut s = Store::new();
        let a = s.insert(frame("from")).unwrap();
        let b = s.insert(cell("to")).unwrap();

        let first = s.link(a, b, "contains").unwrap();
        let second = s.link(a, b, "contains").unwrap();

        assert_eq!(first, second);
        assert_eq!(s.relations(&a).len(), 1);
        assert_eq!(s.relations(&b).len(), 1);
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
    fn relations_returns_incident_edges_both_directions() {
        // a -> b (refs), c -> b (refs): b has one outgoing (none) and two incoming.
        let mut s = Store::new();
        let a = s.insert(cell("a")).unwrap();
        let b = s.insert(cell("b")).unwrap();
        let c = s.insert(cell("c")).unwrap();
        s.link(a, b, "references").unwrap();
        s.link(c, b, "references").unwrap();

        // From a: one outgoing edge to b, no incoming.
        let a_rel = s.relations(&a);
        assert_eq!(a_rel.len(), 1);
        assert_eq!(a_rel[0].from, a);
        assert_eq!(a_rel[0].to, b);
        assert_eq!(a_rel[0].kind, "references");

        // From b: two incoming (from a and c), no outgoing.
        let b_rel = s.relations(&b);
        assert_eq!(b_rel.len(), 2);
        assert!(b_rel.iter().all(|e| e.to == b && e.from != b));
        let mut sources: Vec<ContentHash> = b_rel.iter().map(|e| e.from).collect();
        sources.sort_by_key(ContentHash::to_hex);
        let mut want = vec![a, c];
        want.sort_by_key(ContentHash::to_hex);
        assert_eq!(sources, want);
    }

    #[test]
    fn relations_of_unknown_address_is_empty_not_error() {
        let s = Store::new();
        assert!(s.relations(&ContentHash::of(b"never stored")).is_empty());
    }

    #[test]
    fn weighted_relations_use_connection_heat() {
        let mut s = Store::new();
        let a = s.insert(cell("a")).unwrap();
        let b = s.insert(cell("b")).unwrap();
        let c = s.insert(cell("c")).unwrap();
        s.link(a, b, "defines").unwrap();
        s.link(c, b, "contradicts").unwrap();

        let b_heat = s.node_heat(&b).expect("stored node has heat");
        assert_eq!(b_heat.degree, 2);
        assert_eq!(b_heat.max_degree, 2);
        assert_eq!(b_heat.heat, 1.0);

        let weighted = s.weighted_relations(&b);
        assert_eq!(weighted.len(), 2);
        assert!(weighted.iter().all(|rel| rel.to_heat.address == b));
        assert!(weighted.iter().all(|rel| rel.to_heat.heat == 1.0));
        assert!(weighted.iter().all(|rel| rel.from_heat.heat == 0.5));
        assert!(weighted.iter().all(|rel| rel.weight == 0.75));
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
        let origins: Vec<&str> = s
            .provenance(&a)
            .iter()
            .map(|sr| sr.origin.as_str())
            .collect();
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

    #[test]
    fn snapshot_round_trips_nodes_edges_provenance_and_receipts() {
        let dir =
            std::env::temp_dir().join(format!("attractor-store-snapshot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");

        let mut s = Store::new();
        let a = s.insert(cell_from("fact", "#general")).unwrap();
        s.insert(cell_from("fact", "#issues")).unwrap();
        let b = s.insert(cell("target")).unwrap();
        s.link(a, b, "references").unwrap();
        s.attach_receipt(a, receipt_for(a, "persist.v0")).unwrap();

        s.save_snapshot(&path).unwrap();
        let loaded = Store::load_snapshot(&path).unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.chain_len(), 2);
        assert!(loaded.verify_chain());
        assert_eq!(loaded.get(&a), s.get(&a));
        assert_eq!(loaded.provenance(&a).len(), 2);
        assert_eq!(loaded.relations(&a).len(), 1);
        assert_eq!(loaded.receipts(&a).len(), 1);
        assert!(loaded.verify_receipts());

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(dir);
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
        assert!(
            !s.verify_chain(),
            "rewriting a middle entry's node must fail"
        );
    }

    #[test]
    fn tampered_last_entry_node_is_detected() {
        // The last entry has no successor committing to it; only the head
        // comparison catches this one.
        let mut s = tampered_base();
        s.chain[2].node = ContentHash::of(b"forged");
        assert!(
            !s.verify_chain(),
            "rewriting the last entry's node must fail"
        );
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
        assert!(
            !s.verify_chain(),
            "dropping the last entry must fail against the head"
        );
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
        assert!(
            !s.verify_chain(),
            "a well-linked appended entry must still fail the head check"
        );
    }

    #[test]
    fn forged_head_is_detected() {
        let mut s = tampered_base();
        s.head = Some(ContentHash::of(b"forged"));
        assert!(
            !s.verify_chain(),
            "a head not matching the last entry must fail"
        );
    }

    #[test]
    fn nonempty_head_over_empty_chain_is_detected() {
        let mut s = Store::new();
        s.head = Some(ContentHash::of(b"forged"));
        assert!(
            !s.verify_chain(),
            "a head with no chain behind it must fail"
        );
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
        let err = s
            .attach_receipt(addr, receipt_for(addr, "t.v0"))
            .unwrap_err();
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
        assert!(
            !s.verify_receipts(),
            "cross-key receipt swap must be caught"
        );
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
        assert!(
            !s.verify_receipts(),
            "receipt without a stored node must be caught"
        );
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
        assert!(
            !s.verify_receipt_chain(),
            "breaking a receipt prev link must fail"
        );
        assert!(!s.verify_receipts());
    }
}
