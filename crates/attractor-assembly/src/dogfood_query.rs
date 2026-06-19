//! #2653 dogfood ingest/query — Milton PR evidence → Secretary-equivalent classification.
//!
//! Replays pinned merge-terminal fixtures (attractor#38 + milton#2827) through the
//! WS-3 assembly path and classifies each PR identity from store receipts only.
//! Query fails closed when provenance is missing (`NoAttractorProvenance`).

use crate::{assemble_notes_idempotent, AssemblyError, AssemblyOutcome, Note};
use attractor_api::dispatch_with_store;
use attractor_proto::{
    ApiRequest, ApiResponse, ContentHashHex, E2eReceipt, GetData, RequestKind,
};
use attractor_projector::{MARKDOWN_FRONTMATTER_ID_PROJECTOR, MARKDOWN_TITLE_PROJECTOR};
use attractor_store::Store;
use std::fmt;

/// Secretary-equivalent PR disposition derived from ingested evidence receipts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrDisposition {
    /// Merge terminal — `state=MERGED` evidence ingested with full provenance.
    TerminalMerged,
    /// Open and blocked on an allocator/host gate.
    Blocked,
    /// Fleet reviewers stamped ready at current head.
    Ready,
    /// No fleet verdict yet; needs review.
    NeedsReview,
}

/// One provenance anchor the classification used (receipt transform + graph address).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvenanceAnchor {
    /// Projector transform id (e.g. markdown title / frontmatter-id).
    pub transform_id: String,
    /// Content-addressed graph node for the receipt.
    pub graph_address: ContentHashHex,
    /// Source origin label from the receipt (must be non-empty for acceptance).
    pub origin: String,
}

/// Query output for a single PR frontmatter identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretaryEquivalentQuery {
    /// Stable frontmatter id (e.g. `milton:pr:2827`).
    pub frontmatter_id: String,
    /// Disposition Secretary would surface for this evidence batch.
    pub disposition: PrDisposition,
    /// Receipt anchors used — never empty on success.
    pub provenance_anchors: Vec<ProvenanceAnchor>,
}

/// Why dogfood ingest/query failed.
#[derive(Debug)]
pub enum DogfoodQueryError {
    /// Assembly or API dispatch failed.
    Assembly(AssemblyError),
    /// Required frontmatter identity was not projected into the store.
    MissingIdentity {
        /// Expected frontmatter id string.
        frontmatter_id: String,
    },
    /// Get returned no provenance refs — fail closed, do not trust raw markdown.
    NoAttractorProvenance {
        /// Frontmatter id that lacked provenance.
        frontmatter_id: String,
    },
    /// Idempotent re-ingest linked new reference edges.
    DuplicateRelationsOnRerun {
        /// New edges linked on second ingest pass.
        relinked: usize,
    },
    /// API returned an unexpected envelope.
    Api(String),
}

impl fmt::Display for DogfoodQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Assembly(e) => write!(f, "dogfood assembly: {e}"),
            Self::MissingIdentity { frontmatter_id } => {
                write!(f, "missing projected identity for {frontmatter_id}")
            }
            Self::NoAttractorProvenance { frontmatter_id } => {
                write!(f, "no_attractor_provenance for {frontmatter_id}")
            }
            Self::DuplicateRelationsOnRerun { relinked } => {
                write!(f, "duplicate relations on idempotent rerun: relinked={relinked}")
            }
            Self::Api(msg) => write!(f, "dogfood api: {msg}"),
        }
    }
}

impl std::error::Error for DogfoodQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Assembly(e) => Some(e),
            _ => None,
        }
    }
}

impl From<AssemblyError> for DogfoodQueryError {
    fn from(value: AssemblyError) -> Self {
        Self::Assembly(value)
    }
}

/// Ingest a dogfood evidence batch twice; second pass must not link new edges.
pub fn ingest_dogfood_idempotent(
    store: &mut Store,
    notes: &[Note],
) -> Result<AssemblyOutcome, DogfoodQueryError> {
    let first = assemble_notes_idempotent(store, notes)?;
    let second = assemble_notes_idempotent(store, notes)?;
    if !second.linked.is_empty() {
        return Err(DogfoodQueryError::DuplicateRelationsOnRerun {
            relinked: second.linked.len(),
        });
    }
    Ok(first)
}

/// Classify one PR identity from ingested receipts + store Get provenance.
pub fn secretary_equivalent_query(
    store: &mut Store,
    frontmatter_id: &str,
    receipts: &[E2eReceipt],
) -> Result<SecretaryEquivalentQuery, DogfoodQueryError> {
    let id_receipt = receipt_for_frontmatter_id(store, receipts, frontmatter_id)?;
    let note_path = id_receipt
        .source_spans
        .first()
        .and_then(|span| span.label.as_deref())
        .map(|label| label.split('#').next().unwrap_or(label).to_owned())
        .ok_or_else(|| DogfoodQueryError::MissingIdentity {
            frontmatter_id: frontmatter_id.to_owned(),
        })?;
    let title_receipt = title_receipt_for_note_path(receipts, &note_path)?;
    let heading = title_receipt
        .source_spans
        .first()
        .and_then(|span| span.label.as_deref())
        .ok_or_else(|| DogfoodQueryError::MissingIdentity {
            frontmatter_id: frontmatter_id.to_owned(),
        })?;
    let disposition = disposition_from_heading(heading);

    let mut provenance_anchors = vec![
        anchor_from_get(store, &id_receipt.graph_address, frontmatter_id)?,
        anchor_from_receipt(&title_receipt, frontmatter_id)?,
    ];
    provenance_anchors.sort_by(|a, b| a.transform_id.cmp(&b.transform_id));
    provenance_anchors.dedup_by(|a, b| a.graph_address == b.graph_address);

    Ok(SecretaryEquivalentQuery {
        frontmatter_id: frontmatter_id.to_owned(),
        disposition,
        provenance_anchors,
    })
}

fn disposition_from_heading(heading: &str) -> PrDisposition {
    if heading.contains("TERMINAL") {
        PrDisposition::TerminalMerged
    } else if heading.contains("BLOCKED") {
        PrDisposition::Blocked
    } else if heading.contains("READY_FOR_GREGORY") || heading.contains("status=READY") {
        PrDisposition::Ready
    } else {
        PrDisposition::NeedsReview
    }
}

fn receipt_for_frontmatter_id<'a>(
    store: &Store,
    receipts: &'a [E2eReceipt],
    frontmatter_id: &str,
) -> Result<&'a E2eReceipt, DogfoodQueryError> {
    use attractor_core::ContentHash;

    receipts
        .iter()
        .filter(|r| r.transform_id == MARKDOWN_FRONTMATTER_ID_PROJECTOR)
        .find(|r| {
            ContentHash::from_hex(r.graph_address.as_str())
                .ok()
                .and_then(|addr| store.get(&addr))
                .and_then(|node| serde_json::to_string(node).ok())
                .is_some_and(|json| json.contains(frontmatter_id))
        })
        .ok_or_else(|| DogfoodQueryError::MissingIdentity {
            frontmatter_id: frontmatter_id.to_owned(),
        })
}

fn title_receipt_for_note_path<'a>(
    receipts: &'a [E2eReceipt],
    note_path: &str,
) -> Result<&'a E2eReceipt, DogfoodQueryError> {
    receipts
        .iter()
        .find(|r| {
            r.transform_id == MARKDOWN_TITLE_PROJECTOR
                && r.source_spans.first().and_then(|s| s.label.as_deref()).is_some_and(
                    |label| label.starts_with(note_path),
                )
        })
        .ok_or_else(|| DogfoodQueryError::MissingIdentity {
            frontmatter_id: note_path.to_owned(),
        })
}

fn anchor_from_receipt(
    receipt: &E2eReceipt,
    frontmatter_id: &str,
) -> Result<ProvenanceAnchor, DogfoodQueryError> {
    let origin = receipt
        .provenance_refs
        .first()
        .map(|r| r.origin.clone())
        .filter(|o| !o.trim().is_empty())
        .ok_or_else(|| DogfoodQueryError::NoAttractorProvenance {
            frontmatter_id: frontmatter_id.to_owned(),
        })?;
    Ok(ProvenanceAnchor {
        transform_id: receipt.transform_id.clone(),
        graph_address: receipt.graph_address.clone(),
        origin,
    })
}

fn anchor_from_get(
    store: &mut Store,
    address: &ContentHashHex,
    frontmatter_id: &str,
) -> Result<ProvenanceAnchor, DogfoodQueryError> {
    match dispatch_with_store(
        &ApiRequest::new(RequestKind::Get {
            address: address.clone(),
        }),
        0,
        store,
    ) {
        ApiResponse::Ok { data, .. } => {
            let get: GetData = serde_json::from_value(data)
                .map_err(|e| DogfoodQueryError::Api(e.to_string()))?;
            if get.provenance_refs.is_empty()
                || get.provenance_refs[0].origin.trim().is_empty()
            {
                return Err(DogfoodQueryError::NoAttractorProvenance {
                    frontmatter_id: frontmatter_id.to_owned(),
                });
            }
            Ok(ProvenanceAnchor {
                transform_id: get.receipt.transform_id,
                graph_address: get.receipt.graph_address,
                origin: get.provenance_refs[0].origin.clone(),
            })
        }
        ApiResponse::Error { message, .. } => Err(DogfoodQueryError::Api(message)),
    }
}

/// Locate the frontmatter-id graph address for `id` from assembly receipts.
pub fn identity_address_for_frontmatter_id(
    store: &Store,
    receipts: &[E2eReceipt],
    frontmatter_id: &str,
) -> Result<ContentHashHex, DogfoodQueryError> {
    Ok(receipt_for_frontmatter_id(store, receipts, frontmatter_id)?.graph_address.clone())
}
