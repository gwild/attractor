//! Local issue-wiki materializer.
//!
//! This crate is the first write-boundary slice after the no-IO apply receipt:
//! it consumes a self-validating [`IssueWikiApplyReceipt`], applies it to a
//! caller-supplied wiki tree, and emits a durable write receipt. It performs no
//! network IO and no git push.

use attractor_proto::{
    issue_wiki_page_snapshot_hash, ContentHashHex, IssueWikiApplyAction, IssueWikiApplyReceipt,
    IssueWikiPageSnapshot,
};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Stable schema id for local issue-wiki write receipts.
pub const ISSUE_WIKI_WRITE_RECEIPT_SCHEMA_ID_V0: &str = "attractor.issue_wiki.write_receipt.v0";

/// Materialized local write action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWikiWriteAction {
    /// The target page was created.
    Create,
    /// The target page was replaced with deterministic typed content.
    Update,
    /// The target page already contained the deterministic typed content.
    Noop,
}

impl IssueWikiWriteAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Noop => "noop",
        }
    }
}

/// Durable receipt emitted after a local wiki-tree materialization attempt
/// succeeds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IssueWikiWriteReceipt {
    /// Schema id this write receipt used.
    pub schema_id: String,
    /// Local write action performed.
    pub action: IssueWikiWriteAction,
    /// Contract page path from the consumed apply receipt.
    pub page_path: String,
    /// Normalized relative file path under the caller-supplied wiki root.
    pub relative_file_path: String,
    /// Apply receipt key this local write consumed.
    pub apply_key: ContentHashHex,
    /// Whether the page existed in the local wiki tree before any write.
    pub pre_write_exists: bool,
    /// Snapshot observed in the local wiki tree before any write.
    pub pre_write_snapshot: IssueWikiPageSnapshot,
    /// Whether the page existed in the local wiki tree after write/noop.
    pub post_write_exists: bool,
    /// Snapshot observed after the write/noop.
    pub post_write_snapshot: IssueWikiPageSnapshot,
    /// Deterministic key for idempotent write receipt replay.
    pub write_key: ContentHashHex,
}

impl<'de> Deserialize<'de> for IssueWikiWriteReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let receipt = IssueWikiWriteReceiptWire::deserialize(deserializer)?;
        let receipt = Self {
            schema_id: receipt.schema_id,
            action: receipt.action,
            page_path: receipt.page_path,
            relative_file_path: receipt.relative_file_path,
            apply_key: receipt.apply_key,
            pre_write_exists: receipt.pre_write_exists,
            pre_write_snapshot: receipt.pre_write_snapshot,
            post_write_exists: receipt.post_write_exists,
            post_write_snapshot: receipt.post_write_snapshot,
            write_key: receipt.write_key,
        };
        receipt
            .validate_write_key()
            .map_err(serde::de::Error::custom)?;
        receipt
            .validate_structural_invariants()
            .map_err(serde::de::Error::custom)?;
        Ok(receipt)
    }
}

#[derive(Deserialize)]
struct IssueWikiWriteReceiptWire {
    schema_id: String,
    action: IssueWikiWriteAction,
    page_path: String,
    relative_file_path: String,
    apply_key: ContentHashHex,
    pre_write_exists: bool,
    pre_write_snapshot: IssueWikiPageSnapshot,
    post_write_exists: bool,
    post_write_snapshot: IssueWikiPageSnapshot,
    write_key: ContentHashHex,
}

impl IssueWikiWriteReceipt {
    fn validate_write_key(&self) -> Result<(), String> {
        let expected = issue_wiki_write_key(self);
        if self.write_key == expected {
            Ok(())
        } else {
            Err(format!(
                "issue_wiki write_key_mismatch expected={} actual={}",
                expected.as_str(),
                self.write_key.as_str()
            ))
        }
    }

    fn validate_structural_invariants(&self) -> Result<(), String> {
        if self.schema_id != ISSUE_WIKI_WRITE_RECEIPT_SCHEMA_ID_V0 {
            return Err(format!(
                "issue_wiki write_schema_mismatch expected={} actual={}",
                ISSUE_WIKI_WRITE_RECEIPT_SCHEMA_ID_V0, self.schema_id
            ));
        }
        if self.page_path != self.pre_write_snapshot.page_path {
            return Err(format!(
                "issue_wiki write_pre_snapshot_path_mismatch page_path={} snapshot_path={}",
                self.page_path, self.pre_write_snapshot.page_path
            ));
        }
        if self.page_path != self.post_write_snapshot.page_path {
            return Err(format!(
                "issue_wiki write_post_snapshot_path_mismatch page_path={} snapshot_path={}",
                self.page_path, self.post_write_snapshot.page_path
            ));
        }
        let expected_relative = relative_page_file_path(&self.page_path)
            .map_err(|err| err.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        if self.relative_file_path != expected_relative {
            return Err(format!(
                "issue_wiki write_relative_path_mismatch expected={} actual={}",
                expected_relative, self.relative_file_path
            ));
        }
        match self.action {
            IssueWikiWriteAction::Create => {
                if self.pre_write_exists {
                    return Err("issue_wiki write_create_pre_exists".to_owned());
                }
                if !self.post_write_exists {
                    return Err("issue_wiki write_create_post_missing".to_owned());
                }
                if self.pre_write_snapshot == self.post_write_snapshot {
                    return Err(
                        "issue_wiki write_create_snapshot_mismatch pre_and_post_equal".to_owned(),
                    );
                }
            }
            IssueWikiWriteAction::Update => {
                if !self.pre_write_exists {
                    return Err("issue_wiki write_update_pre_missing".to_owned());
                }
                if !self.post_write_exists {
                    return Err("issue_wiki write_update_post_missing".to_owned());
                }
                if self.pre_write_snapshot == self.post_write_snapshot {
                    return Err(format!(
                        "issue_wiki write_action_snapshot_mismatch action={} pre_and_post_equal",
                        self.action.as_str()
                    ));
                }
            }
            IssueWikiWriteAction::Noop => {
                if self.pre_write_exists != self.post_write_exists {
                    return Err(
                        "issue_wiki write_noop_existence_mismatch pre_and_post_differ".to_owned(),
                    );
                }
                if self.pre_write_snapshot != self.post_write_snapshot {
                    return Err(
                        "issue_wiki write_noop_snapshot_mismatch pre_and_post_differ".to_owned(),
                    );
                }
            }
        }
        Ok(())
    }
}

/// Materializer errors. All failures happen before mutating the target file.
#[derive(Debug)]
pub enum IssueWikiWriteError {
    /// The apply receipt was serialized but failed its own deserialize proof.
    InvalidApplyReceipt { reason: String },
    /// Conflict apply receipts must be handled by replanning, not by writing.
    RefusedConflict { reason: String },
    /// The page path cannot be mapped safely under the caller-supplied wiki root.
    UnsafePagePath { page_path: String, reason: String },
    /// The local page changed since the apply receipt's observed snapshot and
    /// does not already equal the deterministic target bytes.
    NonFastForward {
        page_path: String,
        expected_hash: ContentHashHex,
        actual_hash: ContentHashHex,
    },
    /// Filesystem error.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for IssueWikiWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidApplyReceipt { reason } => {
                write!(f, "issue_wiki invalid_apply_receipt {reason}")
            }
            Self::RefusedConflict { reason } => write!(f, "issue_wiki refused_conflict {reason}"),
            Self::UnsafePagePath { page_path, reason } => write!(
                f,
                "issue_wiki unsafe_page_path page_path={page_path} reason={reason}"
            ),
            Self::NonFastForward {
                page_path,
                expected_hash,
                actual_hash,
            } => write!(
                f,
                "issue_wiki non_fast_forward page_path={page_path} expected={} actual={}",
                expected_hash.as_str(),
                actual_hash.as_str()
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for IssueWikiWriteError {}

/// Parse a serialized apply receipt, letting `attractor-proto` verify its
/// embedded apply/dry-run/snapshot proofs, then materialize it into `wiki_root`.
///
/// # Errors
/// Returns [`IssueWikiWriteError`] if the receipt is invalid, conflicts, drifts,
/// or cannot be written.
pub fn materialize_issue_wiki_apply_receipt_json(
    wiki_root: impl AsRef<Path>,
    receipt_json: &[u8],
) -> Result<IssueWikiWriteReceipt, IssueWikiWriteError> {
    let receipt: IssueWikiApplyReceipt = serde_json::from_slice(receipt_json).map_err(|err| {
        IssueWikiWriteError::InvalidApplyReceipt {
            reason: err.to_string(),
        }
    })?;
    materialize_issue_wiki_apply_receipt(wiki_root, &receipt)
}

/// Materialize an already self-validating apply receipt into a local wiki tree.
///
/// Replaying the same apply receipt after a successful create/update is
/// idempotent: when the current page already equals the deterministic typed
/// target bytes, the function emits a `noop` write receipt. If the current page
/// is neither the apply receipt's observed snapshot nor the deterministic target
/// bytes, the function returns `non_fast_forward` and leaves the file untouched.
///
/// # Errors
/// Returns [`IssueWikiWriteError`] before writing on conflict, unsafe path,
/// snapshot drift, or filesystem failure.
pub fn materialize_issue_wiki_apply_receipt(
    wiki_root: impl AsRef<Path>,
    receipt: &IssueWikiApplyReceipt,
) -> Result<IssueWikiWriteReceipt, IssueWikiWriteError> {
    let receipt = revalidate_apply_receipt(receipt)?;
    if receipt.action == IssueWikiApplyAction::Conflict {
        return Err(IssueWikiWriteError::RefusedConflict {
            reason: receipt
                .conflict_reason
                .clone()
                .unwrap_or_else(|| "issue_wiki apply receipt is conflict".to_owned()),
        });
    }

    let relative = relative_page_file_path(&receipt.actual_snapshot.page_path)?;
    let path = wiki_root.as_ref().join(&relative);
    let (pre_write_exists, before) = read_existing_or_empty(&path)?;
    let pre_write_snapshot =
        IssueWikiPageSnapshot::from_page_bytes(receipt.actual_snapshot.page_path.clone(), &before);
    let target = render_issue_wiki_page(&receipt);
    let target_hash =
        issue_wiki_page_snapshot_hash(&receipt.actual_snapshot.page_path, target.as_bytes());

    let action = if pre_write_snapshot.page_hash == target_hash {
        IssueWikiWriteAction::Noop
    } else if pre_write_snapshot != receipt.actual_snapshot {
        return Err(IssueWikiWriteError::NonFastForward {
            page_path: receipt.actual_snapshot.page_path.clone(),
            expected_hash: receipt.actual_snapshot.page_hash.clone(),
            actual_hash: pre_write_snapshot.page_hash,
        });
    } else {
        match receipt.action {
            IssueWikiApplyAction::Create => IssueWikiWriteAction::Create,
            IssueWikiApplyAction::Update => IssueWikiWriteAction::Update,
            IssueWikiApplyAction::Noop => IssueWikiWriteAction::Noop,
            IssueWikiApplyAction::Conflict => unreachable!("conflict returned above"),
        }
    };

    if action != IssueWikiWriteAction::Noop {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| IssueWikiWriteError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::write(&path, target.as_bytes()).map_err(|source| IssueWikiWriteError::Io {
            path: path.clone(),
            source,
        })?;
    }

    let (post_write_exists, after) = read_existing_or_empty(&path)?;
    let post_write_snapshot =
        IssueWikiPageSnapshot::from_page_bytes(receipt.actual_snapshot.page_path.clone(), &after);
    let relative_file_path = relative.to_string_lossy().replace('\\', "/");
    let out = IssueWikiWriteReceipt {
        schema_id: ISSUE_WIKI_WRITE_RECEIPT_SCHEMA_ID_V0.to_owned(),
        action,
        page_path: receipt.actual_snapshot.page_path.clone(),
        relative_file_path,
        apply_key: receipt.apply_key.clone(),
        pre_write_exists,
        pre_write_snapshot,
        post_write_exists,
        post_write_snapshot,
        write_key: ContentHashHex::new("0".repeat(64))
            .expect("zero placeholder is valid lowercase hex"),
    };
    Ok(IssueWikiWriteReceipt {
        write_key: issue_wiki_write_key(&out),
        ..out
    })
}

fn revalidate_apply_receipt(
    receipt: &IssueWikiApplyReceipt,
) -> Result<IssueWikiApplyReceipt, IssueWikiWriteError> {
    let value =
        serde_json::to_value(receipt).map_err(|err| IssueWikiWriteError::InvalidApplyReceipt {
            reason: err.to_string(),
        })?;
    serde_json::from_value(value).map_err(|err| IssueWikiWriteError::InvalidApplyReceipt {
        reason: err.to_string(),
    })
}

/// Deterministic full-page markdown emitted by the local materializer.
///
/// It contains only typed receipt fields: identity, title, reference targets,
/// and receipt provenance keys. It never carries raw issue body text.
#[must_use]
pub fn render_issue_wiki_page(receipt: &IssueWikiApplyReceipt) -> String {
    let mut out = String::new();
    out.push_str("# ");
    out.push_str(&receipt.title);
    out.push_str("\n\n");
    out.push_str("<!-- attractor:issue-wiki-materialized.v0 -->\n");
    out.push_str("identity: ");
    out.push_str(&receipt.identity);
    out.push('\n');
    out.push_str("source_ref: ");
    out.push_str(&receipt.source_ref);
    out.push('\n');
    out.push_str("source_hash: ");
    out.push_str(receipt.source_hash.as_str());
    out.push('\n');
    out.push_str("apply_key: ");
    out.push_str(receipt.apply_key.as_str());
    out.push_str("\n\n");
    out.push_str("## References\n");
    if receipt.references.is_empty() {
        out.push_str("- none\n");
    } else {
        for reference in &receipt.references {
            out.push_str("- ");
            out.push_str(&reference.target);
            out.push('\n');
        }
    }
    out
}

fn read_existing_or_empty(path: &Path) -> Result<(bool, Vec<u8>), IssueWikiWriteError> {
    match fs::read(path) {
        Ok(bytes) => Ok((true, bytes)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok((false, Vec::new())),
        Err(source) => Err(IssueWikiWriteError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn relative_page_file_path(page_path: &str) -> Result<PathBuf, IssueWikiWriteError> {
    if page_path.trim().is_empty() {
        return Err(IssueWikiWriteError::UnsafePagePath {
            page_path: page_path.to_owned(),
            reason: "empty path".to_owned(),
        });
    }
    if page_path.contains(':') {
        return Err(IssueWikiWriteError::UnsafePagePath {
            page_path: page_path.to_owned(),
            reason: "colon/drive prefixes are not allowed".to_owned(),
        });
    }
    let normalized = page_path.replace('\\', "/");
    let mut out = PathBuf::new();
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(IssueWikiWriteError::UnsafePagePath {
                page_path: page_path.to_owned(),
                reason: "empty, current, or parent segment".to_owned(),
            });
        }
        let component_path = Path::new(segment);
        if component_path.components().any(|component| {
            !matches!(
                component,
                Component::Normal(_) | Component::CurDir | Component::ParentDir
            )
        }) {
            return Err(IssueWikiWriteError::UnsafePagePath {
                page_path: page_path.to_owned(),
                reason: "absolute or prefixed component".to_owned(),
            });
        }
        out.push(segment);
    }
    if out.extension().is_none() {
        out.set_extension("md");
    }
    Ok(out)
}

fn issue_wiki_write_key(receipt: &IssueWikiWriteReceipt) -> ContentHashHex {
    let mut hasher = blake3::Hasher::new();
    hash_part(&mut hasher, receipt.schema_id.as_bytes());
    hash_part(&mut hasher, receipt.action.as_str().as_bytes());
    hash_part(&mut hasher, receipt.page_path.as_bytes());
    hash_part(&mut hasher, receipt.relative_file_path.as_bytes());
    hash_part(&mut hasher, receipt.apply_key.as_str().as_bytes());
    hash_part(&mut hasher, &[u8::from(receipt.pre_write_exists)]);
    hash_page_snapshot(&mut hasher, &receipt.pre_write_snapshot);
    hash_part(&mut hasher, &[u8::from(receipt.post_write_exists)]);
    hash_page_snapshot(&mut hasher, &receipt.post_write_snapshot);
    ContentHashHex::new(hasher.finalize().to_hex().to_string())
        .expect("blake3 lowercase hex is always valid")
}

fn hash_page_snapshot(hasher: &mut blake3::Hasher, snapshot: &IssueWikiPageSnapshot) {
    hash_part(hasher, snapshot.page_path.as_bytes());
    hash_part(hasher, snapshot.page_hash.as_str().as_bytes());
}

fn hash_part(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use attractor_proto::{
        apply_issue_wiki_mutation_dry_run, plan_issue_wiki_mutation, EvidenceCapability, FieldRef,
        IssueWikiAccepted, IssueWikiConformanceReceipt, IssueWikiCurrentNote,
        IssueWikiMutationAction, IssueWikiNoteEvidence, IssueWikiProjectionEnvelope,
        IssueWikiProjectionResult, IssueWikiProjectionState, IssueWikiReference, IssueWikiSource,
        ScoreContribution, SourceSpan, SpanBoundText, ISSUE_WIKI_SCHEMA_ID_V0,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "attractor_wiki_test_{}_{}",
                std::process::id(),
                n
            ));
            if path.exists() {
                fs::remove_dir_all(&path).unwrap();
            }
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn hash_bytes(bytes: &[u8]) -> ContentHashHex {
        ContentHashHex::new(blake3::hash(bytes).to_hex().to_string()).unwrap()
    }

    fn value_hash(value: &str) -> ContentHashHex {
        hash_bytes(value.as_bytes())
    }

    fn span(source_hash: &ContentHashHex, start: u64, end: u64) -> SourceSpan {
        SourceSpan {
            source_hash: source_hash.clone(),
            start,
            end,
            label: Some("issue:50".to_owned()),
        }
    }

    fn conformance(
        source_hash: &ContentHashHex,
        field_path: &str,
        value: &str,
        span: SourceSpan,
    ) -> IssueWikiConformanceReceipt {
        IssueWikiConformanceReceipt {
            source_hash: source_hash.clone(),
            field_path: field_path.to_owned(),
            value_hash: value_hash(value),
            span,
        }
    }

    fn projection() -> IssueWikiProjectionResult {
        let source_hash = hash_bytes(b"typed issue fixture");
        let identity = "attractor.issue.43.issue-to-wiki-migration";
        let title = "Track issue-to-wiki migration contract";
        let target = "wiki:Attractor/Issue-to-Wiki-Migration";
        let identity_span = span(&source_hash, 0, identity.len() as u64);
        let title_span = span(&source_hash, 100, 100 + title.len() as u64);
        let target_span = span(&source_hash, 200, 200 + target.len() as u64);

        IssueWikiProjectionResult::Accepted(Box::new(IssueWikiAccepted {
            schema_id: ISSUE_WIKI_SCHEMA_ID_V0.to_owned(),
            source: IssueWikiSource {
                repo: "gwild/attractor".to_owned(),
                issue_number: 50,
                source_ref: "github:gwild/attractor#50".to_owned(),
                source_hash: source_hash.clone(),
            },
            note: IssueWikiNoteEvidence {
                identity: SpanBoundText {
                    value: identity.to_owned(),
                    span: identity_span.clone(),
                },
                title: SpanBoundText {
                    value: title.to_owned(),
                    span: title_span.clone(),
                },
                references: vec![IssueWikiReference {
                    target: target.to_owned(),
                    label: None,
                    span: target_span.clone(),
                }],
            },
            conformance_receipts: vec![
                conformance(&source_hash, "identity", identity, identity_span),
                conformance(&source_hash, "title", title, title_span),
                conformance(&source_hash, "references[0].target", target, target_span),
            ],
            envelope: IssueWikiProjectionEnvelope {
                projection_state: IssueWikiProjectionState::Accepted,
                capability: EvidenceCapability {
                    visible_for_resolution: true,
                    usable_as_positive_evidence: true,
                    score: ScoreContribution::Positive,
                },
                source_ref: "github:gwild/attractor#50".to_owned(),
                source_hash,
                transform_id: "issue_wiki.v0".to_owned(),
                field_refs: vec![
                    FieldRef {
                        path: "identity".to_owned(),
                    },
                    FieldRef {
                        path: "title".to_owned(),
                    },
                    FieldRef {
                        path: "references[0].target".to_owned(),
                    },
                ],
            },
        }))
    }

    fn receipt_for(
        current_note: Option<IssueWikiCurrentNote>,
        expected_bytes: &[u8],
        actual_bytes: &[u8],
    ) -> IssueWikiApplyReceipt {
        receipt_for_page_path(
            "Attractor/Issue-to-Wiki-Migration",
            current_note,
            expected_bytes,
            actual_bytes,
        )
    }

    fn receipt_for_page_path(
        page_path: &str,
        current_note: Option<IssueWikiCurrentNote>,
        expected_bytes: &[u8],
        actual_bytes: &[u8],
    ) -> IssueWikiApplyReceipt {
        let projection = projection();
        let dry_run = plan_issue_wiki_mutation(&projection, current_note.as_ref()).unwrap();
        let expected = IssueWikiPageSnapshot::from_page_bytes(page_path, expected_bytes);
        let actual = IssueWikiPageSnapshot::from_page_bytes(page_path, actual_bytes);
        apply_issue_wiki_mutation_dry_run(&dry_run, &expected, &actual).unwrap()
    }

    #[test]
    fn create_writes_page_and_replay_is_noop() {
        let dir = TestDir::new();
        let receipt = receipt_for(None, b"", b"");

        let first = materialize_issue_wiki_apply_receipt(dir.path(), &receipt).unwrap();
        let second = materialize_issue_wiki_apply_receipt(dir.path(), &receipt).unwrap();
        let page =
            fs::read_to_string(dir.path().join("Attractor/Issue-to-Wiki-Migration.md")).unwrap();

        assert_eq!(first.action, IssueWikiWriteAction::Create);
        assert!(!first.pre_write_exists);
        assert!(first.post_write_exists);
        assert_eq!(second.action, IssueWikiWriteAction::Noop);
        assert!(second.pre_write_exists);
        assert!(second.post_write_exists);
        assert_eq!(first.post_write_snapshot, second.pre_write_snapshot);
        assert!(page.contains("# Track issue-to-wiki migration contract"));
        assert!(page.contains("wiki:Attractor/Issue-to-Wiki-Migration"));
        assert!(!page.contains("RAW ISSUE BODY"));
    }

    #[test]
    fn update_replaces_matching_snapshot_page() {
        let dir = TestDir::new();
        let path = dir.path().join("Attractor/Issue-to-Wiki-Migration.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old typed page").unwrap();
        let current = IssueWikiCurrentNote {
            identity: "attractor.issue.43.issue-to-wiki-migration".to_owned(),
            title: "Old title".to_owned(),
            references: vec![],
        };
        let receipt = receipt_for(Some(current), b"old typed page", b"old typed page");

        let write = materialize_issue_wiki_apply_receipt(dir.path(), &receipt).unwrap();
        let page = fs::read_to_string(path).unwrap();

        assert_eq!(receipt.dry_run_action, IssueWikiMutationAction::Update);
        assert_eq!(write.action, IssueWikiWriteAction::Update);
        assert!(write.pre_write_exists);
        assert!(write.post_write_exists);
        assert!(page.contains("# Track issue-to-wiki migration contract"));
    }

    #[test]
    fn matching_desired_page_is_noop() {
        let dir = TestDir::new();
        let receipt = receipt_for(None, b"", b"");
        let target = render_issue_wiki_page(&receipt);
        let path = dir.path().join("Attractor/Issue-to-Wiki-Migration.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, target).unwrap();

        let write = materialize_issue_wiki_apply_receipt(dir.path(), &receipt).unwrap();

        assert_eq!(write.action, IssueWikiWriteAction::Noop);
        assert!(write.pre_write_exists);
        assert!(write.post_write_exists);
        assert_eq!(write.pre_write_snapshot, write.post_write_snapshot);
    }

    #[test]
    fn drifted_page_conflicts_without_write() {
        let dir = TestDir::new();
        let path = dir.path().join("Attractor/Issue-to-Wiki-Migration.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "edited by somebody else").unwrap();
        let receipt = receipt_for(None, b"", b"");

        let err = materialize_issue_wiki_apply_receipt(dir.path(), &receipt).unwrap_err();
        let after = fs::read_to_string(path).unwrap();

        assert!(matches!(err, IssueWikiWriteError::NonFastForward { .. }));
        assert_eq!(after, "edited by somebody else");
    }

    #[test]
    fn conflict_apply_receipt_is_refused() {
        let dir = TestDir::new();
        let receipt = receipt_for(None, b"expected", b"actual");

        let err = materialize_issue_wiki_apply_receipt(dir.path(), &receipt).unwrap_err();

        assert_eq!(receipt.action, IssueWikiApplyAction::Conflict);
        assert!(matches!(err, IssueWikiWriteError::RefusedConflict { .. }));
    }

    #[test]
    fn invalid_apply_receipt_json_is_refused_before_write() {
        let dir = TestDir::new();
        let receipt = receipt_for(None, b"", b"");
        let mut value = serde_json::to_value(receipt).unwrap();
        value["title"] = serde_json::Value::String("tampered".to_owned());
        let bytes = serde_json::to_vec(&value).unwrap();

        let err = materialize_issue_wiki_apply_receipt_json(dir.path(), &bytes).unwrap_err();

        assert!(matches!(
            err,
            IssueWikiWriteError::InvalidApplyReceipt { .. }
        ));
        assert!(!dir
            .path()
            .join("Attractor/Issue-to-Wiki-Migration.md")
            .exists());
    }

    #[test]
    fn write_receipt_rejects_key_tamper_on_deserialize() {
        let dir = TestDir::new();
        let receipt = receipt_for(None, b"", b"");
        let write = materialize_issue_wiki_apply_receipt(dir.path(), &receipt).unwrap();
        let mut value = serde_json::to_value(write).unwrap();
        value["action"] = serde_json::Value::String("noop".to_owned());

        let err = serde_json::from_value::<IssueWikiWriteReceipt>(value).unwrap_err();

        assert!(err.to_string().contains("write_key_mismatch"));
    }

    #[test]
    fn write_receipt_rejects_structural_tamper_even_with_recomputed_key() {
        let dir = TestDir::new();
        let receipt = receipt_for(None, b"", b"");
        let mut write = materialize_issue_wiki_apply_receipt(dir.path(), &receipt).unwrap();
        write.action = IssueWikiWriteAction::Noop;
        write.write_key = issue_wiki_write_key(&write);
        let value = serde_json::to_value(write).unwrap();

        let err = serde_json::from_value::<IssueWikiWriteReceipt>(value).unwrap_err();

        assert!(err.to_string().contains("write_noop_existence_mismatch"));
    }

    #[test]
    fn write_receipt_rejects_create_to_update_tamper_even_with_recomputed_key() {
        let dir = TestDir::new();
        let receipt = receipt_for(None, b"", b"");
        let mut write = materialize_issue_wiki_apply_receipt(dir.path(), &receipt).unwrap();
        assert_eq!(write.action, IssueWikiWriteAction::Create);
        assert!(!write.pre_write_exists);
        write.action = IssueWikiWriteAction::Update;
        write.write_key = issue_wiki_write_key(&write);
        let value = serde_json::to_value(write).unwrap();

        let err = serde_json::from_value::<IssueWikiWriteReceipt>(value).unwrap_err();

        assert!(err.to_string().contains("write_update_pre_missing"));
    }

    #[test]
    fn write_receipt_rejects_path_tamper_even_with_recomputed_key() {
        let dir = TestDir::new();
        let receipt = receipt_for(None, b"", b"");
        let mut write = materialize_issue_wiki_apply_receipt(dir.path(), &receipt).unwrap();
        write.relative_file_path = "Elsewhere.md".to_owned();
        write.write_key = issue_wiki_write_key(&write);
        let value = serde_json::to_value(write).unwrap();

        let err = serde_json::from_value::<IssueWikiWriteReceipt>(value).unwrap_err();

        assert!(err.to_string().contains("write_relative_path_mismatch"));
    }

    #[test]
    fn unsafe_page_path_is_rejected() {
        let dir = TestDir::new();
        let receipt = receipt_for_page_path("../outside", None, b"", b"");

        let err = materialize_issue_wiki_apply_receipt(dir.path(), &receipt).unwrap_err();

        assert!(matches!(err, IssueWikiWriteError::UnsafePagePath { .. }));
    }
}
