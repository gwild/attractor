//! Shared projection-row spine for proof/evidence producers.
//!
//! This module is intentionally pure schema. Milton's PR-body projector and
//! Attractor's resolver/indexer should serialize through these same variants
//! instead of maintaining parallel row shapes.

use serde::{Deserialize, Serialize};

/// Stable schema id for the first projection-row spine.
pub const PROJECTION_SCHEMA_ID: &str = "gwild.attractor.projection-row.v1";

/// Whether a visible evidence row contributes to candidate scoring.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreContribution {
    /// The row corroborates the candidate.
    Positive,
    /// The row falsifies the candidate.
    Negative,
    /// The row is routable work but does not vote either way.
    Neutral,
}

/// Which APIs may consume a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceCapability {
    /// The row is visible to resolution/routing.
    VisibleForResolution,
    /// The row may satisfy positive proof obligations.
    PositiveEvidence,
    /// The row is visible but barred from positive proof APIs.
    NotPositiveEvidence,
}

/// Typed projection output shared by producers and consumers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "projection_state", rename_all = "snake_case")]
pub enum ProjectionRow {
    /// The row parsed and is usable according to its score/capability.
    Ok(ProjectionOk),
    /// The stored receipt no longer answers the current claim.
    Stale(ProjectionStale),
    /// The source row is malformed but remains visible evidence.
    Invalid(ProjectionInvalid),
    /// The consumer does not support the stored projection schema tuple.
    SchemaUnsupported(ProjectionSchemaUnsupported),
    /// A fixture hash was presented with the wrong manifest kind.
    FixtureManifestKindMismatch(FixtureManifestKindMismatch),
}

/// A usable typed projection row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionOk {
    /// Source artifact that produced this row.
    pub source_ref: String,
    /// Stable claim hash produced from claim-side fields.
    pub claim_hash: String,
    /// Stable receipt hash produced from receipt-side fields.
    pub receipt_hash: String,
    /// Row score contribution.
    pub score: ScoreContribution,
    /// Row evidence capability.
    pub capability: EvidenceCapability,
}

/// A row whose claim is current but whose receipt needs replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionStale {
    /// Source artifact that produced this row.
    pub source_ref: String,
    /// Stable claim hash that still defines the question.
    pub claim_hash: String,
    /// Hash of the stored receipt that no longer answers the claim.
    pub stored_receipt_hash: String,
    /// Replay obligation id for this exact claim/receipt mismatch.
    pub reexec_request_id: String,
    /// Preregistered fields that replay must answer.
    pub mismatch_fields: Vec<String>,
    /// Row score contribution. Stale rows must be neutral.
    pub score: ScoreContribution,
    /// Row evidence capability. Stale rows remain visible, not positive.
    pub capability: EvidenceCapability,
}

/// A malformed projection row that should not disappear from routing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionInvalid {
    /// Source artifact that produced this row.
    pub source_ref: String,
    /// Load-bearing fields involved in the invalid projection.
    pub field_refs: Vec<String>,
    /// Stable invalid-reason code.
    pub invalid_reason: String,
    /// Row score contribution. Invalid rows are neutral.
    pub score: ScoreContribution,
}

/// A stored projection tuple newer or otherwise unsupported by a consumer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionSchemaUnsupported {
    /// Source artifact that produced this row.
    pub source_ref: String,
    /// Consumer that lacked support.
    pub consumer_id: String,
    /// Stored schema/projector/fixture tuple.
    pub stored_tuple: String,
    /// Content-addressed snapshot of the consumer's supported tuple set.
    pub supported_tuples_hash: String,
    /// Bounded remediation route.
    pub remediation: UnsupportedRemediation,
    /// Unsupported schema rows are neutral.
    pub score: ScoreContribution,
}

/// Bounded remediation routes for unsupported projection schemas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedRemediation {
    /// Add support for the stored tuple.
    AddSupport,
    /// Migrate the stored projection into a supported tuple.
    MigrateProjection,
    /// Route to an owner because the resolver cannot classify the remediation.
    RemediationUnclassified,
}

/// Typed rejection for swapped claim/receipt fixture manifests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureManifestKindMismatch {
    /// Manifest kind the receiving type expected.
    pub expected_kind: FixtureManifestKind,
    /// Manifest reference actually supplied.
    pub actual_manifest_ref: String,
    /// Digest that must not be accepted as an unbranded raw string.
    pub digest_ref: String,
}

/// Manifest kinds that brand fixture hashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FixtureManifestKind {
    /// Claim-side fixture manifest.
    ClaimFixtures,
    /// Receipt-side fixture manifest.
    ReceiptFixtures,
}

/// Stored result of meeting all action gates for a candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "meet_state", rename_all = "snake_case")]
pub enum GateMeetStored {
    /// A resolved action set, possibly empty only with explicit policy proof.
    Allowed(AllowedActionSet),
    /// At least one gate was unmapped; action projection must stop.
    Unmapped(UnmappedGate),
}

/// Explicitly allowed actions after policy evaluation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedActionSet {
    /// Allowed action kinds. Empty is valid only because `policy_ref` exists.
    pub actions: Vec<ActionKind>,
    /// Policy proof that produced this action set.
    pub policy_ref: String,
}

impl AllowedActionSet {
    /// Construct an explicit policy-denied empty action set.
    #[must_use]
    pub fn empty(policy_ref: impl Into<String>) -> Self {
        Self {
            actions: Vec::new(),
            policy_ref: policy_ref.into(),
        }
    }
}

/// Executable action proposal kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    /// Link as duplicate.
    LinkDuplicate,
    /// Add an extends edge.
    AddExtendsEdge,
    /// Open a new enum/probe case.
    OpenEnumCase,
    /// Update a durable guard.
    UpdateDurableGuard,
}

/// Terminal meet result for unresolved action mappings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmappedGate {
    /// Exact gate refs that lacked mappings.
    pub refs: Vec<String>,
}

/// Partial replay receipt acceptance result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "partial_state", rename_all = "snake_case")]
pub enum PartialReexecResult {
    /// The receipt answered a non-empty subset of the request scope.
    Accepted(AcceptedPartial),
    /// The receipt answered fields outside the request scope.
    Rejected(RejectedPartial),
}

/// Accepted partial replay fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedPartial {
    /// Fields answered by this partial receipt.
    pub answered_fields: Vec<String>,
}

/// Rejected partial replay fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedPartial {
    /// Fields that do not belong to the replay request.
    pub orphan_fields: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        EvidenceCapability, FixtureManifestKind, GateMeetStored, ProjectionRow, ScoreContribution,
    };

    fn fixture(name: &str) -> serde_json::Value {
        let raw = match name {
            "projection-stale-cross-platform-9-vs-11" => include_str!(
                "../../../fixtures/projection-spine/negative/projection-stale-cross-platform-9-vs-11.json"
            ),
            "allowed-action-empty-without-policy-ref" => include_str!(
                "../../../fixtures/projection-spine/negative/allowed-action-empty-without-policy-ref.json"
            ),
            "fixture-manifest-kind-mismatch-claim-as-receipt" => include_str!(
                "../../../fixtures/projection-spine/negative/fixture-manifest-kind-mismatch-claim-as-receipt.json"
            ),
            "constraint-promotion-unmapped-action" => include_str!(
                "../../../fixtures/projection-spine/negative/constraint-promotion-unmapped-action.json"
            ),
            _ => panic!("unknown fixture {name}"),
        };
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn stale_projection_fixture_is_neutral_not_positive() {
        let row: ProjectionRow =
            serde_json::from_value(fixture("projection-stale-cross-platform-9-vs-11")).unwrap();
        let ProjectionRow::Stale(stale) = row else {
            panic!("expected stale row");
        };
        assert_eq!(stale.score, ScoreContribution::Neutral);
        assert_eq!(stale.capability, EvidenceCapability::NotPositiveEvidence);
        assert_eq!(stale.mismatch_fields, ["passed_count", "output_excerpt"]);
    }

    #[test]
    fn empty_allowed_action_set_without_policy_ref_rejects() {
        let fixture = fixture("allowed-action-empty-without-policy-ref");
        let wire = fixture.get("wire_value").unwrap().clone();
        assert!(serde_json::from_value::<GateMeetStored>(wire).is_err());
    }

    #[test]
    fn swapped_manifest_fixture_is_typed_rejection() {
        let row: ProjectionRow =
            serde_json::from_value(fixture("fixture-manifest-kind-mismatch-claim-as-receipt"))
                .unwrap();
        let ProjectionRow::FixtureManifestKindMismatch(mismatch) = row else {
            panic!("expected manifest-kind mismatch");
        };
        assert_eq!(mismatch.expected_kind, FixtureManifestKind::ReceiptFixtures);
        assert_eq!(
            mismatch.actual_manifest_ref,
            "claim-fixtures@blake3:example"
        );
    }

    #[test]
    fn fresh_constraint_without_action_mapping_is_unmapped() {
        let meet: GateMeetStored =
            serde_json::from_value(fixture("constraint-promotion-unmapped-action")).unwrap();
        let GateMeetStored::Unmapped(unmapped) = meet else {
            panic!("expected unmapped gate");
        };
        assert_eq!(
            unmapped.refs,
            ["constraint:newly-promoted-without-action-map"]
        );
    }
}
