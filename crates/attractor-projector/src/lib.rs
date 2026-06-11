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
use attractor_proto::{ContentHashHex, E2eReceipt, ProvenanceRef};
use attractor_store::{Node, Store, StoreError};
use std::fmt;

/// Stable transform identifier for the WS-3 markdown title projector.
pub const MARKDOWN_TITLE_PROJECTOR: &str = "attractor.projector.markdown.title.v0";
/// Edge kind from a derived markdown value back to its source document ref.
pub const EDGE_DERIVED_FROM: &str = "derived_from";

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
    /// No promotable markdown title was found.
    MissingTitle,
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
            Self::MissingTitle => f.write_str("markdown source has no promotable title"),
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
            Self::MissingTitle | Self::InvalidSourceHash(_) => None,
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
/// Returns [`ProjectorError::InvalidUtf8`] for non-UTF-8 markdown,
/// [`ProjectorError::MissingTitle`] when no markdown heading exists, and
/// address/serialization errors if derived values cannot be addressed.
pub fn project_markdown_title(
    source: &MarkdownSource,
    markdown: &[u8],
) -> Result<Projection, ProjectorError> {
    let text = std::str::from_utf8(markdown).map_err(ProjectorError::InvalidUtf8)?;
    let heading = first_heading(text).ok_or(ProjectorError::MissingTitle)?;
    let core_source_hash = content_hash_from_hex(&source.source_hash)?;
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
    store.attach_receipt(graph_addr, projection.receipt.clone());
    Ok(projection.receipt)
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
        project_markdown_title, project_markdown_title_into_store, MarkdownSource,
        EDGE_DERIVED_FROM, MARKDOWN_TITLE_PROJECTOR,
    };
    use attractor_core::ContentHash;
    use attractor_proto::{ApiRequest, ApiResponse, ContentHashHex, GetData, RequestKind};
    use attractor_store::Store;

    fn source(markdown: &[u8]) -> MarkdownSource {
        MarkdownSource {
            source_hash: ContentHashHex::new(ContentHash::of(markdown).to_hex()).unwrap(),
            origin: "repo:milton".to_owned(),
            label: "tasks/wendy.md".to_owned(),
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
        let receipt = project_markdown_title_into_store(&mut store, &source(markdown), markdown)
            .unwrap();
        assert_eq!(receipt.source_spans.len(), 1);
        assert_eq!(store.len(), 2);
        assert_eq!(store.provenance(&ContentHash::of(markdown)).len(), 0);
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
            other => panic!("expected Ok, got {other:?}"),
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
        assert!(project_markdown_title_into_store(&mut store, &source(markdown), markdown).is_err());
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
}
