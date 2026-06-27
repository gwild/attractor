//! Shared projection/evidence spine for Attractor overlap resolution.
//!
//! This module is intentionally pure serde. It is the single wire contract that
//! Attractor consumers and external projectors import instead of mirroring
//! projection result shapes in each repository.

use crate::ContentHashHex;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// Stable schema id for the first projection/evidence spine.
pub const PROJECTION_SCHEMA_ID_V0: &str = "attractor.projection.evidence.v0";

/// A source field inside the artifact being projected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldRef {
    /// Stable field path, for example `cross_platform[linux].output`.
    pub path: String,
}

/// Git blob SHA-1 used to bind an external projector path at a repository SHA.
///
/// This is deliberately not [`ContentHashHex`]: Git blob object ids in the
/// current cross-repo contract are 40-character SHA-1 hex strings.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct GitBlobSha(String);

impl GitBlobSha {
    /// Build a validated lowercase Git SHA-1 blob id.
    ///
    /// # Errors
    /// Returns [`GitBlobShaError`] when `value` is not exactly 40 lowercase hex
    /// characters.
    pub fn new(value: impl Into<String>) -> Result<Self, GitBlobShaError> {
        let value = value.into();
        if value.len() != 40 {
            return Err(GitBlobShaError::WrongLength { len: value.len() });
        }
        if let Some(bad) = value
            .bytes()
            .find(|&b| !matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(GitBlobShaError::InvalidByte { byte: bad });
        }
        Ok(Self(value))
    }

    /// Borrow the validated lowercase Git blob id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for GitBlobSha {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Why a Git blob SHA was invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitBlobShaError {
    /// Digest was not 40 characters long.
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

impl fmt::Display for GitBlobShaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength { len } => write!(f, "git blob sha length {len} != 40"),
            Self::InvalidByte { byte } => {
                write!(
                    f,
                    "git blob sha contains non-lowercase-hex byte 0x{byte:02x}"
                )
            }
        }
    }
}

impl std::error::Error for GitBlobShaError {}

/// What kind of fixture manifest produced a fixture hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureManifestKind {
    /// Fixtures for the claim/trigger surface.
    Claim,
    /// Fixtures for the receipt/adjudication surface.
    Receipt,
}

/// Branded fixture-set hash for claim projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClaimFixtureHash {
    /// Manifest kind. Must be [`FixtureManifestKind::Claim`] on the wire.
    pub manifest_kind: FixtureManifestKind,
    /// Manifest reference; must point at a claim fixture manifest.
    pub manifest_ref: String,
    /// Digest of the manifest contents.
    pub digest: ContentHashHex,
}

/// Branded fixture-set hash for receipt projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReceiptFixtureHash {
    /// Manifest kind. Must be [`FixtureManifestKind::Receipt`] on the wire.
    pub manifest_kind: FixtureManifestKind,
    /// Manifest reference; must point at a receipt fixture manifest.
    pub manifest_ref: String,
    /// Digest of the manifest contents.
    pub digest: ContentHashHex,
}

impl<'de> Deserialize<'de> for ClaimFixtureHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = FixtureHashWire::deserialize(deserializer)?;
        if value.manifest_kind != FixtureManifestKind::Claim {
            return Err(serde::de::Error::custom(format!(
                "fixture_manifest_kind_mismatch: expected claim, got {:?}",
                value.manifest_kind
            )));
        }
        Ok(Self {
            manifest_kind: value.manifest_kind,
            manifest_ref: value.manifest_ref,
            digest: value.digest,
        })
    }
}

impl<'de> Deserialize<'de> for ReceiptFixtureHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = FixtureHashWire::deserialize(deserializer)?;
        if value.manifest_kind != FixtureManifestKind::Receipt {
            return Err(serde::de::Error::custom(format!(
                "fixture_manifest_kind_mismatch: expected receipt, got {:?}",
                value.manifest_kind
            )));
        }
        Ok(Self {
            manifest_kind: value.manifest_kind,
            manifest_ref: value.manifest_ref,
            digest: value.digest,
        })
    }
}

#[derive(Deserialize)]
struct FixtureHashWire {
    manifest_kind: FixtureManifestKind,
    manifest_ref: String,
    digest: ContentHashHex,
}

/// Versioned projection schema tuple a consumer must explicitly support.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionSchemaTuple {
    /// Projection schema identifier.
    pub projection_schema_id: String,
    /// Digest of the projector implementation artifact.
    pub projector_digest: ContentHashHex,
    /// Digest of the fixture set that constrains semantic projection.
    pub fixture_set_digest: ContentHashHex,
}

/// Cross-repository Milton projector binding carried by the Layer-1 contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MiltonProjectorRef {
    /// Repository identity, for example `gwild/milton`.
    pub repo: String,
    /// Git commit containing the projector file.
    pub sha: String,
    /// Projector file path at `sha`.
    pub path: String,
    /// Git blob hash of `path` at `sha`, verified before deriving the schema digest.
    pub blob_hash: GitBlobSha,
}

/// Review-visible cross-repo contract block for projection PRs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossRepoContract {
    /// External tracker for the Milton half of the contract.
    pub tracked_by: String,
    /// Digest of the shared schema artifact.
    pub schema_artifact_digest: ContentHashHex,
    /// Exact Milton projector artifact this contract expects.
    pub milton_projector_ref: MiltonProjectorRef,
    /// Fixture name proving stale proof rows are rejected.
    pub negative_stale_fixture: String,
}

/// Projection claim surface. This decides whether a clearance needs replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionClaim {
    /// Hash over claim fields using [`Self::claim_fixture_hash`].
    pub claim_hash: ClaimHash,
    /// Repository commit the claim applies to.
    pub repo_sha: String,
    /// Artifact being projected, such as a PR body or issue comment.
    pub artifact_ref: String,
    /// Platform row or host class, when the claim is platform scoped.
    pub platform: String,
    /// Replay command declared by the proof row.
    pub cmd: String,
    /// Fields that the receipt must answer before this claim can clear.
    pub expected_fields: Vec<FieldRef>,
    /// Branded fixture hash for the claim clock.
    pub claim_fixture_hash: ClaimFixtureHash,
}

/// Claim hash; distinct from [`ReceiptHash`] so the two clocks cannot be mixed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimHash {
    /// Fixture manifest hash used for claim projection.
    pub fixture_hash: ClaimFixtureHash,
    /// Digest of canonicalized claim fields.
    pub digest: ContentHashHex,
}

/// Receipt hash; distinct from [`ClaimHash`] so parser changes cannot masquerade
/// as claim changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptHash {
    /// Fixture manifest hash used for receipt projection.
    pub fixture_hash: ReceiptFixtureHash,
    /// Digest of canonicalized receipt fields.
    pub digest: ContentHashHex,
}

/// Integer receipt field with explicit unknown state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReceiptInt {
    /// Field parsed cleanly.
    Known {
        /// Parsed integer value.
        value: u64,
    },
    /// Field could not be parsed or was absent.
    Unknown {
        /// Reason this field is unknown.
        reason: String,
        /// Source field that failed to provide the value.
        field_ref: FieldRef,
    },
}

/// String-list receipt field with explicit unknown state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReceiptStringList {
    /// Field parsed cleanly.
    Known {
        /// Parsed values.
        value: Vec<String>,
    },
    /// Field could not be parsed or was absent.
    Unknown {
        /// Reason this field is unknown.
        reason: String,
        /// Source field that failed to provide the value.
        field_ref: FieldRef,
    },
}

/// Typed proof receipt produced by re-executing a claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedReceipt {
    /// Hash over receipt fields using [`Self::receipt_fixture_hash`].
    pub receipt_hash: ReceiptHash,
    /// Request id this receipt was produced to answer.
    pub reexec_request_id: String,
    /// Exit status from the replay command.
    pub exit: ReceiptInt,
    /// Parsed passing test count.
    pub passed_count: ReceiptInt,
    /// Parsed failing test count.
    pub failed_count: ReceiptInt,
    /// Parsed collected test count.
    pub collected_count: ReceiptInt,
    /// Parsed suite identifiers, if available.
    pub suite_ids: ReceiptStringList,
    /// Branded fixture hash for the receipt clock.
    pub receipt_fixture_hash: ReceiptFixtureHash,
}

/// Result of comparing a body claim with a replay receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReceiptComparison {
    /// Receipt answers every load-bearing field and matches the claim.
    Equal,
    /// Receipt answers every load-bearing field but differs from the claim.
    Mismatch {
        /// Fields that differed.
        mismatch_fields: Vec<FieldRef>,
    },
    /// Receipt is missing load-bearing fields and needs replay or migration.
    Incomplete {
        /// Unknown fields that prevent adjudication.
        unknown_fields: Vec<FieldRef>,
    },
    /// Claim/receipt clocks require a concrete replay request.
    NeedsReexec {
        /// Claim hash that requires replay.
        claim_hash: ClaimHash,
        /// Stored receipt hash that was insufficient.
        receipt_hash: ReceiptHash,
    },
}

/// State of the projection result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionState {
    /// Projection is usable as positive evidence.
    Ok,
    /// Projection is malformed and quarantined as visible evidence.
    Invalid,
    /// Projection parsed, but its receipt is stale against the claim.
    Stale,
    /// Consumer cannot read the projection schema tuple.
    SchemaUnsupported,
}

/// How an evidence row may contribute to scoring.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreContribution {
    /// Supports the candidate.
    Positive,
    /// Falsifies or weakens the candidate.
    Negative,
    /// Visible for routing but excluded from score denominators.
    Neutral,
}

/// Evidence capability envelope shared by every projection row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceCapability {
    /// Whether routing/candidate resolution should see this row.
    pub visible_for_resolution: bool,
    /// Whether this row may be passed to positive-evidence APIs.
    pub usable_as_positive_evidence: bool,
    /// Scoring contribution; do not infer this from positive usability.
    pub score: ScoreContribution,
}

/// Common envelope carried by projection rows, including quarantined rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceEnvelope {
    /// Current projection state.
    pub projection_state: ProjectionState,
    /// Evidence capability for routing and scoring.
    pub capability: EvidenceCapability,
    /// Source artifact this row came from.
    pub source_ref: String,
    /// Fields used or rejected by this projection.
    pub field_refs: Vec<FieldRef>,
}

/// Successful projection result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionOk {
    /// Schema tuple this projection used.
    pub schema: ProjectionSchemaTuple,
    /// Claim surface.
    pub claim: ProjectionClaim,
    /// Parsed receipt.
    pub parsed_receipt: TypedReceipt,
    /// Shared evidence envelope.
    pub envelope: EvidenceEnvelope,
}

/// Malformed projection that remains addressable as quarantine evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionInvalid {
    /// Schema tuple, when it could be parsed before invalidity was detected.
    pub schema: Option<ProjectionSchemaTuple>,
    /// Machine-readable invalidity reason.
    pub reason: String,
    /// Fields responsible for invalidity.
    pub field_refs: Vec<FieldRef>,
    /// Shared evidence envelope.
    pub envelope: EvidenceEnvelope,
}

/// Projection whose claim is parseable but whose receipt needs replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionStale {
    /// Schema tuple this projection used.
    pub schema: ProjectionSchemaTuple,
    /// Claim surface.
    pub claim: ProjectionClaim,
    /// Stored receipt that did not clear the claim.
    pub parsed_receipt: TypedReceipt,
    /// Concrete replay obligation id.
    pub reexec_request_id: String,
    /// Fields that must be answered before this can clear.
    pub mismatch_fields: Vec<FieldRef>,
    /// Shared evidence envelope.
    pub envelope: EvidenceEnvelope,
}

/// Unsupported projection schema that remains visible as neutral evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionUnsupported {
    /// Tuple found on the stored projection.
    pub stored_tuple: ProjectionSchemaTuple,
    /// Consumer that could not support the tuple.
    pub consumer_id: String,
    /// Hash of the immutable consumer capability snapshot.
    pub supported_tuples_hash: ContentHashHex,
    /// Bounded remediation classification.
    pub remediation: UnsupportedRemediation,
    /// Shared evidence envelope.
    pub envelope: EvidenceEnvelope,
}

/// Bounded remediation for unsupported projection schemas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedRemediation {
    /// Consumer must add support for the stored tuple.
    AddSupport,
    /// Projection should migrate to a tuple the consumer supports.
    MigrateProjection,
    /// Resolver could not classify the remediation.
    RemediationUnclassified,
}

/// Total projection result; consumers must handle every variant explicitly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "projection_state", content = "row", rename_all = "snake_case")]
pub enum ProjectionResult {
    /// Projection is usable as positive evidence.
    Ok(ProjectionOk),
    /// Projection is malformed but visible for routing.
    Invalid(ProjectionInvalid),
    /// Projection needs a replay receipt.
    Stale(ProjectionStale),
    /// Consumer does not support the schema tuple.
    SchemaUnsupported(ProjectionUnsupported),
}

impl<'de> Deserialize<'de> for ProjectionResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ProjectionResultWire::deserialize(deserializer)?;
        match value {
            ProjectionResultWire::Ok(row) => {
                expect_projection_state(row.envelope.projection_state, ProjectionState::Ok)?;
                Ok(Self::Ok(row))
            }
            ProjectionResultWire::Invalid(row) => {
                expect_projection_state(row.envelope.projection_state, ProjectionState::Invalid)?;
                Ok(Self::Invalid(row))
            }
            ProjectionResultWire::Stale(row) => {
                expect_projection_state(row.envelope.projection_state, ProjectionState::Stale)?;
                Ok(Self::Stale(row))
            }
            ProjectionResultWire::SchemaUnsupported(row) => {
                expect_projection_state(
                    row.envelope.projection_state,
                    ProjectionState::SchemaUnsupported,
                )?;
                Ok(Self::SchemaUnsupported(row))
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "projection_state", content = "row", rename_all = "snake_case")]
enum ProjectionResultWire {
    Ok(ProjectionOk),
    Invalid(ProjectionInvalid),
    Stale(ProjectionStale),
    SchemaUnsupported(ProjectionUnsupported),
}

fn expect_projection_state<E>(actual: ProjectionState, expected: ProjectionState) -> Result<(), E>
where
    E: serde::de::Error,
{
    if actual == expected {
        Ok(())
    } else {
        Err(E::custom(format!(
            "projection_state mismatch: tag={expected}, envelope={actual}"
        )))
    }
}

/// Reference to an action-set definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionSetRef {
    /// Stable action-set identifier.
    pub id: String,
}

/// Resolved action set. Empty action sets must preserve deny provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ActionSet {
    /// Stable action-set identifier.
    pub id: String,
    /// Actions allowed by the gate meet.
    pub allowed_actions: Vec<String>,
    /// Schema tuple that produced this action set.
    pub source_schema: ProjectionSchemaTuple,
    /// Required when `allowed_actions` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_ref: Option<String>,
}

impl<'de> Deserialize<'de> for ActionSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ActionSetWire::deserialize(deserializer)?;
        if value.allowed_actions.is_empty() && value.policy_ref.is_none() {
            return Err(serde::de::Error::custom(
                "empty action set requires policy_ref",
            ));
        }
        Ok(Self {
            id: value.id,
            allowed_actions: value.allowed_actions,
            source_schema: value.source_schema,
            policy_ref: value.policy_ref,
        })
    }
}

#[derive(Deserialize)]
struct ActionSetWire {
    id: String,
    allowed_actions: Vec<String>,
    source_schema: ProjectionSchemaTuple,
    #[serde(default)]
    policy_ref: Option<String>,
}

/// Gate that could not be mapped to an action set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmappedGate {
    /// Bucket that failed to map.
    pub bucket: String,
    /// Constraint member that failed to map.
    pub constraint_member: String,
    /// Stable reason, normally `unmapped_gate`.
    pub why_not: String,
}

/// Result of meeting candidate action gates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "result", rename_all = "snake_case")]
pub enum GateMeetResult {
    /// Meet produced a resolved action set.
    Allowed(ActionSet),
    /// Meet stopped on unmapped gates; this cannot feed `next_action`.
    BlockedByUnmapped {
        /// Unresolved gate refs.
        refs: Vec<UnmappedGate>,
    },
}

impl ProjectionOk {
    /// Borrow this projection as positive evidence.
    #[must_use]
    pub fn positive_evidence(&self) -> Option<&Self> {
        (self.envelope.capability.usable_as_positive_evidence
            && self.envelope.capability.score == ScoreContribution::Positive)
            .then_some(self)
    }
}

impl fmt::Display for ProjectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::Invalid => f.write_str("invalid"),
            Self::Stale => f.write_str("stale"),
            Self::SchemaUnsupported => f.write_str("schema_unsupported"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u8) -> ContentHashHex {
        ContentHashHex::new(format!("{seed:064x}")).unwrap()
    }

    fn claim_fixture() -> ClaimFixtureHash {
        ClaimFixtureHash {
            manifest_kind: FixtureManifestKind::Claim,
            manifest_ref: "fixtures/proof-claim.v0.json".to_owned(),
            digest: hash(1),
        }
    }

    fn receipt_fixture() -> ReceiptFixtureHash {
        ReceiptFixtureHash {
            manifest_kind: FixtureManifestKind::Receipt,
            manifest_ref: "fixtures/proof-receipt.v0.json".to_owned(),
            digest: hash(2),
        }
    }

    fn schema() -> ProjectionSchemaTuple {
        ProjectionSchemaTuple {
            projection_schema_id: PROJECTION_SCHEMA_ID_V0.to_owned(),
            projector_digest: hash(3),
            fixture_set_digest: hash(4),
        }
    }

    fn field(path: &str) -> FieldRef {
        FieldRef {
            path: path.to_owned(),
        }
    }

    fn positive_capability() -> EvidenceCapability {
        EvidenceCapability {
            visible_for_resolution: true,
            usable_as_positive_evidence: true,
            score: ScoreContribution::Positive,
        }
    }

    fn neutral_capability() -> EvidenceCapability {
        EvidenceCapability {
            visible_for_resolution: true,
            usable_as_positive_evidence: false,
            score: ScoreContribution::Neutral,
        }
    }

    fn envelope(state: ProjectionState, capability: EvidenceCapability) -> EvidenceEnvelope {
        EvidenceEnvelope {
            projection_state: state,
            capability,
            source_ref: "pr:3031#body".to_owned(),
            field_refs: vec![field("cross_platform.linux.output")],
        }
    }

    fn claim() -> ProjectionClaim {
        ProjectionClaim {
            claim_hash: ClaimHash {
                fixture_hash: claim_fixture(),
                digest: hash(5),
            },
            repo_sha: "86cacfda1ac0721bb2fafe187e9857de8d3d3a09".to_owned(),
            artifact_ref: "github:gwild/milton#3031.body".to_owned(),
            platform: "linux".to_owned(),
            cmd: "venv/bin/pytest tests/test_start_service_reaps_on_restart.py -q".to_owned(),
            expected_fields: vec![field("exit"), field("passed_count")],
            claim_fixture_hash: claim_fixture(),
        }
    }

    fn receipt(passed: u64) -> TypedReceipt {
        TypedReceipt {
            receipt_hash: ReceiptHash {
                fixture_hash: receipt_fixture(),
                digest: hash(6),
            },
            reexec_request_id: "reexec:claim5:receipt6:linux".to_owned(),
            exit: ReceiptInt::Known { value: 0 },
            passed_count: ReceiptInt::Known { value: passed },
            failed_count: ReceiptInt::Known { value: 0 },
            collected_count: ReceiptInt::Unknown {
                reason: "pytest_quiet_output".to_owned(),
                field_ref: field("output"),
            },
            suite_ids: ReceiptStringList::Unknown {
                reason: "suite_ids_not_declared".to_owned(),
                field_ref: field("cmd"),
            },
            receipt_fixture_hash: receipt_fixture(),
        }
    }

    #[test]
    fn projection_ok_round_trips_as_positive_evidence() {
        let row = ProjectionResult::Ok(ProjectionOk {
            schema: schema(),
            claim: claim(),
            parsed_receipt: receipt(11),
            envelope: envelope(ProjectionState::Ok, positive_capability()),
        });

        let json = serde_json::to_string(&row).unwrap();
        assert!(json.contains(r#""projection_state":"ok""#));
        let back: ProjectionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(row, back);
        match back {
            ProjectionResult::Ok(ok) => assert!(ok.positive_evidence().is_some()),
            other => panic!("expected ok projection, got {other:?}"),
        }
    }

    #[test]
    fn stale_projection_stays_visible_but_not_positive() {
        let row = ProjectionResult::Stale(ProjectionStale {
            schema: schema(),
            claim: claim(),
            parsed_receipt: receipt(9),
            reexec_request_id: "reexec:3031:linux".to_owned(),
            mismatch_fields: vec![field("passed_count")],
            envelope: envelope(ProjectionState::Stale, neutral_capability()),
        });

        let json = serde_json::to_string(&row).unwrap();
        assert!(json.contains(r#""projection_state":"stale""#));
        let back: ProjectionResult = serde_json::from_str(&json).unwrap();
        match back {
            ProjectionResult::Stale(stale) => {
                assert!(stale.envelope.capability.visible_for_resolution);
                assert!(!stale.envelope.capability.usable_as_positive_evidence);
                assert_eq!(stale.envelope.capability.score, ScoreContribution::Neutral);
                assert_eq!(stale.mismatch_fields, vec![field("passed_count")]);
            }
            other => panic!("expected stale projection, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_projection_is_neutral_evidence_not_a_drop() {
        let row = ProjectionResult::SchemaUnsupported(ProjectionUnsupported {
            stored_tuple: schema(),
            consumer_id: "attractor.resolver.v0".to_owned(),
            supported_tuples_hash: hash(7),
            remediation: UnsupportedRemediation::AddSupport,
            envelope: envelope(ProjectionState::SchemaUnsupported, neutral_capability()),
        });

        let json = serde_json::to_string(&row).unwrap();
        let back: ProjectionResult = serde_json::from_str(&json).unwrap();
        match back {
            ProjectionResult::SchemaUnsupported(unsupported) => {
                assert!(unsupported.envelope.capability.visible_for_resolution);
                assert_eq!(
                    unsupported.envelope.capability.score,
                    ScoreContribution::Neutral
                );
                assert_eq!(unsupported.remediation, UnsupportedRemediation::AddSupport);
            }
            other => panic!("expected unsupported projection, got {other:?}"),
        }
    }

    #[test]
    fn projection_result_rejects_tag_envelope_state_disagreement() {
        let row = serde_json::json!({
            "projection_state": "ok",
            "row": {
                "schema": schema(),
                "claim": claim(),
                "parsed_receipt": receipt(11),
                "envelope": envelope(ProjectionState::Stale, positive_capability())
            }
        });
        let err = serde_json::from_value::<ProjectionResult>(row).unwrap_err();
        assert!(err
            .to_string()
            .contains("projection_state mismatch: tag=ok, envelope=stale"));
    }

    #[test]
    fn receipt_comparison_variants_round_trip() {
        let variants = [
            ReceiptComparison::Equal,
            ReceiptComparison::Mismatch {
                mismatch_fields: vec![field("passed_count")],
            },
            ReceiptComparison::Incomplete {
                unknown_fields: vec![field("collected_count")],
            },
            ReceiptComparison::NeedsReexec {
                claim_hash: claim().claim_hash,
                receipt_hash: receipt(9).receipt_hash,
            },
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let back: ReceiptComparison = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, back);
        }
    }

    #[test]
    fn empty_action_set_requires_policy_ref_on_the_wire() {
        let invalid = serde_json::json!({
            "id": "deny-all",
            "allowed_actions": [],
            "source_schema": schema()
        });
        let err = serde_json::from_value::<ActionSet>(invalid).unwrap_err();
        assert!(err
            .to_string()
            .contains("empty action set requires policy_ref"));

        let valid = ActionSet {
            id: "deny-all".to_owned(),
            allowed_actions: Vec::new(),
            source_schema: schema(),
            policy_ref: Some("policy:no-destructive-action-for-isomorph".to_owned()),
        };
        let json = serde_json::to_string(&valid).unwrap();
        let back: ActionSet = serde_json::from_str(&json).unwrap();
        assert_eq!(valid, back);
    }

    #[test]
    fn git_blob_sha_validates_wire_values() {
        assert!(GitBlobSha::new("a0b1c2d3e4f506172839405060708090aabbccdd").is_ok());
        assert!(matches!(
            GitBlobSha::new("a".repeat(39)),
            Err(GitBlobShaError::WrongLength { len: 39 })
        ));
        let bad = format!("{}G", "a".repeat(39));
        assert!(matches!(
            GitBlobSha::new(bad),
            Err(GitBlobShaError::InvalidByte { byte: b'G' })
        ));
    }

    #[test]
    fn fixture_hash_kind_mismatch_is_a_typed_boundary_error() {
        let swapped = serde_json::json!({
            "manifest_kind": "receipt",
            "manifest_ref": "fixtures/proof-receipt.v0.json",
            "digest": hash(8)
        });
        let err = serde_json::from_value::<ClaimFixtureHash>(swapped).unwrap_err();
        assert!(err
            .to_string()
            .contains("fixture_manifest_kind_mismatch: expected claim"));
    }

    #[test]
    fn gate_meet_keeps_unmapped_separate_from_allowed() {
        let blocked = GateMeetResult::BlockedByUnmapped {
            refs: vec![UnmappedGate {
                bucket: "possible_isomorph".to_owned(),
                constraint_member: "silent_failure_spawns_dup".to_owned(),
                why_not: "unmapped_gate".to_owned(),
            }],
        };
        let json = serde_json::to_string(&blocked).unwrap();
        assert!(json.contains(r#""status":"blocked_by_unmapped""#));
        let back: GateMeetResult = serde_json::from_str(&json).unwrap();
        assert_eq!(blocked, back);
    }

    #[test]
    fn cross_repo_contract_binds_milton_projector_blob() {
        let contract = CrossRepoContract {
            tracked_by: "milton#3030".to_owned(),
            schema_artifact_digest: hash(9),
            milton_projector_ref: MiltonProjectorRef {
                repo: "gwild/milton".to_owned(),
                sha: "86cacfda1ac0721bb2fafe187e9857de8d3d3a09".to_owned(),
                path: ".claude/hooks/block-pr-create-without-headings.py".to_owned(),
                blob_hash: GitBlobSha::new("a0b1c2d3e4f506172839405060708090aabbccdd").unwrap(),
            },
            negative_stale_fixture: "pr3031_commit_rebind_stale_9_passed".to_owned(),
        };

        let json = serde_json::to_string(&contract).unwrap();
        assert!(json.contains(r#""repo":"gwild/milton""#));
        assert!(json.contains(r#""blob_hash":"a0b1c2d3e4f506172839405060708090aabbccdd""#));
        let back: CrossRepoContract = serde_json::from_str(&json).unwrap();
        assert_eq!(contract, back);
    }
}
