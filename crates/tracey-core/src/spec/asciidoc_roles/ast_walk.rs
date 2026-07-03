//! AST walk for the `AsciiDocRoles` backend: extract `[role="requirement", ...]`
//! open blocks, headings, and inline code spans.

use std::collections::HashSet;

use asciidork_ast::{
    AttrData, AttrValue, Block, BlockContent, BlockContext, DocContent, Document, Inline,
    InlineNodes, ReadAttr, Section,
};
use marq::{DocElement, Heading, InlineCodeSpan, Paragraph, ReqDefinition, ReqMetadata, SourceSpan};

use crate::spec::{SlugAllocator, byte_offset_to_line, req_anchor_id};

/// Result of walking the asciidork AST.
pub struct WalkResult {
    pub reqs: Vec<ReqDefinition>,
    pub headings: Vec<Heading>,
    pub elements: Vec<DocElement>,
    pub inline_code_spans: Vec<InlineCodeSpan>,
    pub weight: i32,
    /// Mapping from asciidork section IDs to our allocated slugs.
    /// Each entry is `(asciidork_id, our_slug)`, in document order.
    pub section_id_map: Vec<(String, String)>,
}

/// Walk the asciidork document AST to extract spec data.
pub fn walk<'arena>(
    doc: &Document<'arena>,
    source: &str,
    alloc: &mut SlugAllocator,
) -> eyre::Result<WalkResult> {
    let mut result = WalkResult {
        reqs: Vec::new(),
        headings: Vec::new(),
        elements: Vec::new(),
        inline_code_spans: Vec::new(),
        weight: 0,
        section_id_map: Vec::new(),
    };

    if let Some(AttrValue::String(w)) = doc.meta.get("weight") {
        result.weight = w.trim().parse::<i32>().unwrap_or(0);
    }

    let mut seen_bases = HashSet::new();

    match &doc.content {
        DocContent::Blocks(blocks) => {
            walk_blocks(blocks, source, alloc, &mut result, &mut seen_bases)?;
        }
        DocContent::Sections(sectioned) => {
            if let Some(preamble) = &sectioned.preamble {
                walk_blocks(preamble, source, alloc, &mut result, &mut seen_bases)?;
            }
            for section in &sectioned.sections {
                walk_section(section, source, alloc, &mut result, &mut seen_bases)?;
            }
        }
        DocContent::Parts(_) => {}
    }

    Ok(result)
}

fn walk_blocks<'arena>(
    blocks: &[Block<'arena>],
    source: &str,
    alloc: &mut SlugAllocator,
    result: &mut WalkResult,
    seen_bases: &mut HashSet<String>,
) -> eyre::Result<()> {
    for block in blocks {
        walk_block(block, source, alloc, result, seen_bases)?;
    }
    Ok(())
}

fn walk_section<'arena>(
    section: &Section<'arena>,
    source: &str,
    alloc: &mut SlugAllocator,
    result: &mut WalkResult,
    seen_bases: &mut HashSet<String>,
) -> eyre::Result<()> {
    let title = section.heading.plain_text().join("");
    let base_slug = marq::slugify(&title);
    let our_slug = alloc.alloc(&base_slug);

    if let Some(adoc_id) = &section.id {
        result
            .section_id_map
            .push((adoc_id.as_str().to_string(), our_slug.clone()));
    }

    let start_pos = section.loc.start_pos as usize;
    let line = byte_offset_to_line(source, start_pos);

    let heading = Heading {
        title: title.clone(),
        id: our_slug.clone(),
        level: section.level,
        line,
    };
    result.headings.push(heading.clone());
    result.elements.push(DocElement::Heading(heading));

    collect_spans_from_inlines(&section.heading, &mut result.inline_code_spans);
    walk_blocks(&section.blocks, source, alloc, result, seen_bases)?;
    Ok(())
}

fn walk_block<'arena>(
    block: &Block<'arena>,
    source: &str,
    alloc: &mut SlugAllocator,
    result: &mut WalkResult,
    seen_bases: &mut HashSet<String>,
) -> eyre::Result<()> {
    if block.context == BlockContext::Open && block.meta.attrs.has_role("requirement") {
        extract_req_block(block, source, result, seen_bases)?;
        return Ok(());
    }

    match &block.content {
        BlockContent::Simple(inlines) => {
            collect_spans_from_inlines(inlines, &mut result.inline_code_spans);
            let line = inlines
                .first()
                .map_or(0, |first| byte_offset_to_line(source, first.loc.start as usize));
            result
                .elements
                .push(DocElement::Paragraph(Paragraph { line, offset: 0 }));
        }
        BlockContent::Section(section) => {
            walk_section(section, source, alloc, result, seen_bases)?;
        }
        BlockContent::Compound(inner_blocks) => {
            walk_blocks(inner_blocks, source, alloc, result, seen_bases)?;
        }
        BlockContent::DocumentAttribute(key, AttrValue::String(w)) if key == "weight" => {
            result.weight = w.trim().parse::<i32>().unwrap_or(0);
        }
        _ => {}
    }
    Ok(())
}

fn extract_req_block<'arena>(
    block: &Block<'arena>,
    source: &str,
    result: &mut WalkResult,
    seen_bases: &mut HashSet<String>,
) -> eyre::Result<()> {
    let attrs = &block.meta.attrs;

    let id_str = attrs
        .id()
        .map(|s| s.as_ref())
        .ok_or_else(|| eyre::eyre!("requirement block missing `id=\"...\"` attribute"))?;
    let req_id = marq::parse_rule_id(id_str)
        .ok_or_else(|| eyre::eyre!("invalid requirement id '{}'", id_str))?;

    if seen_bases.contains(&req_id.base) {
        eyre::bail!("Duplicate requirement '{}' in AsciiDoc file", req_id.base);
    }
    seen_bases.insert(req_id.base.clone());

    let mut metadata = ReqMetadata::default();
    if let Some(v) = attrs.named("status") {
        metadata.status = marq::ReqStatus::parse(v);
    }
    if let Some(v) = attrs.named("level") {
        metadata.level = marq::ReqLevel::parse(v);
    }
    if let Some(v) = attrs.named("since") {
        metadata.since = Some(v.to_string());
    }
    if let Some(v) = attrs.named("until") {
        metadata.until = Some(v.to_string());
    }
    if let Some(v) = attrs.named("tags") {
        metadata.tags = v.split(',').map(|s| s.trim().to_string()).collect();
    }

    // Marker span: the source range of whichever `[...]` attribute-list line
    // carries the `id=` attribute (brackets included), so `rewrite_marker` can
    // splice a bumped id in place, byte-for-byte.
    let marker_loc = attrs
        .iter()
        .find(|a| a.id.is_some())
        .map(|a| a.loc)
        .unwrap_or(block.meta.start_loc);
    let marker_span = SourceSpan {
        offset: marker_loc.start as usize,
        length: (marker_loc.end - marker_loc.start) as usize,
    };

    let (span_start, span_end) = content_span(&block.content).unwrap_or((
        marker_loc.end as usize,
        marker_loc.end as usize,
    ));
    let line = byte_offset_to_line(source, span_start);
    let anchor = req_anchor_id(&req_id.to_string());

    let raw = source
        .get(span_start..span_end.min(source.len()))
        .unwrap_or("")
        .trim()
        .to_string();

    let req = ReqDefinition {
        id: req_id,
        anchor_id: anchor,
        marker_span,
        span: SourceSpan {
            offset: span_start,
            length: span_end.saturating_sub(span_start),
        },
        line,
        metadata,
        raw,
        // Filled in later, once the AsciiDoc HTML backend has rendered the
        // full document — see `mod.rs`'s `post_process_html`. This block is a
        // real, distinct AsciiDoc construct, so its HTML is spliced out of
        // asciidork's own render rather than reimplemented here.
        html: String::new(),
    };

    result.elements.push(DocElement::Req(req.clone()));
    result.reqs.push(req);

    // Still walk any nested blocks (e.g. admonitions, nested lists) so their
    // inline code spans are collected for hover/search.
    if let BlockContent::Compound(inner_blocks) = &block.content {
        for inner in inner_blocks {
            collect_inline_spans_recursive(inner, &mut result.inline_code_spans);
        }
    } else if let BlockContent::Simple(inlines) = &block.content {
        collect_spans_from_inlines(inlines, &mut result.inline_code_spans);
    }

    Ok(())
}

/// Compute the byte span of a block's own content, excluding its attribute
/// list, title, and (for compound blocks) delimiter lines.
fn content_span(content: &BlockContent<'_>) -> Option<(usize, usize)> {
    match content {
        BlockContent::Simple(inlines) => {
            let start = inlines.first()?.loc.start as usize;
            let end = inlines.last_loc().map_or(start, |l| l.end as usize);
            Some((start, end))
        }
        BlockContent::Compound(inner_blocks) => {
            let start = inner_blocks.first()?.loc.start_pos as usize;
            let end = content.last_loc().map_or(start, |l| l.end as usize);
            Some((start, end))
        }
        _ => None,
    }
}

fn collect_inline_spans_recursive<'arena>(
    block: &Block<'arena>,
    spans: &mut Vec<InlineCodeSpan>,
) {
    match &block.content {
        BlockContent::Simple(inlines) => collect_spans_from_inlines(inlines, spans),
        BlockContent::Compound(inner_blocks) => {
            for inner in inner_blocks {
                collect_inline_spans_recursive(inner, spans);
            }
        }
        _ => {}
    }
}

fn collect_spans_from_inlines<'arena>(
    inlines: &InlineNodes<'arena>,
    spans: &mut Vec<InlineCodeSpan>,
) {
    for node in inlines.iter() {
        match &node.content {
            Inline::Span(asciidork_ast::SpanKind::LitMono, _, inner)
            | Inline::Span(asciidork_ast::SpanKind::Mono, _, inner) => {
                let content = inner.plain_text().join("");
                let start = node.loc.start as usize;
                let end = node.loc.end as usize;
                spans.push(InlineCodeSpan {
                    content,
                    span: SourceSpan {
                        offset: start,
                        length: end.saturating_sub(start),
                    },
                });
                collect_spans_from_inlines(inner, spans);
            }
            Inline::Span(_, _, inner) => {
                collect_spans_from_inlines(inner, spans);
            }
            _ => {}
        }
    }
}
