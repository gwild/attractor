//! Issue-wiki materializer and publish boundary.
//!
//! This crate is the first write-boundary slice after the no-IO apply receipt:
//! it consumes a self-validating [`IssueWikiApplyReceipt`], applies it to a
//! caller-supplied wiki tree, and emits a durable write receipt. The publish
//! layer consumes only a frozen [`IssueWikiBatchManifest`] plus its explicit
//! manifest hash, then delegates live IO to an opt-in transport.

use attractor_projector::{project_issue_wiki, ISSUE_WIKI_PROJECTOR};
use attractor_proto::{
    apply_issue_wiki_mutation_dry_run, issue_wiki_page_snapshot_hash, plan_issue_wiki_mutation,
    ContentHashHex, IssueWikiApplyAction, IssueWikiApplyReceipt, IssueWikiCurrentNote,
    IssueWikiPageSnapshot, IssueWikiProjectionResult, IssueWikiSource,
    ISSUE_WIKI_APPLY_RECEIPT_SCHEMA_ID_V0, ISSUE_WIKI_MUTATION_PLAN_SCHEMA_ID_V0,
    ISSUE_WIKI_SCHEMA_ID_V0,
};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Stable schema id for local issue-wiki write receipts.
pub const ISSUE_WIKI_WRITE_RECEIPT_SCHEMA_ID_V0: &str = "attractor.issue_wiki.write_receipt.v0";
/// Stable schema id for issue-wiki batch manifests.
pub const ISSUE_WIKI_BATCH_MANIFEST_SCHEMA_ID_V0: &str = "attractor.issue_wiki.batch_manifest.v0";
/// Stable schema id for issue-wiki publish receipts.
pub const ISSUE_WIKI_PUBLISH_RECEIPT_SCHEMA_ID_V0: &str = "attractor.issue_wiki.publish_receipt.v0";
/// Stable schema id for issue-wiki batch driver receipts.
pub const ISSUE_WIKI_BATCH_RUN_RECEIPT_SCHEMA_ID_V0: &str =
    "attractor.issue_wiki.batch_run_receipt.v0";

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

/// Publish-stage action for one manifest page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWikiPublishAction {
    /// The publisher wrote the manifest-pinned page content into the checkout.
    Published,
    /// The checkout already contained the manifest-pinned page content.
    Noop,
}

impl IssueWikiPublishAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Noop => "noop",
        }
    }
}

/// Orchestration-stage action for a batch driver receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWikiBatchRunAction {
    /// A pinned export was projected and materialized into a frozen manifest.
    Prepared,
    /// The prepared manifest changed the live wiki checkout and transport ran.
    Published,
    /// The prepared manifest was already reflected in the live wiki checkout.
    Noop,
}

impl IssueWikiBatchRunAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Published => "published",
            Self::Noop => "noop",
        }
    }
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

/// Caller-owned source export for a local batch migration run.
///
/// The source bytes are consumed to compute `issue_export_hash` and run the
/// extractor. They are deliberately skipped on serialization so durable
/// manifests never carry raw issue body text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiBatchExport {
    /// Stable selector for the issue set, for example `repo=gwild/attractor state=open`.
    pub issue_set_selector: String,
    /// Exact issue payloads in the export.
    pub issues: Vec<IssueWikiBatchIssue>,
}

/// One issue payload in a caller-owned batch export.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiBatchIssue {
    /// Source issue identity and declared payload hash.
    pub source: IssueWikiSource,
    /// Exact source payload bytes consumed by the projector.
    #[serde(skip)]
    pub source_bytes: Vec<u8>,
}

/// Batch issue status recorded in the durable manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueWikiBatchStatus {
    /// The issue wrote or updated a page.
    Written,
    /// The target page already matched the issue-derived content.
    Noop,
    /// The issue could not be applied because the page snapshot drifted or the
    /// dry-run found a typed conflict.
    Conflict,
    /// The source hash changed before projection.
    Stale,
    /// The issue source could not be projected.
    Malformed,
}

impl IssueWikiBatchStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Written => "written",
            Self::Noop => "noop",
            Self::Conflict => "conflict",
            Self::Stale => "stale",
            Self::Malformed => "malformed",
        }
    }
}

/// One issue result in the durable batch manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiBatchManifestEntry {
    /// Stable source reference, for example `github:gwild/attractor#43`.
    pub source_ref: String,
    /// Issue number in the source repository.
    pub issue_number: u64,
    /// Source hash declared by the export row.
    pub source_hash: ContentHashHex,
    /// Target wiki page path selected by the pipeline.
    pub page_path: String,
    /// Batch result status.
    pub status: IssueWikiBatchStatus,
    /// Apply receipt key when an apply receipt was built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_key: Option<ContentHashHex>,
    /// Write receipt key when the local materializer emitted one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_key: Option<ContentHashHex>,
    /// Full apply receipt for conflict/noop/write replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_receipt: Option<IssueWikiApplyReceipt>,
    /// Full local write receipt for successful materializations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_receipt: Option<IssueWikiWriteReceipt>,
    /// Machine-readable refusal or malformedness reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Durable no-network manifest for an issue-to-wiki batch run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IssueWikiBatchManifest {
    /// Schema id this manifest used.
    pub schema_id: String,
    /// Hash of the exact caller-owned issue export bytes and selector.
    pub issue_export_hash: ContentHashHex,
    /// Stable selector for the issue set.
    pub issue_set_selector: String,
    /// Projection schema id consumed by this batch.
    pub projection_schema_id: String,
    /// Stable projector implementation id consumed by this batch.
    pub projector_id: String,
    /// Digest of the projection/apply/write schema tuple this manifest expects.
    pub projection_schema_digest: ContentHashHex,
    /// Per-issue write/conflict/noop/stale/malformed results.
    pub entries: Vec<IssueWikiBatchManifestEntry>,
    /// Deterministic key for publish-stage replay.
    pub manifest_key: ContentHashHex,
}

/// One page result in a publish receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiPublishReceiptEntry {
    /// Manifest page path that was checked.
    pub page_path: String,
    /// Manifest write receipt key this publish entry consumed.
    pub write_key: ContentHashHex,
    /// Publish-stage action.
    pub action: IssueWikiPublishAction,
    /// Snapshot observed in the wiki checkout before publish handling.
    pub pre_publish_snapshot: IssueWikiPageSnapshot,
    /// Snapshot observed in the wiki checkout after publish handling.
    pub post_publish_snapshot: IssueWikiPageSnapshot,
}

/// Durable receipt emitted after a manifest-bound publish attempt succeeds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IssueWikiPublishReceipt {
    /// Schema id this publish receipt used.
    pub schema_id: String,
    /// Frozen manifest key consumed by the publish boundary.
    pub manifest_key: ContentHashHex,
    /// Projection schema digest carried by the consumed manifest.
    pub projection_schema_digest: ContentHashHex,
    /// Deterministic tree hash over all manifest write pages before publish.
    pub pre_tree_hash: ContentHashHex,
    /// Deterministic tree hash over all manifest write pages after publish.
    pub post_tree_hash: ContentHashHex,
    /// Per-page publish/noop results.
    pub entries: Vec<IssueWikiPublishReceiptEntry>,
    /// Whether the live transport was entered.
    pub pushed: bool,
    /// Deterministic key for idempotent publish receipt replay.
    pub publish_key: ContentHashHex,
}

/// Stable summary counts for a batch driver receipt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWikiBatchRunSummary {
    /// Manifest entries with written status.
    pub written: u64,
    /// Manifest entries with noop status.
    pub noop: u64,
    /// Manifest entries with conflict status.
    pub conflict: u64,
    /// Manifest entries with stale status.
    pub stale: u64,
    /// Manifest entries with malformed status.
    pub malformed: u64,
    /// Publish entries that changed the live checkout.
    pub publish_published: u64,
    /// Publish entries that were already present in the live checkout.
    pub publish_noop: u64,
}

/// Durable receipt emitted by the issue-wiki batch orchestration driver.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IssueWikiBatchRunReceipt {
    /// Schema id this batch run receipt used.
    pub schema_id: String,
    /// Orchestration-stage action.
    pub action: IssueWikiBatchRunAction,
    /// Hash of the exact caller-owned issue export bytes and selector.
    pub issue_export_hash: ContentHashHex,
    /// Stable selector from the pinned export.
    pub issue_set_selector: String,
    /// Frozen manifest key handed to the publish boundary.
    pub manifest_key: ContentHashHex,
    /// Count summary bound into the receipt.
    pub summary: IssueWikiBatchRunSummary,
    /// Frozen manifest emitted by the prepare stage.
    pub manifest: IssueWikiBatchManifest,
    /// Publish receipt when the prepared manifest was live-published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_receipt: Option<IssueWikiPublishReceipt>,
    /// Deterministic key for idempotent batch-run receipt replay.
    pub run_key: ContentHashHex,
}

impl<'de> Deserialize<'de> for IssueWikiBatchManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let manifest = IssueWikiBatchManifestWire::deserialize(deserializer)?;
        let manifest = Self {
            schema_id: manifest.schema_id,
            issue_export_hash: manifest.issue_export_hash,
            issue_set_selector: manifest.issue_set_selector,
            projection_schema_id: manifest.projection_schema_id,
            projector_id: manifest.projector_id,
            projection_schema_digest: manifest.projection_schema_digest,
            entries: manifest.entries,
            manifest_key: manifest.manifest_key,
        };
        manifest
            .validate_manifest_key()
            .map_err(serde::de::Error::custom)?;
        manifest
            .validate_structural_invariants()
            .map_err(serde::de::Error::custom)?;
        Ok(manifest)
    }
}

#[derive(Deserialize)]
struct IssueWikiBatchManifestWire {
    schema_id: String,
    issue_export_hash: ContentHashHex,
    issue_set_selector: String,
    projection_schema_id: String,
    projector_id: String,
    projection_schema_digest: ContentHashHex,
    entries: Vec<IssueWikiBatchManifestEntry>,
    manifest_key: ContentHashHex,
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

impl<'de> Deserialize<'de> for IssueWikiPublishReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let receipt = IssueWikiPublishReceiptWire::deserialize(deserializer)?;
        let receipt = Self {
            schema_id: receipt.schema_id,
            manifest_key: receipt.manifest_key,
            projection_schema_digest: receipt.projection_schema_digest,
            pre_tree_hash: receipt.pre_tree_hash,
            post_tree_hash: receipt.post_tree_hash,
            entries: receipt.entries,
            pushed: receipt.pushed,
            publish_key: receipt.publish_key,
        };
        receipt
            .validate_publish_key()
            .map_err(serde::de::Error::custom)?;
        receipt
            .validate_structural_invariants()
            .map_err(serde::de::Error::custom)?;
        Ok(receipt)
    }
}

#[derive(Deserialize)]
struct IssueWikiPublishReceiptWire {
    schema_id: String,
    manifest_key: ContentHashHex,
    projection_schema_digest: ContentHashHex,
    pre_tree_hash: ContentHashHex,
    post_tree_hash: ContentHashHex,
    entries: Vec<IssueWikiPublishReceiptEntry>,
    pushed: bool,
    publish_key: ContentHashHex,
}

impl<'de> Deserialize<'de> for IssueWikiBatchRunReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let receipt = IssueWikiBatchRunReceiptWire::deserialize(deserializer)?;
        let receipt = Self {
            schema_id: receipt.schema_id,
            action: receipt.action,
            issue_export_hash: receipt.issue_export_hash,
            issue_set_selector: receipt.issue_set_selector,
            manifest_key: receipt.manifest_key,
            summary: receipt.summary,
            manifest: receipt.manifest,
            publish_receipt: receipt.publish_receipt,
            run_key: receipt.run_key,
        };
        receipt
            .validate_run_key()
            .map_err(serde::de::Error::custom)?;
        receipt
            .validate_structural_invariants()
            .map_err(serde::de::Error::custom)?;
        Ok(receipt)
    }
}

#[derive(Deserialize)]
struct IssueWikiBatchRunReceiptWire {
    schema_id: String,
    action: IssueWikiBatchRunAction,
    issue_export_hash: ContentHashHex,
    issue_set_selector: String,
    manifest_key: ContentHashHex,
    summary: IssueWikiBatchRunSummary,
    manifest: IssueWikiBatchManifest,
    #[serde(default)]
    publish_receipt: Option<IssueWikiPublishReceipt>,
    run_key: ContentHashHex,
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

impl IssueWikiPublishReceipt {
    fn validate_publish_key(&self) -> Result<(), String> {
        let expected = issue_wiki_publish_key(self);
        if self.publish_key == expected {
            Ok(())
        } else {
            Err(format!(
                "issue_wiki publish_key_mismatch expected={} actual={}",
                expected.as_str(),
                self.publish_key.as_str()
            ))
        }
    }

    fn validate_structural_invariants(&self) -> Result<(), String> {
        if self.schema_id != ISSUE_WIKI_PUBLISH_RECEIPT_SCHEMA_ID_V0 {
            return Err(format!(
                "issue_wiki publish_schema_mismatch expected={} actual={}",
                ISSUE_WIKI_PUBLISH_RECEIPT_SCHEMA_ID_V0, self.schema_id
            ));
        }
        for entry in &self.entries {
            if entry.page_path != entry.pre_publish_snapshot.page_path {
                return Err(format!(
                    "issue_wiki publish_pre_snapshot_path_mismatch page_path={} snapshot_path={}",
                    entry.page_path, entry.pre_publish_snapshot.page_path
                ));
            }
            if entry.page_path != entry.post_publish_snapshot.page_path {
                return Err(format!(
                    "issue_wiki publish_post_snapshot_path_mismatch page_path={} snapshot_path={}",
                    entry.page_path, entry.post_publish_snapshot.page_path
                ));
            }
            match entry.action {
                IssueWikiPublishAction::Published => {
                    if entry.pre_publish_snapshot == entry.post_publish_snapshot {
                        return Err("issue_wiki publish_entry_published_pre_post_equal".to_owned());
                    }
                }
                IssueWikiPublishAction::Noop => {
                    if entry.pre_publish_snapshot != entry.post_publish_snapshot {
                        return Err("issue_wiki publish_entry_noop_pre_post_differ".to_owned());
                    }
                }
            }
        }
        if !self.pushed && self.pre_tree_hash != self.post_tree_hash {
            return Err("issue_wiki publish_tree_changed_without_push".to_owned());
        }
        let any_published = self
            .entries
            .iter()
            .any(|entry| entry.action == IssueWikiPublishAction::Published);
        if self.pushed != any_published {
            return Err(format!(
                "issue_wiki publish_pushed_action_mismatch pushed={} any_published={}",
                self.pushed, any_published
            ));
        }
        Ok(())
    }
}

impl IssueWikiBatchRunReceipt {
    fn validate_run_key(&self) -> Result<(), String> {
        let expected = issue_wiki_batch_run_key(self);
        if self.run_key == expected {
            Ok(())
        } else {
            Err(format!(
                "issue_wiki batch_run_key_mismatch expected={} actual={}",
                expected.as_str(),
                self.run_key.as_str()
            ))
        }
    }

    fn validate_structural_invariants(&self) -> Result<(), String> {
        if self.schema_id != ISSUE_WIKI_BATCH_RUN_RECEIPT_SCHEMA_ID_V0 {
            return Err(format!(
                "issue_wiki batch_run_schema_mismatch expected={} actual={}",
                ISSUE_WIKI_BATCH_RUN_RECEIPT_SCHEMA_ID_V0, self.schema_id
            ));
        }
        if self.issue_export_hash != self.manifest.issue_export_hash {
            return Err(format!(
                "issue_wiki batch_run_export_hash_mismatch receipt={} manifest={}",
                self.issue_export_hash.as_str(),
                self.manifest.issue_export_hash.as_str()
            ));
        }
        if self.issue_set_selector != self.manifest.issue_set_selector {
            return Err(format!(
                "issue_wiki batch_run_selector_mismatch receipt={} manifest={}",
                self.issue_set_selector, self.manifest.issue_set_selector
            ));
        }
        if self.manifest_key != self.manifest.manifest_key {
            return Err(format!(
                "issue_wiki batch_run_manifest_key_mismatch receipt={} manifest={}",
                self.manifest_key.as_str(),
                self.manifest.manifest_key.as_str()
            ));
        }
        let expected_summary =
            issue_wiki_batch_run_summary(&self.manifest, self.publish_receipt.as_ref());
        if self.summary != expected_summary {
            return Err("issue_wiki batch_run_summary_mismatch".to_owned());
        }
        match (&self.action, &self.publish_receipt) {
            (IssueWikiBatchRunAction::Prepared, None) => Ok(()),
            (IssueWikiBatchRunAction::Prepared, Some(_)) => {
                Err("issue_wiki batch_run_prepared_has_publish_receipt".to_owned())
            }
            (IssueWikiBatchRunAction::Published, Some(publish)) => {
                if !publish.pushed {
                    return Err("issue_wiki batch_run_published_without_push".to_owned());
                }
                self.validate_publish_binding(publish)
            }
            (IssueWikiBatchRunAction::Noop, Some(publish)) => {
                if publish.pushed {
                    return Err("issue_wiki batch_run_noop_with_push".to_owned());
                }
                self.validate_publish_binding(publish)
            }
            (IssueWikiBatchRunAction::Published | IssueWikiBatchRunAction::Noop, None) => {
                Err("issue_wiki batch_run_publish_action_missing_receipt".to_owned())
            }
        }
    }

    fn validate_publish_binding(&self, publish: &IssueWikiPublishReceipt) -> Result<(), String> {
        if publish.manifest_key != self.manifest_key {
            return Err(format!(
                "issue_wiki batch_run_publish_manifest_key_mismatch receipt={} publish={}",
                self.manifest_key.as_str(),
                publish.manifest_key.as_str()
            ));
        }
        if publish.projection_schema_digest != self.manifest.projection_schema_digest {
            return Err(format!(
                "issue_wiki batch_run_publish_projection_digest_mismatch manifest={} publish={}",
                self.manifest.projection_schema_digest.as_str(),
                publish.projection_schema_digest.as_str()
            ));
        }
        Ok(())
    }
}

impl IssueWikiBatchManifest {
    fn validate_manifest_key(&self) -> Result<(), String> {
        if self.schema_id != ISSUE_WIKI_BATCH_MANIFEST_SCHEMA_ID_V0 {
            return Err(format!(
                "issue_wiki batch_manifest_schema_mismatch expected={} actual={}",
                ISSUE_WIKI_BATCH_MANIFEST_SCHEMA_ID_V0, self.schema_id
            ));
        }
        let expected_digest = issue_wiki_projection_schema_digest();
        if self.projection_schema_digest != expected_digest {
            return Err(format!(
                "issue_wiki batch_manifest_projection_schema_digest_mismatch expected={} actual={}",
                expected_digest.as_str(),
                self.projection_schema_digest.as_str()
            ));
        }
        self.validate_manifest_key_without_projection_digest()
    }

    fn validate_manifest_key_without_projection_digest(&self) -> Result<(), String> {
        if self.schema_id != ISSUE_WIKI_BATCH_MANIFEST_SCHEMA_ID_V0 {
            return Err(format!(
                "issue_wiki batch_manifest_schema_mismatch expected={} actual={}",
                ISSUE_WIKI_BATCH_MANIFEST_SCHEMA_ID_V0, self.schema_id
            ));
        }
        let expected = issue_wiki_batch_manifest_key(self);
        if self.manifest_key == expected {
            Ok(())
        } else {
            Err(format!(
                "issue_wiki batch_manifest_key_mismatch expected={} actual={}",
                expected.as_str(),
                self.manifest_key.as_str()
            ))
        }
    }

    fn validate_structural_invariants(&self) -> Result<(), String> {
        if self.projection_schema_id != ISSUE_WIKI_SCHEMA_ID_V0 {
            return Err(format!(
                "issue_wiki batch_manifest_projection_schema_mismatch expected={} actual={}",
                ISSUE_WIKI_SCHEMA_ID_V0, self.projection_schema_id
            ));
        }
        if self.projector_id != ISSUE_WIKI_PROJECTOR {
            return Err(format!(
                "issue_wiki batch_manifest_projector_mismatch expected={} actual={}",
                ISSUE_WIKI_PROJECTOR, self.projector_id
            ));
        }
        for entry in &self.entries {
            entry.validate_structural_invariants()?;
        }
        Ok(())
    }
}

impl IssueWikiBatchManifestEntry {
    fn validate_structural_invariants(&self) -> Result<(), String> {
        match self.status {
            IssueWikiBatchStatus::Written => self.validate_written_entry(),
            IssueWikiBatchStatus::Noop => self.validate_noop_entry(),
            IssueWikiBatchStatus::Conflict => self.validate_conflict_entry(),
            IssueWikiBatchStatus::Stale | IssueWikiBatchStatus::Malformed => {
                self.validate_refused_entry()
            }
        }
    }

    fn validate_written_entry(&self) -> Result<(), String> {
        let apply = self.expect_apply_receipt("written")?;
        let write = self.expect_write_receipt("written")?;
        self.validate_common_receipt_binding(apply, Some(write))?;
        if !matches!(
            write.action,
            IssueWikiWriteAction::Create | IssueWikiWriteAction::Update
        ) {
            return Err(format!(
                "issue_wiki batch_manifest_status_action_mismatch status=written write_action={}",
                write.action.as_str()
            ));
        }
        if !matches!(
            apply.action,
            IssueWikiApplyAction::Create | IssueWikiApplyAction::Update
        ) {
            return Err(format!(
                "issue_wiki batch_manifest_status_action_mismatch status=written apply_action={}",
                issue_wiki_apply_action_str(apply.action)
            ));
        }
        if self.reason.is_some() {
            return Err("issue_wiki batch_manifest_written_has_reason".to_owned());
        }
        Ok(())
    }

    fn validate_noop_entry(&self) -> Result<(), String> {
        let apply = self.expect_apply_receipt("noop")?;
        let write = self.expect_write_receipt("noop")?;
        self.validate_common_receipt_binding(apply, Some(write))?;
        if write.action != IssueWikiWriteAction::Noop {
            return Err(format!(
                "issue_wiki batch_manifest_status_action_mismatch status=noop write_action={}",
                write.action.as_str()
            ));
        }
        if apply.action != IssueWikiApplyAction::Noop {
            return Err(format!(
                "issue_wiki batch_manifest_status_action_mismatch status=noop apply_action={}",
                issue_wiki_apply_action_str(apply.action)
            ));
        }
        if self.reason.is_some() {
            return Err("issue_wiki batch_manifest_noop_has_reason".to_owned());
        }
        Ok(())
    }

    fn validate_conflict_entry(&self) -> Result<(), String> {
        let apply = self.expect_apply_receipt("conflict")?;
        self.validate_common_receipt_binding(apply, None)?;
        if apply.action != IssueWikiApplyAction::Conflict {
            return Err(format!(
                "issue_wiki batch_manifest_status_action_mismatch status=conflict apply_action={}",
                issue_wiki_apply_action_str(apply.action)
            ));
        }
        if self.write_key.is_some() || self.write_receipt.is_some() {
            return Err("issue_wiki batch_manifest_conflict_has_write_receipt".to_owned());
        }
        if self.reason.as_deref() != apply.conflict_reason.as_deref() {
            return Err(format!(
                "issue_wiki batch_manifest_conflict_reason_mismatch expected={} actual={}",
                apply.conflict_reason.as_deref().unwrap_or("<none>"),
                self.reason.as_deref().unwrap_or("<none>")
            ));
        }
        Ok(())
    }

    fn validate_refused_entry(&self) -> Result<(), String> {
        if self.apply_key.is_some()
            || self.write_key.is_some()
            || self.apply_receipt.is_some()
            || self.write_receipt.is_some()
        {
            return Err(format!(
                "issue_wiki batch_manifest_refused_has_receipt status={}",
                self.status.as_str()
            ));
        }
        if !self.page_path.is_empty() {
            return Err(format!(
                "issue_wiki batch_manifest_refused_page_path_not_empty status={} page_path={}",
                self.status.as_str(),
                self.page_path
            ));
        }
        if self.reason.is_none() {
            return Err(format!(
                "issue_wiki batch_manifest_refused_missing_reason status={}",
                self.status.as_str()
            ));
        }
        Ok(())
    }

    fn validate_common_receipt_binding(
        &self,
        apply: &IssueWikiApplyReceipt,
        write: Option<&IssueWikiWriteReceipt>,
    ) -> Result<(), String> {
        if self.apply_key.as_ref() != Some(&apply.apply_key) {
            return Err("issue_wiki batch_manifest_apply_key_mismatch".to_owned());
        }
        if self.source_ref != apply.source_ref {
            return Err(format!(
                "issue_wiki batch_manifest_source_ref_mismatch entry={} apply={}",
                self.source_ref, apply.source_ref
            ));
        }
        if self.source_hash != apply.source_hash {
            return Err(format!(
                "issue_wiki batch_manifest_source_hash_mismatch entry={} apply={}",
                self.source_hash.as_str(),
                apply.source_hash.as_str()
            ));
        }
        if self.page_path != apply.actual_snapshot.page_path {
            return Err(format!(
                "issue_wiki batch_manifest_apply_page_mismatch entry={} apply={}",
                self.page_path, apply.actual_snapshot.page_path
            ));
        }
        if let Some(write) = write {
            if self.write_key.as_ref() != Some(&write.write_key) {
                return Err("issue_wiki batch_manifest_write_key_mismatch".to_owned());
            }
            if write.apply_key != apply.apply_key {
                return Err(format!(
                    "issue_wiki batch_manifest_write_apply_key_mismatch write={} apply={}",
                    write.apply_key.as_str(),
                    apply.apply_key.as_str()
                ));
            }
            if write.page_path != self.page_path {
                return Err(format!(
                    "issue_wiki batch_manifest_write_page_mismatch entry={} write={}",
                    self.page_path, write.page_path
                ));
            }
            if write.pre_write_snapshot != apply.actual_snapshot {
                return Err(format!(
                    "issue_wiki batch_manifest_write_pre_snapshot_mismatch write={} apply={}",
                    write.pre_write_snapshot.page_hash.as_str(),
                    apply.actual_snapshot.page_hash.as_str()
                ));
            }
            let expected_post_hash = issue_wiki_page_snapshot_hash(
                &apply.actual_snapshot.page_path,
                render_issue_wiki_page(apply).as_bytes(),
            );
            if write.post_write_snapshot.page_hash != expected_post_hash {
                return Err(format!(
                    "issue_wiki batch_manifest_write_post_snapshot_mismatch expected={} actual={}",
                    expected_post_hash.as_str(),
                    write.post_write_snapshot.page_hash.as_str()
                ));
            }
        } else if self.write_key.is_some() || self.write_receipt.is_some() {
            return Err("issue_wiki batch_manifest_unexpected_write_receipt".to_owned());
        }
        Ok(())
    }

    fn expect_apply_receipt(&self, status: &str) -> Result<&IssueWikiApplyReceipt, String> {
        self.apply_receipt
            .as_ref()
            .ok_or_else(|| format!("issue_wiki batch_manifest_{status}_missing_apply_receipt"))
    }

    fn expect_write_receipt(&self, status: &str) -> Result<&IssueWikiWriteReceipt, String> {
        self.write_receipt
            .as_ref()
            .ok_or_else(|| format!("issue_wiki batch_manifest_{status}_missing_write_receipt"))
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
    /// The wiki root, an intermediate directory, or the target file is a
    /// symlink. The materializer refuses these because otherwise a lexical
    /// under-root path can write outside the caller-supplied tree.
    SymlinkBoundary { path: PathBuf },
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
    /// Projection or apply receipt construction failed.
    Pipeline { source_ref: String, reason: String },
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
            Self::SymlinkBoundary { path } => {
                write!(f, "issue_wiki symlink_boundary path={}", path.display())
            }
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
            Self::Pipeline { source_ref, reason } => {
                write!(
                    f,
                    "issue_wiki pipeline source_ref={source_ref} reason={reason}"
                )
            }
        }
    }
}

impl std::error::Error for IssueWikiWriteError {}

/// Transport used by the publish boundary for the single side effect.
///
/// Tests inject a spy transport. Production callers can use
/// [`IssueWikiGitPushTransport`]. The publish function performs all manifest,
/// drift, path, and replay checks before this trait is entered.
pub trait IssueWikiPublishTransport {
    /// Push the already-updated wiki checkout.
    ///
    /// # Errors
    /// Returns a reason when the transport could not complete the live write.
    fn push_manifest(
        &mut self,
        wiki_root: &Path,
        manifest_key: &ContentHashHex,
        relative_paths: &[PathBuf],
    ) -> Result<(), String>;
}

/// Git-backed publish transport for a checked-out wiki repository.
pub struct IssueWikiGitPushTransport {
    remote: String,
    branch: String,
}

impl IssueWikiGitPushTransport {
    /// Create a git push transport.
    #[must_use]
    pub fn new(remote: impl Into<String>, branch: impl Into<String>) -> Self {
        Self {
            remote: remote.into(),
            branch: branch.into(),
        }
    }
}

impl IssueWikiPublishTransport for IssueWikiGitPushTransport {
    fn push_manifest(
        &mut self,
        wiki_root: &Path,
        manifest_key: &ContentHashHex,
        relative_paths: &[PathBuf],
    ) -> Result<(), String> {
        reject_staged_paths_outside_manifest(wiki_root, relative_paths)?;
        let mut add_args = vec!["add".to_owned(), "--".to_owned()];
        add_args.extend(
            relative_paths
                .iter()
                .map(|path| path.to_string_lossy().replace('\\', "/")),
        );
        run_git_owned(wiki_root, &add_args)?;
        reject_staged_paths_outside_manifest(wiki_root, relative_paths)?;
        let commit_message = format!(
            "Publish issue wiki manifest {}",
            &manifest_key.as_str()[..12]
        );
        run_git(wiki_root, &["commit", "-m", &commit_message])?;
        let destination = format!("HEAD:{}", self.branch);
        run_git(wiki_root, &["push", &self.remote, &destination])
    }
}

fn reject_staged_paths_outside_manifest(
    wiki_root: &Path,
    relative_paths: &[PathBuf],
) -> Result<(), String> {
    let allowed = relative_paths
        .iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect::<std::collections::BTreeSet<_>>();
    let staged = run_git_stdout(wiki_root, &["diff", "--cached", "--name-only", "--"])?;
    for path in staged.lines().filter(|line| !line.trim().is_empty()) {
        if !allowed.contains(path) {
            return Err(format!("git staged path outside manifest: {path}"));
        }
    }
    Ok(())
}

fn run_git(wiki_root: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(wiki_root)
        .args(args)
        .output()
        .map_err(|err| format!("git {} exec failed: {err}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "git {} rc={} stderr={}",
        args.join(" "),
        output
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn run_git_owned(wiki_root: &Path, args: &[String]) -> Result<(), String> {
    let display = args.join(" ");
    let output = Command::new("git")
        .arg("-C")
        .arg(wiki_root)
        .args(args)
        .output()
        .map_err(|err| format!("git {display} exec failed: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "git {display} rc={} stderr={}",
        output
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn run_git_stdout(wiki_root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(wiki_root)
        .args(args)
        .output()
        .map_err(|err| format!("git {} exec failed: {err}", args.join(" ")))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(format!(
        "git {} rc={} stderr={}",
        args.join(" "),
        output
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Publish-boundary errors. All refusal variants happen before transport entry.
#[derive(Debug)]
pub enum IssueWikiPublishError {
    /// Live IO requires an explicit caller opt-in.
    ExplicitOptInRequired,
    /// The supplied manifest failed its own deserialize proof.
    InvalidManifest { reason: String },
    /// The caller did not pass the exact manifest hash being published.
    ManifestKeyMismatch {
        expected: ContentHashHex,
        actual: ContentHashHex,
    },
    /// The manifest was produced by a different projection/schema contract.
    StaleProjection {
        manifest_digest: ContentHashHex,
        live_digest: ContentHashHex,
    },
    /// The page path cannot be mapped safely under the caller-supplied wiki root.
    UnsafePagePath { page_path: String, reason: String },
    /// The wiki root, an intermediate directory, or the target file is a symlink.
    SymlinkBoundary { path: PathBuf },
    /// The checkout changed since the manifest write receipt's pre snapshot and
    /// does not already equal the manifest-pinned post snapshot.
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
    /// The live transport was entered and returned failure.
    Push { reason: String },
}

impl fmt::Display for IssueWikiPublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExplicitOptInRequired => write!(f, "issue_wiki publish explicit_opt_in_required"),
            Self::InvalidManifest { reason } => write!(f, "issue_wiki invalid_manifest {reason}"),
            Self::ManifestKeyMismatch { expected, actual } => write!(
                f,
                "issue_wiki manifest_key_mismatch expected={} actual={}",
                expected.as_str(),
                actual.as_str()
            ),
            Self::StaleProjection {
                manifest_digest,
                live_digest,
            } => write!(
                f,
                "issue_wiki stale_projection manifest_digest={} live_digest={}",
                manifest_digest.as_str(),
                live_digest.as_str()
            ),
            Self::UnsafePagePath { page_path, reason } => write!(
                f,
                "issue_wiki publish_unsafe_page_path page_path={page_path} reason={reason}"
            ),
            Self::SymlinkBoundary { path } => {
                write!(
                    f,
                    "issue_wiki publish_symlink_boundary path={}",
                    path.display()
                )
            }
            Self::NonFastForward {
                page_path,
                expected_hash,
                actual_hash,
            } => write!(
                f,
                "issue_wiki publish_non_fast_forward page_path={page_path} expected={} actual={}",
                expected_hash.as_str(),
                actual_hash.as_str()
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Push { reason } => write!(f, "issue_wiki publish_push_failed {reason}"),
        }
    }
}

impl std::error::Error for IssueWikiPublishError {}

impl From<IssueWikiWriteError> for IssueWikiPublishError {
    fn from(value: IssueWikiWriteError) -> Self {
        match value {
            IssueWikiWriteError::UnsafePagePath { page_path, reason } => {
                Self::UnsafePagePath { page_path, reason }
            }
            IssueWikiWriteError::SymlinkBoundary { path } => Self::SymlinkBoundary { path },
            IssueWikiWriteError::Io { path, source } => Self::Io { path, source },
            IssueWikiWriteError::NonFastForward {
                page_path,
                expected_hash,
                actual_hash,
            } => Self::NonFastForward {
                page_path,
                expected_hash,
                actual_hash,
            },
            other => Self::InvalidManifest {
                reason: other.to_string(),
            },
        }
    }
}

/// Batch orchestration errors. Publish variants preserve the underlying
/// publish-boundary refusal so callers can stop on drift/stale/symlink.
#[derive(Debug)]
pub enum IssueWikiBatchRunError {
    /// Local manifest materialization failed.
    Prepare(IssueWikiWriteError),
    /// A prepared run receipt failed its deserialize proof.
    InvalidPreparedRun { reason: String },
    /// The caller did not pass the exact manifest key being published.
    ManifestKeyMismatch {
        expected: ContentHashHex,
        actual: ContentHashHex,
    },
    /// Manifest serialization failed before publish.
    SerializeManifest { reason: String },
    /// Manifest-bound live publish failed or refused before transport.
    Publish(IssueWikiPublishError),
}

impl fmt::Display for IssueWikiBatchRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare(err) => write!(f, "issue_wiki batch_run_prepare_failed {err}"),
            Self::InvalidPreparedRun { reason } => {
                write!(f, "issue_wiki batch_run_invalid_prepared {reason}")
            }
            Self::ManifestKeyMismatch { expected, actual } => write!(
                f,
                "issue_wiki batch_run_manifest_key_mismatch expected={} actual={}",
                expected.as_str(),
                actual.as_str()
            ),
            Self::SerializeManifest { reason } => {
                write!(f, "issue_wiki batch_run_manifest_serialize_failed {reason}")
            }
            Self::Publish(err) => write!(f, "issue_wiki batch_run_publish_failed {err}"),
        }
    }
}

impl std::error::Error for IssueWikiBatchRunError {}

/// Project and materialize a pinned issue export into a frozen manifest receipt.
///
/// This is the non-side-effecting batch driver handoff: it may write to the
/// caller-owned staging wiki tree, but performs no live publish transport.
///
/// # Errors
/// Returns [`IssueWikiBatchRunError::Prepare`] when projection/materialization
/// cannot complete outside normal per-issue stale/malformed/conflict rows.
pub fn prepare_issue_wiki_batch_run(
    staging_wiki_root: impl AsRef<Path>,
    export: &IssueWikiBatchExport,
) -> Result<IssueWikiBatchRunReceipt, IssueWikiBatchRunError> {
    let manifest = materialize_issue_wiki_batch_manifest(staging_wiki_root, export)
        .map_err(IssueWikiBatchRunError::Prepare)?;
    Ok(issue_wiki_batch_run_receipt(
        IssueWikiBatchRunAction::Prepared,
        manifest,
        None,
    ))
}

/// Publish a previously prepared batch run receipt by explicit manifest key.
///
/// The side-effecting half accepts no issue export, selector, or projector
/// input. It consumes only the self-validating prepared run receipt, the exact
/// manifest key being published, and explicit opt-in.
///
/// # Errors
/// Returns [`IssueWikiBatchRunError`] if the prepared receipt is invalid, the
/// manifest key is not exact, or the publish boundary refuses before transport.
pub fn publish_prepared_issue_wiki_batch_run_json(
    live_wiki_root: impl AsRef<Path>,
    prepared_run_json: &[u8],
    expected_manifest_key: &ContentHashHex,
    explicit_opt_in: bool,
    transport: &mut impl IssueWikiPublishTransport,
) -> Result<IssueWikiBatchRunReceipt, IssueWikiBatchRunError> {
    let prepared =
        serde_json::from_slice::<IssueWikiBatchRunReceipt>(prepared_run_json).map_err(|err| {
            IssueWikiBatchRunError::InvalidPreparedRun {
                reason: err.to_string(),
            }
        })?;
    if prepared.action != IssueWikiBatchRunAction::Prepared || prepared.publish_receipt.is_some() {
        return Err(IssueWikiBatchRunError::InvalidPreparedRun {
            reason: "issue_wiki batch_run_publish_requires_prepared_receipt".to_owned(),
        });
    }
    if &prepared.manifest_key != expected_manifest_key {
        return Err(IssueWikiBatchRunError::ManifestKeyMismatch {
            expected: expected_manifest_key.clone(),
            actual: prepared.manifest_key,
        });
    }
    let manifest_json = serde_json::to_vec(&prepared.manifest).map_err(|err| {
        IssueWikiBatchRunError::SerializeManifest {
            reason: err.to_string(),
        }
    })?;
    let publish = publish_issue_wiki_batch_manifest_json(
        live_wiki_root,
        &manifest_json,
        expected_manifest_key,
        explicit_opt_in,
        transport,
    )
    .map_err(IssueWikiBatchRunError::Publish)?;
    let action = if publish.pushed {
        IssueWikiBatchRunAction::Published
    } else {
        IssueWikiBatchRunAction::Noop
    };
    Ok(issue_wiki_batch_run_receipt(
        action,
        prepared.manifest,
        Some(publish),
    ))
}

/// Publish a frozen issue-wiki batch manifest into a wiki checkout and enter the
/// supplied transport only if the checkout changed.
///
/// The side-effect boundary is intentionally narrow: callers pass the serialized
/// manifest, the exact manifest hash they intend to publish, and an explicit
/// opt-in boolean. There is no issue selector, source export, or projector input
/// here, so publish cannot reselect or regenerate a different plan.
///
/// # Errors
/// Returns [`IssueWikiPublishError`] before transport entry when the manifest
/// hash is not explicit/current, when the projection digest is stale, when a
/// page drifted, or when a symlink/path boundary is unsafe. Transport failures
/// are reported after direct push rc capture by the chosen transport.
pub fn publish_issue_wiki_batch_manifest_json(
    wiki_root: impl AsRef<Path>,
    manifest_json: &[u8],
    expected_manifest_key: &ContentHashHex,
    explicit_opt_in: bool,
    transport: &mut impl IssueWikiPublishTransport,
) -> Result<IssueWikiPublishReceipt, IssueWikiPublishError> {
    publish_issue_wiki_batch_manifest_json_with_digest(
        wiki_root,
        manifest_json,
        expected_manifest_key,
        explicit_opt_in,
        &issue_wiki_projection_schema_digest(),
        transport,
    )
}

fn publish_issue_wiki_batch_manifest_json_with_digest(
    wiki_root: impl AsRef<Path>,
    manifest_json: &[u8],
    expected_manifest_key: &ContentHashHex,
    explicit_opt_in: bool,
    live_projection_schema_digest: &ContentHashHex,
    transport: &mut impl IssueWikiPublishTransport,
) -> Result<IssueWikiPublishReceipt, IssueWikiPublishError> {
    if !explicit_opt_in {
        return Err(IssueWikiPublishError::ExplicitOptInRequired);
    }
    let manifest = deserialize_publish_manifest(manifest_json)?;
    if &manifest.manifest_key != expected_manifest_key {
        return Err(IssueWikiPublishError::ManifestKeyMismatch {
            expected: expected_manifest_key.clone(),
            actual: manifest.manifest_key,
        });
    }
    if &manifest.projection_schema_digest != live_projection_schema_digest {
        return Err(IssueWikiPublishError::StaleProjection {
            manifest_digest: manifest.projection_schema_digest,
            live_digest: live_projection_schema_digest.clone(),
        });
    }
    publish_issue_wiki_batch_manifest(
        wiki_root,
        &manifest,
        live_projection_schema_digest,
        transport,
    )
}

fn deserialize_publish_manifest(
    manifest_json: &[u8],
) -> Result<IssueWikiBatchManifest, IssueWikiPublishError> {
    let manifest =
        serde_json::from_slice::<IssueWikiBatchManifestWire>(manifest_json).map_err(|err| {
            IssueWikiPublishError::InvalidManifest {
                reason: err.to_string(),
            }
        })?;
    let manifest = IssueWikiBatchManifest {
        schema_id: manifest.schema_id,
        issue_export_hash: manifest.issue_export_hash,
        issue_set_selector: manifest.issue_set_selector,
        projection_schema_id: manifest.projection_schema_id,
        projector_id: manifest.projector_id,
        projection_schema_digest: manifest.projection_schema_digest,
        entries: manifest.entries,
        manifest_key: manifest.manifest_key,
    };
    manifest
        .validate_manifest_key_without_projection_digest()
        .map_err(|reason| IssueWikiPublishError::InvalidManifest { reason })?;
    manifest
        .validate_structural_invariants()
        .map_err(|reason| IssueWikiPublishError::InvalidManifest { reason })?;
    Ok(manifest)
}

fn publish_issue_wiki_batch_manifest(
    wiki_root: impl AsRef<Path>,
    manifest: &IssueWikiBatchManifest,
    live_projection_schema_digest: &ContentHashHex,
    transport: &mut impl IssueWikiPublishTransport,
) -> Result<IssueWikiPublishReceipt, IssueWikiPublishError> {
    let wiki_root = wiki_root.as_ref();
    let pre_tree_hash = issue_wiki_manifest_tree_hash(wiki_root, manifest)?;
    let mut plans = Vec::new();

    for manifest_entry in &manifest.entries {
        let (Some(apply), Some(write)) = (
            manifest_entry.apply_receipt.as_ref(),
            manifest_entry.write_receipt.as_ref(),
        ) else {
            continue;
        };
        let relative = relative_page_file_path(&write.page_path)?;
        reject_symlink_boundary(wiki_root, &relative)?;
        let full_path = wiki_root.join(&relative);
        let (_, before) = read_existing_or_empty(&full_path)?;
        let pre_publish_snapshot =
            IssueWikiPageSnapshot::from_page_bytes(write.page_path.clone(), &before);
        let target = render_issue_wiki_page(apply);
        let target_hash = issue_wiki_page_snapshot_hash(&write.page_path, target.as_bytes());
        if target_hash != write.post_write_snapshot.page_hash {
            return Err(IssueWikiPublishError::InvalidManifest {
                reason: format!(
                    "issue_wiki publish_manifest_post_hash_mismatch expected={} actual={}",
                    target_hash.as_str(),
                    write.post_write_snapshot.page_hash.as_str()
                ),
            });
        }

        let action = if pre_publish_snapshot.page_hash == target_hash {
            IssueWikiPublishAction::Noop
        } else if pre_publish_snapshot.page_hash != write.pre_write_snapshot.page_hash {
            return Err(IssueWikiPublishError::NonFastForward {
                page_path: write.page_path.clone(),
                expected_hash: write.pre_write_snapshot.page_hash.clone(),
                actual_hash: pre_publish_snapshot.page_hash,
            });
        } else {
            IssueWikiPublishAction::Published
        };

        plans.push(IssueWikiPublishPlan {
            page_path: write.page_path.clone(),
            write_key: write.write_key.clone(),
            relative,
            target,
            pre_publish_snapshot,
            action,
        });
    }

    for plan in &plans {
        if plan.action != IssueWikiPublishAction::Published {
            continue;
        }
        let full_path = wiki_root.join(&plan.relative);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).map_err(|source| IssueWikiPublishError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        reject_symlink_boundary(wiki_root, &plan.relative)?;
        fs::write(&full_path, plan.target.as_bytes()).map_err(|source| {
            IssueWikiPublishError::Io {
                path: full_path.clone(),
                source,
            }
        })?;
    }

    let mut entries = Vec::with_capacity(plans.len());
    for plan in plans {
        let full_path = wiki_root.join(&plan.relative);
        let (_, after) = read_existing_or_empty(&full_path)?;
        let post_publish_snapshot = IssueWikiPageSnapshot::from_page_bytes(&plan.page_path, &after);
        entries.push(IssueWikiPublishReceiptEntry {
            page_path: plan.page_path,
            write_key: plan.write_key,
            action: plan.action,
            pre_publish_snapshot: plan.pre_publish_snapshot,
            post_publish_snapshot,
        });
    }

    let post_tree_hash = issue_wiki_manifest_tree_hash(wiki_root, manifest)?;
    let changed = entries
        .iter()
        .any(|entry| entry.action == IssueWikiPublishAction::Published);
    let pushed = if changed {
        let push_paths = entries
            .iter()
            .map(|entry| relative_page_file_path(&entry.page_path))
            .collect::<Result<Vec<_>, _>>()?;
        transport
            .push_manifest(wiki_root, &manifest.manifest_key, &push_paths)
            .map_err(|reason| IssueWikiPublishError::Push { reason })?;
        true
    } else {
        false
    };
    let receipt = IssueWikiPublishReceipt {
        schema_id: ISSUE_WIKI_PUBLISH_RECEIPT_SCHEMA_ID_V0.to_owned(),
        manifest_key: manifest.manifest_key.clone(),
        projection_schema_digest: live_projection_schema_digest.clone(),
        pre_tree_hash,
        post_tree_hash,
        entries,
        pushed,
        publish_key: ContentHashHex::new("0".repeat(64))
            .expect("zero placeholder is valid lowercase hex"),
    };
    Ok(IssueWikiPublishReceipt {
        publish_key: issue_wiki_publish_key(&receipt),
        ..receipt
    })
}

struct IssueWikiPublishPlan {
    page_path: String,
    write_key: ContentHashHex,
    relative: PathBuf,
    target: String,
    pre_publish_snapshot: IssueWikiPageSnapshot,
    action: IssueWikiPublishAction,
}

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

/// Run a no-network issue-to-wiki batch into a local wiki tree and emit a
/// durable manifest for the later publish boundary.
///
/// The returned manifest binds the exact issue export hash, issue selector,
/// projection schema/projector identity, and every per-issue apply/write status.
/// It does not store raw source issue bytes and performs no network IO or git
/// push.
///
/// # Errors
/// Returns [`IssueWikiWriteError`] when a projection/apply receipt fails to
/// construct or local filesystem materialization fails outside normal
/// conflict/stale/malformed outcomes.
pub fn materialize_issue_wiki_batch_manifest(
    wiki_root: impl AsRef<Path>,
    export: &IssueWikiBatchExport,
) -> Result<IssueWikiBatchManifest, IssueWikiWriteError> {
    let wiki_root = wiki_root.as_ref();
    let issue_export_hash = issue_wiki_export_hash(export);
    let mut entries = Vec::with_capacity(export.issues.len());

    for issue in &export.issues {
        let projection = project_issue_wiki(issue.source.clone(), &issue.source_bytes);
        let entry = match projection {
            IssueWikiProjectionResult::Accepted(_) => {
                materialize_issue_wiki_batch_entry(wiki_root, &projection)?
            }
            IssueWikiProjectionResult::Stale(row) => IssueWikiBatchManifestEntry {
                source_ref: row.source_ref.clone(),
                issue_number: issue.source.issue_number,
                source_hash: issue.source.source_hash.clone(),
                page_path: String::new(),
                status: IssueWikiBatchStatus::Stale,
                apply_key: None,
                write_key: None,
                apply_receipt: None,
                write_receipt: None,
                reason: Some(format!(
                    "source_hash_stale expected={} actual={}",
                    row.expected_source_hash.as_str(),
                    row.actual_source_hash.as_str()
                )),
            },
            IssueWikiProjectionResult::Malformed(row) => IssueWikiBatchManifestEntry {
                source_ref: row.source_ref.clone(),
                issue_number: issue.source.issue_number,
                source_hash: issue.source.source_hash.clone(),
                page_path: String::new(),
                status: IssueWikiBatchStatus::Malformed,
                apply_key: None,
                write_key: None,
                apply_receipt: None,
                write_receipt: None,
                reason: Some(row.reason.clone()),
            },
        };
        entries.push(entry);
    }

    let manifest = IssueWikiBatchManifest {
        schema_id: ISSUE_WIKI_BATCH_MANIFEST_SCHEMA_ID_V0.to_owned(),
        issue_export_hash,
        issue_set_selector: export.issue_set_selector.clone(),
        projection_schema_id: ISSUE_WIKI_SCHEMA_ID_V0.to_owned(),
        projector_id: ISSUE_WIKI_PROJECTOR.to_owned(),
        projection_schema_digest: issue_wiki_projection_schema_digest(),
        entries,
        manifest_key: ContentHashHex::new("0".repeat(64))
            .expect("zero placeholder is valid lowercase hex"),
    };
    Ok(IssueWikiBatchManifest {
        manifest_key: issue_wiki_batch_manifest_key(&manifest),
        ..manifest
    })
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
    reject_symlink_boundary(wiki_root.as_ref(), &relative)?;
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
        reject_symlink_boundary(wiki_root.as_ref(), &relative)?;
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

fn materialize_issue_wiki_batch_entry(
    wiki_root: &Path,
    projection: &IssueWikiProjectionResult,
) -> Result<IssueWikiBatchManifestEntry, IssueWikiWriteError> {
    let accepted = match projection {
        IssueWikiProjectionResult::Accepted(row) => row.as_ref(),
        IssueWikiProjectionResult::Stale(_) | IssueWikiProjectionResult::Malformed(_) => {
            unreachable!("non-accepted projections handled by caller")
        }
    };
    let page_path = issue_wiki_page_path_for_projection(projection);
    let page_file = relative_page_file_path(&page_path)?;
    let full_path = wiki_root.join(&page_file);
    reject_symlink_boundary(wiki_root, &page_file)?;
    let (_, current_bytes) = read_existing_or_empty(&full_path)?;
    let current_note = parse_materialized_current_note(&current_bytes);
    let dry_run = plan_issue_wiki_mutation(projection, current_note.as_ref()).map_err(|err| {
        IssueWikiWriteError::Pipeline {
            source_ref: accepted.source.source_ref.clone(),
            reason: err.to_string(),
        }
    })?;
    let snapshot = IssueWikiPageSnapshot::from_page_bytes(page_path.clone(), &current_bytes);
    let apply =
        apply_issue_wiki_mutation_dry_run(&dry_run, &snapshot, &snapshot).map_err(|err| {
            IssueWikiWriteError::Pipeline {
                source_ref: accepted.source.source_ref.clone(),
                reason: err.to_string(),
            }
        })?;
    if apply.action == IssueWikiApplyAction::Conflict {
        return Ok(IssueWikiBatchManifestEntry {
            source_ref: accepted.source.source_ref.clone(),
            issue_number: accepted.source.issue_number,
            source_hash: accepted.source.source_hash.clone(),
            page_path,
            status: IssueWikiBatchStatus::Conflict,
            apply_key: Some(apply.apply_key.clone()),
            write_key: None,
            apply_receipt: Some(apply.clone()),
            write_receipt: None,
            reason: apply.conflict_reason.clone(),
        });
    }

    let write = materialize_issue_wiki_apply_receipt(wiki_root, &apply)?;
    let status = match write.action {
        IssueWikiWriteAction::Create | IssueWikiWriteAction::Update => {
            IssueWikiBatchStatus::Written
        }
        IssueWikiWriteAction::Noop => IssueWikiBatchStatus::Noop,
    };
    Ok(IssueWikiBatchManifestEntry {
        source_ref: accepted.source.source_ref.clone(),
        issue_number: accepted.source.issue_number,
        source_hash: accepted.source.source_hash.clone(),
        page_path,
        status,
        apply_key: Some(apply.apply_key.clone()),
        write_key: Some(write.write_key.clone()),
        apply_receipt: Some(apply),
        write_receipt: Some(write),
        reason: None,
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

fn issue_wiki_page_path_for_projection(projection: &IssueWikiProjectionResult) -> String {
    let IssueWikiProjectionResult::Accepted(row) = projection else {
        return String::new();
    };
    row.note
        .references
        .iter()
        .find_map(|reference| reference.target.strip_prefix("wiki:"))
        .map(str::to_owned)
        .unwrap_or_else(|| row.note.identity.value.replace('.', "/"))
}

fn parse_materialized_current_note(bytes: &[u8]) -> Option<IssueWikiCurrentNote> {
    if bytes.is_empty() {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let title = text
        .lines()
        .find_map(|line| line.strip_prefix("# ").map(str::to_owned))?;
    let identity = text
        .lines()
        .find_map(|line| line.strip_prefix("identity: ").map(str::to_owned))?;
    let mut references = Vec::new();
    let mut in_references = false;
    for line in text.lines() {
        if line == "## References" {
            in_references = true;
            continue;
        }
        if in_references {
            if let Some(reference) = line.strip_prefix("- ") {
                if reference != "none" {
                    references.push(reference.to_owned());
                }
            }
        }
    }
    Some(IssueWikiCurrentNote {
        identity,
        title,
        references,
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

fn reject_symlink_boundary(root: &Path, relative: &Path) -> Result<(), IssueWikiWriteError> {
    reject_symlink_if_exists(root)?;
    let mut path = root.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(segment) => {
                path.push(segment);
                reject_symlink_if_exists(&path)?;
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(IssueWikiWriteError::UnsafePagePath {
                    page_path: relative.to_string_lossy().into_owned(),
                    reason: "relative path escaped normalized components".to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn reject_symlink_if_exists(path: &Path) -> Result<(), IssueWikiWriteError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(IssueWikiWriteError::SymlinkBoundary {
                path: path.to_path_buf(),
            })
        }
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
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

fn issue_wiki_export_hash(export: &IssueWikiBatchExport) -> ContentHashHex {
    let mut hasher = blake3::Hasher::new();
    hash_part(&mut hasher, export.issue_set_selector.as_bytes());
    for issue in &export.issues {
        hash_part(&mut hasher, issue.source.repo.as_bytes());
        hash_part(&mut hasher, &issue.source.issue_number.to_le_bytes());
        hash_part(&mut hasher, issue.source.source_ref.as_bytes());
        hash_part(&mut hasher, issue.source.source_hash.as_str().as_bytes());
        hash_part(&mut hasher, &issue.source_bytes);
    }
    ContentHashHex::new(hasher.finalize().to_hex().to_string())
        .expect("blake3 lowercase hex is always valid")
}

fn issue_wiki_projection_schema_digest() -> ContentHashHex {
    let mut hasher = blake3::Hasher::new();
    hash_part(&mut hasher, ISSUE_WIKI_SCHEMA_ID_V0.as_bytes());
    hash_part(&mut hasher, ISSUE_WIKI_PROJECTOR.as_bytes());
    hash_part(
        &mut hasher,
        ISSUE_WIKI_MUTATION_PLAN_SCHEMA_ID_V0.as_bytes(),
    );
    hash_part(
        &mut hasher,
        ISSUE_WIKI_APPLY_RECEIPT_SCHEMA_ID_V0.as_bytes(),
    );
    hash_part(
        &mut hasher,
        ISSUE_WIKI_WRITE_RECEIPT_SCHEMA_ID_V0.as_bytes(),
    );
    hash_part(
        &mut hasher,
        ISSUE_WIKI_BATCH_MANIFEST_SCHEMA_ID_V0.as_bytes(),
    );
    ContentHashHex::new(hasher.finalize().to_hex().to_string())
        .expect("blake3 lowercase hex is always valid")
}

fn issue_wiki_batch_manifest_key(manifest: &IssueWikiBatchManifest) -> ContentHashHex {
    let mut hasher = blake3::Hasher::new();
    hash_part(&mut hasher, manifest.schema_id.as_bytes());
    hash_part(&mut hasher, manifest.issue_export_hash.as_str().as_bytes());
    hash_part(&mut hasher, manifest.issue_set_selector.as_bytes());
    hash_part(&mut hasher, manifest.projection_schema_id.as_bytes());
    hash_part(&mut hasher, manifest.projector_id.as_bytes());
    hash_part(
        &mut hasher,
        manifest.projection_schema_digest.as_str().as_bytes(),
    );
    for entry in &manifest.entries {
        hash_part(&mut hasher, entry.source_ref.as_bytes());
        hash_part(&mut hasher, &entry.issue_number.to_le_bytes());
        hash_part(&mut hasher, entry.source_hash.as_str().as_bytes());
        hash_part(&mut hasher, entry.page_path.as_bytes());
        hash_part(&mut hasher, entry.status.as_str().as_bytes());
        if let Some(apply_key) = &entry.apply_key {
            hash_part(&mut hasher, apply_key.as_str().as_bytes());
        }
        if let Some(write_key) = &entry.write_key {
            hash_part(&mut hasher, write_key.as_str().as_bytes());
        }
        if let Some(apply) = &entry.apply_receipt {
            hash_part(&mut hasher, apply.apply_key.as_str().as_bytes());
            hash_part(
                &mut hasher,
                issue_wiki_apply_action_str(apply.action).as_bytes(),
            );
        }
        if let Some(write) = &entry.write_receipt {
            hash_part(&mut hasher, write.write_key.as_str().as_bytes());
            hash_part(&mut hasher, write.action.as_str().as_bytes());
        }
        if let Some(reason) = &entry.reason {
            hash_part(&mut hasher, reason.as_bytes());
        }
    }
    ContentHashHex::new(hasher.finalize().to_hex().to_string())
        .expect("blake3 lowercase hex is always valid")
}

fn issue_wiki_batch_run_receipt(
    action: IssueWikiBatchRunAction,
    manifest: IssueWikiBatchManifest,
    publish_receipt: Option<IssueWikiPublishReceipt>,
) -> IssueWikiBatchRunReceipt {
    let receipt = IssueWikiBatchRunReceipt {
        schema_id: ISSUE_WIKI_BATCH_RUN_RECEIPT_SCHEMA_ID_V0.to_owned(),
        action,
        issue_export_hash: manifest.issue_export_hash.clone(),
        issue_set_selector: manifest.issue_set_selector.clone(),
        manifest_key: manifest.manifest_key.clone(),
        summary: issue_wiki_batch_run_summary(&manifest, publish_receipt.as_ref()),
        manifest,
        publish_receipt,
        run_key: ContentHashHex::new("0".repeat(64))
            .expect("zero placeholder is valid lowercase hex"),
    };
    IssueWikiBatchRunReceipt {
        run_key: issue_wiki_batch_run_key(&receipt),
        ..receipt
    }
}

fn issue_wiki_batch_run_summary(
    manifest: &IssueWikiBatchManifest,
    publish_receipt: Option<&IssueWikiPublishReceipt>,
) -> IssueWikiBatchRunSummary {
    let mut summary = IssueWikiBatchRunSummary::default();
    for entry in &manifest.entries {
        match entry.status {
            IssueWikiBatchStatus::Written => summary.written += 1,
            IssueWikiBatchStatus::Noop => summary.noop += 1,
            IssueWikiBatchStatus::Conflict => summary.conflict += 1,
            IssueWikiBatchStatus::Stale => summary.stale += 1,
            IssueWikiBatchStatus::Malformed => summary.malformed += 1,
        }
    }
    if let Some(publish) = publish_receipt {
        for entry in &publish.entries {
            match entry.action {
                IssueWikiPublishAction::Published => summary.publish_published += 1,
                IssueWikiPublishAction::Noop => summary.publish_noop += 1,
            }
        }
    }
    summary
}

fn issue_wiki_batch_run_key(receipt: &IssueWikiBatchRunReceipt) -> ContentHashHex {
    let mut hasher = blake3::Hasher::new();
    hash_part(&mut hasher, receipt.schema_id.as_bytes());
    hash_part(&mut hasher, receipt.action.as_str().as_bytes());
    hash_part(&mut hasher, receipt.issue_export_hash.as_str().as_bytes());
    hash_part(&mut hasher, receipt.issue_set_selector.as_bytes());
    hash_part(&mut hasher, receipt.manifest_key.as_str().as_bytes());
    hash_part(&mut hasher, &receipt.summary.written.to_le_bytes());
    hash_part(&mut hasher, &receipt.summary.noop.to_le_bytes());
    hash_part(&mut hasher, &receipt.summary.conflict.to_le_bytes());
    hash_part(&mut hasher, &receipt.summary.stale.to_le_bytes());
    hash_part(&mut hasher, &receipt.summary.malformed.to_le_bytes());
    hash_part(
        &mut hasher,
        &receipt.summary.publish_published.to_le_bytes(),
    );
    hash_part(&mut hasher, &receipt.summary.publish_noop.to_le_bytes());
    hash_part(
        &mut hasher,
        receipt.manifest.manifest_key.as_str().as_bytes(),
    );
    if let Some(publish) = &receipt.publish_receipt {
        hash_part(&mut hasher, publish.publish_key.as_str().as_bytes());
    }
    ContentHashHex::new(hasher.finalize().to_hex().to_string())
        .expect("blake3 lowercase hex is always valid")
}

fn issue_wiki_publish_key(receipt: &IssueWikiPublishReceipt) -> ContentHashHex {
    let mut hasher = blake3::Hasher::new();
    hash_part(&mut hasher, receipt.schema_id.as_bytes());
    hash_part(&mut hasher, receipt.manifest_key.as_str().as_bytes());
    hash_part(
        &mut hasher,
        receipt.projection_schema_digest.as_str().as_bytes(),
    );
    hash_part(&mut hasher, receipt.pre_tree_hash.as_str().as_bytes());
    hash_part(&mut hasher, receipt.post_tree_hash.as_str().as_bytes());
    for entry in &receipt.entries {
        hash_part(&mut hasher, entry.page_path.as_bytes());
        hash_part(&mut hasher, entry.write_key.as_str().as_bytes());
        hash_part(&mut hasher, entry.action.as_str().as_bytes());
        hash_page_snapshot(&mut hasher, &entry.pre_publish_snapshot);
        hash_page_snapshot(&mut hasher, &entry.post_publish_snapshot);
    }
    hash_part(&mut hasher, &[u8::from(receipt.pushed)]);
    ContentHashHex::new(hasher.finalize().to_hex().to_string())
        .expect("blake3 lowercase hex is always valid")
}

fn issue_wiki_manifest_tree_hash(
    wiki_root: &Path,
    manifest: &IssueWikiBatchManifest,
) -> Result<ContentHashHex, IssueWikiPublishError> {
    let mut page_paths = manifest
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .write_receipt
                .as_ref()
                .map(|write| write.page_path.clone())
        })
        .collect::<Vec<_>>();
    page_paths.sort();
    page_paths.dedup();

    let mut hasher = blake3::Hasher::new();
    for page_path in page_paths {
        let relative = relative_page_file_path(&page_path)?;
        reject_symlink_boundary(wiki_root, &relative)?;
        let full_path = wiki_root.join(relative);
        let (exists, bytes) = read_existing_or_empty(&full_path)?;
        hash_part(&mut hasher, page_path.as_bytes());
        hash_part(&mut hasher, &[u8::from(exists)]);
        hash_part(&mut hasher, &bytes);
    }
    Ok(ContentHashHex::new(hasher.finalize().to_hex().to_string())
        .expect("blake3 lowercase hex is always valid"))
}

fn issue_wiki_apply_action_str(action: IssueWikiApplyAction) -> &'static str {
    match action {
        IssueWikiApplyAction::Create => "create",
        IssueWikiApplyAction::Update => "update",
        IssueWikiApplyAction::Noop => "noop",
        IssueWikiApplyAction::Conflict => "conflict",
    }
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

    #[cfg(unix)]
    fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
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

    fn issue_export(
        selector: &str,
        issue_number: u64,
        source_bytes: &[u8],
    ) -> IssueWikiBatchExport {
        let source_hash = hash_bytes(source_bytes);
        IssueWikiBatchExport {
            issue_set_selector: selector.to_owned(),
            issues: vec![IssueWikiBatchIssue {
                source: IssueWikiSource {
                    repo: "gwild/attractor".to_owned(),
                    issue_number,
                    source_ref: format!("github:gwild/attractor#{issue_number}"),
                    source_hash,
                },
                source_bytes: source_bytes.to_vec(),
            }],
        }
    }

    fn issue_source_bytes() -> &'static [u8] {
        b"identity: attractor.issue.52.batch-manifest\n\
title: Batch issue wiki migration manifest\n\
reference: wiki:Attractor/Batch-Migration-Manifest\n"
    }

    #[derive(Default)]
    struct RecordingPush {
        calls: usize,
        manifest_keys: Vec<ContentHashHex>,
        relative_paths: Vec<Vec<String>>,
    }

    impl IssueWikiPublishTransport for RecordingPush {
        fn push_manifest(
            &mut self,
            _wiki_root: &Path,
            manifest_key: &ContentHashHex,
            relative_paths: &[PathBuf],
        ) -> Result<(), String> {
            self.calls += 1;
            self.manifest_keys.push(manifest_key.clone());
            self.relative_paths.push(
                relative_paths
                    .iter()
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
                    .collect(),
            );
            Ok(())
        }
    }

    fn batch_manifest_fixture() -> IssueWikiBatchManifest {
        let staging = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        materialize_issue_wiki_batch_manifest(staging.path(), &export).unwrap()
    }

    fn two_page_publish_manifest_fixture() -> IssueWikiBatchManifest {
        let staging = TestDir::new();
        let first_apply = receipt_for_page_path("Attractor/First", None, b"", b"");
        let second_apply = receipt_for_page_path("Attractor/Second", None, b"", b"");
        let first_write = materialize_issue_wiki_apply_receipt(staging.path(), &first_apply)
            .expect("first staging write");
        let second_write = materialize_issue_wiki_apply_receipt(staging.path(), &second_apply)
            .expect("second staging write");
        let mut manifest = IssueWikiBatchManifest {
            schema_id: ISSUE_WIKI_BATCH_MANIFEST_SCHEMA_ID_V0.to_owned(),
            issue_export_hash: hash_bytes(b"two-page export"),
            issue_set_selector: "repo=gwild/attractor issue in (53,54)".to_owned(),
            projection_schema_id: ISSUE_WIKI_SCHEMA_ID_V0.to_owned(),
            projector_id: ISSUE_WIKI_PROJECTOR.to_owned(),
            projection_schema_digest: issue_wiki_projection_schema_digest(),
            entries: vec![
                IssueWikiBatchManifestEntry {
                    source_ref: first_apply.source_ref.clone(),
                    issue_number: 53,
                    source_hash: first_apply.source_hash.clone(),
                    page_path: first_apply.actual_snapshot.page_path.clone(),
                    status: IssueWikiBatchStatus::Written,
                    apply_key: Some(first_apply.apply_key.clone()),
                    write_key: Some(first_write.write_key.clone()),
                    apply_receipt: Some(first_apply),
                    write_receipt: Some(first_write),
                    reason: None,
                },
                IssueWikiBatchManifestEntry {
                    source_ref: second_apply.source_ref.clone(),
                    issue_number: 54,
                    source_hash: second_apply.source_hash.clone(),
                    page_path: second_apply.actual_snapshot.page_path.clone(),
                    status: IssueWikiBatchStatus::Written,
                    apply_key: Some(second_apply.apply_key.clone()),
                    write_key: Some(second_write.write_key.clone()),
                    apply_receipt: Some(second_apply),
                    write_receipt: Some(second_write),
                    reason: None,
                },
            ],
            manifest_key: ContentHashHex::new("0".repeat(64))
                .expect("zero placeholder is valid lowercase hex"),
        };
        manifest.manifest_key = issue_wiki_batch_manifest_key(&manifest);
        manifest
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

    #[test]
    fn symlinked_wiki_subdir_is_rejected_before_write() {
        let root = TestDir::new();
        let outside = TestDir::new();
        let outside_page = outside.path().join("Issue-to-Wiki-Migration.md");
        fs::write(&outside_page, "outside original").unwrap();
        let link = root.path().join("Attractor");
        if symlink_dir(outside.path(), &link).is_err() {
            // Some Windows hosts require Developer Mode/admin for symlink
            // creation. The materializer code path is still covered anywhere
            // the platform lets this fixture build the boundary.
            return;
        }
        let current = IssueWikiCurrentNote {
            identity: "attractor.issue.43.issue-to-wiki-migration".to_owned(),
            title: "Old title".to_owned(),
            references: vec![],
        };
        let receipt = receipt_for(Some(current), b"outside original", b"outside original");

        let err = materialize_issue_wiki_apply_receipt(root.path(), &receipt).unwrap_err();
        let outside_after = fs::read_to_string(outside_page).unwrap();

        assert!(matches!(err, IssueWikiWriteError::SymlinkBoundary { .. }));
        assert_eq!(outside_after, "outside original");
    }

    #[test]
    fn batch_manifest_writes_issue_export_without_raw_source_bytes() {
        let dir = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());

        let manifest = materialize_issue_wiki_batch_manifest(dir.path(), &export).unwrap();
        let json = serde_json::to_string(&manifest).unwrap();
        let page =
            fs::read_to_string(dir.path().join("Attractor/Batch-Migration-Manifest.md")).unwrap();

        assert_eq!(manifest.schema_id, ISSUE_WIKI_BATCH_MANIFEST_SCHEMA_ID_V0);
        assert_eq!(manifest.issue_set_selector, "repo=gwild/attractor issue=52");
        assert_eq!(manifest.projection_schema_id, ISSUE_WIKI_SCHEMA_ID_V0);
        assert_eq!(manifest.projector_id, ISSUE_WIKI_PROJECTOR);
        assert_eq!(
            manifest.projection_schema_digest,
            issue_wiki_projection_schema_digest()
        );
        assert_eq!(manifest.entries.len(), 1);
        let entry = &manifest.entries[0];
        assert_eq!(entry.status, IssueWikiBatchStatus::Written);
        assert_eq!(entry.page_path, "Attractor/Batch-Migration-Manifest");
        assert!(entry.apply_key.is_some());
        assert!(entry.write_key.is_some());
        assert!(entry.apply_receipt.is_some());
        assert!(entry.write_receipt.is_some());
        assert!(page.contains("# Batch issue wiki migration manifest"));
        assert!(!json.contains("identity: attractor.issue.52.batch-manifest"));
        assert!(!json.contains("reference: wiki:Attractor/Batch-Migration-Manifest"));
    }

    #[test]
    fn batch_manifest_replay_consumes_current_page_as_noop() {
        let dir = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        let first = materialize_issue_wiki_batch_manifest(dir.path(), &export).unwrap();

        let second = materialize_issue_wiki_batch_manifest(dir.path(), &export).unwrap();

        assert_eq!(first.issue_export_hash, second.issue_export_hash);
        assert_eq!(second.entries[0].status, IssueWikiBatchStatus::Noop);
        assert!(
            second.entries[0]
                .write_receipt
                .as_ref()
                .unwrap()
                .pre_write_exists
        );
        assert_eq!(
            second.entries[0]
                .write_receipt
                .as_ref()
                .unwrap()
                .pre_write_snapshot,
            second.entries[0]
                .write_receipt
                .as_ref()
                .unwrap()
                .post_write_snapshot
        );
    }

    #[test]
    fn batch_manifest_key_rejects_rekeyless_tamper() {
        let dir = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        let manifest = materialize_issue_wiki_batch_manifest(dir.path(), &export).unwrap();
        let mut value = serde_json::to_value(manifest).unwrap();
        value["issue_set_selector"] =
            serde_json::Value::String("repo=gwild/attractor all".to_owned());

        let err = serde_json::from_value::<IssueWikiBatchManifest>(value).unwrap_err();

        assert!(err.to_string().contains("batch_manifest_key_mismatch"));
    }

    #[test]
    fn batch_manifest_projection_schema_digest_rejects_recomputed_key_tamper() {
        let dir = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        let mut manifest = materialize_issue_wiki_batch_manifest(dir.path(), &export).unwrap();
        manifest.projection_schema_digest = hash_bytes(b"wrong schema digest");
        manifest.manifest_key = issue_wiki_batch_manifest_key(&manifest);
        let value = serde_json::to_value(manifest).unwrap();

        let err = serde_json::from_value::<IssueWikiBatchManifest>(value).unwrap_err();

        assert!(err
            .to_string()
            .contains("batch_manifest_projection_schema_digest_mismatch"));
    }

    #[test]
    fn batch_manifest_projector_id_rejects_recomputed_key_tamper() {
        let dir = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        let mut manifest = materialize_issue_wiki_batch_manifest(dir.path(), &export).unwrap();
        manifest.projector_id = "issue_wiki.v999".to_owned();
        manifest.manifest_key = issue_wiki_batch_manifest_key(&manifest);
        let value = serde_json::to_value(manifest).unwrap();

        let err = serde_json::from_value::<IssueWikiBatchManifest>(value).unwrap_err();

        assert!(err
            .to_string()
            .contains("batch_manifest_projector_mismatch"));
    }

    #[test]
    fn batch_manifest_status_rejects_recomputed_key_tamper() {
        let dir = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        let mut manifest = materialize_issue_wiki_batch_manifest(dir.path(), &export).unwrap();
        manifest.entries[0].status = IssueWikiBatchStatus::Noop;
        manifest.manifest_key = issue_wiki_batch_manifest_key(&manifest);
        let value = serde_json::to_value(manifest).unwrap();

        let err = serde_json::from_value::<IssueWikiBatchManifest>(value).unwrap_err();

        assert!(err
            .to_string()
            .contains("batch_manifest_status_action_mismatch"));
    }

    #[test]
    fn batch_manifest_write_pre_snapshot_rejects_recomputed_key_tamper() {
        let dir = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        let mut manifest = materialize_issue_wiki_batch_manifest(dir.path(), &export).unwrap();
        let entry = &mut manifest.entries[0];
        let write = entry.write_receipt.as_mut().unwrap();
        write.pre_write_snapshot =
            IssueWikiPageSnapshot::from_page_bytes(entry.page_path.clone(), b"forged before bytes");
        write.write_key = issue_wiki_write_key(write);
        entry.write_key = Some(write.write_key.clone());
        manifest.manifest_key = issue_wiki_batch_manifest_key(&manifest);
        let value = serde_json::to_value(manifest).unwrap();

        let err = serde_json::from_value::<IssueWikiBatchManifest>(value).unwrap_err();

        assert!(err
            .to_string()
            .contains("batch_manifest_write_pre_snapshot_mismatch"));
    }

    #[test]
    fn batch_manifest_write_post_snapshot_rejects_recomputed_key_tamper() {
        let dir = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        let mut manifest = materialize_issue_wiki_batch_manifest(dir.path(), &export).unwrap();
        let entry = &mut manifest.entries[0];
        let write = entry.write_receipt.as_mut().unwrap();
        write.post_write_snapshot =
            IssueWikiPageSnapshot::from_page_bytes(entry.page_path.clone(), b"forged after bytes");
        write.write_key = issue_wiki_write_key(write);
        entry.write_key = Some(write.write_key.clone());
        manifest.manifest_key = issue_wiki_batch_manifest_key(&manifest);
        let value = serde_json::to_value(manifest).unwrap();

        let err = serde_json::from_value::<IssueWikiBatchManifest>(value).unwrap_err();

        assert!(err
            .to_string()
            .contains("batch_manifest_write_post_snapshot_mismatch"));
    }

    #[test]
    fn batch_manifest_records_stale_projection_without_write_receipt() {
        let dir = TestDir::new();
        let mut export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        export.issues[0].source.source_hash = hash_bytes(b"old source");

        let manifest = materialize_issue_wiki_batch_manifest(dir.path(), &export).unwrap();

        assert_eq!(manifest.entries[0].status, IssueWikiBatchStatus::Stale);
        assert!(manifest.entries[0].write_receipt.is_none());
        assert!(!dir
            .path()
            .join("Attractor/Batch-Migration-Manifest.md")
            .exists());
    }

    #[test]
    fn batch_run_prepare_retains_manifest_without_publish() {
        let staging = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());

        let receipt = prepare_issue_wiki_batch_run(staging.path(), &export).unwrap();
        let json = serde_json::to_string(&receipt).unwrap();
        let round_trip: IssueWikiBatchRunReceipt = serde_json::from_str(&json).unwrap();

        assert_eq!(receipt.action, IssueWikiBatchRunAction::Prepared);
        assert!(receipt.publish_receipt.is_none());
        assert_eq!(receipt.summary.written, 1);
        assert_eq!(receipt.summary.publish_published, 0);
        assert_eq!(receipt.manifest_key, receipt.manifest.manifest_key);
        assert_eq!(round_trip.run_key, receipt.run_key);
        assert!(!json.contains("identity: attractor.issue.52.batch-manifest"));
    }

    #[test]
    fn batch_run_publish_prepared_manifest_then_replay_noops_without_push() {
        let staging = TestDir::new();
        let live = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        let prepared = prepare_issue_wiki_batch_run(staging.path(), &export).unwrap();
        let prepared_json = serde_json::to_vec(&prepared).unwrap();
        let mut push = RecordingPush::default();

        let first = publish_prepared_issue_wiki_batch_run_json(
            live.path(),
            &prepared_json,
            &prepared.manifest_key,
            true,
            &mut push,
        )
        .unwrap();
        let second = publish_prepared_issue_wiki_batch_run_json(
            live.path(),
            &prepared_json,
            &prepared.manifest_key,
            true,
            &mut push,
        )
        .unwrap();

        assert_eq!(first.action, IssueWikiBatchRunAction::Published);
        assert_eq!(first.summary.publish_published, 1);
        assert_eq!(push.calls, 1);
        assert_eq!(second.action, IssueWikiBatchRunAction::Noop);
        assert_eq!(second.summary.publish_noop, 1);
        assert_eq!(push.calls, 1);
        assert_eq!(first.manifest_key, second.manifest_key);
        assert_eq!(
            first.publish_receipt.as_ref().unwrap().post_tree_hash,
            second.publish_receipt.as_ref().unwrap().pre_tree_hash
        );
    }

    #[test]
    fn batch_run_publish_requires_explicit_prepared_manifest_key() {
        let staging = TestDir::new();
        let live = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        let prepared = prepare_issue_wiki_batch_run(staging.path(), &export).unwrap();
        let prepared_json = serde_json::to_vec(&prepared).unwrap();
        let wrong_key = hash_bytes(b"wrong manifest key");
        let mut push = RecordingPush::default();

        let err = publish_prepared_issue_wiki_batch_run_json(
            live.path(),
            &prepared_json,
            &wrong_key,
            true,
            &mut push,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            IssueWikiBatchRunError::ManifestKeyMismatch { .. }
        ));
        assert_eq!(push.calls, 0);
    }

    #[test]
    fn batch_run_publish_stops_on_drift_before_transport() {
        let staging = TestDir::new();
        let live = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        let prepared = prepare_issue_wiki_batch_run(staging.path(), &export).unwrap();
        let prepared_json = serde_json::to_vec(&prepared).unwrap();
        let drifted = live.path().join("Attractor/Batch-Migration-Manifest.md");
        fs::create_dir_all(drifted.parent().unwrap()).unwrap();
        fs::write(&drifted, "edited outside manifest").unwrap();
        let mut push = RecordingPush::default();

        let err = publish_prepared_issue_wiki_batch_run_json(
            live.path(),
            &prepared_json,
            &prepared.manifest_key,
            true,
            &mut push,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            IssueWikiBatchRunError::Publish(IssueWikiPublishError::NonFastForward { .. })
        ));
        assert_eq!(push.calls, 0);
    }

    #[test]
    fn batch_run_publish_refuses_already_published_receipt_as_input() {
        let staging = TestDir::new();
        let live = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        let prepared = prepare_issue_wiki_batch_run(staging.path(), &export).unwrap();
        let prepared_json = serde_json::to_vec(&prepared).unwrap();
        let mut push = RecordingPush::default();
        let published = publish_prepared_issue_wiki_batch_run_json(
            live.path(),
            &prepared_json,
            &prepared.manifest_key,
            true,
            &mut push,
        )
        .unwrap();
        let published_json = serde_json::to_vec(&published).unwrap();

        let err = publish_prepared_issue_wiki_batch_run_json(
            live.path(),
            &published_json,
            &prepared.manifest_key,
            true,
            &mut push,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            IssueWikiBatchRunError::InvalidPreparedRun { .. }
        ));
        assert_eq!(push.calls, 1);
    }

    #[test]
    fn batch_run_receipt_rejects_recomputed_summary_tamper() {
        let staging = TestDir::new();
        let export = issue_export("repo=gwild/attractor issue=52", 52, issue_source_bytes());
        let mut receipt = prepare_issue_wiki_batch_run(staging.path(), &export).unwrap();
        receipt.summary.written = 0;
        receipt.run_key = issue_wiki_batch_run_key(&receipt);
        let value = serde_json::to_value(receipt).unwrap();

        let err = serde_json::from_value::<IssueWikiBatchRunReceipt>(value).unwrap_err();

        assert!(err.to_string().contains("batch_run_summary_mismatch"));
    }

    #[test]
    fn publish_manifest_clean_ff_pushes_once_then_replay_noops_without_push() {
        let manifest = batch_manifest_fixture();
        let manifest_json = serde_json::to_vec(&manifest).unwrap();
        let live = TestDir::new();
        let mut push = RecordingPush::default();

        let first = publish_issue_wiki_batch_manifest_json(
            live.path(),
            &manifest_json,
            &manifest.manifest_key,
            true,
            &mut push,
        )
        .unwrap();
        let second = publish_issue_wiki_batch_manifest_json(
            live.path(),
            &manifest_json,
            &manifest.manifest_key,
            true,
            &mut push,
        )
        .unwrap();

        assert!(first.pushed);
        assert_eq!(push.calls, 1);
        assert_eq!(push.manifest_keys, vec![manifest.manifest_key.clone()]);
        assert_eq!(
            push.relative_paths,
            vec![vec!["Attractor/Batch-Migration-Manifest.md".to_owned()]]
        );
        assert_eq!(first.entries[0].action, IssueWikiPublishAction::Published);
        assert!(!second.pushed);
        assert_eq!(push.calls, 1, "replay must use the same push spy handle");
        assert!(second
            .entries
            .iter()
            .all(|entry| entry.action == IssueWikiPublishAction::Noop
                && entry.pre_publish_snapshot == entry.post_publish_snapshot));
        assert_eq!(second.pre_tree_hash, second.post_tree_hash);
        assert_eq!(first.post_tree_hash, second.pre_tree_hash);
        assert_eq!(first.manifest_key, second.manifest_key);
    }

    #[test]
    fn publish_requires_explicit_manifest_hash_and_opt_in_before_push() {
        let manifest = batch_manifest_fixture();
        let manifest_json = serde_json::to_vec(&manifest).unwrap();
        let live = TestDir::new();
        let mut push = RecordingPush::default();

        let no_opt_in = publish_issue_wiki_batch_manifest_json(
            live.path(),
            &manifest_json,
            &manifest.manifest_key,
            false,
            &mut push,
        )
        .unwrap_err();
        let wrong_key = hash_bytes(b"different manifest key");
        let wrong_hash = publish_issue_wiki_batch_manifest_json(
            live.path(),
            &manifest_json,
            &wrong_key,
            true,
            &mut push,
        )
        .unwrap_err();

        assert!(matches!(
            no_opt_in,
            IssueWikiPublishError::ExplicitOptInRequired
        ));
        assert!(matches!(
            wrong_hash,
            IssueWikiPublishError::ManifestKeyMismatch { .. }
        ));
        assert_eq!(push.calls, 0);
    }

    #[test]
    fn publish_refuses_out_of_band_drift_before_push_with_hash_payload() {
        let manifest = batch_manifest_fixture();
        let manifest_json = serde_json::to_vec(&manifest).unwrap();
        let live = TestDir::new();
        let path = live.path().join("Attractor/Batch-Migration-Manifest.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "edited outside manifest").unwrap();
        let mut push = RecordingPush::default();

        let err = publish_issue_wiki_batch_manifest_json(
            live.path(),
            &manifest_json,
            &manifest.manifest_key,
            true,
            &mut push,
        )
        .unwrap_err();

        match err {
            IssueWikiPublishError::NonFastForward {
                page_path,
                expected_hash,
                actual_hash,
            } => {
                assert_eq!(page_path, "Attractor/Batch-Migration-Manifest");
                let write = manifest.entries[0].write_receipt.as_ref().unwrap();
                assert_eq!(expected_hash, write.pre_write_snapshot.page_hash);
                assert_eq!(
                    actual_hash,
                    issue_wiki_page_snapshot_hash(
                        "Attractor/Batch-Migration-Manifest",
                        b"edited outside manifest"
                    )
                );
            }
            other => panic!("expected typed NonFastForward, got {other:?}"),
        }
        assert_eq!(push.calls, 0);
    }

    #[test]
    fn publish_preflights_all_pages_before_any_local_write() {
        let manifest = two_page_publish_manifest_fixture();
        let manifest_json = serde_json::to_vec(&manifest).unwrap();
        let live = TestDir::new();
        let drifted = live.path().join("Attractor/Second.md");
        fs::create_dir_all(drifted.parent().unwrap()).unwrap();
        fs::write(&drifted, "edited outside manifest").unwrap();
        let mut push = RecordingPush::default();

        let err = publish_issue_wiki_batch_manifest_json(
            live.path(),
            &manifest_json,
            &manifest.manifest_key,
            true,
            &mut push,
        )
        .unwrap_err();

        assert!(matches!(err, IssueWikiPublishError::NonFastForward { .. }));
        assert_eq!(push.calls, 0);
        assert!(
            !live.path().join("Attractor/First.md").exists(),
            "first page must not be written when a later page refuses"
        );
        assert_eq!(
            fs::read_to_string(drifted).unwrap(),
            "edited outside manifest"
        );
    }

    #[test]
    fn publish_refuses_stale_projection_before_push() {
        let mut manifest = batch_manifest_fixture();
        let stale_digest = hash_bytes(b"old projection schema");
        manifest.projection_schema_digest = stale_digest.clone();
        manifest.manifest_key = issue_wiki_batch_manifest_key(&manifest);
        let manifest_json = serde_json::to_vec(&manifest).unwrap();
        let live = TestDir::new();
        let mut push = RecordingPush::default();
        let live_digest = issue_wiki_projection_schema_digest();

        let err = publish_issue_wiki_batch_manifest_json(
            live.path(),
            &manifest_json,
            &manifest.manifest_key,
            true,
            &mut push,
        )
        .unwrap_err();

        match err {
            IssueWikiPublishError::StaleProjection {
                manifest_digest,
                live_digest: actual_live_digest,
            } => {
                assert_eq!(manifest_digest, stale_digest);
                assert_eq!(actual_live_digest, live_digest);
            }
            other => panic!("expected typed StaleProjection, got {other:?}"),
        }
        assert_eq!(push.calls, 0);
    }

    #[test]
    fn publish_refuses_symlink_boundary_before_push() {
        let manifest = batch_manifest_fixture();
        let manifest_json = serde_json::to_vec(&manifest).unwrap();
        let live = TestDir::new();
        let outside = TestDir::new();
        if symlink_dir(outside.path(), &live.path().join("Attractor")).is_err() {
            return;
        }
        let mut push = RecordingPush::default();

        let err = publish_issue_wiki_batch_manifest_json(
            live.path(),
            &manifest_json,
            &manifest.manifest_key,
            true,
            &mut push,
        )
        .unwrap_err();

        assert!(matches!(err, IssueWikiPublishError::SymlinkBoundary { .. }));
        assert_eq!(push.calls, 0);
    }

    #[test]
    fn git_transport_rejects_staged_path_outside_manifest() {
        let dir = TestDir::new();
        run_git(dir.path(), &["init"]).unwrap();
        run_git(
            dir.path(),
            &["config", "user.email", "test@example.invalid"],
        )
        .unwrap();
        run_git(dir.path(), &["config", "user.name", "Attractor Test"]).unwrap();
        fs::write(dir.path().join("Unrelated.md"), "not in manifest").unwrap();
        run_git(dir.path(), &["add", "Unrelated.md"]).unwrap();
        let manifest_key = hash_bytes(b"manifest key");
        let mut transport = IssueWikiGitPushTransport::new("origin", "master");

        let err = transport
            .push_manifest(
                dir.path(),
                &manifest_key,
                &[PathBuf::from("Attractor/Batch-Migration-Manifest.md")],
            )
            .unwrap_err();

        assert!(err.contains("staged path outside manifest: Unrelated.md"));
        let staged = run_git_stdout(dir.path(), &["diff", "--cached", "--name-only", "--"])
            .expect("staged names");
        assert_eq!(staged.trim(), "Unrelated.md");
    }

    #[test]
    fn publish_receipt_rejects_recomputed_key_tamper() {
        let manifest = batch_manifest_fixture();
        let manifest_json = serde_json::to_vec(&manifest).unwrap();
        let live = TestDir::new();
        let mut push = RecordingPush::default();
        let mut receipt = publish_issue_wiki_batch_manifest_json(
            live.path(),
            &manifest_json,
            &manifest.manifest_key,
            true,
            &mut push,
        )
        .unwrap();
        receipt.entries[0].action = IssueWikiPublishAction::Noop;
        receipt.publish_key = issue_wiki_publish_key(&receipt);
        let value = serde_json::to_value(receipt).unwrap();

        let err = serde_json::from_value::<IssueWikiPublishReceipt>(value).unwrap_err();

        assert!(err
            .to_string()
            .contains("publish_entry_noop_pre_post_differ"));
    }

    #[test]
    fn publish_receipt_rejects_pushed_flag_tamper_even_with_recomputed_key() {
        let manifest = batch_manifest_fixture();
        let manifest_json = serde_json::to_vec(&manifest).unwrap();
        let live = TestDir::new();
        let mut push = RecordingPush::default();
        let mut receipt = publish_issue_wiki_batch_manifest_json(
            live.path(),
            &manifest_json,
            &manifest.manifest_key,
            true,
            &mut push,
        )
        .unwrap();
        receipt.pushed = false;
        receipt.pre_tree_hash = receipt.post_tree_hash.clone();
        receipt.publish_key = issue_wiki_publish_key(&receipt);
        let value = serde_json::to_value(receipt).unwrap();

        let err = serde_json::from_value::<IssueWikiPublishReceipt>(value).unwrap_err();

        assert!(err.to_string().contains("publish_pushed_action_mismatch"));
    }
}
