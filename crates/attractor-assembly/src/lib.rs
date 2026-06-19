#![forbid(unsafe_code)]
#![warn(clippy::pedantic, clippy::nursery)]
#![deny(missing_docs, missing_debug_implementations)]

//! # attractor-assembly
//!
//! WS-3 batch assembly — the step that turns a set of markdown notes into a
//! linked store. It is the orchestration layer above the pure pieces:
//!
//! 1. **Project** every note with the store-writing projectors
//!    ([`attractor_projector`]): title, frontmatter-id, references. Each derives
//!    its nodes, writes them, and returns a receipt. A note that simply lacks a
//!    title or a frontmatter `id` is not an error — that projector is skipped
//!    for that note; malformed input (bad UTF-8, hash mismatch) still fails.
//! 2. **Resolve** the collected receipts ([`attractor_resolver`]): classify each
//!    reference against the batch's identities, then compute the
//!    `referencing-note-identity -> target-identity` edges (deferring cyclic and
//!    no-identity references as evidence).
//! 3. **Link** the resolved edges into the store. Deferred references are *not*
//!    linked — their evidence nodes are already in the store from step 1, and
//!    the [`AssemblyOutcome`] carries the deferral records (with provenance and
//!    occurrence counts) for a later multiset/provenance gate.
//!
//! Nodes for the whole batch are written before any edge is linked, so a
//! reference whose target note is projected later still resolves and links — the
//! resolve/link phase sees the complete batch.

use attractor_core::{ContentHash, ParseHashError};
use attractor_projector::{
    project_markdown_frontmatter_id_into_store, project_markdown_references_into_store,
    project_markdown_title_into_store, MarkdownSource, ProjectorError,
};
use attractor_proto::{ContentHashHex, E2eReceipt};
use attractor_resolver::{
    materialize_references, resolve_batch, DeferredReference, ResolvedEdge, ResolverError,
};
use attractor_store::{Store, StoreError};
use std::fmt;

/// One markdown note entering the batch: its source metadata plus raw bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// Source metadata (hash / origin / label) for the projectors.
    pub source: MarkdownSource,
    /// Raw markdown bytes (stay caller-owned; never written to the store).
    pub markdown: Vec<u8>,
}

/// What a batch assembly produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AssemblyOutcome {
    /// Every projector receipt written, in projection order.
    pub receipts: Vec<E2eReceipt>,
    /// `references` edges linked into the store.
    pub linked: Vec<ResolvedEdge>,
    /// Resolved references withheld from the store (cyclic / no referencing
    /// identity), recorded as evidence with their attempted endpoints + reason.
    pub deferred: Vec<DeferredReference>,
}

/// Why a batch could not be assembled.
#[derive(Debug)]
pub enum AssemblyError {
    /// A projector failed on a note in a way that is not "feature absent"
    /// (e.g. bad UTF-8 or a declared-vs-actual source-hash mismatch). Carries the
    /// note's label so the offending note is identifiable.
    Projector {
        /// The failing note's [`MarkdownSource::label`].
        label: String,
        /// The underlying projector error.
        source: ProjectorError,
    },
    /// The resolver rejected the collected receipts (malformed receipt).
    Resolver(ResolverError),
    /// A resolved edge endpoint hex could not be parsed back into an address.
    Hash(ParseHashError),
    /// Linking a resolved edge into the store failed. The resolver only emits
    /// acyclic edges with present endpoints, so this indicates an inconsistency
    /// between resolution and the store rather than a normal deferral.
    Link {
        /// Edge source endpoint.
        from: ContentHashHex,
        /// Edge target endpoint.
        to: ContentHashHex,
        /// The underlying store error.
        source: StoreError,
    },
}

impl fmt::Display for AssemblyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Projector { label, source } => {
                write!(f, "projecting note {label}: {source}")
            }
            Self::Resolver(e) => write!(f, "resolving batch: {e}"),
            Self::Hash(e) => write!(f, "parsing resolved-edge endpoint: {e}"),
            Self::Link { from, to, source } => write!(
                f,
                "linking resolved edge {} -> {}: {source}",
                from.as_str(),
                to.as_str()
            ),
        }
    }
}

impl std::error::Error for AssemblyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Projector { source, .. } => Some(source),
            Self::Resolver(e) => Some(e),
            Self::Hash(e) => Some(e),
            Self::Link { source, .. } => Some(source),
        }
    }
}

/// Project a note's title, ignoring a missing title (which is not an error —
/// the note simply has no promotable title).
fn project_title(
    store: &mut Store,
    note: &Note,
    receipts: &mut Vec<E2eReceipt>,
) -> Result<(), AssemblyError> {
    match project_markdown_title_into_store(store, &note.source, &note.markdown) {
        Ok(receipt) => {
            receipts.push(receipt);
            Ok(())
        }
        Err(ProjectorError::MissingTitle) => Ok(()),
        Err(source) => Err(AssemblyError::Projector {
            label: note.source.label.clone(),
            source,
        }),
    }
}

/// Project a note's frontmatter `id`, ignoring its absence.
fn project_frontmatter_id(
    store: &mut Store,
    note: &Note,
    receipts: &mut Vec<E2eReceipt>,
) -> Result<(), AssemblyError> {
    match project_markdown_frontmatter_id_into_store(store, &note.source, &note.markdown) {
        Ok(receipt) => {
            receipts.push(receipt);
            Ok(())
        }
        Err(ProjectorError::MissingFrontmatterId) => Ok(()),
        Err(source) => Err(AssemblyError::Projector {
            label: note.source.label.clone(),
            source,
        }),
    }
}

/// Co-project a set of notes into `store`, then link the references that resolve
/// to an identity in the batch.
///
/// Identity (title / frontmatter-id) and reference evidence for every note are
/// written first; then references are resolved across the whole batch and the
/// acyclic `referencing-note-identity -> target-identity` edges are linked.
/// Cyclic or unanchored references are returned as
/// [`AssemblyOutcome::deferred`] rather than linked.
///
/// # Errors
/// [`AssemblyError`] if a projector fails on malformed input, the resolver
/// rejects a receipt, or a resolved edge cannot be parsed or linked.
pub fn assemble_notes(store: &mut Store, notes: &[Note]) -> Result<AssemblyOutcome, AssemblyError> {
    let mut receipts = Vec::new();

    // Phase 1: project every note's nodes into the store.
    for note in notes {
        project_title(store, note, &mut receipts)?;
        project_frontmatter_id(store, note, &mut receipts)?;
        // References never report a "missing" feature — no refs yields an empty
        // vec, not an error.
        let refs = project_markdown_references_into_store(store, &note.source, &note.markdown)
            .map_err(|source| AssemblyError::Projector {
                label: note.source.label.clone(),
                source,
            })?;
        receipts.extend(refs);
    }

    // Phase 2: resolve the whole batch and link the edges that hold in the DAG.
    let batch = resolve_batch(&receipts).map_err(AssemblyError::Resolver)?;
    let edges = materialize_references(&batch);

    let mut linked = Vec::with_capacity(edges.linked.len());
    for edge in edges.linked {
        let from = ContentHash::from_hex(edge.from.as_str()).map_err(AssemblyError::Hash)?;
        let to = ContentHash::from_hex(edge.to.as_str()).map_err(AssemblyError::Hash)?;
        store
            .link(from, to, edge.kind.clone())
            .map_err(|source| AssemblyError::Link {
                from: edge.from.clone(),
                to: edge.to.clone(),
                source,
            })?;
        linked.push(edge);
    }

    Ok(AssemblyOutcome {
        receipts,
        linked,
        deferred: edges.deferred,
    })
}

#[cfg(test)]
mod tests {
    use super::{assemble_notes, Note};
    use attractor_core::ContentHash;
    use attractor_proto::ContentHashHex;
    use attractor_resolver::{DeferralReason, EDGE_REFERENCES};
    use attractor_store::Store;

    fn note(markdown: &[u8], label: &str) -> Note {
        Note {
            source: attractor_projector::MarkdownSource {
                source_hash: ContentHashHex::new(ContentHash::of(markdown).to_hex()).unwrap(),
                origin: "repo:milton".to_owned(),
                label: label.to_owned(),
            },
            markdown: markdown.to_vec(),
        }
    }

    #[test]
    fn links_resolved_reference_across_two_notes() {
        // Note A titled "Alpha"; note B titled "B Note" references [[Alpha]].
        let notes = vec![
            note(b"# Alpha\n", "a.md"),
            note(b"# B Note\n\nsee [[Alpha]]\n", "b.md"),
        ];
        let mut store = Store::new();
        let out = assemble_notes(&mut store, &notes).unwrap();

        assert_eq!(out.linked.len(), 1, "one references edge linked");
        assert!(out.deferred.is_empty());
        let e = &out.linked[0];
        assert_eq!(e.kind, EDGE_REFERENCES);

        // Both endpoints are stored, and the edge is actually in the store graph
        // (a second link of the reverse direction would now cycle).
        let from = ContentHash::from_hex(e.from.as_str()).unwrap();
        let to = ContentHash::from_hex(e.to.as_str()).unwrap();
        assert!(store.contains(&from));
        assert!(store.contains(&to));
        // Store stayed an acyclic DAG with an intact receipt chain.
        assert!(store.verify_chain());
        assert!(store.verify_receipts());

        // D3 payoff: the converged target node ("Alpha") is BOTH note A's title
        // and note B's reference target, so it carries both derivations' receipts
        // (the multiset that closes the provenance hole) — not one overwriting
        // the other.
        let kinds: Vec<&str> = store
            .receipts(&to)
            .iter()
            .map(|r| r.transform_id.as_str())
            .collect();
        assert_eq!(
            kinds.len(),
            2,
            "converged node holds title + references receipts"
        );
        assert!(kinds.iter().any(|k| k.contains("title")));
        assert!(kinds.iter().any(|k| k.contains("references")));
    }

    #[test]
    fn mutual_reference_links_one_and_defers_the_cycle_keeping_store_acyclic() {
        let notes = vec![
            note(b"# A\n\n[[B]]\n", "a.md"),
            note(b"# B\n\n[[A]]\n", "b.md"),
        ];
        let mut store = Store::new();
        let out = assemble_notes(&mut store, &notes).unwrap();

        assert_eq!(out.linked.len(), 1, "one direction holds in the DAG");
        assert_eq!(out.deferred.len(), 1, "the cycle-closing direction defers");
        assert_eq!(out.deferred[0].reason, DeferralReason::WouldCycle);
        // The deferred edge was NOT written — only the one linked edge exists.
        // (If it had been linked the store would be cyclic, which link() forbids.)
    }

    #[test]
    fn unresolved_and_no_identity_references_link_nothing() {
        // Note B has a title and references [[Ghost]] (unresolved); note C has no
        // title and references [[Alpha]] which never appears -> also unresolved.
        let notes = vec![
            note(b"# B Note\n\n[[Ghost]]\n", "b.md"),
            note(b"[[Ghost]]\n", "c.md"),
        ];
        let mut store = Store::new();
        let out = assemble_notes(&mut store, &notes).unwrap();

        assert!(out.linked.is_empty());
        assert!(
            out.deferred.is_empty(),
            "unresolved refs stay evidence, not deferred"
        );
        // Evidence nodes were still written.
        assert!(!out.receipts.is_empty());
    }

    #[test]
    fn note_without_title_still_projects_references_as_evidence() {
        // A title-less note's reference resolves to another note's title; but the
        // referencing note has no identity, so the edge defers (no `from`).
        let notes = vec![note(b"# Alpha\n", "a.md"), note(b"[[Alpha]]\n", "b.md")];
        let mut store = Store::new();
        let out = assemble_notes(&mut store, &notes).unwrap();

        assert!(out.linked.is_empty());
        assert_eq!(out.deferred.len(), 1);
        assert_eq!(
            out.deferred[0].reason,
            attractor_resolver::DeferralReason::NoReferencingIdentity
        );
    }

    #[test]
    fn malformed_source_hash_fails_loud_with_note_label() {
        // Declared hash does not match the bytes -> projector SourceHashMismatch,
        // surfaced (not swallowed) with the note label.
        let mut bad = note(b"# Alpha\n", "bad.md");
        bad.source.source_hash = ContentHashHex::new("0".repeat(64)).unwrap();
        let mut store = Store::new();
        let err = assemble_notes(&mut store, &[bad]).unwrap_err();
        assert!(
            matches!(err, super::AssemblyError::Projector { ref label, .. } if label == "bad.md")
        );
    }

    #[test]
    fn empty_batch_is_an_empty_outcome() {
        let mut store = Store::new();
        let out = assemble_notes(&mut store, &[]).unwrap();
        assert!(out.receipts.is_empty());
        assert!(out.linked.is_empty());
        assert!(out.deferred.is_empty());
        assert_eq!(store.len(), 0);
    }

    /// WS-7 / #2510 consumer recall smoke: Relations traversal + `receipts()`
    /// multiset on converged evidence refs. Run via `scripts/recall-smoke.sh`
    /// on sd-3 arm64 (native) or the store-linux Docker leg on Homer (#2288).
    #[test]
    fn recall_smoke_ws7_receipts_multiset_on_converged_target() {
        let notes = vec![
            note(b"# Alpha\n", "a.md"),
            note(b"# B Note\n\nsee [[Alpha]]\n", "b.md"),
        ];
        let mut store = Store::new();
        let out = assemble_notes(&mut store, &notes).unwrap();
        assert_eq!(out.linked.len(), 1, "one references edge linked");

        let to = ContentHash::from_hex(out.linked[0].to.as_str()).unwrap();
        let from = ContentHash::from_hex(out.linked[0].from.as_str()).unwrap();

        // Consumer recall walk: Relations on the referencing identity, then the
        // converged target address returned on the edge.
        let rel_from: Vec<_> = store
            .relations(&from)
            .into_iter()
            .filter(|e| e.kind == EDGE_REFERENCES)
            .collect();
        assert_eq!(
            rel_from.len(),
            1,
            "referencing identity has one references edge"
        );
        assert_eq!(rel_from[0].to, to);

        let rel_to = store.relations(&to);
        assert!(
            rel_to.iter().any(|e| e.kind == EDGE_REFERENCES && e.from == from),
            "converged target is reachable via Relations"
        );

        // Acceptance bar (#2716): full `receipts()` vector, never compat `receipt()`.
        let kinds: Vec<String> = store
            .receipts(&to)
            .iter()
            .map(|r| r.transform_id.clone())
            .collect();
        assert_eq!(
            kinds.len(),
            2,
            "converged node holds title + references receipts: {kinds:?}"
        );
        assert!(
            kinds.iter().any(|k| k.contains("title")),
            "title derivation present: {kinds:?}"
        );
        assert!(
            kinds.iter().any(|k| k.contains("references")),
            "references derivation present: {kinds:?}"
        );
        assert_eq!(
            kinds.iter().collect::<std::collections::BTreeSet<_>>().len(),
            2,
            "receipts must be distinct sources, not duplicates: {kinds:?}"
        );

        // Idempotent re-attach: exact duplicate must not grow the multiset.
        let dup = store.receipts(&to)[0].clone();
        let chain_before = store.receipt_chain_len();
        store.attach_receipt(to, dup).unwrap();
        assert_eq!(
            store.receipts(&to).len(),
            2,
            "idempotent re-attach keeps multiset size at 2"
        );
        assert_eq!(
            store.receipt_chain_len(),
            chain_before,
            "idempotent re-attach must not extend audit chain"
        );
        assert!(store.verify_receipts(), "multiset still verifies after re-attach");

        // Compat `receipt()` drops all-but-first — must not be used for acceptance.
        let singular = store.receipt(&to).expect("compat accessor returns first");
        assert_eq!(
            singular.transform_id, kinds[0],
            "receipt() singular is first-only; acceptance must use receipts()"
        );
        assert_ne!(
            singular.transform_id, kinds[1],
            "receipt() would false-green multi-source recall"
        );
    }

    /// WS-7 first end-to-end proof (#2346): ingest → assembly → API Relations + Get.
    /// Consumer path must match store-level recall smoke; run via `scripts/ws7-e2e-api-smoke.sh`.
    #[test]
    fn ws7_e2e_api_relations_and_get_after_assembly() {
        use attractor_api::dispatch_with_store;
        use attractor_proto::{
            ApiRequest, ApiResponse, GetData, RelationsData, RequestKind,
        };

        let notes = vec![
            note(b"# Alpha\n", "a.md"),
            note(b"# B Note\n\nsee [[Alpha]]\n", "b.md"),
        ];
        let mut store = Store::new();
        let out = assemble_notes(&mut store, &notes).unwrap();
        assert_eq!(out.linked.len(), 1);

        let to = ContentHash::from_hex(out.linked[0].to.as_str()).unwrap();
        let from_hex = out.linked[0].from.clone();
        let to_hex = out.linked[0].to.clone();

        match dispatch_with_store(
            &ApiRequest::new(RequestKind::Relations { address: from_hex.clone() }),
            0,
            &mut store,
        ) {
            ApiResponse::Ok { data, .. } => {
                let rel: RelationsData = serde_json::from_value(data).unwrap();
                let ref_edges: Vec<_> = rel
                    .edges
                    .iter()
                    .filter(|e| e.kind == EDGE_REFERENCES)
                    .collect();
                assert_eq!(
                    ref_edges.len(),
                    1,
                    "referencing identity has one references edge via API Relations"
                );
                assert_eq!(ref_edges[0].to, to_hex);
            }
            other => panic!("Relations expected Ok, got {other:?}"),
        }

        match dispatch_with_store(
            &ApiRequest::new(RequestKind::Get { address: to_hex.clone() }),
            0,
            &mut store,
        ) {
            ApiResponse::Ok { data, .. } => {
                let get: GetData = serde_json::from_value(data).unwrap();
                assert_eq!(get.address, to_hex);
                assert!(!get.provenance_refs.is_empty());
                assert_eq!(get.provenance_refs[0].origin, "repo:milton");
                assert!(
                    get.receipt.transform_id.contains("title"),
                    "Get receipt must carry transform metadata: {}",
                    get.receipt.transform_id
                );
                assert_eq!(get.receipt.graph_address, to_hex);
            }
            other => panic!("Get expected Ok, got {other:?}"),
        }

        let kinds: Vec<String> = store
            .receipts(&to)
            .iter()
            .map(|r| r.transform_id.clone())
            .collect();
        assert_eq!(kinds.len(), 2, "store multiset unchanged after API queries: {kinds:?}");
    }
}
