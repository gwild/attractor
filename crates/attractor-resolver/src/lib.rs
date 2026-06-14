#![forbid(unsafe_code)]
#![warn(clippy::pedantic, clippy::nursery)]
#![deny(missing_docs, missing_debug_implementations)]

//! # attractor-resolver
//!
//! WS-3 batch-time reference resolution. The references projector
//! ([`attractor_projector::project_markdown_references`]) emits one
//! *evidence* node per referenced target (`[[Alpha]]` -> `Cell{Text("Alpha")}`)
//! but deliberately materializes **no** cross-note edge — that is this crate's
//! job, under the allowlist rule "evidence becomes an approved edge only when
//! the target resolves".
//!
//! ## What "resolves" means
//!
//! Identity in Attractor is value-only (`attractor-core` #2249): a `Cell`'s
//! address hashes its `value`, not its `source`. So a reference target
//! `Cell{Text("Alpha")}` and the identity node a note promotes for its own
//! title/frontmatter-id `Cell{Text("Alpha")}` **converge to the same address**.
//! A reference therefore "resolves" exactly when an identity node (a title or
//! frontmatter-id promotion) with the same address is present.
//!
//! ## Why batch-time (v0)
//!
//! The store is address-only (`get`/`contains`; no value->role lookup, no
//! enumeration), so it cannot tell whether a matching address is a *genuine
//! identity* (a title/frontmatter-id promotion) or an incidental same-valued
//! cell. Within a single co-projected **batch** every node's projector role is
//! known directly from its receipt's `transform_id`, so we can resolve against
//! genuine identities only. A store-wide identity index (value -> identity role)
//! is the follow-up that lifts this beyond one batch.
//!
//! ## Scope of v0 (this crate)
//!
//! This crate performs the **pure classification**: given the receipts emitted
//! by a co-projected batch, it reports, per reference target, whether it
//! resolves to an identity present in the batch, and surfaces the
//! `source_hash -> identity` join an edge-materialization layer needs. It does
//! **not** write the store or construct edges: the edge endpoints and the
//! handling of mutual-reference cycles against the store's DAG invariant are a
//! separate decision (the store is acyclic; a symmetric `references` relation is
//! not). Keeping resolution pure means the classification is correct regardless
//! of how — or whether — edges are later materialized.

use attractor_proto::{ContentHashHex, E2eReceipt};
use std::collections::HashSet;
use std::fmt;

use attractor_projector::{
    MARKDOWN_FRONTMATTER_ID_PROJECTOR, MARKDOWN_REFERENCES_PROJECTOR, MARKDOWN_TITLE_PROJECTOR,
};

/// Stable transform identifier for this resolver (carried on any receipt or
/// edge a later materialization layer attributes to batch-time resolution).
pub const MARKDOWN_REFERENCES_RESOLVER: &str = "attractor.resolver.markdown.references.v0";

/// Edge kind a resolved reference would materialize as. Defined here so the
/// identifier has a single home; this crate does not itself emit the edge.
pub const EDGE_REFERENCES: &str = "references";

/// Which allowlisted promotion produced an identity node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityKind {
    /// Promoted from a markdown title (`# Heading`).
    Title,
    /// Promoted from a leading-frontmatter `id` value.
    FrontmatterId,
}

/// An identity node present in the batch — the resolvable target of a reference.
///
/// `address` is value-only, so two notes that promote the same identity value
/// (e.g. both titled `Alpha`) yield two `Identity` rows with the **same**
/// `address` and different `source_hash`. That is not a bug: the converged node
/// is one identity with two provenances; a consumer that needs note-level edges
/// must decide how to treat that fan-out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// Content address of the identity node (value-only).
    pub address: ContentHashHex,
    /// Source hash of the note that promoted this identity.
    pub source_hash: ContentHashHex,
    /// Which promotion produced it.
    pub kind: IdentityKind,
}

/// Whether a reference target matched a genuine identity present in the batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionStatus {
    /// At least one identity node in the batch has this target's address. The
    /// matching identities (and their owning notes) are listed in
    /// [`BatchResolution::identities`]; join on `address` to recover them — the
    /// match may fan out across more than one note.
    Resolved,
    /// No identity node in the batch has this address; the reference stays
    /// evidence.
    Unresolved,
}

/// One reference target's resolution against the co-projected batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// Address of the reference-target evidence node. When [`ResolutionStatus::Resolved`]
    /// this equals the matched identity node's address (value convergence).
    pub target_address: ContentHashHex,
    /// Source hash of the note the reference occurs in (the referencing doc).
    pub referencing_source_hash: ContentHashHex,
    /// Number of occurrences (source spans) the projector recorded for this
    /// target — repeated `[[Alpha]]` references collapse to one node with N spans.
    pub occurrences: usize,
    /// Whether the target resolves to a batch identity.
    pub status: ResolutionStatus,
}

/// Resolution of a whole co-projected batch: every reference target classified,
/// plus the identity nodes available as resolution targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchResolution {
    /// One entry per reference target (first-seen order of the input receipts).
    pub resolutions: Vec<Resolution>,
    /// Identity nodes the batch promoted, available as resolution targets.
    pub identities: Vec<Identity>,
}

impl BatchResolution {
    /// Iterate the reference targets that resolved to a batch identity.
    pub fn resolved(&self) -> impl Iterator<Item = &Resolution> {
        self.resolutions
            .iter()
            .filter(|r| r.status == ResolutionStatus::Resolved)
    }

    /// Iterate the reference targets that did not resolve (stay evidence).
    pub fn unresolved(&self) -> impl Iterator<Item = &Resolution> {
        self.resolutions
            .iter()
            .filter(|r| r.status == ResolutionStatus::Unresolved)
    }

    /// The identities whose address equals `address` (the fan-out a resolved
    /// target maps to; usually one, more than one when several notes promote the
    /// same identity value).
    pub fn identities_at<'a>(
        &'a self,
        address: &'a ContentHashHex,
    ) -> impl Iterator<Item = &'a Identity> {
        self.identities
            .iter()
            .filter(move |i| &i.address == address)
    }
}

/// Why a batch could not be resolved.
#[derive(Debug, PartialEq, Eq)]
pub enum ResolverError {
    /// A receipt carried no source spans, so its owning note's source hash could
    /// not be determined. Identity and reference receipts always carry at least
    /// one span; an empty one is malformed input, rejected rather than guessed.
    MissingSourceSpan {
        /// The graph address of the offending receipt.
        graph_address: ContentHashHex,
    },
    /// A receipt's source spans did not all share one source hash, so the note
    /// it derives from is ambiguous. A single receipt is projected from exactly
    /// one document; disagreement is malformed input.
    InconsistentSourceHash {
        /// The graph address of the offending receipt.
        graph_address: ContentHashHex,
    },
}

impl fmt::Display for ResolverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSourceSpan { graph_address } => write!(
                f,
                "receipt {} has no source spans; owning-note source hash is undetermined",
                graph_address.as_str()
            ),
            Self::InconsistentSourceHash { graph_address } => write!(
                f,
                "receipt {} has source spans from more than one source hash",
                graph_address.as_str()
            ),
        }
    }
}

impl std::error::Error for ResolverError {}

/// The single source hash a receipt derives from.
///
/// Fails fast (rather than guessing) on a span-less receipt or one whose spans
/// disagree — both are malformed projector output, not a recoverable state.
fn owning_source_hash(receipt: &E2eReceipt) -> Result<ContentHashHex, ResolverError> {
    let first = receipt
        .source_spans
        .first()
        .ok_or_else(|| ResolverError::MissingSourceSpan {
            graph_address: receipt.graph_address.clone(),
        })?;
    if receipt
        .source_spans
        .iter()
        .any(|s| s.source_hash != first.source_hash)
    {
        return Err(ResolverError::InconsistentSourceHash {
            graph_address: receipt.graph_address.clone(),
        });
    }
    Ok(first.source_hash.clone())
}

/// Resolve a co-projected batch: classify every reference target against the
/// identity nodes (title / frontmatter-id promotions) present in the same batch.
///
/// Receipts whose `transform_id` is neither an identity promotion nor the
/// references projector are ignored — the resolver only relates references to
/// identities, and tolerating other transforms keeps it forward-compatible with
/// projectors added later.
///
/// # Errors
/// [`ResolverError`] if any title / frontmatter-id / references receipt is
/// malformed (no source spans, or spans from more than one source hash).
pub fn resolve_batch(receipts: &[E2eReceipt]) -> Result<BatchResolution, ResolverError> {
    let mut identities = Vec::new();
    for receipt in receipts {
        let kind = match receipt.transform_id.as_str() {
            MARKDOWN_TITLE_PROJECTOR => IdentityKind::Title,
            MARKDOWN_FRONTMATTER_ID_PROJECTOR => IdentityKind::FrontmatterId,
            _ => continue,
        };
        identities.push(Identity {
            address: receipt.graph_address.clone(),
            source_hash: owning_source_hash(receipt)?,
            kind,
        });
    }

    let identity_addresses: HashSet<&ContentHashHex> =
        identities.iter().map(|i| &i.address).collect();

    let mut resolutions = Vec::new();
    for receipt in receipts {
        if receipt.transform_id != MARKDOWN_REFERENCES_PROJECTOR {
            continue;
        }
        let status = if identity_addresses.contains(&receipt.graph_address) {
            ResolutionStatus::Resolved
        } else {
            ResolutionStatus::Unresolved
        };
        resolutions.push(Resolution {
            target_address: receipt.graph_address.clone(),
            referencing_source_hash: owning_source_hash(receipt)?,
            occurrences: receipt.source_spans.len(),
            status,
        });
    }

    Ok(BatchResolution {
        resolutions,
        identities,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        owning_source_hash, resolve_batch, IdentityKind, ResolutionStatus, ResolverError,
        MARKDOWN_REFERENCES_PROJECTOR,
    };
    use attractor_core::ContentHash;
    use attractor_projector::{
        project_markdown_frontmatter_id, project_markdown_references, project_markdown_title,
        MarkdownSource,
    };
    use attractor_proto::{ContentHashHex, E2eReceipt, SourceSpan};

    /// A `MarkdownSource` whose declared hash matches `markdown` (the projectors
    /// reject a mismatch), labelled by `path` so distinct notes get distinct
    /// source hashes from their distinct bytes.
    fn source(markdown: &[u8], path: &str) -> MarkdownSource {
        MarkdownSource {
            source_hash: ContentHashHex::new(ContentHash::of(markdown).to_hex()).unwrap(),
            origin: "repo:milton".to_owned(),
            label: path.to_owned(),
        }
    }

    fn ref_receipts(markdown: &[u8], path: &str) -> Vec<E2eReceipt> {
        project_markdown_references(&source(markdown, path), markdown)
            .unwrap()
            .into_iter()
            .map(|p| p.receipt)
            .collect()
    }

    fn title_receipt(markdown: &[u8], path: &str) -> E2eReceipt {
        project_markdown_title(&source(markdown, path), markdown)
            .unwrap()
            .receipt
    }

    fn frontmatter_receipt(markdown: &[u8], path: &str) -> E2eReceipt {
        project_markdown_frontmatter_id(&source(markdown, path), markdown)
            .unwrap()
            .receipt
    }

    #[test]
    fn reference_resolves_to_title_identity_in_batch() {
        // Note A is titled "Alpha"; note B references [[Alpha]] and [[Ghost]].
        let note_a = b"# Alpha\n\nbody\n";
        let note_b = b"see [[Alpha]] and [[Ghost]]\n";

        let mut receipts = vec![title_receipt(note_a, "a.md")];
        receipts.extend(ref_receipts(note_b, "b.md"));

        let batch = resolve_batch(&receipts).unwrap();

        // One identity (Alpha from A); two reference targets (Alpha, Ghost).
        assert_eq!(batch.identities.len(), 1);
        assert_eq!(batch.identities[0].kind, IdentityKind::Title);
        assert_eq!(batch.resolutions.len(), 2);

        let resolved: Vec<_> = batch.resolved().collect();
        let unresolved: Vec<_> = batch.unresolved().collect();
        assert_eq!(resolved.len(), 1, "Alpha resolves");
        assert_eq!(unresolved.len(), 1, "Ghost stays evidence");

        // The resolved target's address equals the identity's address (value
        // convergence), and the join recovers the owning note.
        let alpha = resolved[0];
        assert_eq!(alpha.target_address, batch.identities[0].address);
        let owners: Vec<_> = batch.identities_at(&alpha.target_address).collect();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].source_hash, batch.identities[0].source_hash);

        // The unresolved target is genuinely absent from the identity set.
        assert!(batch
            .identities_at(&unresolved[0].target_address)
            .next()
            .is_none());
    }

    #[test]
    fn reference_resolves_to_frontmatter_id_identity() {
        let note_a = b"---\nid: Alpha\n---\n\n# heading\n";
        let note_b = b"link to [[Alpha]]\n";

        let mut receipts = vec![frontmatter_receipt(note_a, "a.md")];
        receipts.extend(ref_receipts(note_b, "b.md"));

        let batch = resolve_batch(&receipts).unwrap();
        assert_eq!(batch.identities.len(), 1);
        assert_eq!(batch.identities[0].kind, IdentityKind::FrontmatterId);
        assert_eq!(batch.resolved().count(), 1);
        assert_eq!(batch.unresolved().count(), 0);
    }

    #[test]
    fn no_identities_leaves_every_reference_unresolved() {
        let note_b = b"[[Alpha]] [[Beta]]\n";
        let receipts = ref_receipts(note_b, "b.md");
        assert_eq!(receipts.len(), 2);

        let batch = resolve_batch(&receipts).unwrap();
        assert!(batch.identities.is_empty());
        assert_eq!(batch.resolved().count(), 0);
        assert_eq!(batch.unresolved().count(), 2);
    }

    #[test]
    fn repeated_reference_collapses_to_one_resolution_carrying_occurrences() {
        // [[Alpha]] appears twice -> one target node, two spans -> occurrences=2.
        let note_a = b"# Alpha\n";
        let note_b = b"[[Alpha]] then again [[Alpha]]\n";

        let mut receipts = vec![title_receipt(note_a, "a.md")];
        receipts.extend(ref_receipts(note_b, "b.md"));

        let batch = resolve_batch(&receipts).unwrap();
        assert_eq!(batch.resolutions.len(), 1, "one target despite two refs");
        assert_eq!(batch.resolutions[0].occurrences, 2);
        assert_eq!(batch.resolutions[0].status, ResolutionStatus::Resolved);
    }

    #[test]
    fn same_identity_value_in_two_notes_fans_out_but_resolves_once() {
        // Two notes both titled "Alpha" converge to ONE identity address with two
        // provenances; a single [[Alpha]] reference still resolves, and the join
        // surfaces both owning notes.
        let note_a = b"# Alpha\n";
        let note_a2 = b"# Alpha\n\ndifferent body so the source hash differs\n";
        let note_b = b"[[Alpha]]\n";

        let mut receipts = vec![
            title_receipt(note_a, "a.md"),
            title_receipt(note_a2, "a2.md"),
        ];
        receipts.extend(ref_receipts(note_b, "b.md"));

        let batch = resolve_batch(&receipts).unwrap();
        assert_eq!(batch.identities.len(), 2, "two notes, two provenances");
        assert_eq!(
            batch.identities[0].address, batch.identities[1].address,
            "but one converged identity address"
        );
        assert_eq!(batch.resolved().count(), 1);
        let target = &batch.resolved().next().unwrap().target_address;
        assert_eq!(
            batch.identities_at(target).count(),
            2,
            "join fans out to both owning notes"
        );
    }

    #[test]
    fn unrelated_transform_ids_are_ignored() {
        // A receipt from some other transform must not be treated as an identity
        // or a reference.
        let other = E2eReceipt {
            graph_address: ContentHashHex::new("a".repeat(64)).unwrap(),
            source_spans: vec![SourceSpan {
                source_hash: ContentHashHex::new("b".repeat(64)).unwrap(),
                start: 0,
                end: 1,
                label: None,
            }],
            transform_id: "attractor.projector.markdown.something_else.v0".to_owned(),
            confidence: Some(1.0),
            provenance_refs: Vec::new(),
        };
        let batch = resolve_batch(&[other]).unwrap();
        assert!(batch.identities.is_empty());
        assert!(batch.resolutions.is_empty());
    }

    #[test]
    fn empty_batch_resolves_to_nothing() {
        let batch = resolve_batch(&[]).unwrap();
        assert!(batch.identities.is_empty());
        assert!(batch.resolutions.is_empty());
    }

    #[test]
    fn span_less_reference_receipt_is_rejected() {
        let malformed = E2eReceipt {
            graph_address: ContentHashHex::new("c".repeat(64)).unwrap(),
            source_spans: Vec::new(),
            transform_id: MARKDOWN_REFERENCES_PROJECTOR.to_owned(),
            confidence: Some(1.0),
            provenance_refs: Vec::new(),
        };
        assert!(matches!(
            resolve_batch(&[malformed]),
            Err(ResolverError::MissingSourceSpan { .. })
        ));
    }

    #[test]
    fn reference_receipt_with_two_source_hashes_is_rejected() {
        let malformed = E2eReceipt {
            graph_address: ContentHashHex::new("d".repeat(64)).unwrap(),
            source_spans: vec![
                SourceSpan {
                    source_hash: ContentHashHex::new("e".repeat(64)).unwrap(),
                    start: 0,
                    end: 1,
                    label: None,
                },
                SourceSpan {
                    source_hash: ContentHashHex::new("f".repeat(64)).unwrap(),
                    start: 2,
                    end: 3,
                    label: None,
                },
            ],
            transform_id: MARKDOWN_REFERENCES_PROJECTOR.to_owned(),
            confidence: Some(1.0),
            provenance_refs: Vec::new(),
        };
        assert!(matches!(
            resolve_batch(&[malformed]),
            Err(ResolverError::InconsistentSourceHash { .. })
        ));
    }

    #[test]
    fn owning_source_hash_reads_the_single_note_hash() {
        let note = b"# Alpha\n";
        let receipt = title_receipt(note, "a.md");
        let expected = ContentHashHex::new(ContentHash::of(note).to_hex()).unwrap();
        assert_eq!(owning_source_hash(&receipt).unwrap(), expected);
    }
}
