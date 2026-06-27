//! Issue-to-wiki migration projection contract.
//!
//! This module freezes the first Attractor-owned slice for converting
//! issue-shaped source material into wiki-addressable evidence. It carries only
//! typed extracted fields and source-span/hash bindings; raw issue bodies stay
//! outside derived graph rows.

use crate::projection::{EvidenceCapability, FieldRef, ScoreContribution};
use crate::{ContentHashHex, SourceSpan};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable schema id for issue-to-wiki migration projections.
pub const ISSUE_WIKI_SCHEMA_ID_V0: &str = "attractor.issue_wiki.projection.v0";

/// Stable schema id for issue-to-wiki dry-run mutation plans.
pub const ISSUE_WIKI_MUTATION_PLAN_SCHEMA_ID_V0: &str = "attractor.issue_wiki.mutation_plan.v0";

/// Source issue identity and content hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiSource {
    /// Repository identity, for example `gwild/attractor`.
    pub repo: String,
    /// Issue number in `repo`.
    pub issue_number: u64,
    /// Stable source reference, for example `github:gwild/attractor#43`.
    pub source_ref: String,
    /// Content hash of the exact issue payload projected.
    pub source_hash: ContentHashHex,
}

/// Extracted text value bound to the source span that produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanBoundText {
    /// Derived text value.
    pub value: String,
    /// Source span and source hash for this value.
    pub span: SourceSpan,
}

/// Extractor-time proof that a typed value is bound to its source bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiConformanceReceipt {
    /// Content hash of the exact source bytes checked by the extractor.
    pub source_hash: ContentHashHex,
    /// Typed field path, for example `identity`, `title`, or `references[0].target`.
    pub field_path: String,
    /// Content hash of the typed field value bytes.
    pub value_hash: ContentHashHex,
    /// Source span whose bytes must equal the typed value.
    pub span: SourceSpan,
}

/// Typed wiki reference evidence extracted from an issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiReference {
    /// Target wiki path, issue reference, or external artifact id.
    pub target: String,
    /// Optional label text, if distinct from the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<SpanBoundText>,
    /// Source span that justified the reference.
    pub span: SourceSpan,
}

/// Wiki-note fields derived from an issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiNoteEvidence {
    /// Stable wiki note identity.
    pub identity: SpanBoundText,
    /// Wiki note title.
    pub title: SpanBoundText,
    /// Typed references extracted from the issue.
    pub references: Vec<IssueWikiReference>,
}

/// State of an issue-to-wiki projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWikiProjectionState {
    /// Projection is usable as positive wiki migration evidence.
    Accepted,
    /// Source hash changed or source clock is stale.
    Stale,
    /// Source could not be projected into typed wiki evidence.
    Malformed,
}

/// Common envelope for every issue-to-wiki projection row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiProjectionEnvelope {
    /// Current issue-to-wiki projection state.
    pub projection_state: IssueWikiProjectionState,
    /// Evidence capability for routing and scoring.
    pub capability: EvidenceCapability,
    /// Source artifact this row came from.
    pub source_ref: String,
    /// Content hash of the source payload this row projected or rejected.
    pub source_hash: ContentHashHex,
    /// Stable transform identifier, for example `issue_wiki.v0`.
    pub transform_id: String,
    /// Fields used or rejected by this projection.
    pub field_refs: Vec<FieldRef>,
}

/// Accepted issue-to-wiki projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IssueWikiAccepted {
    /// Schema id this projection used.
    pub schema_id: String,
    /// Source issue identity and hash.
    pub source: IssueWikiSource,
    /// Typed wiki-note evidence. This deliberately omits raw issue body text.
    pub note: IssueWikiNoteEvidence,
    /// Extractor-time value/span/source-hash receipts for load-bearing fields.
    pub conformance_receipts: Vec<IssueWikiConformanceReceipt>,
    /// Shared evidence envelope.
    pub envelope: IssueWikiProjectionEnvelope,
}

impl<'de> Deserialize<'de> for IssueWikiAccepted {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let row = IssueWikiAcceptedWire::deserialize(deserializer)?;
        let accepted = Self {
            schema_id: row.schema_id,
            source: row.source,
            note: row.note,
            conformance_receipts: row.conformance_receipts,
            envelope: row.envelope,
        };
        accepted
            .validate_offline_value_hashes()
            .map_err(serde::de::Error::custom)?;
        Ok(accepted)
    }
}

#[derive(Deserialize)]
struct IssueWikiAcceptedWire {
    schema_id: String,
    source: IssueWikiSource,
    note: IssueWikiNoteEvidence,
    conformance_receipts: Vec<IssueWikiConformanceReceipt>,
    envelope: IssueWikiProjectionEnvelope,
}

fn issue_wiki_value_hash(value: &str) -> ContentHashHex {
    ContentHashHex::new(blake3::hash(value.as_bytes()).to_hex().to_string())
        .expect("blake3 lowercase hex is always valid")
}

/// Stale issue source that must be reprojected before store mutation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiStale {
    /// Schema id this projection used.
    pub schema_id: String,
    /// Source artifact this row tried to project.
    pub source_ref: String,
    /// Source hash the projection expected.
    pub expected_source_hash: ContentHashHex,
    /// Source hash observed at consumption time.
    pub actual_source_hash: ContentHashHex,
    /// Shared evidence envelope.
    pub envelope: IssueWikiProjectionEnvelope,
}

/// Malformed issue source that remains visible for routing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiMalformed {
    /// Schema id, when it could be parsed before malformedness was detected.
    pub schema_id: Option<String>,
    /// Source artifact this row tried to project.
    pub source_ref: String,
    /// Machine-readable malformedness reason.
    pub reason: String,
    /// Fields responsible for malformedness.
    pub field_refs: Vec<FieldRef>,
    /// Shared evidence envelope.
    pub envelope: IssueWikiProjectionEnvelope,
}

/// Total issue-to-wiki projection result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "projection_state", content = "row", rename_all = "snake_case")]
pub enum IssueWikiProjectionResult {
    /// Projection is usable as positive wiki migration evidence.
    Accepted(Box<IssueWikiAccepted>),
    /// Source hash changed or source clock is stale.
    Stale(Box<IssueWikiStale>),
    /// Source could not be projected into typed wiki evidence.
    Malformed(Box<IssueWikiMalformed>),
}

impl<'de> Deserialize<'de> for IssueWikiProjectionResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = IssueWikiProjectionResultWire::deserialize(deserializer)?;
        match value {
            IssueWikiProjectionResultWire::Accepted(row) => {
                expect_issue_wiki_state(
                    row.envelope.projection_state,
                    IssueWikiProjectionState::Accepted,
                )?;
                Ok(Self::Accepted(row))
            }
            IssueWikiProjectionResultWire::Stale(row) => {
                expect_issue_wiki_state(
                    row.envelope.projection_state,
                    IssueWikiProjectionState::Stale,
                )?;
                Ok(Self::Stale(row))
            }
            IssueWikiProjectionResultWire::Malformed(row) => {
                expect_issue_wiki_state(
                    row.envelope.projection_state,
                    IssueWikiProjectionState::Malformed,
                )?;
                Ok(Self::Malformed(row))
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "projection_state", content = "row", rename_all = "snake_case")]
enum IssueWikiProjectionResultWire {
    Accepted(Box<IssueWikiAccepted>),
    Stale(Box<IssueWikiStale>),
    Malformed(Box<IssueWikiMalformed>),
}

fn expect_issue_wiki_state<E>(
    actual: IssueWikiProjectionState,
    expected: IssueWikiProjectionState,
) -> Result<(), E>
where
    E: serde::de::Error,
{
    if actual == expected {
        Ok(())
    } else {
        Err(E::custom(format!(
            "issue_wiki projection_state mismatch: tag={expected}, envelope={actual}"
        )))
    }
}

/// Derived store mutation payload for an accepted issue-to-wiki row.
///
/// This payload contains only typed derived fields and their source spans; it
/// never carries the raw issue body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiStoreMutation {
    /// Stable wiki note identity.
    pub identity: String,
    /// Wiki note title.
    pub title: String,
    /// Typed reference targets with the source span that justified each one.
    pub references: Vec<IssueWikiMutationReference>,
    /// Source artifact the mutation was derived from.
    pub source_ref: String,
    /// Source hash verified immediately before mutation construction.
    pub source_hash: ContentHashHex,
    /// Stable transform identifier.
    pub transform_id: String,
    /// Source spans that support the derived fields.
    pub source_spans: Vec<SourceSpan>,
}

/// Reference target preserved at the store-mutation boundary with provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiMutationReference {
    /// Typed reference target.
    pub target: String,
    /// Source span that justified this target.
    pub span: SourceSpan,
}

/// Existing wiki note snapshot used only for dry-run planning.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiCurrentNote {
    /// Stable wiki note identity.
    pub identity: String,
    /// Current wiki note title.
    pub title: String,
    /// Current reference targets already present on the note.
    pub references: Vec<String>,
}

/// Deterministic wiki mutation action selected by the dry-run planner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWikiMutationAction {
    /// No current note exists for the accepted issue row.
    Create,
    /// A note with the same identity exists but its typed content differs.
    Update,
    /// The current note already matches the accepted issue row.
    Noop,
    /// The caller supplied a current note for a different identity.
    Conflict,
}

/// Dry-run wiki mutation receipt produced from an accepted issue-to-wiki row.
///
/// This receipt is the first consumer-facing mutation contract: it preserves the
/// typed row's source and conformance receipts, chooses a deterministic action,
/// and carries no raw issue body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IssueWikiMutationDryRunReceipt {
    /// Schema id this dry-run receipt used.
    pub schema_id: String,
    /// Planned wiki mutation action.
    pub action: IssueWikiMutationAction,
    /// Stable wiki note identity.
    pub identity: String,
    /// Wiki note title from the accepted row.
    pub title: String,
    /// Reference targets from the accepted row.
    pub references: Vec<IssueWikiMutationReference>,
    /// Source artifact the mutation plan was derived from.
    pub source_ref: String,
    /// Source hash carried by the accepted row.
    pub source_hash: ContentHashHex,
    /// Extractor-time value/span/source-hash receipts preserved for the writer.
    pub conformance_receipts: Vec<IssueWikiConformanceReceipt>,
    /// Stable transform identifier from the accepted row.
    pub transform_id: String,
    /// Deterministic key for idempotent dry-run replay.
    pub idempotency_key: ContentHashHex,
    /// Conflict reason when `action == conflict`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_reason: Option<String>,
}

impl<'de> Deserialize<'de> for IssueWikiMutationDryRunReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let receipt = IssueWikiMutationDryRunReceiptWire::deserialize(deserializer)?;
        let receipt = Self {
            schema_id: receipt.schema_id,
            action: receipt.action,
            identity: receipt.identity,
            title: receipt.title,
            references: receipt.references,
            source_ref: receipt.source_ref,
            source_hash: receipt.source_hash,
            conformance_receipts: receipt.conformance_receipts,
            transform_id: receipt.transform_id,
            idempotency_key: receipt.idempotency_key,
            conflict_reason: receipt.conflict_reason,
        };
        receipt
            .validate_idempotency_key()
            .map_err(serde::de::Error::custom)?;
        Ok(receipt)
    }
}

#[derive(Deserialize)]
struct IssueWikiMutationDryRunReceiptWire {
    schema_id: String,
    action: IssueWikiMutationAction,
    identity: String,
    title: String,
    references: Vec<IssueWikiMutationReference>,
    source_ref: String,
    source_hash: ContentHashHex,
    conformance_receipts: Vec<IssueWikiConformanceReceipt>,
    transform_id: String,
    idempotency_key: ContentHashHex,
    #[serde(default)]
    conflict_reason: Option<String>,
}

/// Why an issue-to-wiki projection cannot produce a store mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueWikiProjectionError {
    /// The source hash changed before store mutation.
    SourceHashMismatch {
        /// Source artifact whose hash changed.
        source_ref: String,
        /// Hash declared by the accepted projection.
        expected: ContentHashHex,
        /// Hash observed immediately before mutation.
        actual: ContentHashHex,
    },
    /// The accepted-row envelope names a different source artifact.
    SourceRefMismatch {
        /// Source reference declared by the accepted source identity.
        expected: String,
        /// Source reference declared by the evidence envelope.
        actual: String,
    },
    /// The accepted row carried a non-accepted envelope state.
    EnvelopeStateMismatch {
        /// Envelope state found on the accepted row.
        actual: IssueWikiProjectionState,
    },
    /// The accepted row did not carry positive evidence capability.
    CapabilityMismatch {
        /// Whether routing/candidate resolution can see this row.
        visible_for_resolution: bool,
        /// Whether this row may be passed to positive-evidence APIs.
        usable_as_positive_evidence: bool,
        /// Scoring contribution declared by the row.
        score: ScoreContribution,
    },
    /// A derived field's span points at a different source hash.
    SpanHashMismatch {
        /// Derived field whose span was not bound to the source hash.
        field_path: String,
        /// Hash declared by the source issue.
        expected: ContentHashHex,
        /// Hash carried by the field span.
        actual: ContentHashHex,
    },
    /// A source span has an invalid byte range.
    SpanRangeInvalid {
        /// Derived field whose span range was invalid.
        field_path: String,
        /// Byte offset where the cited source span begins.
        start: u64,
        /// Byte offset where the cited source span ends.
        end: u64,
    },
    /// A non-accepted projection row was passed to the mutation planner.
    NonAcceptedProjection {
        /// Projection state that was refused.
        state: IssueWikiProjectionState,
        /// Source artifact carried by the refused row.
        source_ref: String,
    },
    /// The accepted row's stored value/hash receipts no longer self-prove.
    ConformanceMismatch {
        /// Machine-readable mismatch reason.
        reason: String,
    },
}

/// Plan a dry-run wiki mutation from a projection result.
///
/// Only accepted rows can produce a mutation receipt. Stale and malformed rows
/// stay visible for routing but are refused before a mutation plan exists.
pub fn plan_issue_wiki_mutation(
    projection: &IssueWikiProjectionResult,
    current_note: Option<&IssueWikiCurrentNote>,
) -> Result<IssueWikiMutationDryRunReceipt, IssueWikiProjectionError> {
    let accepted = match projection {
        IssueWikiProjectionResult::Accepted(row) => row.as_ref(),
        IssueWikiProjectionResult::Stale(row) => {
            return Err(IssueWikiProjectionError::NonAcceptedProjection {
                state: IssueWikiProjectionState::Stale,
                source_ref: row.source_ref.clone(),
            });
        }
        IssueWikiProjectionResult::Malformed(row) => {
            return Err(IssueWikiProjectionError::NonAcceptedProjection {
                state: IssueWikiProjectionState::Malformed,
                source_ref: row.source_ref.clone(),
            });
        }
    };

    accepted.to_mutation_dry_run(current_note)
}

impl IssueWikiAccepted {
    fn validate_offline_value_hashes(&self) -> Result<(), String> {
        self.expect_offline_receipt(
            "identity",
            &self.note.identity.value,
            &self.note.identity.span,
        )?;
        self.expect_offline_receipt("title", &self.note.title.value, &self.note.title.span)?;
        for (idx, reference) in self.note.references.iter().enumerate() {
            self.expect_offline_receipt(
                &format!("references[{idx}].target"),
                &reference.target,
                &reference.span,
            )?;
            if let Some(label) = &reference.label {
                self.expect_offline_receipt(
                    &format!("references[{idx}].label"),
                    &label.value,
                    &label.span,
                )?;
            }
        }
        Ok(())
    }

    fn expect_offline_receipt(
        &self,
        field_path: &str,
        value: &str,
        span: &SourceSpan,
    ) -> Result<(), String> {
        let receipt = self
            .conformance_receipts
            .iter()
            .find(|receipt| receipt.field_path == field_path)
            .ok_or_else(|| format!("issue_wiki missing_conformance_receipt field={field_path}"))?;
        if receipt.source_hash != self.source.source_hash {
            return Err(format!(
                "issue_wiki conformance_source_hash_mismatch field={field_path} expected={} actual={}",
                self.source.source_hash.as_str(),
                receipt.source_hash.as_str()
            ));
        }
        if receipt.span.source_hash != self.source.source_hash {
            return Err(format!(
                "issue_wiki conformance_span_hash_mismatch field={field_path} expected={} actual={}",
                self.source.source_hash.as_str(),
                receipt.span.source_hash.as_str()
            ));
        }
        if receipt.span != *span {
            return Err(format!(
                "issue_wiki conformance_span_mismatch field={field_path} expected={}-{} actual={}-{}",
                span.start, span.end, receipt.span.start, receipt.span.end
            ));
        }
        let expected_hash = issue_wiki_value_hash(value);
        if receipt.value_hash != expected_hash {
            return Err(format!(
                "issue_wiki conformance_value_hash_mismatch field={field_path} expected={} actual={}",
                expected_hash.as_str(),
                receipt.value_hash.as_str()
            ));
        }
        Ok(())
    }

    /// Convert accepted projection evidence into a store mutation only after
    /// checking the current source hash and every derived field span.
    ///
    /// # Errors
    /// Returns [`IssueWikiProjectionError`] when the observed source hash no
    /// longer matches the projection or any field span points at another source.
    pub fn to_store_mutation(
        &self,
        observed_source_hash: &ContentHashHex,
    ) -> Result<IssueWikiStoreMutation, IssueWikiProjectionError> {
        self.validate_accepted_envelope()?;
        if observed_source_hash != &self.source.source_hash {
            return Err(IssueWikiProjectionError::SourceHashMismatch {
                source_ref: self.source.source_ref.clone(),
                expected: self.source.source_hash.clone(),
                actual: observed_source_hash.clone(),
            });
        }
        self.validate_source_bindings()?;

        let mut source_spans = vec![
            self.note.identity.span.clone(),
            self.note.title.span.clone(),
        ];
        source_spans.extend(
            self.note
                .references
                .iter()
                .map(|reference| reference.span.clone()),
        );
        source_spans.extend(
            self.note
                .references
                .iter()
                .filter_map(|reference| reference.label.as_ref().map(|label| label.span.clone())),
        );

        Ok(IssueWikiStoreMutation {
            identity: self.note.identity.value.clone(),
            title: self.note.title.value.clone(),
            references: self
                .note
                .references
                .iter()
                .map(|reference| IssueWikiMutationReference {
                    target: reference.target.clone(),
                    span: reference.span.clone(),
                })
                .collect(),
            source_ref: self.source.source_ref.clone(),
            source_hash: self.source.source_hash.clone(),
            transform_id: self.envelope.transform_id.clone(),
            source_spans,
        })
    }

    /// Convert accepted projection evidence into a deterministic dry-run wiki
    /// mutation receipt.
    ///
    /// # Errors
    /// Returns [`IssueWikiProjectionError`] when the accepted row is internally
    /// inconsistent. Stale and malformed projection results must be handled by
    /// [`plan_issue_wiki_mutation`], which refuses them before this method is
    /// called.
    pub fn to_mutation_dry_run(
        &self,
        current_note: Option<&IssueWikiCurrentNote>,
    ) -> Result<IssueWikiMutationDryRunReceipt, IssueWikiProjectionError> {
        self.validate_offline_value_hashes()
            .map_err(|reason| IssueWikiProjectionError::ConformanceMismatch { reason })?;
        let mutation = self.to_store_mutation(&self.source.source_hash)?;
        let planned_reference_targets = reference_targets(&mutation);
        let action = match current_note {
            None => IssueWikiMutationAction::Create,
            Some(note) if note.identity != mutation.identity => IssueWikiMutationAction::Conflict,
            Some(note)
                if note.title == mutation.title
                    && note.references.as_slice() == planned_reference_targets.as_slice() =>
            {
                IssueWikiMutationAction::Noop
            }
            Some(_) => IssueWikiMutationAction::Update,
        };
        let conflict_reason = match (action, current_note) {
            (IssueWikiMutationAction::Conflict, Some(note)) => Some(format!(
                "issue_wiki identity_conflict expected={} actual={}",
                mutation.identity, note.identity
            )),
            _ => None,
        };
        let receipt = IssueWikiMutationDryRunReceipt {
            schema_id: ISSUE_WIKI_MUTATION_PLAN_SCHEMA_ID_V0.to_owned(),
            action,
            identity: mutation.identity,
            title: mutation.title,
            references: mutation.references,
            source_ref: mutation.source_ref,
            source_hash: mutation.source_hash,
            conformance_receipts: self.conformance_receipts.clone(),
            transform_id: mutation.transform_id,
            idempotency_key: ContentHashHex::new("0".repeat(64))
                .expect("zero placeholder is valid lowercase hex"),
            conflict_reason,
        };
        Ok(IssueWikiMutationDryRunReceipt {
            idempotency_key: issue_wiki_mutation_idempotency_key(&receipt),
            ..receipt
        })
    }

    fn validate_accepted_envelope(&self) -> Result<(), IssueWikiProjectionError> {
        if self.envelope.projection_state != IssueWikiProjectionState::Accepted {
            return Err(IssueWikiProjectionError::EnvelopeStateMismatch {
                actual: self.envelope.projection_state,
            });
        }
        if self.envelope.source_ref != self.source.source_ref {
            return Err(IssueWikiProjectionError::SourceRefMismatch {
                expected: self.source.source_ref.clone(),
                actual: self.envelope.source_ref.clone(),
            });
        }
        let capability = &self.envelope.capability;
        if !capability.visible_for_resolution
            || !capability.usable_as_positive_evidence
            || capability.score != ScoreContribution::Positive
        {
            return Err(IssueWikiProjectionError::CapabilityMismatch {
                visible_for_resolution: capability.visible_for_resolution,
                usable_as_positive_evidence: capability.usable_as_positive_evidence,
                score: capability.score,
            });
        }
        Ok(())
    }

    fn validate_source_bindings(&self) -> Result<(), IssueWikiProjectionError> {
        self.expect_span_hash("identity", &self.note.identity.span)?;
        self.expect_span_hash("title", &self.note.title.span)?;
        if self.envelope.source_hash != self.source.source_hash {
            return Err(IssueWikiProjectionError::SpanHashMismatch {
                field_path: "envelope.source_hash".to_owned(),
                expected: self.source.source_hash.clone(),
                actual: self.envelope.source_hash.clone(),
            });
        }
        for (idx, reference) in self.note.references.iter().enumerate() {
            self.expect_span_hash(&format!("references[{idx}]"), &reference.span)?;
            if let Some(label) = &reference.label {
                self.expect_span_hash(&format!("references[{idx}].label"), &label.span)?;
            }
        }
        Ok(())
    }

    fn expect_span_hash(
        &self,
        field_path: &str,
        span: &SourceSpan,
    ) -> Result<(), IssueWikiProjectionError> {
        if span.start > span.end {
            return Err(IssueWikiProjectionError::SpanRangeInvalid {
                field_path: field_path.to_owned(),
                start: span.start,
                end: span.end,
            });
        }
        if span.source_hash == self.source.source_hash {
            Ok(())
        } else {
            Err(IssueWikiProjectionError::SpanHashMismatch {
                field_path: field_path.to_owned(),
                expected: self.source.source_hash.clone(),
                actual: span.source_hash.clone(),
            })
        }
    }
}

impl IssueWikiMutationDryRunReceipt {
    fn validate_idempotency_key(&self) -> Result<(), String> {
        let expected = issue_wiki_mutation_idempotency_key(self);
        if self.idempotency_key == expected {
            Ok(())
        } else {
            Err(format!(
                "issue_wiki idempotency_key_mismatch expected={} actual={}",
                expected.as_str(),
                self.idempotency_key.as_str()
            ))
        }
    }
}

fn reference_targets(mutation: &IssueWikiStoreMutation) -> Vec<String> {
    mutation
        .references
        .iter()
        .map(|reference| reference.target.clone())
        .collect()
}

fn issue_wiki_mutation_idempotency_key(receipt: &IssueWikiMutationDryRunReceipt) -> ContentHashHex {
    let mut hasher = blake3::Hasher::new();
    hash_part(&mut hasher, receipt.schema_id.as_bytes());
    hash_part(&mut hasher, receipt.action.as_str().as_bytes());
    hash_part(&mut hasher, receipt.identity.as_bytes());
    hash_part(&mut hasher, receipt.title.as_bytes());
    for reference in &receipt.references {
        hash_part(&mut hasher, reference.target.as_bytes());
        hash_span(&mut hasher, &reference.span);
    }
    hash_part(&mut hasher, receipt.source_ref.as_bytes());
    hash_part(&mut hasher, receipt.source_hash.as_str().as_bytes());
    for conformance in &receipt.conformance_receipts {
        hash_part(&mut hasher, conformance.source_hash.as_str().as_bytes());
        hash_part(&mut hasher, conformance.field_path.as_bytes());
        hash_part(&mut hasher, conformance.value_hash.as_str().as_bytes());
        hash_span(&mut hasher, &conformance.span);
    }
    hash_part(&mut hasher, receipt.transform_id.as_bytes());
    if let Some(reason) = &receipt.conflict_reason {
        hash_part(&mut hasher, reason.as_bytes());
    }
    ContentHashHex::new(hasher.finalize().to_hex().to_string())
        .expect("blake3 lowercase hex is always valid")
}

fn hash_span(hasher: &mut blake3::Hasher, span: &SourceSpan) {
    hash_part(hasher, span.source_hash.as_str().as_bytes());
    hash_part(hasher, &span.start.to_le_bytes());
    hash_part(hasher, &span.end.to_le_bytes());
    if let Some(label) = &span.label {
        hash_part(hasher, label.as_bytes());
    }
}

fn hash_part(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

impl fmt::Display for IssueWikiProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceHashMismatch {
                source_ref,
                expected,
                actual,
            } => write!(
                f,
                "issue_wiki source_hash_mismatch source_ref={source_ref} expected={} actual={}",
                expected.as_str(),
                actual.as_str()
            ),
            Self::SourceRefMismatch { expected, actual } => write!(
                f,
                "issue_wiki source_ref_mismatch expected={expected} actual={actual}"
            ),
            Self::EnvelopeStateMismatch { actual } => write!(
                f,
                "issue_wiki envelope_state_mismatch expected=accepted actual={actual}"
            ),
            Self::CapabilityMismatch {
                visible_for_resolution,
                usable_as_positive_evidence,
                score,
            } => write!(
                f,
                "issue_wiki capability_mismatch visible_for_resolution={visible_for_resolution} usable_as_positive_evidence={usable_as_positive_evidence} score={score:?}"
            ),
            Self::SpanHashMismatch {
                field_path,
                expected,
                actual,
            } => write!(
                f,
                "issue_wiki span_hash_mismatch field={field_path} expected={} actual={}",
                expected.as_str(),
                actual.as_str()
            ),
            Self::SpanRangeInvalid {
                field_path,
                start,
                end,
            } => write!(
                f,
                "issue_wiki span_range_invalid field={field_path} start={start} end={end}"
            ),
            Self::NonAcceptedProjection { state, source_ref } => write!(
                f,
                "issue_wiki non_accepted_projection state={state} source_ref={source_ref}"
            ),
            Self::ConformanceMismatch { reason } => {
                write!(f, "issue_wiki conformance_mismatch {reason}")
            }
        }
    }
}

impl std::error::Error for IssueWikiProjectionError {}

impl fmt::Display for IssueWikiProjectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accepted => f.write_str("accepted"),
            Self::Stale => f.write_str("stale"),
            Self::Malformed => f.write_str("malformed"),
        }
    }
}

impl IssueWikiMutationAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Noop => "noop",
            Self::Conflict => "conflict",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{ScoreContribution, PROJECTION_SCHEMA_ID_V0};

    fn hash(seed: u8) -> ContentHashHex {
        ContentHashHex::new(format!("{seed:064x}")).unwrap()
    }

    fn field(path: &str) -> FieldRef {
        FieldRef {
            path: path.to_owned(),
        }
    }

    fn span(source_hash: ContentHashHex, start: u64, end: u64, label: &str) -> SourceSpan {
        SourceSpan {
            source_hash,
            start,
            end,
            label: Some(label.to_owned()),
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

    fn envelope(
        state: IssueWikiProjectionState,
        source_hash: ContentHashHex,
    ) -> IssueWikiProjectionEnvelope {
        IssueWikiProjectionEnvelope {
            projection_state: state,
            capability: match state {
                IssueWikiProjectionState::Accepted => positive_capability(),
                IssueWikiProjectionState::Stale | IssueWikiProjectionState::Malformed => {
                    neutral_capability()
                }
            },
            source_ref: "github:gwild/attractor#43".to_owned(),
            source_hash,
            transform_id: "issue_wiki.v0".to_owned(),
            field_refs: vec![field("title"), field("body.references")],
        }
    }

    fn accepted() -> IssueWikiAccepted {
        let source_hash = hash(1);
        IssueWikiAccepted {
            schema_id: ISSUE_WIKI_SCHEMA_ID_V0.to_owned(),
            source: IssueWikiSource {
                repo: "gwild/attractor".to_owned(),
                issue_number: 43,
                source_ref: "github:gwild/attractor#43".to_owned(),
                source_hash: source_hash.clone(),
            },
            note: IssueWikiNoteEvidence {
                identity: SpanBoundText {
                    value: "attractor.issue.43.issue-to-wiki-migration".to_owned(),
                    span: span(source_hash.clone(), 0, 32, "issue.number"),
                },
                title: SpanBoundText {
                    value: "Track issue-to-wiki migration contract".to_owned(),
                    span: span(source_hash.clone(), 33, 76, "issue.title"),
                },
                references: vec![IssueWikiReference {
                    target: "wiki:Attractor/Issue-to-Wiki-Migration".to_owned(),
                    label: Some(SpanBoundText {
                        value: "issue-to-wiki migration".to_owned(),
                        span: span(source_hash.clone(), 104, 127, "issue.body"),
                    }),
                    span: span(source_hash.clone(), 96, 127, "issue.body"),
                }],
            },
            conformance_receipts: vec![
                IssueWikiConformanceReceipt {
                    source_hash: source_hash.clone(),
                    field_path: "identity".to_owned(),
                    value_hash: issue_wiki_value_hash("attractor.issue.43.issue-to-wiki-migration"),
                    span: span(source_hash.clone(), 0, 32, "issue.number"),
                },
                IssueWikiConformanceReceipt {
                    source_hash: source_hash.clone(),
                    field_path: "title".to_owned(),
                    value_hash: issue_wiki_value_hash("Track issue-to-wiki migration contract"),
                    span: span(source_hash.clone(), 33, 76, "issue.title"),
                },
                IssueWikiConformanceReceipt {
                    source_hash: source_hash.clone(),
                    field_path: "references[0].target".to_owned(),
                    value_hash: issue_wiki_value_hash("wiki:Attractor/Issue-to-Wiki-Migration"),
                    span: span(source_hash.clone(), 96, 127, "issue.body"),
                },
                IssueWikiConformanceReceipt {
                    source_hash: source_hash.clone(),
                    field_path: "references[0].label".to_owned(),
                    value_hash: issue_wiki_value_hash("issue-to-wiki migration"),
                    span: span(source_hash.clone(), 104, 127, "issue.body"),
                },
            ],
            envelope: envelope(IssueWikiProjectionState::Accepted, source_hash),
        }
    }

    #[test]
    fn accepted_issue_wiki_projection_round_trips_without_raw_body() {
        let row = IssueWikiProjectionResult::Accepted(Box::new(accepted()));
        let json = serde_json::to_string(&row).unwrap();

        assert!(json.contains(r#""projection_state":"accepted""#));
        assert!(json.contains("Track issue-to-wiki migration contract"));
        assert!(!json.contains("RAW ISSUE BODY"));
        assert!(!json.contains("raw_issue_body"));

        let back: IssueWikiProjectionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(row, back);
    }

    #[test]
    fn accepted_projection_builds_mutation_only_after_source_hash_check() {
        let row = accepted();
        let mutation = row.to_store_mutation(&row.source.source_hash).unwrap();

        assert_eq!(
            mutation.identity,
            "attractor.issue.43.issue-to-wiki-migration"
        );
        assert_eq!(mutation.title, "Track issue-to-wiki migration contract");
        assert_eq!(
            mutation.references,
            vec![IssueWikiMutationReference {
                target: "wiki:Attractor/Issue-to-Wiki-Migration".to_owned(),
                span: span(row.source.source_hash.clone(), 96, 127, "issue.body"),
            }]
        );
        assert_eq!(mutation.source_hash, row.source.source_hash);
        assert_eq!(mutation.source_spans.len(), 4);
    }

    #[test]
    fn accepted_projection_builds_dry_run_create_receipt_without_raw_body() {
        let row = accepted();
        let projection = IssueWikiProjectionResult::Accepted(Box::new(row.clone()));

        let receipt = plan_issue_wiki_mutation(&projection, None).unwrap();
        let replayed = plan_issue_wiki_mutation(&projection, None).unwrap();
        let json = serde_json::to_string(&receipt).unwrap();

        assert_eq!(receipt.schema_id, ISSUE_WIKI_MUTATION_PLAN_SCHEMA_ID_V0);
        assert_eq!(receipt.action, IssueWikiMutationAction::Create);
        assert_eq!(
            receipt.identity,
            "attractor.issue.43.issue-to-wiki-migration"
        );
        assert_eq!(receipt.source_ref, row.source.source_ref);
        assert_eq!(receipt.source_hash, row.source.source_hash);
        assert_eq!(receipt.conformance_receipts, row.conformance_receipts);
        assert_eq!(receipt.idempotency_key, replayed.idempotency_key);
        assert!(!json.contains("RAW ISSUE BODY"));
        assert!(!json.contains("raw_issue_body"));
    }

    #[test]
    fn dry_run_receipt_rejects_idempotency_key_tamper_on_deserialize() {
        let row = accepted();
        let projection = IssueWikiProjectionResult::Accepted(Box::new(row));
        let receipt = plan_issue_wiki_mutation(&projection, None).unwrap();
        let mut value = serde_json::to_value(receipt).unwrap();
        value["idempotency_key"] = serde_json::Value::String(hash(9).as_str().to_owned());

        let err = serde_json::from_value::<IssueWikiMutationDryRunReceipt>(value).unwrap_err();

        assert!(err.to_string().contains("idempotency_key_mismatch"));
    }

    #[test]
    fn accepted_row_rejects_typed_span_tamper_with_unchanged_receipt() {
        let row = IssueWikiProjectionResult::Accepted(Box::new(accepted()));
        let mut value = serde_json::to_value(row).unwrap();
        value["row"]["note"]["title"]["span"]["start"] = serde_json::json!(34_u64);

        let err = serde_json::from_value::<IssueWikiProjectionResult>(value).unwrap_err();

        assert!(err
            .to_string()
            .contains("issue_wiki conformance_span_mismatch field=title"));
    }

    #[test]
    fn accepted_projection_dry_run_is_idempotent_noop_for_matching_note() {
        let row = accepted();
        let current = IssueWikiCurrentNote {
            identity: row.note.identity.value.clone(),
            title: row.note.title.value.clone(),
            references: row
                .note
                .references
                .iter()
                .map(|reference| reference.target.clone())
                .collect(),
        };
        let projection = IssueWikiProjectionResult::Accepted(Box::new(row));

        let first = plan_issue_wiki_mutation(&projection, Some(&current)).unwrap();
        let second = plan_issue_wiki_mutation(&projection, Some(&current)).unwrap();

        assert_eq!(first.action, IssueWikiMutationAction::Noop);
        assert_eq!(first, second);
    }

    #[test]
    fn accepted_projection_dry_run_selects_update_for_changed_note() {
        let row = accepted();
        let current = IssueWikiCurrentNote {
            identity: row.note.identity.value.clone(),
            title: "Old title".to_owned(),
            references: vec![],
        };
        let projection = IssueWikiProjectionResult::Accepted(Box::new(row));

        let receipt = plan_issue_wiki_mutation(&projection, Some(&current)).unwrap();

        assert_eq!(receipt.action, IssueWikiMutationAction::Update);
        assert!(receipt.conflict_reason.is_none());
    }

    #[test]
    fn accepted_projection_dry_run_selects_conflict_for_wrong_identity() {
        let row = accepted();
        let current = IssueWikiCurrentNote {
            identity: "attractor.issue.99.other".to_owned(),
            title: row.note.title.value.clone(),
            references: row
                .note
                .references
                .iter()
                .map(|reference| reference.target.clone())
                .collect(),
        };
        let projection = IssueWikiProjectionResult::Accepted(Box::new(row));

        let receipt = plan_issue_wiki_mutation(&projection, Some(&current)).unwrap();

        assert_eq!(receipt.action, IssueWikiMutationAction::Conflict);
        assert!(receipt
            .conflict_reason
            .as_deref()
            .unwrap()
            .contains("identity_conflict"));
    }

    #[test]
    fn stale_and_malformed_rows_refuse_dry_run_mutation() {
        let stale = IssueWikiProjectionResult::Stale(Box::new(IssueWikiStale {
            schema_id: ISSUE_WIKI_SCHEMA_ID_V0.to_owned(),
            source_ref: "github:gwild/attractor#43".to_owned(),
            expected_source_hash: hash(1),
            actual_source_hash: hash(2),
            envelope: envelope(IssueWikiProjectionState::Stale, hash(2)),
        }));
        let malformed = IssueWikiProjectionResult::Malformed(Box::new(IssueWikiMalformed {
            schema_id: Some(ISSUE_WIKI_SCHEMA_ID_V0.to_owned()),
            source_ref: "github:gwild/attractor#43".to_owned(),
            reason: "missing_title_span".to_owned(),
            field_refs: vec![field("title")],
            envelope: envelope(IssueWikiProjectionState::Malformed, hash(3)),
        }));

        for (row, state) in [
            (stale, IssueWikiProjectionState::Stale),
            (malformed, IssueWikiProjectionState::Malformed),
        ] {
            let err = plan_issue_wiki_mutation(&row, None).unwrap_err();
            assert!(matches!(
                err,
                IssueWikiProjectionError::NonAcceptedProjection { state: actual, .. }
                if actual == state
            ));
            assert!(err.to_string().contains("non_accepted_projection"));
        }
    }

    #[test]
    fn dry_run_refuses_accepted_row_with_stale_value_hash_receipt() {
        let mut row = accepted();
        row.note.title.value = "Tampered title".to_owned();
        let projection = IssueWikiProjectionResult::Accepted(Box::new(row));

        let err = plan_issue_wiki_mutation(&projection, None).unwrap_err();

        assert!(matches!(
            err,
            IssueWikiProjectionError::ConformanceMismatch { .. }
        ));
        assert!(err.to_string().contains("conformance_value_hash_mismatch"));
    }

    #[test]
    fn source_hash_mismatch_fails_before_store_mutation() {
        let row = accepted();
        let err = row.to_store_mutation(&hash(9)).unwrap_err();

        assert!(matches!(
            err,
            IssueWikiProjectionError::SourceHashMismatch { .. }
        ));
        assert!(err.to_string().contains("source_hash_mismatch"));
    }

    #[test]
    fn span_hash_mismatch_fails_before_store_mutation() {
        let mut row = accepted();
        row.note.title.span.source_hash = hash(8);
        let err = row.to_store_mutation(&row.source.source_hash).unwrap_err();

        assert!(matches!(
            err,
            IssueWikiProjectionError::SpanHashMismatch { .. }
        ));
        assert!(err.to_string().contains("field=title"));
    }

    #[test]
    fn source_ref_mismatch_fails_before_store_mutation() {
        let mut row = accepted();
        row.envelope.source_ref = "github:gwild/attractor#99".to_owned();
        let err = row.to_store_mutation(&row.source.source_hash).unwrap_err();

        assert!(matches!(
            err,
            IssueWikiProjectionError::SourceRefMismatch { .. }
        ));
        assert!(err.to_string().contains("source_ref_mismatch"));
    }

    #[test]
    fn non_accepted_envelope_state_fails_before_store_mutation() {
        let mut row = accepted();
        row.envelope.projection_state = IssueWikiProjectionState::Stale;
        row.envelope.capability = neutral_capability();
        let err = row.to_store_mutation(&row.source.source_hash).unwrap_err();

        assert!(matches!(
            err,
            IssueWikiProjectionError::EnvelopeStateMismatch { .. }
        ));
        assert!(err.to_string().contains("envelope_state_mismatch"));
    }

    #[test]
    fn non_positive_capability_fails_before_store_mutation() {
        let mut row = accepted();
        row.envelope.capability = neutral_capability();
        let err = row.to_store_mutation(&row.source.source_hash).unwrap_err();

        assert!(matches!(
            err,
            IssueWikiProjectionError::CapabilityMismatch { .. }
        ));
        assert!(err.to_string().contains("capability_mismatch"));
    }

    #[test]
    fn invalid_span_range_fails_before_store_mutation() {
        let mut row = accepted();
        row.note.title.span.start = 76;
        row.note.title.span.end = 33;
        let err = row.to_store_mutation(&row.source.source_hash).unwrap_err();

        assert!(matches!(
            err,
            IssueWikiProjectionError::SpanRangeInvalid { .. }
        ));
        assert!(err.to_string().contains("span_range_invalid"));
    }

    #[test]
    fn stale_and_malformed_rows_stay_visible_but_not_positive() {
        let stale = IssueWikiProjectionResult::Stale(Box::new(IssueWikiStale {
            schema_id: ISSUE_WIKI_SCHEMA_ID_V0.to_owned(),
            source_ref: "github:gwild/attractor#43".to_owned(),
            expected_source_hash: hash(1),
            actual_source_hash: hash(2),
            envelope: envelope(IssueWikiProjectionState::Stale, hash(2)),
        }));
        let malformed = IssueWikiProjectionResult::Malformed(Box::new(IssueWikiMalformed {
            schema_id: Some(ISSUE_WIKI_SCHEMA_ID_V0.to_owned()),
            source_ref: "github:gwild/attractor#43".to_owned(),
            reason: "missing_title_span".to_owned(),
            field_refs: vec![field("title")],
            envelope: envelope(IssueWikiProjectionState::Malformed, hash(3)),
        }));

        for row in [stale, malformed] {
            let json = serde_json::to_string(&row).unwrap();
            let back: IssueWikiProjectionResult = serde_json::from_str(&json).unwrap();
            let capability = match back {
                IssueWikiProjectionResult::Stale(row) => row.envelope.capability,
                IssueWikiProjectionResult::Malformed(row) => row.envelope.capability,
                IssueWikiProjectionResult::Accepted(_) => panic!("expected quarantine row"),
            };
            assert!(capability.visible_for_resolution);
            assert!(!capability.usable_as_positive_evidence);
            assert_eq!(capability.score, ScoreContribution::Neutral);
        }
    }

    #[test]
    fn issue_wiki_result_rejects_tag_envelope_state_disagreement() {
        let row = serde_json::json!({
            "projection_state": "accepted",
            "row": {
                "schema_id": ISSUE_WIKI_SCHEMA_ID_V0,
                "source": accepted().source,
                "note": accepted().note,
                "conformance_receipts": accepted().conformance_receipts,
                "envelope": envelope(IssueWikiProjectionState::Stale, hash(1))
            }
        });
        let err = serde_json::from_value::<IssueWikiProjectionResult>(row).unwrap_err();
        assert!(err
            .to_string()
            .contains("issue_wiki projection_state mismatch: tag=accepted, envelope=stale"));
    }

    #[test]
    fn accepted_row_rejects_stored_value_hash_mismatch_without_source_bytes() {
        let row = IssueWikiProjectionResult::Accepted(Box::new(accepted()));
        let mut value = serde_json::to_value(row).unwrap();
        value["row"]["note"]["title"]["value"] =
            serde_json::Value::String("Tampered title".to_owned());

        let err = serde_json::from_value::<IssueWikiProjectionResult>(value).unwrap_err();
        assert!(err
            .to_string()
            .contains("issue_wiki conformance_value_hash_mismatch field=title"));
    }

    #[test]
    fn issue_wiki_schema_is_distinct_from_projection_spine_schema() {
        assert_ne!(ISSUE_WIKI_SCHEMA_ID_V0, PROJECTION_SCHEMA_ID_V0);
    }
}
