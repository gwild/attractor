//! Markdown source-mapping contract (#2343).
//!
//! Markdown ingest is a **source-map problem, not only a parser/trust problem**
//! (#2343, Wendy via the Studio Obsidian guide). A note is internally
//! addressable at several grains — file, heading, block id, wikilink/embed
//! target, task line, table row, code fence/callout — so every value Attractor
//! derives from markdown should carry a reversible thread back to the exact
//! span that produced it: vault, git commit, path, frontmatter id, heading
//! path, optional block id, byte/line range, and syntax kind. That thread is
//! what gives the graph incremental invalidation and audit ("show me where this
//! came from") instead of a correct-looking graph you cannot inspect.
//!
//! ## What this module is (and is not)
//!
//! This is the **typed contract only** — the input surface a markdown projector
//! fills, and the lowering from that rich locator down to the frozen wire
//! [`SourceSpan`](attractor_proto::SourceSpan) carried by an
//! [`E2eReceipt`](attractor_proto::E2eReceipt). It deliberately contains **no
//! markdown parser**: extracting [`TypedEvidence`] from real vault text is the
//! WS-3 projector's job, and persistence of spans is the store/receipt layer's.
//! Keeping the contract here — in the dependency-light, 1.92-floor
//! `attractor-core` — lets it be attested identically on every fleet platform
//! alongside the rest of core.
//!
//! ## Why a separate type from `proto::SourceSpan`
//!
//! The WS-1 receipt wire contract ([`SourceSpan`](attractor_proto::SourceSpan) /
//! [`E2eReceipt`](attractor_proto::E2eReceipt)) was frozen in #14 and is the
//! generic byte-range shape every transform lowers into. The markdown grains are
//! richer than that wire shape, so they live in [`MarkdownSourceSpan`] and are
//! *lowered* via [`MarkdownSourceSpan::to_source_span`] — the frozen struct is
//! never widened. The composed [`SourceSpan::label`](attractor_proto::SourceSpan)
//! keeps the wire receipt human-inspectable without leaking the markdown-specific
//! fields into the universal contract.

use crate::{ContentHash, Primitive};
use attractor_proto::SourceSpan;
use serde::{Deserialize, Serialize};

/// The markdown syntactic grain a [`MarkdownSourceSpan`] points at.
///
/// `#[non_exhaustive]`: the addressable grains can grow (the Studio guide lists
/// more than v0 needs) without breaking the contract for downstream crates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SyntaxKind {
    /// A heading line (`#`..`######`).
    Heading,
    /// A paragraph / prose block.
    Paragraph,
    /// A single list item (ordered or unordered).
    ListItem,
    /// A task/checkbox line (`- [ ]` / `- [x]`) — the unit of task invalidation.
    TaskBlock,
    /// An Obsidian block reference target (`^block-id`).
    BlockId,
    /// A wikilink target (`[[note#heading]]`).
    Wikilink,
    /// An embed/transclusion target (`![[note]]`).
    Embed,
    /// A single row of a markdown table.
    TableRow,
    /// A fenced code block.
    CodeFence,
    /// A callout/admonition block (`> [!note]`).
    Callout,
    /// A YAML frontmatter region.
    Frontmatter,
}

/// A reversible locator from a derived value back to the exact markdown grain
/// that produced it (#2343).
///
/// This is the rich, markdown-specific source span. It lowers to the frozen
/// portable wire shape with [`Self::to_source_span`]; the wire receipt only ever
/// carries the lowered [`SourceSpan`](attractor_proto::SourceSpan), so adding
/// grains here does not touch the WS-1 contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkdownSourceSpan {
    /// Repo/vault identifier the note lives in.
    pub vault: String,
    /// Git commit the span was read at; `None` for a dirty/unversioned read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Note path within the vault (e.g. `Projects/Attractor.md`).
    pub path: String,
    /// Frontmatter id, if the note declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontmatter_id: Option<String>,
    /// Heading path to the span, outermost first (e.g. `["Project", "Status"]`).
    /// Empty when the span is above the first heading.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heading_path: Vec<String>,
    /// Obsidian block id (`^id`) attached to the span, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    /// Byte range `[start, end)` of the span within the source note payload.
    pub byte_range: ByteRange,
    /// 1-based inclusive line range `[start, end]` of the span.
    pub line_range: LineRange,
    /// Which markdown grain this span points at.
    pub syntax_kind: SyntaxKind,
    /// Content address of the source note payload — the anchor every lowered
    /// [`SourceSpan`](attractor_proto::SourceSpan) is keyed on.
    pub source_hash: ContentHash,
}

/// Byte range `[start, end)` into a source payload.
///
/// A named struct (not a bare tuple) so the serde shape is self-describing on
/// the wire and a zero-width range (`start == end`, an insertion point) is
/// unambiguous.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    /// First byte offset of the span.
    pub start: u64,
    /// One-past-the-last byte offset of the span.
    pub end: u64,
}

/// 1-based inclusive line range `[start, end]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    /// First line (1-based, inclusive).
    pub start: u32,
    /// Last line (1-based, inclusive).
    pub end: u32,
}

impl MarkdownSourceSpan {
    /// Lower this rich markdown locator to the frozen portable wire span.
    ///
    /// The wire [`SourceSpan`](attractor_proto::SourceSpan) carries only the
    /// universal fields (`source_hash`, `start`, `end`, `label`); the
    /// markdown-specific grains are folded into a stable, human-inspectable
    /// `label` of the form `vault/path#heading > path^block-id` (heading and
    /// block-id segments are appended only when present). This keeps a receipt
    /// readable — "show me where this came from" — without widening the WS-1
    /// contract.
    ///
    /// # Panics
    /// Never in practice: [`ContentHash::to_hex`] always yields the 64
    /// lowercase-hex characters that
    /// [`ContentHashHex::new`](attractor_proto::ContentHashHex::new) validates,
    /// so the conversion cannot fail. The `expect` documents that invariant
    /// rather than guarding a reachable error (same pattern as `attractor-api`).
    #[must_use]
    pub fn to_source_span(&self) -> SourceSpan {
        SourceSpan {
            // `ContentHash::to_hex` always yields 64 lowercase hex chars, which
            // is exactly what `ContentHashHex::new` validates — same invariant
            // relied on in `attractor-api`'s `hash_hex`.
            source_hash: attractor_proto::ContentHashHex::new(self.source_hash.to_hex())
                .expect("core content hash hex is always valid"),
            start: self.byte_range.start,
            end: self.byte_range.end,
            label: Some(self.compose_label()),
        }
    }

    /// Build the stable, human-readable label folded into the lowered span.
    ///
    /// Shape: `vault/path` then `#h1 > h2 > …` for the heading path (if any)
    /// then `^block-id` (if present). Deterministic for equal grains.
    #[must_use]
    fn compose_label(&self) -> String {
        let mut label = format!("{}/{}", self.vault, self.path);
        if !self.heading_path.is_empty() {
            label.push('#');
            label.push_str(&self.heading_path.join(" > "));
        }
        if let Some(block_id) = &self.block_id {
            label.push('^');
            label.push_str(block_id);
        }
        label
    }
}

/// One extracted fact plus the markdown span it came from (#2343).
///
/// This is the unit a markdown projector (WS-3) produces and the store/receipt
/// layer lowers — via [`MarkdownSourceSpan::to_source_span`] — into the
/// `source_spans` of an [`E2eReceipt`](attractor_proto::E2eReceipt). The `value`
/// reuses core's [`Primitive`] so the fact is content-addressable on the same
/// value-only identity rules as every other cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TypedEvidence {
    /// The extracted value (the addressable fact).
    pub value: Primitive,
    /// Where in the markdown source the value came from.
    pub span: MarkdownSourceSpan,
}

#[cfg(test)]
mod tests {
    use super::{ByteRange, LineRange, MarkdownSourceSpan, SyntaxKind, TypedEvidence};
    use crate::{ContentHash, Primitive};

    fn span() -> MarkdownSourceSpan {
        MarkdownSourceSpan {
            vault: "studio".to_owned(),
            commit: Some("abc123".to_owned()),
            path: "Projects/Attractor.md".to_owned(),
            frontmatter_id: Some("attractor-001".to_owned()),
            heading_path: vec!["Project".to_owned(), "Status".to_owned()],
            block_id: Some("blk7".to_owned()),
            byte_range: ByteRange { start: 10, end: 42 },
            line_range: LineRange { start: 3, end: 4 },
            syntax_kind: SyntaxKind::TaskBlock,
            source_hash: ContentHash::of(b"# note body"),
        }
    }

    #[test]
    fn lowers_byte_range_and_hash() {
        let s = span();
        let wire = s.to_source_span();
        assert_eq!(wire.start, 10);
        assert_eq!(wire.end, 42);
        // Hash lowers losslessly to the wire hex and round-trips back to bytes.
        assert_eq!(wire.source_hash.as_str(), s.source_hash.to_hex());
    }

    #[test]
    fn label_carries_grains_and_is_stable() {
        let wire = span().to_source_span();
        let label = wire.label.expect("lowering always sets a label");
        assert_eq!(label, "studio/Projects/Attractor.md#Project > Status^blk7");
        // Determinism: equal grains → equal label.
        assert_eq!(span().to_source_span().label.unwrap(), label);
    }

    #[test]
    fn label_omits_absent_optional_grains() {
        let mut s = span();
        s.heading_path.clear();
        s.block_id = None;
        let label = s.to_source_span().label.unwrap();
        assert_eq!(label, "studio/Projects/Attractor.md");
    }

    #[test]
    fn zero_width_byte_range_is_allowed() {
        let mut s = span();
        s.byte_range = ByteRange { start: 7, end: 7 };
        let wire = s.to_source_span();
        assert_eq!(wire.start, wire.end);
    }

    #[test]
    fn span_serde_round_trips() {
        let s = span();
        let json = serde_json::to_string(&s).unwrap();
        let back: MarkdownSourceSpan = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn absent_optionals_are_omitted_from_json() {
        let mut s = span();
        s.commit = None;
        s.frontmatter_id = None;
        s.block_id = None;
        s.heading_path.clear();
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("commit"), "None commit must be omitted: {json}");
        assert!(!json.contains("frontmatter_id"));
        assert!(!json.contains("block_id"));
        assert!(!json.contains("heading_path"));
    }

    #[test]
    fn syntax_kind_serializes_snake_case() {
        let json = serde_json::to_string(&SyntaxKind::TaskBlock).unwrap();
        assert_eq!(json, "\"task_block\"");
    }

    #[test]
    fn typed_evidence_round_trips() {
        let ev = TypedEvidence {
            value: Primitive::Text("ship it".to_owned()),
            span: span(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: TypedEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }
}
