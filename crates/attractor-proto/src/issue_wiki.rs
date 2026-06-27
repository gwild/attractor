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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiAccepted {
    /// Schema id this projection used.
    pub schema_id: String,
    /// Source issue identity and hash.
    pub source: IssueWikiSource,
    /// Typed wiki-note evidence. This deliberately omits raw issue body text.
    pub note: IssueWikiNoteEvidence,
    /// Shared evidence envelope.
    pub envelope: IssueWikiProjectionEnvelope,
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
}

impl IssueWikiAccepted {
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
                "envelope": envelope(IssueWikiProjectionState::Stale, hash(1))
            }
        });
        let err = serde_json::from_value::<IssueWikiProjectionResult>(row).unwrap_err();
        assert!(err
            .to_string()
            .contains("issue_wiki projection_state mismatch: tag=accepted, envelope=stale"));
    }

    #[test]
    fn issue_wiki_schema_is_distinct_from_projection_spine_schema() {
        assert_ne!(ISSUE_WIKI_SCHEMA_ID_V0, PROJECTION_SCHEMA_ID_V0);
    }
}
