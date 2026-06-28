#![forbid(unsafe_code)]
#![warn(clippy::pedantic, clippy::nursery)]
#![deny(missing_docs, missing_debug_implementations)]

//! # attractor-projector
//!
//! WS-3 markdown projection boundary for Attractor.
//!
//! Markdown is authored input, so this crate does not write raw markdown or
//! scratch text to the shared store. It accepts caller-owned raw bytes plus
//! source metadata, derives a small typed node set, and returns the nodes/links
//! a caller may insert after projection. The returned receipt carries populated
//! `source_spans`, which is the acceptance surface tracked by ISSUE #2517.

use attractor_core::{
    ByteRange, Cell, ContentHash, LineRange, MarkdownSourceSpan, Primitive, SourceRef, SyntaxKind,
};
use attractor_proto::{
    ContentHashHex, E2eReceipt, EvidenceCapability, FieldRef, IssueWikiAccepted,
    IssueWikiConformanceReceipt, IssueWikiMalformed, IssueWikiNoteEvidence,
    IssueWikiProjectionEnvelope, IssueWikiProjectionResult, IssueWikiProjectionState,
    IssueWikiReference, IssueWikiSource, IssueWikiStale, ProvenanceRef, ScoreContribution,
    SourceSpan, SpanBoundText, ISSUE_WIKI_FULL_TEXT_SCHEMA_ID_V0, ISSUE_WIKI_SCHEMA_ID_V0,
};
use attractor_store::{Node, Store, StoreError};
use std::fmt;

/// Stable transform identifier for the WS-3 markdown title projector.
pub const MARKDOWN_TITLE_PROJECTOR: &str = "attractor.projector.markdown.title.v0";
/// Stable transform identifier for the WS-3 frontmatter `id` projector.
///
/// This is the allowlisted `frontmatter id -> node identity` promotion from the
/// WS-3 working plan (Layer 4): the only frontmatter key promoted to a derived
/// node by this transform. Every other frontmatter key remains evidence.
pub const MARKDOWN_FRONTMATTER_ID_PROJECTOR: &str =
    "attractor.projector.markdown.frontmatter_id.v0";
/// Stable transform identifier for the WS-3 markdown references projector.
///
/// References (`[[wikilink]]`, `![[embed]]`) are **evidence, not promotion**:
/// this transform derives one node per referenced target with a `source_span`
/// per occurrence, but does NOT materialize a cross-note edge to the target —
/// that is the resolver + projector-approval's job (allowlist rule: everything
/// that is not an allowlisted promotion stays evidence).
pub const MARKDOWN_REFERENCES_PROJECTOR: &str = "attractor.projector.markdown.references.v0";
/// Edge kind from a derived markdown value back to its source document ref.
pub const EDGE_DERIVED_FROM: &str = "derived_from";
/// Stable transform identifier for the issue-to-wiki extractor.
pub const ISSUE_WIKI_PROJECTOR: &str = "attractor.projector.issue_wiki.v0";
/// Stable transform identifier for the Phase 2 full-text issue-to-wiki extractor.
pub const ISSUE_WIKI_FULL_TEXT_PROJECTOR: &str = "attractor.projector.issue_wiki.full_text.v0";

/// Source metadata for a markdown document entering the projector.
///
/// The raw markdown bytes stay with the caller. Attractor receives only
/// derived nodes plus source hashes/spans that let the raw evidence be resolved
/// outside the shared store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownSource {
    /// Content hash of the raw markdown payload.
    pub source_hash: ContentHashHex,
    /// Provenance origin, such as `vault:ops` or `repo:milton`.
    pub origin: String,
    /// Path or stable human label for the source document.
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IssueWikiField {
    value: String,
    start: u64,
    end: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IssueWikiParsed {
    identity: IssueWikiField,
    title: IssueWikiField,
    full_text: Option<IssueWikiField>,
    references: Vec<IssueWikiField>,
}

/// Derived projection output that is ready for a caller to write to the store.
#[derive(Clone, Debug, PartialEq)]
pub struct Projection {
    /// Derived nodes only. Raw markdown/scratch text is never included here.
    pub nodes: Vec<Node>,
    /// Edges to add after all nodes are inserted.
    pub links: Vec<ProjectedLink>,
    /// E2E receipt with non-empty `source_spans` for derived query acceptance.
    pub receipt: E2eReceipt,
}

/// A typed edge between projected nodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedLink {
    /// Source node address.
    pub from: ContentHashHex,
    /// Destination node address.
    pub to: ContentHashHex,
    /// Edge kind.
    pub kind: String,
}

/// Why markdown projection failed.
#[derive(Debug)]
pub enum ProjectorError {
    /// Markdown bytes were not valid UTF-8.
    InvalidUtf8(std::str::Utf8Error),
    /// Caller-declared source hash did not match the markdown bytes.
    SourceHashMismatch {
        /// Hash supplied in [`MarkdownSource::source_hash`].
        declared: ContentHashHex,
        /// Hash computed from the markdown bytes.
        actual: ContentHashHex,
    },
    /// No promotable markdown title was found.
    MissingTitle,
    /// No promotable frontmatter `id` was found in a leading `---` block.
    MissingFrontmatterId,
    /// Issue source did not contain a required issue-to-wiki field.
    MissingIssueWikiField {
        /// Required field path that was absent.
        field_path: String,
    },
    /// Issue source declared an invalid reference index.
    InvalidIssueWikiReferenceIndex {
        /// Field path that could not be mapped to a parsed reference.
        field_path: String,
    },
    /// Content addressing failed.
    Address(attractor_core::AddressError),
    /// JSON content addressing failed for a derived source-ref node.
    Serde(serde_json::Error),
    /// Source hash hex was valid on the wire type but could not be decoded.
    InvalidSourceHash(String),
    /// Store insertion or link failed after projection succeeded.
    Store(StoreError),
}

impl fmt::Display for ProjectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8(e) => write!(f, "markdown source is not UTF-8: {e}"),
            Self::SourceHashMismatch { declared, actual } => write!(
                f,
                "markdown source hash mismatch: declared {} but computed {}",
                declared.as_str(),
                actual.as_str()
            ),
            Self::MissingTitle => f.write_str("markdown source has no promotable title"),
            Self::MissingFrontmatterId => {
                f.write_str("markdown source has no promotable frontmatter id")
            }
            Self::MissingIssueWikiField { field_path } => {
                write!(
                    f,
                    "issue_wiki source is missing required field {field_path}"
                )
            }
            Self::InvalidIssueWikiReferenceIndex { field_path } => {
                write!(
                    f,
                    "issue_wiki source has invalid reference field {field_path}"
                )
            }
            Self::Address(e) => write!(f, "cannot address projected node: {e}"),
            Self::Serde(e) => write!(f, "cannot address projected source ref: {e}"),
            Self::InvalidSourceHash(e) => write!(f, "invalid source hash: {e}"),
            Self::Store(e) => write!(f, "store write failed after projection: {e}"),
        }
    }
}

impl std::error::Error for ProjectorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidUtf8(e) => Some(e),
            Self::Address(e) => Some(e),
            Self::Serde(e) => Some(e),
            Self::Store(e) => Some(e),
            Self::SourceHashMismatch { .. }
            | Self::MissingTitle
            | Self::MissingFrontmatterId
            | Self::MissingIssueWikiField { .. }
            | Self::InvalidIssueWikiReferenceIndex { .. }
            | Self::InvalidSourceHash(_) => None,
        }
    }
}

impl From<attractor_core::AddressError> for ProjectorError {
    fn from(e: attractor_core::AddressError) -> Self {
        Self::Address(e)
    }
}

impl From<serde_json::Error> for ProjectorError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serde(e)
    }
}

impl From<StoreError> for ProjectorError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// Project one markdown note into derived store nodes plus a populated receipt.
///
/// This function is intentionally store-free: it performs projection before any
/// shared-store write can happen. Callers insert [`Projection::nodes`] and then
/// [`Projection::links`] only after this function returns `Ok`.
///
/// # Errors
/// Returns [`ProjectorError::SourceHashMismatch`] when the caller-declared
/// source hash does not match the markdown bytes,
/// [`ProjectorError::InvalidUtf8`] for non-UTF-8 markdown,
/// [`ProjectorError::MissingTitle`] when no markdown heading exists, and
/// address/serialization errors if derived values cannot be addressed.
pub fn project_markdown_title(
    source: &MarkdownSource,
    markdown: &[u8],
) -> Result<Projection, ProjectorError> {
    let core_source_hash = content_hash_from_hex(&source.source_hash)?;
    let actual_source_hash = ContentHash::of(markdown);
    if core_source_hash != actual_source_hash {
        return Err(ProjectorError::SourceHashMismatch {
            declared: source.source_hash.clone(),
            actual: hash_hex(&actual_source_hash),
        });
    }
    let text = std::str::from_utf8(markdown).map_err(ProjectorError::InvalidUtf8)?;
    let heading = first_heading(text).ok_or(ProjectorError::MissingTitle)?;
    let source_ref = SourceRef {
        content_hash: core_source_hash,
        origin: source.origin.clone(),
    };
    let derived = Cell {
        value: Primitive::Text(heading.title.to_owned()),
        source: source_ref.clone(),
    };
    let derived_addr = derived.address()?;
    let derived_hex = hash_hex(&derived_addr);

    let source_ref_cell = Cell {
        value: Primitive::Ref(core_source_hash),
        source: source_ref,
    };
    let source_ref_addr = source_ref_cell.address()?;
    let source_ref_hex = hash_hex(&source_ref_addr);

    let span = MarkdownSourceSpan {
        vault: source.origin.clone(),
        commit: None,
        path: source.label.clone(),
        frontmatter_id: None,
        heading_path: vec![heading.title.to_owned()],
        block_id: None,
        byte_range: ByteRange {
            start: heading.start,
            end: heading.end,
        },
        line_range: LineRange {
            start: heading.line,
            end: heading.line,
        },
        syntax_kind: SyntaxKind::Heading,
        source_hash: core_source_hash,
    }
    .to_source_span();
    let provenance = ProvenanceRef {
        content_hash: source.source_hash.clone(),
        origin: source.origin.clone(),
    };
    let receipt = E2eReceipt {
        graph_address: derived_hex.clone(),
        source_spans: vec![span],
        transform_id: MARKDOWN_TITLE_PROJECTOR.to_owned(),
        confidence: Some(1.0),
        provenance_refs: vec![provenance],
    };

    Ok(Projection {
        nodes: vec![Node::Cell(derived), Node::Cell(source_ref_cell)],
        links: vec![ProjectedLink {
            from: derived_hex,
            to: source_ref_hex,
            kind: EDGE_DERIVED_FROM.to_owned(),
        }],
        receipt,
    })
}

/// Project markdown and write only the accepted derived nodes into `store`.
///
/// This is the WS-3 e2e helper: projection is completed before any store write,
/// then all projected nodes are inserted and linked. The returned receipt is
/// the same populated receipt from [`project_markdown_title`].
///
/// # Errors
/// Returns the projection errors from [`project_markdown_title`] before any
/// store mutation. Returns [`ProjectorError::Store`] if an accepted projected
/// node or link cannot be written.
pub fn project_markdown_title_into_store(
    store: &mut Store,
    source: &MarkdownSource,
    markdown: &[u8],
) -> Result<E2eReceipt, ProjectorError> {
    let projection = project_markdown_title(source, markdown)?;
    let graph_addr = content_hash_from_hex(&projection.receipt.graph_address)?;
    for node in projection.nodes {
        store.insert(node)?;
    }
    for link in projection.links {
        store.link(
            content_hash_from_hex(&link.from)?,
            content_hash_from_hex(&link.to)?,
            link.kind,
        )?;
    }
    store.attach_receipt(graph_addr, projection.receipt.clone())?;
    Ok(projection.receipt)
}

/// Extract issue-to-wiki evidence from source bytes with value/span receipts.
///
/// This is the L2 issue-wiki conformance boundary: raw issue bytes are consumed
/// by the extractor, but accepted rows contain only typed values plus
/// extractor-time receipts binding `{source_hash, field_path, value_hash,
/// span}`. No store mutation is constructed here.
#[must_use]
pub fn project_issue_wiki(
    source: IssueWikiSource,
    source_bytes: &[u8],
) -> IssueWikiProjectionResult {
    project_issue_wiki_with_contract(
        source,
        source_bytes,
        ISSUE_WIKI_SCHEMA_ID_V0,
        ISSUE_WIKI_PROJECTOR,
        false,
    )
}

/// Extract full-text issue-to-wiki evidence from source bytes.
///
/// This Phase 2 projector uses a distinct schema/projector identity and carries
/// an optional full-text evidence payload in the projection row. Downstream
/// manifest, apply, write, publish, lease, and receipt types are unchanged.
#[must_use]
pub fn project_issue_wiki_full_text(
    source: IssueWikiSource,
    source_bytes: &[u8],
) -> IssueWikiProjectionResult {
    project_issue_wiki_with_contract(
        source,
        source_bytes,
        ISSUE_WIKI_FULL_TEXT_SCHEMA_ID_V0,
        ISSUE_WIKI_FULL_TEXT_PROJECTOR,
        true,
    )
}

fn project_issue_wiki_with_contract(
    source: IssueWikiSource,
    source_bytes: &[u8],
    schema_id: &str,
    projector_id: &str,
    require_full_text: bool,
) -> IssueWikiProjectionResult {
    let actual_source_hash = hash_hex(&ContentHash::of(source_bytes));
    if actual_source_hash != source.source_hash {
        return IssueWikiProjectionResult::Stale(Box::new(IssueWikiStale {
            schema_id: schema_id.to_owned(),
            source_ref: source.source_ref.clone(),
            expected_source_hash: source.source_hash.clone(),
            actual_source_hash: actual_source_hash.clone(),
            envelope: issue_wiki_envelope(
                IssueWikiProjectionState::Stale,
                &source.source_ref,
                actual_source_hash,
                projector_id,
                Vec::new(),
            ),
        }));
    }

    let note = match parse_issue_wiki_note(&source, source_bytes, require_full_text) {
        Ok(note) => note,
        Err(err) => {
            return issue_wiki_malformed_with_contract(
                &source,
                schema_id,
                projector_id,
                err.to_string(),
                vec![issue_wiki_error_field_ref(&err)],
            );
        }
    };
    project_issue_wiki_candidate_with_contract(source, source_bytes, note, schema_id, projector_id)
}

#[cfg(test)]
fn project_issue_wiki_candidate(
    source: IssueWikiSource,
    source_bytes: &[u8],
    note: IssueWikiNoteEvidence,
) -> IssueWikiProjectionResult {
    project_issue_wiki_candidate_with_contract(
        source,
        source_bytes,
        note,
        ISSUE_WIKI_SCHEMA_ID_V0,
        ISSUE_WIKI_PROJECTOR,
    )
}

fn project_issue_wiki_candidate_with_contract(
    source: IssueWikiSource,
    source_bytes: &[u8],
    note: IssueWikiNoteEvidence,
    schema_id: &str,
    projector_id: &str,
) -> IssueWikiProjectionResult {
    let actual_source_hash = hash_hex(&ContentHash::of(source_bytes));
    if actual_source_hash != source.source_hash {
        return IssueWikiProjectionResult::Stale(Box::new(IssueWikiStale {
            schema_id: schema_id.to_owned(),
            source_ref: source.source_ref.clone(),
            expected_source_hash: source.source_hash.clone(),
            actual_source_hash: actual_source_hash.clone(),
            envelope: issue_wiki_envelope(
                IssueWikiProjectionState::Stale,
                &source.source_ref,
                actual_source_hash,
                projector_id,
                Vec::new(),
            ),
        }));
    }

    let receipts = match validate_issue_wiki_conformance(&source, source_bytes, &note) {
        Ok(receipts) => receipts,
        Err((reason, field_refs)) => {
            return issue_wiki_malformed_with_contract(
                &source,
                schema_id,
                projector_id,
                reason,
                field_refs,
            );
        }
    };
    let field_refs = receipts
        .iter()
        .map(|receipt| FieldRef {
            path: receipt.field_path.clone(),
        })
        .collect();

    IssueWikiProjectionResult::Accepted(Box::new(IssueWikiAccepted {
        schema_id: schema_id.to_owned(),
        source: source.clone(),
        note,
        conformance_receipts: receipts,
        envelope: issue_wiki_envelope(
            IssueWikiProjectionState::Accepted,
            &source.source_ref,
            source.source_hash,
            projector_id,
            field_refs,
        ),
    }))
}

/// Project a note's frontmatter `id` into a derived identity node plus receipt.
///
/// This is the allowlisted `frontmatter id -> node identity` promotion. Like
/// [`project_markdown_title`] it is store-free and produces exactly one derived
/// node (the identity value) linked `derived_from` its source ref, with a
/// populated `source_span` pointing at the `id` value inside the frontmatter
/// block. Only an `id` key in a leading `---` fenced block is promoted; any
/// other frontmatter key stays evidence and is not projected here.
///
/// # Errors
/// Returns [`ProjectorError::InvalidUtf8`] for non-UTF-8 markdown,
/// [`ProjectorError::MissingFrontmatterId`] when there is no leading frontmatter
/// block carrying a non-empty `id`, [`ProjectorError::SourceHashMismatch`] when
/// the caller-declared `source_hash` does not match the markdown bytes, and
/// address/serialization errors if derived values cannot be addressed.
pub fn project_markdown_frontmatter_id(
    source: &MarkdownSource,
    markdown: &[u8],
) -> Result<Projection, ProjectorError> {
    let core_source_hash = content_hash_from_hex(&source.source_hash)?;
    let actual_source_hash = ContentHash::of(markdown);
    if core_source_hash != actual_source_hash {
        return Err(ProjectorError::SourceHashMismatch {
            declared: source.source_hash.clone(),
            actual: hash_hex(&actual_source_hash),
        });
    }
    let text = std::str::from_utf8(markdown).map_err(ProjectorError::InvalidUtf8)?;
    let id = frontmatter_id(text).ok_or(ProjectorError::MissingFrontmatterId)?;
    let source_ref = SourceRef {
        content_hash: core_source_hash,
        origin: source.origin.clone(),
    };
    let derived = Cell {
        value: Primitive::Text(id.value.to_owned()),
        source: source_ref.clone(),
    };
    let derived_addr = derived.address()?;
    let derived_hex = hash_hex(&derived_addr);

    let source_ref_cell = Cell {
        value: Primitive::Ref(core_source_hash),
        source: source_ref,
    };
    let source_ref_addr = source_ref_cell.address()?;
    let source_ref_hex = hash_hex(&source_ref_addr);

    let span = MarkdownSourceSpan {
        vault: source.origin.clone(),
        commit: None,
        path: source.label.clone(),
        frontmatter_id: Some(id.value.to_owned()),
        heading_path: Vec::new(),
        block_id: None,
        byte_range: ByteRange {
            start: id.start,
            end: id.end,
        },
        line_range: LineRange {
            start: id.line,
            end: id.line,
        },
        syntax_kind: SyntaxKind::Frontmatter,
        source_hash: core_source_hash,
    }
    .to_source_span();
    let provenance = ProvenanceRef {
        content_hash: source.source_hash.clone(),
        origin: source.origin.clone(),
    };
    let receipt = E2eReceipt {
        graph_address: derived_hex.clone(),
        source_spans: vec![span],
        transform_id: MARKDOWN_FRONTMATTER_ID_PROJECTOR.to_owned(),
        confidence: Some(1.0),
        provenance_refs: vec![provenance],
    };

    Ok(Projection {
        nodes: vec![Node::Cell(derived), Node::Cell(source_ref_cell)],
        links: vec![ProjectedLink {
            from: derived_hex,
            to: source_ref_hex,
            kind: EDGE_DERIVED_FROM.to_owned(),
        }],
        receipt,
    })
}

/// Project a note's frontmatter `id` and write the derived nodes into `store`.
///
/// The store-writing mirror of [`project_markdown_frontmatter_id`], matching the
/// projection-before-write discipline of [`project_markdown_title_into_store`].
///
/// # Errors
/// Returns the projection errors from [`project_markdown_frontmatter_id`] before
/// any store mutation, then [`ProjectorError::Store`] if an accepted projected
/// node or link cannot be written.
pub fn project_markdown_frontmatter_id_into_store(
    store: &mut Store,
    source: &MarkdownSource,
    markdown: &[u8],
) -> Result<E2eReceipt, ProjectorError> {
    let projection = project_markdown_frontmatter_id(source, markdown)?;
    let graph_addr = content_hash_from_hex(&projection.receipt.graph_address)?;
    for node in projection.nodes {
        store.insert(node)?;
    }
    for link in projection.links {
        store.link(
            content_hash_from_hex(&link.from)?,
            content_hash_from_hex(&link.to)?,
            link.kind,
        )?;
    }
    store.attach_receipt(graph_addr, projection.receipt.clone())?;
    Ok(projection.receipt)
}

/// Project a note's markdown references (`[[wikilink]]` / `![[embed]]`) into one
/// derived evidence node per referenced target.
///
/// Each unique target becomes one content-addressed node linked `derived_from`
/// the source ref; its receipt carries a `source_span` for EVERY occurrence of
/// that target — so a note that links `[[foo]]` twice yields one node with two
/// spans. This reuses the receipt's existing `source_spans` vector; no new
/// shape. References are **evidence only**: no cross-note edge to the target is
/// materialized here (that is the resolver's job, per the allowlist rule).
///
/// Returns one [`Projection`] per unique target, in first-seen order. A note
/// with no references returns an empty vec — references are optional evidence,
/// not a required promotion, so their absence is not an error (unlike
/// [`project_markdown_title`] / [`project_markdown_frontmatter_id`]).
///
/// # Errors
/// [`ProjectorError::SourceHashMismatch`] when the declared `source_hash` does
/// not match the bytes, [`ProjectorError::InvalidUtf8`] for non-UTF-8 markdown,
/// and address/serialization errors if a derived value cannot be addressed.
pub fn project_markdown_references(
    source: &MarkdownSource,
    markdown: &[u8],
) -> Result<Vec<Projection>, ProjectorError> {
    let core_source_hash = content_hash_from_hex(&source.source_hash)?;
    let actual_source_hash = ContentHash::of(markdown);
    if core_source_hash != actual_source_hash {
        return Err(ProjectorError::SourceHashMismatch {
            declared: source.source_hash.clone(),
            actual: hash_hex(&actual_source_hash),
        });
    }
    let text = std::str::from_utf8(markdown).map_err(ProjectorError::InvalidUtf8)?;

    // Group occurrences by target (first-seen order), so repeated references to
    // one target collapse to a single content-addressed node carrying every
    // span, rather than colliding receipts on the same graph address.
    let mut groups: Vec<(&str, Vec<SourceSpan>)> = Vec::new();
    for r in scan_references(text) {
        let span = MarkdownSourceSpan {
            vault: source.origin.clone(),
            commit: None,
            path: source.label.clone(),
            frontmatter_id: None,
            heading_path: Vec::new(),
            block_id: None,
            byte_range: ByteRange {
                start: r.start,
                end: r.end,
            },
            line_range: LineRange {
                start: r.line,
                end: r.line,
            },
            syntax_kind: r.kind,
            source_hash: core_source_hash,
        }
        .to_source_span();
        if let Some(slot) = groups.iter_mut().find(|(t, _)| *t == r.target) {
            slot.1.push(span);
        } else {
            groups.push((r.target, vec![span]));
        }
    }

    let mut projections = Vec::with_capacity(groups.len());
    for (target, spans) in groups {
        let source_ref = SourceRef {
            content_hash: core_source_hash,
            origin: source.origin.clone(),
        };
        let derived = Cell {
            value: Primitive::Text(target.to_owned()),
            source: source_ref.clone(),
        };
        let derived_hex = hash_hex(&derived.address()?);
        let source_ref_cell = Cell {
            value: Primitive::Ref(core_source_hash),
            source: source_ref,
        };
        let source_ref_hex = hash_hex(&source_ref_cell.address()?);
        let receipt = E2eReceipt {
            graph_address: derived_hex.clone(),
            source_spans: spans,
            transform_id: MARKDOWN_REFERENCES_PROJECTOR.to_owned(),
            confidence: Some(1.0),
            provenance_refs: vec![ProvenanceRef {
                content_hash: source.source_hash.clone(),
                origin: source.origin.clone(),
            }],
        };
        projections.push(Projection {
            nodes: vec![Node::Cell(derived), Node::Cell(source_ref_cell)],
            links: vec![ProjectedLink {
                from: derived_hex,
                to: source_ref_hex,
                kind: EDGE_DERIVED_FROM.to_owned(),
            }],
            receipt,
        });
    }
    Ok(projections)
}

/// Project markdown references and write the derived evidence nodes into
/// `store`. Store-writing mirror of [`project_markdown_references`]; returns one
/// receipt per unique target.
///
/// # Errors
/// The projection errors from [`project_markdown_references`] before any store
/// mutation, then [`ProjectorError::Store`] if a node or link cannot be written.
pub fn project_markdown_references_into_store(
    store: &mut Store,
    source: &MarkdownSource,
    markdown: &[u8],
) -> Result<Vec<E2eReceipt>, ProjectorError> {
    let projections = project_markdown_references(source, markdown)?;
    let mut receipts = Vec::with_capacity(projections.len());
    for projection in projections {
        let graph_addr = content_hash_from_hex(&projection.receipt.graph_address)?;
        for node in projection.nodes {
            store.insert(node)?;
        }
        for link in projection.links {
            store.link(
                content_hash_from_hex(&link.from)?,
                content_hash_from_hex(&link.to)?,
                link.kind,
            )?;
        }
        store.attach_receipt(graph_addr, projection.receipt.clone())?;
        receipts.push(projection.receipt);
    }
    Ok(receipts)
}

fn issue_wiki_envelope(
    state: IssueWikiProjectionState,
    source_ref: &str,
    source_hash: ContentHashHex,
    projector_id: &str,
    field_refs: Vec<FieldRef>,
) -> IssueWikiProjectionEnvelope {
    IssueWikiProjectionEnvelope {
        projection_state: state,
        capability: match state {
            IssueWikiProjectionState::Accepted => EvidenceCapability {
                visible_for_resolution: true,
                usable_as_positive_evidence: true,
                score: ScoreContribution::Positive,
            },
            IssueWikiProjectionState::Stale | IssueWikiProjectionState::Malformed => {
                EvidenceCapability {
                    visible_for_resolution: true,
                    usable_as_positive_evidence: false,
                    score: ScoreContribution::Neutral,
                }
            }
        },
        source_ref: source_ref.to_owned(),
        source_hash,
        transform_id: projector_id.to_owned(),
        field_refs,
    }
}

fn issue_wiki_malformed_with_contract(
    source: &IssueWikiSource,
    schema_id: &str,
    projector_id: &str,
    reason: String,
    field_refs: Vec<FieldRef>,
) -> IssueWikiProjectionResult {
    IssueWikiProjectionResult::Malformed(Box::new(IssueWikiMalformed {
        schema_id: Some(schema_id.to_owned()),
        source_ref: source.source_ref.clone(),
        reason,
        field_refs: field_refs.clone(),
        envelope: issue_wiki_envelope(
            IssueWikiProjectionState::Malformed,
            &source.source_ref,
            source.source_hash.clone(),
            projector_id,
            field_refs,
        ),
    }))
}

fn issue_wiki_error_field_ref(err: &ProjectorError) -> FieldRef {
    match err {
        ProjectorError::MissingIssueWikiField { field_path }
        | ProjectorError::InvalidIssueWikiReferenceIndex { field_path } => FieldRef {
            path: field_path.clone(),
        },
        _ => FieldRef {
            path: "source".to_owned(),
        },
    }
}

fn parse_issue_wiki_note(
    source: &IssueWikiSource,
    source_bytes: &[u8],
    require_full_text: bool,
) -> Result<IssueWikiNoteEvidence, ProjectorError> {
    let parsed = parse_issue_wiki_source(source_bytes)?;
    let full_text = match (require_full_text, parsed.full_text) {
        (true, Some(full_text)) => Some(span_bound_text(
            full_text.value,
            &source.source_hash,
            full_text.start,
            full_text.end,
            "full_text",
        )),
        (true, None) => {
            return Err(ProjectorError::MissingIssueWikiField {
                field_path: "full_text".to_owned(),
            });
        }
        (false, _) => None,
    };
    Ok(IssueWikiNoteEvidence {
        identity: span_bound_text(
            parsed.identity.value,
            &source.source_hash,
            parsed.identity.start,
            parsed.identity.end,
            "identity",
        ),
        title: span_bound_text(
            parsed.title.value,
            &source.source_hash,
            parsed.title.start,
            parsed.title.end,
            "title",
        ),
        full_text,
        references: parsed
            .references
            .into_iter()
            .enumerate()
            .map(|(idx, reference)| IssueWikiReference {
                target: reference.value,
                label: None,
                span: source_span(
                    source.source_hash.clone(),
                    reference.start,
                    reference.end,
                    &format!("references[{idx}].target"),
                ),
            })
            .collect(),
    })
}

fn span_bound_text(
    value: String,
    source_hash: &ContentHashHex,
    start: u64,
    end: u64,
    field_path: &str,
) -> SpanBoundText {
    SpanBoundText {
        value,
        span: source_span(source_hash.clone(), start, end, field_path),
    }
}

fn source_span(source_hash: ContentHashHex, start: u64, end: u64, field_path: &str) -> SourceSpan {
    SourceSpan {
        source_hash,
        start,
        end,
        label: Some(field_path.to_owned()),
    }
}

fn parse_issue_wiki_source(source_bytes: &[u8]) -> Result<IssueWikiParsed, ProjectorError> {
    let text = std::str::from_utf8(source_bytes).map_err(ProjectorError::InvalidUtf8)?;
    let mut identity = None;
    let mut title = None;
    let mut full_text = None;
    let mut references = Vec::new();
    let mut offset = 0usize;

    for line in text.split_inclusive('\n') {
        let line_without_newline = line.trim_end_matches(['\r', '\n']);
        if identity.is_none() {
            identity = issue_wiki_line_field(line_without_newline, offset, "identity:");
        }
        if title.is_none() {
            title = issue_wiki_line_field(line_without_newline, offset, "title:");
        }
        if full_text.is_none() {
            full_text = issue_wiki_line_field(line_without_newline, offset, "body:");
        }
        if let Some(reference) = issue_wiki_line_field(line_without_newline, offset, "reference:") {
            references.push(reference);
        }
        offset += line.len();
    }

    Ok(IssueWikiParsed {
        identity: identity.ok_or_else(|| ProjectorError::MissingIssueWikiField {
            field_path: "identity".to_owned(),
        })?,
        title: title.ok_or_else(|| ProjectorError::MissingIssueWikiField {
            field_path: "title".to_owned(),
        })?,
        full_text,
        references,
    })
}

fn issue_wiki_line_field(line: &str, offset: usize, prefix: &str) -> Option<IssueWikiField> {
    let value = line.strip_prefix(prefix)?;
    let lead_ws = value.len() - value.trim_start().len();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let start = offset + prefix.len() + lead_ws;
    let end = start + trimmed.len();
    Some(IssueWikiField {
        value: trimmed.to_owned(),
        start: u64::try_from(start).unwrap_or(u64::MAX),
        end: u64::try_from(end).unwrap_or(u64::MAX),
    })
}

fn validate_issue_wiki_conformance(
    source: &IssueWikiSource,
    source_bytes: &[u8],
    note: &IssueWikiNoteEvidence,
) -> Result<Vec<IssueWikiConformanceReceipt>, (String, Vec<FieldRef>)> {
    let expected = parse_issue_wiki_source(source_bytes)
        .map_err(|err| (err.to_string(), vec![issue_wiki_error_field_ref(&err)]))?;
    let mut receipts = Vec::with_capacity(3 + note.references.len());

    validate_span_bound_text(
        "identity",
        &expected.identity,
        &note.identity,
        &source.source_hash,
        source_bytes,
        &mut receipts,
    )?;
    validate_span_bound_text(
        "title",
        &expected.title,
        &note.title,
        &source.source_hash,
        source_bytes,
        &mut receipts,
    )?;
    if let Some(full_text) = &note.full_text {
        let Some(expected_full_text) = &expected.full_text else {
            return Err((
                ProjectorError::MissingIssueWikiField {
                    field_path: "full_text".to_owned(),
                }
                .to_string(),
                vec![FieldRef {
                    path: "full_text".to_owned(),
                }],
            ));
        };
        validate_span_bound_text(
            "full_text",
            expected_full_text,
            full_text,
            &source.source_hash,
            source_bytes,
            &mut receipts,
        )?;
    }
    for (idx, reference) in note.references.iter().enumerate() {
        let field_path = format!("references[{idx}].target");
        let Some(expected_ref) = expected.references.get(idx) else {
            return Err((
                ProjectorError::InvalidIssueWikiReferenceIndex {
                    field_path: field_path.clone(),
                }
                .to_string(),
                vec![FieldRef { path: field_path }],
            ));
        };
        validate_text_span(
            &field_path,
            expected_ref,
            &reference.target,
            &reference.span,
            &source.source_hash,
            source_bytes,
            &mut receipts,
        )?;
    }
    Ok(receipts)
}

fn validate_span_bound_text(
    field_path: &str,
    expected: &IssueWikiField,
    actual: &SpanBoundText,
    source_hash: &ContentHashHex,
    source_bytes: &[u8],
    receipts: &mut Vec<IssueWikiConformanceReceipt>,
) -> Result<(), (String, Vec<FieldRef>)> {
    validate_text_span(
        field_path,
        expected,
        &actual.value,
        &actual.span,
        source_hash,
        source_bytes,
        receipts,
    )
}

fn validate_text_span(
    field_path: &str,
    expected: &IssueWikiField,
    value: &str,
    span: &SourceSpan,
    source_hash: &ContentHashHex,
    source_bytes: &[u8],
    receipts: &mut Vec<IssueWikiConformanceReceipt>,
) -> Result<(), (String, Vec<FieldRef>)> {
    let field_ref = FieldRef {
        path: field_path.to_owned(),
    };
    if span.source_hash != *source_hash {
        return Err((
            format!("span_source_hash_mismatch:{field_path}"),
            vec![field_ref],
        ));
    }
    if span.start != expected.start || span.end != expected.end {
        return Err((format!("span_mismatch:{field_path}"), vec![field_ref]));
    }
    if value != expected.value {
        return Err((format!("value_mismatch:{field_path}"), vec![field_ref]));
    }
    let Some(bytes) = source_bytes_for_span(source_bytes, span) else {
        return Err((format!("span_range_invalid:{field_path}"), vec![field_ref]));
    };
    if bytes != value.as_bytes() {
        return Err((format!("value_span_mismatch:{field_path}"), vec![field_ref]));
    }

    receipts.push(IssueWikiConformanceReceipt {
        source_hash: source_hash.clone(),
        field_path: field_path.to_owned(),
        value_hash: hash_hex(&ContentHash::of(bytes)),
        span: span.clone(),
    });
    Ok(())
}

fn source_bytes_for_span<'a>(source_bytes: &'a [u8], span: &SourceSpan) -> Option<&'a [u8]> {
    let start = usize::try_from(span.start).ok()?;
    let end = usize::try_from(span.end).ok()?;
    if start > end || end > source_bytes.len() {
        None
    } else {
        Some(&source_bytes[start..end])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Heading<'a> {
    title: &'a str,
    start: u64,
    end: u64,
    line: u32,
}

fn first_heading(markdown: &str) -> Option<Heading<'_>> {
    let mut offset = 0usize;
    let mut line_number = 1u32;
    for line in markdown.split_inclusive('\n') {
        let line_without_newline = line.trim_end_matches(['\r', '\n']);
        let trimmed = line_without_newline.trim_start();
        if let Some(title) = heading_title(trimmed) {
            let title_start = offset
                + line_without_newline
                    .find(title)
                    .expect("heading title comes from this line");
            let title_end = title_start + title.len();
            return Some(Heading {
                title,
                start: u64::try_from(title_start).unwrap_or(u64::MAX),
                end: u64::try_from(title_end).unwrap_or(u64::MAX),
                line: line_number,
            });
        }
        offset += line.len();
        line_number = line_number.saturating_add(1);
    }
    None
}

fn heading_title(line: &str) -> Option<&str> {
    let marker_len = line.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&marker_len) {
        return None;
    }
    let rest = &line[marker_len..];
    if !rest.starts_with(' ') {
        return None;
    }
    let title = rest.trim();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FrontmatterId<'a> {
    value: &'a str,
    start: u64,
    end: u64,
    line: u32,
}

/// Extract the top-level `id` value from a leading `---` frontmatter block.
///
/// Strict v0 shape (fail-loud, no silent coercion), mirroring [`heading_title`]:
/// the markdown must open with a `---` fence on its first line **and be closed by
/// a matching `---` fence** — an unterminated block is malformed and promotes
/// nothing (the value is only trusted once the block is known to be a complete
/// fenced region). The key must be an unindented `id` immediately before the
/// colon; the value is the trimmed remainder with at most one matched pair of
/// surrounding quotes removed. The returned span covers the value text exactly
/// (quote-stripping shifts `start` inward), preserving the span ==
/// derived-value invariant. Indented keys, block/flow YAML values, and
/// non-leading frontmatter are intentionally not matched here and remain
/// evidence for a later transform.
fn frontmatter_id(markdown: &str) -> Option<FrontmatterId<'_>> {
    let mut offset = 0usize;
    let mut line_number = 1u32;
    let mut in_frontmatter = false;
    let mut candidate: Option<FrontmatterId<'_>> = None;
    for line in markdown.split_inclusive('\n') {
        let line_no_nl = line.trim_end_matches(['\r', '\n']);
        if !in_frontmatter {
            // The fence must be the very first line of the document.
            if line_number != 1 || line_no_nl != "---" {
                return None;
            }
            in_frontmatter = true;
        } else if line_no_nl == "---" {
            // Closing fence — the block is a complete fenced region, so a
            // candidate found inside it is now promotable (None if none seen).
            return candidate;
        } else if candidate.is_none() {
            candidate = frontmatter_id_on_line(line_no_nl, offset, line_number);
        }
        offset += line.len();
        line_number = line_number.saturating_add(1);
    }
    // Reached EOF without a closing fence: not a valid fenced block → promote
    // nothing, even if an `id` line was seen (Wendy data-shape review #26).
    None
}

fn frontmatter_id_on_line(line_no_nl: &str, offset: usize, line: u32) -> Option<FrontmatterId<'_>> {
    let colon = line_no_nl.find(':')?;
    if &line_no_nl[..colon] != "id" {
        return None;
    }
    let after_colon = &line_no_nl[colon + 1..];
    let lead_ws = after_colon.len() - after_colon.trim_start().len();
    let trimmed = after_colon.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut value = trimmed;
    let mut value_start_in_line = colon + 1 + lead_ws;
    let bytes = trimmed.as_bytes();
    if trimmed.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[trimmed.len() - 1] == bytes[0]
    {
        value = &trimmed[1..trimmed.len() - 1];
        value_start_in_line += 1;
        if value.is_empty() {
            return None;
        }
    }
    let start = offset + value_start_in_line;
    let end = start + value.len();
    Some(FrontmatterId {
        value,
        start: u64::try_from(start).unwrap_or(u64::MAX),
        end: u64::try_from(end).unwrap_or(u64::MAX),
        line,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reference<'a> {
    target: &'a str,
    kind: SyntaxKind,
    start: u64,
    end: u64,
    line: u32,
}

/// Scan markdown for Obsidian-style references: `[[target]]` (wikilink) and
/// `![[target]]` (embed). Strict v0 (mirrors the other scanners): a reference
/// must open with `[[` and close with `]]` on the same line; the captured
/// `target` is the trimmed inner text and the span covers exactly that text
/// (so span == target). Alias/heading splitting (`[[note#h|alias]]`) is
/// intentionally deferred — the raw inner target is captured as evidence and
/// the resolver parses it later. An empty `[[]]` is skipped.
fn scan_references(markdown: &str) -> Vec<Reference<'_>> {
    let bytes = markdown.as_bytes();
    let mut refs = Vec::new();
    let mut line = 1u32;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            line = line.saturating_add(1);
            i += 1;
            continue;
        }
        if bytes[i] == b'[' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let inner_start = i + 2;
            let mut j = inner_start;
            let mut close = None;
            while j + 1 < bytes.len() {
                if bytes[j] == b'\n' {
                    break; // a wikilink does not span lines
                }
                if bytes[j] == b']' && bytes[j + 1] == b']' {
                    close = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(close) = close {
                let inner = &markdown[inner_start..close];
                let trimmed = inner.trim();
                if !trimmed.is_empty() {
                    // `![[` is an embed; a bare `[[` is a wikilink.
                    let kind = if i > 0 && bytes[i - 1] == b'!' {
                        SyntaxKind::Embed
                    } else {
                        SyntaxKind::Wikilink
                    };
                    let lead = inner.len() - inner.trim_start().len();
                    let start = inner_start + lead;
                    let end = start + trimmed.len();
                    refs.push(Reference {
                        target: trimmed,
                        kind,
                        start: u64::try_from(start).unwrap_or(u64::MAX),
                        end: u64::try_from(end).unwrap_or(u64::MAX),
                        line,
                    });
                }
                i = close + 2; // resume after `]]` (no newline inside)
                continue;
            }
        }
        i += 1;
    }
    refs
}

fn content_hash_from_hex(hex: &ContentHashHex) -> Result<ContentHash, ProjectorError> {
    let mut digest = [0u8; 32];
    for (i, chunk) in hex.as_str().as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(chunk)
            .map_err(|e| ProjectorError::InvalidSourceHash(e.to_string()))?;
        digest[i] = u8::from_str_radix(pair, 16)
            .map_err(|e| ProjectorError::InvalidSourceHash(e.to_string()))?;
    }
    Ok(ContentHash::from_digest(digest))
}

fn hash_hex(hash: &ContentHash) -> ContentHashHex {
    ContentHashHex::new(hash.to_hex()).expect("core content hash hex is always valid")
}

#[cfg(test)]
mod tests {
    use super::{
        parse_issue_wiki_note, project_issue_wiki, project_issue_wiki_candidate,
        project_issue_wiki_full_text, project_markdown_frontmatter_id,
        project_markdown_frontmatter_id_into_store, project_markdown_references,
        project_markdown_references_into_store, project_markdown_title,
        project_markdown_title_into_store, MarkdownSource, ProjectorError, EDGE_DERIVED_FROM,
        ISSUE_WIKI_FULL_TEXT_PROJECTOR, ISSUE_WIKI_PROJECTOR, MARKDOWN_FRONTMATTER_ID_PROJECTOR,
        MARKDOWN_REFERENCES_PROJECTOR, MARKDOWN_TITLE_PROJECTOR,
    };
    use attractor_core::ContentHash;
    use attractor_proto::{
        ApiRequest, ApiResponse, ContentHashHex, GetData, IssueWikiNoteEvidence,
        IssueWikiProjectionResult, IssueWikiProjectionState, IssueWikiSource, RequestKind,
        ISSUE_WIKI_FULL_TEXT_SCHEMA_ID_V0, ISSUE_WIKI_SCHEMA_ID_V0,
    };
    use attractor_store::Store;

    fn source(markdown: &[u8]) -> MarkdownSource {
        MarkdownSource {
            source_hash: ContentHashHex::new(ContentHash::of(markdown).to_hex()).unwrap(),
            origin: "repo:milton".to_owned(),
            label: "tasks/wendy.md".to_owned(),
        }
    }

    fn source_with_hash(hash_source: &[u8]) -> MarkdownSource {
        MarkdownSource {
            source_hash: ContentHashHex::new(ContentHash::of(hash_source).to_hex()).unwrap(),
            origin: "repo:milton".to_owned(),
            label: "tasks/wendy.md".to_owned(),
        }
    }

    fn issue_source(source_bytes: &[u8]) -> IssueWikiSource {
        IssueWikiSource {
            repo: "gwild/attractor".to_owned(),
            issue_number: 43,
            source_ref: "github:gwild/attractor#43".to_owned(),
            source_hash: ContentHashHex::new(ContentHash::of(source_bytes).to_hex()).unwrap(),
        }
    }

    fn issue_fixture() -> &'static [u8] {
        include_bytes!("../../../fixtures/issue_wiki_conformance/source.issue")
    }

    fn malformed_reason(row: IssueWikiProjectionResult) -> String {
        match row {
            IssueWikiProjectionResult::Malformed(row) => row.reason,
            other => panic!("expected malformed issue_wiki row, got {other:?}"),
        }
    }

    fn apply_issue_wiki_negative_mutation(note: &mut IssueWikiNoteEvidence, mutation: &str) {
        match mutation {
            "identity_span_from_title" => {
                note.identity.span = note.title.span.clone();
            }
            "title_span_from_identity" => {
                note.title.span = note.identity.span.clone();
            }
            "reference0_span_from_reference1" => {
                note.references[0].span = note.references[1].span.clone();
            }
            "identity_value_different" => {
                note.identity.value = "Different".to_owned();
            }
            "title_value_different" => {
                note.title.value = "Different".to_owned();
            }
            "reference0_value_different" => {
                note.references[0].target = "wiki:Different".to_owned();
            }
            _ => panic!("unknown issue_wiki negative mutation {mutation}"),
        }
    }

    #[test]
    fn issue_wiki_extractor_emits_conformance_receipts_without_raw_body() {
        let source_bytes = issue_fixture();
        let row = project_issue_wiki(issue_source(source_bytes), source_bytes);
        let accepted = match row {
            IssueWikiProjectionResult::Accepted(row) => row,
            other => panic!("expected accepted issue_wiki row, got {other:?}"),
        };

        assert_eq!(
            accepted.envelope.projection_state,
            IssueWikiProjectionState::Accepted
        );
        assert_eq!(accepted.envelope.transform_id, ISSUE_WIKI_PROJECTOR);
        assert_eq!(accepted.note.identity.value, "Same");
        assert_eq!(accepted.note.title.value, "Same");
        assert_eq!(accepted.note.references.len(), 2);
        assert_eq!(accepted.conformance_receipts.len(), 4);
        assert_eq!(accepted.conformance_receipts[0].field_path, "identity");
        assert_eq!(accepted.conformance_receipts[1].field_path, "title");
        assert_eq!(
            accepted.conformance_receipts[2].field_path,
            "references[0].target"
        );
        assert_eq!(
            accepted.conformance_receipts[3].field_path,
            "references[1].target"
        );
        for receipt in &accepted.conformance_receipts {
            assert_eq!(receipt.source_hash, accepted.source.source_hash);
            assert_eq!(receipt.span.source_hash, accepted.source.source_hash);
            let start = usize::try_from(receipt.span.start).unwrap();
            let end = usize::try_from(receipt.span.end).unwrap();
            assert_eq!(
                receipt.value_hash.as_str(),
                ContentHash::of(&source_bytes[start..end]).to_hex()
            );
        }
        assert_eq!(
            accepted.conformance_receipts[0].value_hash.as_str(),
            ContentHash::of(b"Same").to_hex()
        );
        assert_eq!(
            accepted.conformance_receipts[2].value_hash.as_str(),
            ContentHash::of(b"wiki:Same").to_hex()
        );

        let json = serde_json::to_string(&accepted).unwrap();
        assert!(!json.contains("RAW ISSUE BODY"));
        assert!(!json.contains("raw_issue_body"));
    }

    #[test]
    fn issue_wiki_full_text_projector_has_distinct_payload_and_identity() {
        let source_bytes = b"identity: attractor.issue.3057.phase-two\n\
title: Phase 2 full text migration\n\
body: Full issue body text that must not collapse to the summary title.\n\
reference: wiki:Attractor/Phase-2-Full-Text\n";
        let source = issue_source(source_bytes);

        let summary = project_issue_wiki(source.clone(), source_bytes);
        let full_text = project_issue_wiki_full_text(source, source_bytes);
        let (summary, full_text) = match (summary, full_text) {
            (
                IssueWikiProjectionResult::Accepted(summary),
                IssueWikiProjectionResult::Accepted(full_text),
            ) => (summary, full_text),
            other => panic!("expected accepted summary/full-text projections, got {other:?}"),
        };

        assert_eq!(summary.schema_id, ISSUE_WIKI_SCHEMA_ID_V0);
        assert_eq!(summary.envelope.transform_id, ISSUE_WIKI_PROJECTOR);
        assert!(summary.note.full_text.is_none());
        assert_eq!(full_text.schema_id, ISSUE_WIKI_FULL_TEXT_SCHEMA_ID_V0);
        assert_eq!(
            full_text.envelope.transform_id,
            ISSUE_WIKI_FULL_TEXT_PROJECTOR
        );
        let body = full_text
            .note
            .full_text
            .as_ref()
            .expect("full-text projector must carry body evidence");
        assert!(body.value.contains("Full issue body text"));
        assert_ne!(
            serde_json::to_vec(&summary.note).unwrap(),
            serde_json::to_vec(&full_text.note).unwrap()
        );
        assert!(full_text
            .conformance_receipts
            .iter()
            .any(|receipt| receipt.field_path == "full_text"));
    }

    #[test]
    fn issue_wiki_negative_conformance_fixtures_are_malformed_before_acceptance() {
        let source_bytes = issue_fixture();
        let source = issue_source(source_bytes);
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../fixtures/issue_wiki_conformance/negative_cases.json"
        ))
        .unwrap();

        for case in cases {
            let id = case["id"].as_str().unwrap();
            let mutation = case["mutation"].as_str().unwrap();
            let reason = case["reason"].as_str().unwrap();
            let mut note = parse_issue_wiki_note(&source, source_bytes, false).unwrap();
            apply_issue_wiki_negative_mutation(&mut note, mutation);

            assert_eq!(
                malformed_reason(project_issue_wiki_candidate(
                    source.clone(),
                    source_bytes,
                    note
                )),
                reason,
                "{id}"
            );
        }
    }

    #[test]
    fn issue_wiki_source_hash_change_is_stale_not_malformed() {
        let source_bytes = issue_fixture();
        let mut source = issue_source(source_bytes);
        source.source_hash = ContentHashHex::new(ContentHash::of(b"old issue").to_hex()).unwrap();

        let row = project_issue_wiki(source, source_bytes);
        match row {
            IssueWikiProjectionResult::Stale(row) => {
                assert_eq!(
                    row.envelope.projection_state,
                    IssueWikiProjectionState::Stale
                );
                assert_eq!(
                    row.actual_source_hash.as_str(),
                    ContentHash::of(source_bytes).to_hex()
                );
            }
            other => panic!("expected stale issue_wiki row, got {other:?}"),
        }
    }

    #[test]
    fn derived_receipt_has_source_span() {
        let markdown = b"front matter\n# WS-3 projector\nbody\n";
        let projection = project_markdown_title(&source(markdown), markdown).unwrap();
        assert_eq!(projection.nodes.len(), 2);
        assert_eq!(projection.links.len(), 1);
        assert_eq!(projection.links[0].kind, EDGE_DERIVED_FROM);
        assert_eq!(projection.receipt.transform_id, MARKDOWN_TITLE_PROJECTOR);
        assert_eq!(projection.receipt.source_spans.len(), 1);
        assert_eq!(projection.receipt.source_spans[0].start, 15);
        assert_eq!(projection.receipt.source_spans[0].end, 29);
        assert_eq!(
            projection.receipt.source_spans[0].label.as_deref(),
            Some("repo:milton/tasks/wendy.md#WS-3 projector")
        );
        assert_eq!(projection.receipt.provenance_refs[0].origin, "repo:milton");
    }

    #[test]
    fn projection_runs_before_store_write() {
        let markdown = b"# Gate\nraw scratch that must not be stored\n";
        let mut store = Store::new();
        assert!(store.is_empty());
        let receipt =
            project_markdown_title_into_store(&mut store, &source(markdown), markdown).unwrap();
        assert_eq!(receipt.source_spans.len(), 1);
        assert_eq!(store.len(), 2);
        assert_eq!(store.provenance(&ContentHash::of(markdown)).len(), 0);
    }

    #[test]
    fn rejects_source_hash_mismatch() {
        let markdown = b"# Current title\n";
        let stale_markdown = b"# Stale title\n";
        let err = project_markdown_title(&source_with_hash(stale_markdown), markdown).unwrap_err();
        match err {
            ProjectorError::SourceHashMismatch { declared, actual } => {
                assert_eq!(declared.as_str(), ContentHash::of(stale_markdown).to_hex());
                assert_eq!(actual.as_str(), ContentHash::of(markdown).to_hex());
            }
            other => panic!("expected SourceHashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn source_hash_mismatch_does_not_write_store() {
        let markdown = b"# Current title\n";
        let stale_markdown = b"# Stale title\n";
        let mut store = Store::new();
        assert!(matches!(
            project_markdown_title_into_store(
                &mut store,
                &source_with_hash(stale_markdown),
                markdown,
            ),
            Err(ProjectorError::SourceHashMismatch { .. })
        ));
        assert!(store.is_empty());
    }

    #[test]
    fn get_returns_projector_receipt_source_spans_after_markdown_insert() {
        let markdown = b"notes\n# WS-3 Acceptance\nbody\n";
        let mut store = Store::new();
        let projector_receipt =
            project_markdown_title_into_store(&mut store, &source(markdown), markdown).unwrap();

        let get_req = ApiRequest::new(RequestKind::Get {
            address: projector_receipt.graph_address,
        });
        match attractor_api::dispatch_with_store(&get_req, 0, &mut store) {
            ApiResponse::Ok { data, .. } => {
                let get: GetData = serde_json::from_value(data).unwrap();
                assert_eq!(get.receipt.transform_id, MARKDOWN_TITLE_PROJECTOR);
                assert_eq!(get.receipt.source_spans.len(), 1);
                assert_eq!(get.receipt.source_spans[0].start, 8);
                assert_eq!(get.receipt.source_spans[0].end, 23);
                assert_eq!(
                    get.receipt.source_spans[0].label.as_deref(),
                    Some("repo:milton/tasks/wendy.md#WS-3 Acceptance")
                );
                assert_eq!(get.receipt.provenance_refs[0].origin, "repo:milton");
            }
            other @ ApiResponse::Error { .. } => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn projection_nodes_do_not_store_raw_markdown() {
        let markdown = b"# Gate\nraw scratch that must not be stored\n";
        let projection = project_markdown_title(&source(markdown), markdown).unwrap();
        let nodes_json = serde_json::to_string(&projection.nodes).unwrap();
        assert!(nodes_json.contains("Gate"));
        assert!(!nodes_json.contains("raw scratch that must not be stored"));
    }

    #[test]
    fn failed_projection_does_not_write_store() {
        let markdown = b"body only\n";
        let mut store = Store::new();
        assert!(
            project_markdown_title_into_store(&mut store, &source(markdown), markdown).is_err()
        );
        assert!(store.is_empty());
    }

    #[test]
    fn fails_loud_without_heading() {
        let markdown = b"body only\n";
        assert!(project_markdown_title(&source(markdown), markdown).is_err());
    }

    #[test]
    fn ignores_non_heading_hash_text() {
        let markdown = b"not # a title\n  ## Real Title\n";
        let projection = project_markdown_title(&source(markdown), markdown).unwrap();
        assert_eq!(projection.receipt.source_spans[0].start, 19);
        assert_eq!(projection.receipt.source_spans[0].end, 29);
    }

    #[test]
    fn trims_heading_marker_spacing_without_shifting_span() {
        let markdown = b"  ###   Title With Space   \n";
        let projection = project_markdown_title(&source(markdown), markdown).unwrap();
        assert_eq!(projection.receipt.source_spans[0].start, 8);
        assert_eq!(projection.receipt.source_spans[0].end, 24);
    }

    #[test]
    fn frontmatter_id_projects_identity_with_span() {
        let markdown = b"---\nid: note-abc\n---\n# Title\n";
        let projection = project_markdown_frontmatter_id(&source(markdown), markdown).unwrap();
        assert_eq!(projection.nodes.len(), 2);
        assert_eq!(projection.links.len(), 1);
        assert_eq!(projection.links[0].kind, EDGE_DERIVED_FROM);
        assert_eq!(
            projection.receipt.transform_id,
            MARKDOWN_FRONTMATTER_ID_PROJECTOR
        );
        assert_eq!(projection.receipt.source_spans.len(), 1);
        // Span covers the `id` value `note-abc` (bytes 8..16), not the key.
        assert_eq!(projection.receipt.source_spans[0].start, 8);
        assert_eq!(projection.receipt.source_spans[0].end, 16);
        assert_eq!(projection.receipt.provenance_refs[0].origin, "repo:milton");
        let nodes_json = serde_json::to_string(&projection.nodes).unwrap();
        assert!(nodes_json.contains("note-abc"));
    }

    #[test]
    fn frontmatter_id_strips_quotes_and_shifts_span_inward() {
        let markdown = b"---\nid: \"quoted-id\"\n---\nbody\n";
        let projection = project_markdown_frontmatter_id(&source(markdown), markdown).unwrap();
        // Quote-stripping moves start inward to the value, span == value text.
        assert_eq!(projection.receipt.source_spans[0].start, 9);
        assert_eq!(projection.receipt.source_spans[0].end, 18);
        let nodes_json = serde_json::to_string(&projection.nodes).unwrap();
        assert!(nodes_json.contains("quoted-id"));
        assert!(!nodes_json.contains("\\\"quoted-id\\\""));
    }

    #[test]
    fn get_returns_frontmatter_id_receipt_after_insert() {
        let markdown = b"---\nid: ws3-identity\n---\n# Body\n";
        let mut store = Store::new();
        let projector_receipt =
            project_markdown_frontmatter_id_into_store(&mut store, &source(markdown), markdown)
                .unwrap();
        assert_eq!(store.len(), 2);
        let get_req = ApiRequest::new(RequestKind::Get {
            address: projector_receipt.graph_address,
        });
        match attractor_api::dispatch_with_store(&get_req, 0, &mut store) {
            ApiResponse::Ok { data, .. } => {
                let get: GetData = serde_json::from_value(data).unwrap();
                assert_eq!(get.receipt.transform_id, MARKDOWN_FRONTMATTER_ID_PROJECTOR);
                assert_eq!(get.receipt.source_spans.len(), 1);
                assert_eq!(get.receipt.provenance_refs[0].origin, "repo:milton");
            }
            other @ ApiResponse::Error { .. } => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn frontmatter_id_promotes_only_top_level_key() {
        // Indented `id` under another key must NOT be promoted; the unindented
        // top-level `id` is the one identity value.
        let markdown = b"---\nmeta:\n  id: nested\nid: real-one\n---\n";
        let projection = project_markdown_frontmatter_id(&source(markdown), markdown).unwrap();
        let nodes_json = serde_json::to_string(&projection.nodes).unwrap();
        assert!(nodes_json.contains("real-one"));
        assert!(!nodes_json.contains("nested"));
    }

    #[test]
    fn frontmatter_id_rejects_source_hash_mismatch() {
        let markdown = b"---\nid: current\n---\n";
        let stale_markdown = b"---\nid: stale\n---\n";
        let err = project_markdown_frontmatter_id(&source_with_hash(stale_markdown), markdown)
            .unwrap_err();
        match err {
            ProjectorError::SourceHashMismatch { declared, actual } => {
                assert_eq!(declared.as_str(), ContentHash::of(stale_markdown).to_hex());
                assert_eq!(actual.as_str(), ContentHash::of(markdown).to_hex());
            }
            other => panic!("expected SourceHashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn frontmatter_id_rejects_unterminated_block() {
        // Wendy #26 data-shape review: an `id` line with no closing `---` fence
        // is not a valid fenced block and must promote nothing, even though the
        // `id` text is present.
        let markdown = b"---\nid: forged\nbody with no closing fence\n";
        assert!(project_markdown_frontmatter_id(&source(markdown), markdown).is_err());
    }

    #[test]
    fn frontmatter_id_fails_loud_without_frontmatter() {
        let markdown = b"# Title only\nbody\n";
        assert!(project_markdown_frontmatter_id(&source(markdown), markdown).is_err());
    }

    #[test]
    fn frontmatter_id_fails_loud_when_block_has_no_id() {
        let markdown = b"---\ntitle: x\naliases: [a]\n---\n";
        assert!(project_markdown_frontmatter_id(&source(markdown), markdown).is_err());
    }

    #[test]
    fn frontmatter_id_requires_fence_on_first_line() {
        // A non-leading frontmatter block is not trusted as document identity.
        let markdown = b"\n---\nid: x\n---\n";
        assert!(project_markdown_frontmatter_id(&source(markdown), markdown).is_err());
    }

    #[test]
    fn failed_frontmatter_projection_does_not_write_store() {
        let markdown = b"no frontmatter here\n";
        let mut store = Store::new();
        assert!(project_markdown_frontmatter_id_into_store(
            &mut store,
            &source(markdown),
            markdown
        )
        .is_err());
        assert!(store.is_empty());
    }

    #[test]
    fn references_project_one_node_per_target_with_spans() {
        let markdown = b"# Note\nSee [[Alpha]] and [[Beta]].\n";
        let projs = project_markdown_references(&source(markdown), markdown).unwrap();
        assert_eq!(projs.len(), 2);
        // First-seen order: Alpha then Beta.
        assert_eq!(projs[0].receipt.transform_id, MARKDOWN_REFERENCES_PROJECTOR);
        assert_eq!(projs[0].receipt.source_spans.len(), 1);
        // Span covers the target text `Alpha` (bytes 13..18), not the brackets.
        assert_eq!(projs[0].receipt.source_spans[0].start, 13);
        assert_eq!(projs[0].receipt.source_spans[0].end, 18);
        assert_eq!(projs[1].receipt.source_spans[0].start, 27);
        assert_eq!(projs[1].receipt.source_spans[0].end, 31);
        let j0 = serde_json::to_string(&projs[0].nodes).unwrap();
        let j1 = serde_json::to_string(&projs[1].nodes).unwrap();
        assert!(j0.contains("Alpha"));
        assert!(j1.contains("Beta"));
        // Each reference links derived_from its source ref, evidence-only.
        assert_eq!(projs[0].links[0].kind, EDGE_DERIVED_FROM);
    }

    #[test]
    fn repeated_target_collapses_to_one_node_with_multiple_spans() {
        let markdown = b"[[Foo]] then [[Foo]] again\n";
        let projs = project_markdown_references(&source(markdown), markdown).unwrap();
        // One content-addressed node for `Foo`, two occurrence spans on it.
        assert_eq!(projs.len(), 1);
        assert_eq!(projs[0].receipt.source_spans.len(), 2);
    }

    #[test]
    fn references_scan_embed_and_wikilink_forms() {
        let markdown = b"![[Embedded]] and [[Linked]]\n";
        let projs = project_markdown_references(&source(markdown), markdown).unwrap();
        assert_eq!(projs.len(), 2);
        let all =
            serde_json::to_string(&projs.iter().map(|p| &p.nodes).collect::<Vec<_>>()).unwrap();
        assert!(all.contains("Embedded")); // ![[...]] embed form scanned
        assert!(all.contains("Linked")); // [[...]] wikilink form scanned
    }

    #[test]
    fn no_references_returns_empty_not_error() {
        // References are optional evidence — absence is Ok([]), NOT an error
        // (unlike title/frontmatter-id which fail loud on a missing promotion).
        let markdown = b"# Just a heading\nplain prose, no links\n";
        let projs = project_markdown_references(&source(markdown), markdown).unwrap();
        assert!(projs.is_empty());
    }

    #[test]
    fn references_skip_empty_and_unterminated() {
        let markdown = b"[[]] and [[ unclosed to EOL\n";
        let projs = project_markdown_references(&source(markdown), markdown).unwrap();
        assert!(projs.is_empty());
    }

    #[test]
    fn references_reject_source_hash_mismatch() {
        let markdown = b"see [[Current]]\n";
        let stale = b"see [[Stale]]\n";
        assert!(matches!(
            project_markdown_references(&source_with_hash(stale), markdown),
            Err(ProjectorError::SourceHashMismatch { .. })
        ));
    }

    #[test]
    fn references_into_store_writes_all_targets() {
        let markdown = b"[[A]] and [[B]]\n";
        let mut store = Store::new();
        let receipts =
            project_markdown_references_into_store(&mut store, &source(markdown), markdown)
                .unwrap();
        assert_eq!(receipts.len(), 2);
        assert!(!store.is_empty());
    }
}
