//! Portable projection/evidence-row contract for Layer 1.
//!
//! This module is the shared wire spine for proof-fidelity rows and future
//! overlap resolution. It deliberately lives in `attractor-proto`: pure serde,
//! no parser implementation, and no Rust-only store dependency. Milton's
//! projector and Attractor's resolver can both conform to this schema via the
//! same serialized fixtures instead of mirroring private types.

use crate::ContentHashHex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Current projection schema identifier for the Layer-1 proof-fidelity spine.
pub const PROJECTION_SCHEMA_ID_V0: &str = "attractor.projection.layer1.v0";

/// Whether a visible evidence row contributes to candidate scoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreContribution {
    /// The row can corroborate a candidate.
    Positive,
    /// The row can falsify or down-rank a candidate.
    Negative,
    /// The row is visible/routable but must not affect candidate score.
    Neutral,
}

/// Evidence capability carried explicitly on every projection row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceCapability {
    /// Whether routing/resolution consumers should see the row.
    pub visible_for_resolution: bool,
    /// Whether proof satisfaction APIs may use the row as positive evidence.
    pub usable_as_positive_evidence: bool,
    /// Candidate-score contribution for aggregators.
    pub score: ScoreContribution,
}

impl EvidenceCapability {
    /// Capability for rows that can corroborate an overlap/proof.
    #[must_use]
    pub const fn positive() -> Self {
        Self {
            visible_for_resolution: true,
            usable_as_positive_evidence: true,
            score: ScoreContribution::Positive,
        }
    }

    /// Capability for rows that are visible but not evidence either way.
    #[must_use]
    pub const fn neutral() -> Self {
        Self {
            visible_for_resolution: true,
            usable_as_positive_evidence: false,
            score: ScoreContribution::Neutral,
        }
    }
}

/// Source row or artifact the projection was derived from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionSourceRef {
    /// Repository or artifact namespace, for example `gwild/milton`.
    pub repo: String,
    /// Stable source artifact ref, for example `pull/3031/body`.
    pub artifact_ref: String,
    /// Content hash of the raw source artifact.
    pub raw_hash: ContentHashHex,
}

/// Declared proof claim that a receipt is meant to satisfy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofClaim {
    /// Command or probe that defines the claim.
    pub cmd: String,
    /// Repository SHA the command is bound to.
    pub repo_sha: String,
    /// Platform/host leg for the claim.
    pub platform: String,
    /// Load-bearing fields the receipt must answer.
    pub expected_fields: Vec<String>,
}

/// Parsed receipt for a proof claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofReceipt {
    /// Claim hash this receipt satisfies.
    pub satisfies_claim_hash: ContentHashHex,
    /// Canonicalized fields parsed from the receipt.
    pub fields: BTreeMap<String, ReceiptFieldValue>,
}

/// Field value parsed from a proof receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReceiptFieldValue {
    /// Known parsed value.
    Known {
        /// Canonical string value.
        value: String,
    },
    /// Addressable unknown value; unknowns never compare equal for clearance.
    Unknown {
        /// Machine-readable reason, for routing through a remediation table.
        reason: String,
        /// Exact field reference that could not be parsed.
        field_ref: String,
    },
}

/// Field mismatch between a stored claim and a replayed receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptMismatch {
    /// Fields that mismatched and therefore define the replay scope.
    pub mismatch_fields: Vec<String>,
    /// Stored receipt hash seen before replay.
    pub stored_receipt_hash: ContentHashHex,
    /// Re-executed receipt hash at the claim point.
    pub reexec_receipt_hash: ContentHashHex,
}

/// Projection row that downstream matchers must handle exhaustively.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "projection_state", rename_all = "snake_case")]
pub enum ProjectionRow {
    /// Claim and receipt projected cleanly and can be positive evidence.
    #[serde(rename = "projection_ok")]
    Ok(ProjectionOk),
    /// Source row was malformed or unprojectable.
    #[serde(rename = "projection_invalid")]
    Invalid(ProjectionInvalid),
    /// Claim projected, but the stored receipt does not satisfy it.
    #[serde(rename = "projection_stale")]
    Stale(ProjectionStale),
    /// Consumer cannot read a newer schema tuple yet.
    #[serde(rename = "projection_schema_unsupported")]
    SchemaUnsupported(ProjectionSchemaUnsupported),
}

impl ProjectionRow {
    /// Evidence capability declared by this row's state.
    #[must_use]
    pub fn expected_capability(&self) -> EvidenceCapability {
        match self {
            Self::Ok(_) => EvidenceCapability::positive(),
            Self::Invalid(_) | Self::Stale(_) | Self::SchemaUnsupported(_) => {
                EvidenceCapability::neutral()
            }
        }
    }

    /// Validate that the serialized row did not forge its capability.
    ///
    /// # Errors
    /// Returns [`ProjectionContractError::CapabilityMismatch`] when the row's
    /// explicit capability does not match its projection state.
    pub fn validate_capability(&self) -> Result<(), ProjectionContractError> {
        let actual = match self {
            Self::Ok(row) => row.evidence_capability,
            Self::Invalid(row) => row.evidence_capability,
            Self::Stale(row) => row.evidence_capability,
            Self::SchemaUnsupported(row) => row.evidence_capability,
        };
        let expected = self.expected_capability();
        if actual == expected {
            Ok(())
        } else {
            Err(ProjectionContractError::CapabilityMismatch { expected, actual })
        }
    }
}

/// Successful proof projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionOk {
    /// Schema identifier for this projection row.
    pub schema_id: String,
    /// Source artifact projected.
    pub source_ref: ProjectionSourceRef,
    /// Explicit evidence capability.
    pub evidence_capability: EvidenceCapability,
    /// Declared claim.
    pub claim: ProofClaim,
    /// Receipt satisfying the claim.
    pub receipt: ProofReceipt,
}

/// Malformed or unprojectable proof row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionInvalid {
    /// Schema identifier for this projection row.
    pub schema_id: String,
    /// Source artifact projected.
    pub source_ref: ProjectionSourceRef,
    /// Explicit evidence capability.
    pub evidence_capability: EvidenceCapability,
    /// Machine-readable invalid reason.
    pub reason: String,
    /// Exact fields that made the row invalid.
    pub field_refs: Vec<String>,
}

/// Stale proof-fidelity projection requiring replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionStale {
    /// Schema identifier for this projection row.
    pub schema_id: String,
    /// Source artifact projected.
    pub source_ref: ProjectionSourceRef,
    /// Explicit evidence capability.
    pub evidence_capability: EvidenceCapability,
    /// Declared claim that is still current.
    pub claim: ProofClaim,
    /// Stored receipt that failed comparison.
    pub stored_receipt: ProofReceipt,
    /// Replay request id for this exact mismatch scope.
    pub reexec_request_id: ContentHashHex,
    /// Mismatch details and preregistered replay scope.
    pub mismatch: ReceiptMismatch,
}

/// Newer projection schema unsupported by this consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionSchemaUnsupported {
    /// Schema identifier for this projection row.
    pub schema_id: String,
    /// Source artifact projected.
    pub source_ref: ProjectionSourceRef,
    /// Explicit evidence capability.
    pub evidence_capability: EvidenceCapability,
    /// Tuple stored on the row that this consumer cannot read.
    pub stored_tuple: ProjectionSchemaTuple,
    /// Consumer that could not read the stored tuple.
    pub consumer_id: String,
    /// Content-addressed capability snapshot for this consumer.
    pub supported_tuples_hash: ContentHashHex,
    /// Bounded remediation kind.
    pub remediation: ProjectionUnsupportedRemediation,
}

/// Schema tuple identifying a projected row's producer contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionSchemaTuple {
    /// Projection schema id.
    pub schema_id: String,
    /// Digest of the projector implementation or schema artifact.
    pub projector_digest: ContentHashHex,
    /// Digest of the fixture set defining this schema.
    pub fixture_set_digest: ContentHashHex,
}

/// Remediation for an unsupported projection schema tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionUnsupportedRemediation {
    /// Add support for the stored tuple.
    AddSupport,
    /// Migrate the projection through a declared migration path.
    MigrateProjection,
    /// Resolver could not classify the remediation yet.
    RemediationUnclassified,
}

/// Cross-repo Layer-1 contract block carried by an attractor PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer1ContractBlock {
    /// Cross-repo tracking issue, for example `gwild/milton#3030`.
    pub tracked_by: String,
    /// Authoritative identity of the schema/golden-corpus artifact.
    pub schema_artifact_digest: ContentHashHex,
    /// Pinned Milton projector provenance used to reproduce the digest.
    pub milton_projector_ref: ProjectorRef,
    /// Golden fixture that proves stale rows reject instead of clear.
    pub negative_stale_row_fixture: String,
}

/// Pinned source artifact reference for a projector implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectorRef {
    /// Repository identity, for example `gwild/milton`.
    pub repo: String,
    /// Commit SHA in `repo`.
    pub sha: String,
    /// Projector path inside the repository at `sha`.
    pub path: String,
    /// Hash algorithm used for [`Self::blob_hash`].
    pub blob_hash_algorithm: String,
    /// VCS blob hash of `path` at `sha`.
    pub blob_hash: String,
}

/// Why a projection contract row failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionContractError {
    /// Serialized row capability did not match its projection state.
    CapabilityMismatch {
        /// Capability required by the row state.
        expected: EvidenceCapability,
        /// Capability actually carried by the row.
        actual: EvidenceCapability,
    },
}

impl std::fmt::Display for ProjectionContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapabilityMismatch { expected, actual } => write!(
                f,
                "projection row capability mismatch: expected {expected:?}, got {actual:?}"
            ),
        }
    }
}

impl std::error::Error for ProjectionContractError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> &'static str {
        match name {
            "projection_ok" => include_str!("../../../fixtures/projection/projection_ok.json"),
            "projection_invalid" => {
                include_str!("../../../fixtures/projection/projection_invalid.json")
            }
            "projection_stale_restamped" => {
                include_str!("../../../fixtures/projection/projection_stale_restamped.json")
            }
            "projection_schema_unsupported" => {
                include_str!("../../../fixtures/projection/projection_schema_unsupported.json")
            }
            "layer1_contract" => include_str!("../../../fixtures/projection/layer1_contract.json"),
            other => panic!("unknown fixture {other}"),
        }
    }

    #[test]
    fn golden_projection_rows_round_trip_and_validate_capability() {
        for name in [
            "projection_ok",
            "projection_invalid",
            "projection_stale_restamped",
            "projection_schema_unsupported",
        ] {
            let row: ProjectionRow = serde_json::from_str(fixture(name)).unwrap();
            row.validate_capability().unwrap();
            let encoded = serde_json::to_string_pretty(&row).unwrap();
            let decoded: ProjectionRow = serde_json::from_str(&encoded).unwrap();
            assert_eq!(row, decoded);
        }
    }

    #[test]
    fn stale_fixture_is_neutral_and_not_positive_evidence() {
        let row: ProjectionRow =
            serde_json::from_str(fixture("projection_stale_restamped")).unwrap();
        let ProjectionRow::Stale(stale) = row else {
            panic!("expected stale projection row");
        };
        assert_eq!(stale.evidence_capability, EvidenceCapability::neutral());
        assert_eq!(stale.mismatch.mismatch_fields, vec!["output".to_owned()]);
    }

    #[test]
    fn unsupported_schema_fixture_is_visible_neutral() {
        let row: ProjectionRow =
            serde_json::from_str(fixture("projection_schema_unsupported")).unwrap();
        let ProjectionRow::SchemaUnsupported(unsupported) = row else {
            panic!("expected unsupported projection row");
        };
        assert_eq!(
            unsupported.evidence_capability,
            EvidenceCapability {
                visible_for_resolution: true,
                usable_as_positive_evidence: false,
                score: ScoreContribution::Neutral,
            }
        );
    }

    #[test]
    fn forged_capability_is_rejected_by_contract_validation() {
        let mut row: ProjectionRow =
            serde_json::from_str(fixture("projection_stale_restamped")).unwrap();
        let ProjectionRow::Stale(stale) = &mut row else {
            panic!("expected stale projection row");
        };
        stale.evidence_capability = EvidenceCapability::positive();
        assert!(matches!(
            row.validate_capability(),
            Err(ProjectionContractError::CapabilityMismatch { .. })
        ));
    }

    #[test]
    fn layer1_contract_block_round_trips() {
        let block: Layer1ContractBlock =
            serde_json::from_str(fixture("layer1_contract")).unwrap();
        assert_eq!(block.tracked_by, "gwild/milton#3030");
        assert_eq!(block.milton_projector_ref.repo, "gwild/milton");
        assert_eq!(
            block.negative_stale_row_fixture,
            "projection_stale_restamped.json"
        );
        let encoded = serde_json::to_string(&block).unwrap();
        let decoded: Layer1ContractBlock = serde_json::from_str(&encoded).unwrap();
        assert_eq!(block, decoded);
    }
}
