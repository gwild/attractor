//! Portable wire types for the Attractor control plane (#2243).
//!
//! Extracted as a standalone crate (the proto-split blessed in the #2243 dedup
//! thread) so the request/response contract stays **cross-platform attestable**
//! — pure `serde`, no OS/async/socket dependencies. The Linux-only transport
//! (`UnixStream`, TCP listener) lives in `attractor-api`; this crate is the
//! shared contract that both the server and any client depend on.
//!
//! Adapted from stringdriver's `terminal_api` envelope shape (the reusable
//! skeleton — its stepper/audio domain verbs are intentionally *not* carried
//! over). v0 request/response traffic ships only the universal verbs (`Ping`,
//! `Status`); the matrix-op verbs (`Get`/`Insert`/`Link`/`Snapshot`) map onto
//! `attractor-store`'s API and are added there once it lands — so
//! [`RequestKind`] is `#[non_exhaustive]` to admit them without breaking the
//! envelope. The WS-1/WS-3 receipt structs below are passive payload contracts:
//! portable serde shapes that higher crates can return or persist once their
//! handlers land.

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// Schema/evidence projection contract shared by Attractor consumers and
/// external projectors.
pub mod projection;

/// Issue-to-wiki migration projection contract.
pub mod issue_wiki;

pub use issue_wiki::{
    IssueWikiAccepted, IssueWikiMalformed, IssueWikiMutationReference, IssueWikiNoteEvidence,
    IssueWikiProjectionEnvelope, IssueWikiProjectionError, IssueWikiProjectionResult,
    IssueWikiProjectionState, IssueWikiReference, IssueWikiSource, IssueWikiStale,
    IssueWikiStoreMutation, SpanBoundText, ISSUE_WIKI_SCHEMA_ID_V0,
};

pub use projection::{
    ActionSet, ActionSetRef, ClaimFixtureHash, ClaimHash, CrossRepoContract, EvidenceCapability,
    EvidenceEnvelope, FieldRef, FixtureManifestKind, GateMeetResult, GitBlobSha,
    MiltonProjectorRef, ProjectionClaim, ProjectionInvalid, ProjectionOk, ProjectionResult,
    ProjectionSchemaTuple, ProjectionStale, ProjectionState, ProjectionUnsupported,
    ReceiptComparison, ReceiptFixtureHash, ReceiptHash, ReceiptInt, ReceiptStringList,
    ScoreContribution, TypedReceipt, UnmappedGate, UnsupportedRemediation, PROJECTION_SCHEMA_ID_V0,
};

/// Portable content-hash representation on the wire.
///
/// Core owns the concrete `ContentHash` bytes; the protocol layer keeps the
/// wire contract crate-independent by carrying the lowercase hex rendering.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ContentHashHex(String);

impl ContentHashHex {
    /// Build a validated lowercase BLAKE3 content-hash hex string.
    ///
    /// # Errors
    /// Returns [`HashHexError`] when `value` is not exactly 64 lowercase hex
    /// characters.
    pub fn new(value: impl Into<String>) -> Result<Self, HashHexError> {
        let value = value.into();
        if value.len() != 64 {
            return Err(HashHexError::WrongLength { len: value.len() });
        }
        if let Some(bad) = value
            .bytes()
            .find(|&b| !matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(HashHexError::InvalidByte { byte: bad });
        }
        Ok(Self(value))
    }

    /// Borrow the validated lowercase hex digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ContentHashHex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Why a wire content hash was invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashHexError {
    /// Digest was not 64 characters long.
    WrongLength {
        /// The supplied string length.
        len: usize,
    },
    /// Digest included a byte outside `0-9a-f`.
    InvalidByte {
        /// The offending byte.
        byte: u8,
    },
}

impl fmt::Display for HashHexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength { len } => write!(f, "content hash hex length {len} != 64"),
            Self::InvalidByte { byte } => {
                write!(
                    f,
                    "content hash hex contains non-lowercase-hex byte 0x{byte:02x}"
                )
            }
        }
    }
}

impl std::error::Error for HashHexError {}

/// A span of source evidence that contributed to a derived graph receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    /// Content hash of the source payload containing this span.
    pub source_hash: ContentHashHex,
    /// Byte offset where the cited source span begins.
    pub start: u64,
    /// Byte offset where the cited source span ends.
    pub end: u64,
    /// Optional human label for the span, such as a channel or transcript id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A source reference carried by an end-to-end receipt.
///
/// This mirrors the core `SourceRef` shape without depending on `attractor-core`
/// from the portable protocol crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceRef {
    /// Content address of the source payload.
    pub content_hash: ContentHashHex,
    /// Human/origin label; not part of graph identity.
    pub origin: String,
}

/// End-to-end graph receipt for WS-1/WS-3 acceptance rows.
///
/// The receipt proves which graph address was produced, which source spans fed
/// it, which transform produced it, and which provenance refs should be visible
/// through store/API queries. It is a portable serde contract only; store/API
/// persistence and query behavior live in higher crates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct E2eReceipt {
    /// Content address of the resulting graph/root frame.
    pub graph_address: ContentHashHex,
    /// Raw evidence spans that fed this derivation.
    pub source_spans: Vec<SourceSpan>,
    /// Stable transform identifier, for example `audmon.partials.v0`.
    pub transform_id: String,
    /// Optional confidence score from the transform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Provenance refs expected to resolve from the store/API for this graph.
    pub provenance_refs: Vec<ProvenanceRef>,
}

/// Terminal state for a boundary proof over an attached graph receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryState {
    /// The receipt key resolves to a stored graph and matches the receipt.
    Verified,
    /// The receipt is keyed to an address that is not present in the store.
    MissingNode,
    /// The store key and the receipt's claimed graph address diverge.
    AddressMismatch,
}

/// Downstream-consumable proof that a graph receipt stayed on its boundary.
///
/// `boundary_owner` is the store/graph identity being checked. Source hashes
/// and provenance refs are evidence only; they make the proof auditable but do
/// not satisfy the boundary by themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryDisposition {
    /// Stable join key for this proof, normally the store key under inspection.
    pub ref_id: ContentHashHex,
    /// Declared graph/store owner whose boundary is being checked.
    pub boundary_owner: ContentHashHex,
    /// Whether the boundary proof passed.
    pub boundary_ok: bool,
    /// Terminal state for the check.
    pub state: BoundaryState,
    /// Graph address claimed by the receipt being checked.
    pub graph_address: ContentHashHex,
    /// Source hashes carried as integrity evidence, not ownership proof.
    pub source_hashes: Vec<ContentHashHex>,
    /// Provenance refs carried as audit evidence.
    pub provenance_refs: Vec<ProvenanceRef>,
    /// Human-readable diagnostic for failed checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Default TCP port for the control plane — the portable transport.
/// (`SD` → `7533` on a phone keypad, inherited from stringdriver.)
pub const TCP_PORT: u16 = 7533;

/// Default Unix-domain socket path — the Linux/Unix local fast path
/// (server-side only; clients on other platforms use [`Transport::Tcp`]).
pub const SOCKET_PATH: &str = "/tmp/attractor_api.sock";

/// Which transport a client/server is using. `Tcp` is portable and works on
/// every platform; `UnixSocket` is the Linux/Unix local fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    UnixSocket,
    Tcp,
}

/// A control-plane request envelope. `id` lets a client correlate the matching
/// [`ApiResponse`]; `kind` carries the verb.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiRequest {
    /// Optional client-assigned correlation id, echoed back in the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub kind: RequestKind,
}

impl ApiRequest {
    /// Convenience constructor for an un-correlated request.
    pub fn new(kind: RequestKind) -> Self {
        Self { id: None, kind }
    }
}

/// WS-1 store/API freeze: response payload for [`RequestKind::Get`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetData {
    /// Content address of the node.
    pub address: ContentHashHex,
    /// Stored node value (core `Cell` or `Frame` JSON).
    pub node: serde_json::Value,
    /// Provenance multiset for this address.
    pub provenance_refs: Vec<ProvenanceRef>,
    /// Portable receipt for WS-3 acceptance rows (#13 contract).
    pub receipt: E2eReceipt,
}

/// Response payload for [`RequestKind::Insert`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InsertData {
    /// Content address assigned by the store.
    pub address: ContentHashHex,
}

/// Response payload for [`RequestKind::Link`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkData {
    /// Edge source address.
    pub from: ContentHashHex,
    /// Edge destination address.
    pub to: ContentHashHex,
    /// Typed edge label.
    pub kind: String,
}

/// Response payload for [`RequestKind::Relations`]: the edges incident to the
/// queried node, outgoing then incoming. Each edge reuses the [`LinkData`]
/// `{from, to, kind}` shape; direction is read off `from`/`to` relative to the
/// queried address.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationsData {
    /// Incident edges (outgoing first, then incoming).
    pub edges: Vec<LinkData>,
}

/// Response payload for [`RequestKind::Snapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotData {
    /// Distinct nodes stored.
    pub node_count: usize,
    /// Provenance chain length.
    pub chain_len: usize,
    /// Chain head digest, if any insertions occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<ContentHashHex>,
    /// Whether [`Store::verify_chain`] would pass.
    pub chain_intact: bool,
}

/// Request verbs. v0 ships only the universal verbs; domain/matrix ops are
/// added by the `attractor-api`/`attractor-store` owners — hence
/// `#[non_exhaustive]`, so adding a variant is not a breaking change here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RequestKind {
    /// Connectivity/liveness check. Server replies `Ok` with empty data.
    Ping,
    /// Service status snapshot. Payload shape is defined by the server.
    Status,
    /// Fetch a stored node and its provenance receipt by content address.
    Get {
        /// Node content address to load.
        address: ContentHashHex,
    },
    /// Insert a core node (`Cell` or `Frame` JSON) into the store.
    Insert {
        /// Serialized core node (`Cell` or tagged frame object).
        node: serde_json::Value,
    },
    /// Add a typed edge between two stored nodes.
    Link {
        /// Edge source address.
        from: ContentHashHex,
        /// Edge destination address.
        to: ContentHashHex,
        /// Edge kind label.
        kind: String,
    },
    /// Store integrity snapshot (counts + chain head).
    Snapshot,
    /// Fetch the edges incident to a node — the relation-graph read/traversal
    /// verb a recall query walks. Returns outgoing then incoming edges.
    Relations {
        /// Node content address whose incident edges to read.
        address: ContentHashHex,
    },
}

/// A control-plane response envelope, serde-tagged on `status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApiResponse {
    Ok {
        /// Echoes the request's correlation id when one was supplied.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        /// Arbitrary JSON payload (the verb decides its shape).
        data: serde_json::Value,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        message: String,
    },
}

impl ApiResponse {
    /// Build an `Ok` response from any serializable payload.
    pub fn ok(data: impl Serialize) -> Self {
        let data = serde_json::to_value(data).expect("ApiResponse payload must serialize");
        ApiResponse::Ok { id: None, data }
    }

    /// Build an `Ok` response carrying no payload.
    pub fn ok_empty() -> Self {
        ApiResponse::Ok {
            id: None,
            data: serde_json::Value::Null,
        }
    }

    /// Build an `Error` response.
    pub fn error(message: impl Into<String>) -> Self {
        ApiResponse::Error {
            id: None,
            message: message.into(),
        }
    }

    /// Attach/override the correlation id (so a server can echo the request's).
    pub fn with_id(mut self, new_id: u64) -> Self {
        match &mut self {
            ApiResponse::Ok { id, .. } | ApiResponse::Error { id, .. } => *id = Some(new_id),
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u8) -> ContentHashHex {
        ContentHashHex::new(format!("{seed:064x}")).unwrap()
    }

    #[test]
    fn request_round_trips() {
        let req = ApiRequest::new(RequestKind::Ping);
        let json = serde_json::to_string(&req).unwrap();
        let back: ApiRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn request_id_omitted_when_absent_present_when_set() {
        assert_eq!(
            serde_json::to_string(&ApiRequest::new(RequestKind::Status)).unwrap(),
            r#"{"kind":{"op":"status"}}"#
        );
        let with = ApiRequest {
            id: Some(9),
            kind: RequestKind::Ping,
        };
        assert_eq!(
            serde_json::to_string(&with).unwrap(),
            r#"{"id":9,"kind":{"op":"ping"}}"#
        );
    }

    #[test]
    fn verbs_tagged_by_op() {
        assert_eq!(
            serde_json::to_string(&RequestKind::Ping).unwrap(),
            r#"{"op":"ping"}"#
        );
        assert_eq!(
            serde_json::to_string(&RequestKind::Status).unwrap(),
            r#"{"op":"status"}"#
        );
        assert_eq!(
            serde_json::to_string(&RequestKind::Get { address: hash(1) }).unwrap(),
            r#"{"op":"get","address":"0000000000000000000000000000000000000000000000000000000000000001"}"#
        );
        assert_eq!(
            serde_json::to_string(&RequestKind::Snapshot).unwrap(),
            r#"{"op":"snapshot"}"#
        );
    }

    #[test]
    fn ok_wraps_payload() {
        match ApiResponse::ok(serde_json::json!({"v": 1})) {
            ApiResponse::Ok { data, .. } => assert_eq!(data, serde_json::json!({"v": 1})),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn error_carries_message() {
        match ApiResponse::error("nope") {
            ApiResponse::Error { message, .. } => assert_eq!(message, "nope"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn response_with_id_echoes() {
        let r = ApiResponse::ok_empty().with_id(42);
        match r {
            ApiResponse::Ok { id, .. } => assert_eq!(id, Some(42)),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn response_round_trips() {
        for r in [ApiResponse::ok_empty(), ApiResponse::error("x").with_id(3)] {
            let json = serde_json::to_string(&r).unwrap();
            let back: ApiResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(r, back);
        }
    }

    #[test]
    fn e2e_receipt_round_trips() {
        let receipt = E2eReceipt {
            graph_address: hash(1),
            source_spans: vec![SourceSpan {
                source_hash: hash(2),
                start: 10,
                end: 42,
                label: Some("#attractor".to_owned()),
            }],
            transform_id: "audmon.partials.v0".to_owned(),
            confidence: Some(0.875),
            provenance_refs: vec![ProvenanceRef {
                content_hash: hash(2),
                origin: "#attractor".to_owned(),
            }],
        };
        let json = serde_json::to_string(&receipt).unwrap();
        let back: E2eReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(receipt, back);
    }

    #[test]
    fn optional_receipt_fields_are_omitted_when_absent() {
        let receipt = E2eReceipt {
            graph_address: hash(1),
            source_spans: vec![SourceSpan {
                source_hash: hash(2),
                start: 0,
                end: 4,
                label: None,
            }],
            transform_id: "projector.v0".to_owned(),
            confidence: None,
            provenance_refs: Vec::new(),
        };
        let json = serde_json::to_value(&receipt).unwrap();
        assert!(json.get("confidence").is_none());
        assert!(json["source_spans"][0].get("label").is_none());
    }

    #[test]
    fn boundary_disposition_round_trips_with_evidence_fields() {
        let disposition = BoundaryDisposition {
            ref_id: hash(1),
            boundary_owner: hash(1),
            boundary_ok: false,
            state: BoundaryState::AddressMismatch,
            graph_address: hash(2),
            source_hashes: vec![hash(3)],
            provenance_refs: vec![ProvenanceRef {
                content_hash: hash(3),
                origin: "repo:milton".to_owned(),
            }],
            message: Some("receipt graph_address does not match store key".to_owned()),
        };
        let json = serde_json::to_string(&disposition).unwrap();
        assert!(json.contains(r#""state":"address_mismatch""#));
        let back: BoundaryDisposition = serde_json::from_str(&json).unwrap();
        assert_eq!(disposition, back);
    }

    #[test]
    fn content_hash_hex_rejects_bad_wire_values() {
        assert!(matches!(
            ContentHashHex::new("abc"),
            Err(HashHexError::WrongLength { len: 3 })
        ));
        let bad = format!("{}G", "0".repeat(63));
        assert!(matches!(
            ContentHashHex::new(bad),
            Err(HashHexError::InvalidByte { byte: b'G' })
        ));
    }

    #[test]
    fn content_hash_hex_deserialization_validates() {
        let bad = serde_json::from_str::<ContentHashHex>(r#""ABC""#);
        assert!(bad.is_err());
        let good =
            serde_json::from_str::<ContentHashHex>(&format!(r#""{}""#, "a".repeat(64))).unwrap();
        assert_eq!(good.as_str(), "a".repeat(64));
    }
}
