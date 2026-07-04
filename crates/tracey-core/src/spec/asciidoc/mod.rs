//! AsciiDoc spec backend — asciidork-parser based.
//!
//! Two-pass implementation:
//! 1. Walk the asciidork AST to extract requirements, headings, and inline code spans.
//! 2. Convert to HTML via `asciidork_dr_html_backend::convert()`, then post-process
//!    to inject `<div class="req-container">` wrappers and fix heading IDs.
//!
//! Requirements can be marked two ways:
//! - a leading `r[id]` token on a paragraph (asciidork treats it as ordinary
//!   paragraph text, so the body HTML is re-rendered by hand);
//! - a native `[role="requirement", id="..."]` open block — a real, distinct
//!   AsciiDoc construct that asciidork renders as its own
//!   `<div id="..." class="openblock requirement">`, which post-processing
//!   locates by its authored `id=` and wraps in place.

mod ast_walk;

use std::ops::Range;

use asciidork_core::JobSettings;
use asciidork_parser::prelude::*;
use bumpalo::Bump;
use marq::{ReqDefinition, SourceSpan};

use super::{
    BadgeFn, NoConfig, REQ_CONTAINER_CLOSE, RenderInput, RenderOutput, RenderedSection,
    SlugAllocator, SpecBackend, SpecDoc, SpecFormat, req_anchor_id,
};

/// AsciiDoc backend.
#[derive(Default)]
pub struct Asciidoc;

#[async_trait::async_trait]
impl SpecBackend for Asciidoc {
    type Config = NoConfig;

    fn format(&self) -> SpecFormat {
        SpecFormat::AsciiDoc
    }
    fn name(&self) -> &'static str {
        "asciidoc"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["adoc", "asciidoc"]
    }

    async fn parse(&self, content: &str) -> eyre::Result<SpecDoc> {
        let req_renderer = |req: &ReqDefinition| {
            let anchor = req_anchor_id(&req.id.to_string());
            let open = format!(
                r#"<div class="req-container req-uncovered" id="{anchor}" data-br="{start}-{end}"><div class="req-content">"#,
                anchor = html_escape(&anchor),
                start = req.span.offset,
                end = req.span.offset + req.span.length,
            );
            (open, REQ_CONTAINER_CLOSE.to_string())
        };
        parse_sync(content, &req_renderer, None)
    }

    fn parse_weight(&self, content: &str) -> i32 {
        // YAML/TOML frontmatter first
        if let Ok((fm, _)) = marq::parse_frontmatter(content) && fm.weight != 0 {
            return fm.weight;
        }
        // AsciiDoc `:weight: N` document attribute (line scan, pre-title only)
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('=') {
                break;
            }
            if let Some(w) = line
                .strip_prefix(":weight:")
                .and_then(|r| r.trim().parse::<i32>().ok())
            {
                return w;
            }
        }
        0
    }

    fn extract_marker_prefix(&self, content: &str, span: SourceSpan) -> Option<String> {
        let start = span.offset;
        let end = start.checked_add(span.length)?;
        let marker = content.get(start..end)?;
        if marker.starts_with('[') {
            // Attribute-list marker from a role-marked block: the requirement
            // prefix comes from an optional `prefix="..."` attribute.
            return Some(
                find_named_attr(marker, "prefix")
                    .map(|(s, e)| marker[s..e].to_string())
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| "r".to_string()),
            );
        }
        let bracket = marker.find('[')?;
        let prefix = marker[..bracket].trim();
        if prefix.is_empty() {
            return None;
        }
        Some(prefix.to_string())
    }

    fn id_range_in_marker(&self, marker: &str) -> eyre::Result<Range<usize>> {
        if marker.starts_with('[') {
            return find_named_attr(marker, "id")
                .map(|(s, e)| s..e)
                .ok_or_else(|| eyre::eyre!("malformed asciidoc requirement marker: {}", marker));
        }
        let open = marker
            .find('[')
            .ok_or_else(|| eyre::eyre!("malformed asciidoc marker: {}", marker))?;
        let close = marker
            .rfind(']')
            .ok_or_else(|| eyre::eyre!("malformed asciidoc marker: {}", marker))?;
        Ok(open + 1..close)
    }

    fn diff_inline(&self, old: &str, new: &str) -> Option<String> {
        Some(marq::diff_markdown_inline(old, new))
    }

    async fn render_html(
        &self,
        input: RenderInput<'_>,
        _cfg: &NoConfig,
    ) -> eyre::Result<RenderOutput> {
        let RenderInput {
            sources,
            root,
            badge_for,
            slugs,
            deps: _,
            ..
        } = input;

        let mut sections = Vec::with_capacity(sources.len());
        let mut first_err: Option<eyre::Error> = None;

        for (idx, src) in sources.iter().enumerate() {
            let abs_source = root.join(src.path).display().to_string();
            let badge_for_clone: BadgeFn = badge_for.clone();
            let abs_source_clone = abs_source.clone();
            let req_renderer = move |req: &ReqDefinition| {
                let open = badge_for_clone(req, &abs_source_clone);
                (open, REQ_CONTAINER_CLOSE.to_string())
            };
            match parse_sync(src.content, &req_renderer, Some(slugs)) {
                Ok(doc) => sections.push(RenderedSection {
                    source_idx: idx,
                    html: doc.html,
                    elements: doc.elements,
                    head_injections: doc.head_injections,
                }),
                Err(e) if first_err.is_none() => first_err = Some(e),
                Err(_) => {}
            }
        }

        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(RenderOutput { sections })
    }
}

fn parse_sync(
    content: &str,
    req_renderer: &dyn Fn(&ReqDefinition) -> (String, String),
    alloc: Option<&mut SlugAllocator>,
) -> eyre::Result<SpecDoc> {
    let arena = Bump::new();
    let mut parser = Parser::from_str(content, SourceFile::Tmp, &arena);
    // Non-strict: cross-file xrefs are unresolvable during per-file parsing
    // but valid at render time when all spec files share one HTML page.
    parser.apply_job_settings(JobSettings {
        strict: false,
        ..JobSettings::default()
    });
    let parsed = parser
        .parse()
        .map_err(|e| eyre::eyre!("AsciiDoc parse error: {:?}", e))?;

    let mut owned_alloc = SlugAllocator::default();
    let alloc_ref = alloc.unwrap_or(&mut owned_alloc);

    let mut walk = ast_walk::walk(&parsed.document, content, alloc_ref)?;

    let html = asciidork_dr_html_backend::convert(parsed.document)
        .map_err(|e| eyre::eyre!("AsciiDoc HTML render error: {:?}", e))?;

    let content_html = extract_content_html(&html);
    let content_html = post_process_html(
        content_html,
        content,
        &mut walk.reqs,
        &walk.section_id_map,
        req_renderer,
    )?;

    Ok(marq::Document {
        raw_metadata: None,
        metadata_format: None,
        frontmatter: None,
        html: content_html,
        headings: walk.headings,
        reqs: walk.reqs,
        code_samples: Vec::new(),
        elements: walk.elements,
        head_injections: Vec::new(),
        inline_code_spans: walk.inline_code_spans,
        source_map: Default::default(),
    })
}

/// Extract the inner HTML of `<div id="content">` from full asciidork output.
fn extract_content_html(full_html: &str) -> &str {
    let content_marker = r#"<div id="content">"#;
    let footer_marker = r#"<div id="footer">"#;

    let Some(content_start) = full_html.find(content_marker) else {
        return full_html;
    };
    let inner_start = content_start + content_marker.len();

    let Some(footer_start) = full_html[inner_start..].find(footer_marker) else {
        if let Some(body_end) = full_html[inner_start..].find("</body>") {
            let raw = &full_html[inner_start..inner_start + body_end];
            return raw.strip_suffix("</div>").unwrap_or(raw);
        }
        return &full_html[inner_start..];
    };

    let before_footer = &full_html[inner_start..inner_start + footer_start];
    before_footer.strip_suffix("</div>").unwrap_or(before_footer)
}

fn post_process_html(
    content_html: &str,
    source: &str,
    reqs: &mut [ReqDefinition],
    section_id_map: &[(String, String)],
    req_renderer: &dyn Fn(&ReqDefinition) -> (String, String),
) -> eyre::Result<String> {
    let mut html = content_html.to_string();
    let mut cursor = 0usize;

    for req in reqs.iter_mut() {
        let marker_text = source
            .get(req.marker_span.offset..req.marker_span.offset + req.marker_span.length)
            .unwrap_or("");
        if marker_text.starts_with('[') {
            // Role-marked open block: asciidork rendered it as its own div, so
            // locate that div by its authored id and splice the req-container
            // wrapper around it instead of re-rendering the body by hand.
            // Requirements appear in document order, so the search only ever
            // moves forward, and any earlier stray text (e.g. an example
            // block showing the syntax) cannot be picked up in place of the
            // real block that comes after it.
            let doc_id = req.id.to_string();
            let (range, inner) = extract_div_by_id(&html, &doc_id, cursor).ok_or_else(|| {
                eyre::eyre!(
                    "could not locate rendered <div id=\"{doc_id}\"> for requirement block"
                )
            })?;
            req.html = inner.clone();
            let (open_html, close_html) = req_renderer(req);
            let replacement = format!("{open_html}{inner}{close_html}");
            cursor = range.start + replacement.len();
            html.replace_range(range, &replacement);
        } else if !marker_text.is_empty() {
            html = replace_req_paragraph(&html, req, marker_text, req_renderer);
        }
    }

    for (adoc_id, our_slug) in section_id_map {
        let from_id = format!(r#" id="{}""#, adoc_id);
        let to_id = format!(r#" id="{}""#, our_slug);
        html = html.replace(&from_id, &to_id);

        let from_href = format!(" href=\"#{}\"", adoc_id);
        let to_href = format!(" href=\"#{}\"", our_slug);
        html = html.replace(&from_href, &to_href);
    }

    Ok(html)
}

fn replace_req_paragraph(
    html: &str,
    req: &ReqDefinition,
    marker_text: &str,
    req_renderer: &dyn Fn(&ReqDefinition) -> (String, String),
) -> String {
    let search = format!(r#"<div class="paragraph"><p>{}"#, marker_text);

    let Some(div_start) = html.find(&search) else {
        return html.to_string();
    };

    let after_marker = &html[div_start + search.len()..];

    let Some(body_end) = after_marker.find("</p></div>") else {
        return html.to_string();
    };

    let body_in_html = &after_marker[..body_end];
    let body_in_html = body_in_html.strip_prefix(' ').unwrap_or(body_in_html);

    let div_end = div_start + search.len() + body_end + "</p></div>".len();

    let (open_html, close_html) = req_renderer(req);
    let replacement = if body_in_html.is_empty() {
        format!("{open_html}{close_html}")
    } else {
        format!("{open_html}<p>{body_in_html}</p>\n{close_html}")
    };

    let mut result = String::with_capacity(html.len());
    result.push_str(&html[..div_start]);
    result.push_str(&replacement);
    result.push_str(&html[div_end..]);
    result
}

/// Find a `key="value"` (or `key=value`) attribute in a marker string that may
/// contain one or more stacked `[...]` attribute-list lines (role, prefix, and
/// id can each live on their own line). Returns the byte range of the value,
/// quotes excluded.
///
/// Within each bracket group, splits on top-level commas (quote-aware, so a
/// quoted value like `tags="foo, id=5"` cannot produce a false `id` match) and
/// requires each candidate segment to start with exactly `key=`, so `key`
/// never matches inside a longer attribute name. If `key` isn't found in one
/// group, the search continues into the next `[...]` group rather than
/// stopping at the first `]`.
fn find_named_attr(marker: &str, key: &str) -> Option<(usize, usize)> {
    let bytes = marker.as_bytes();
    let mut group_from = 0usize;
    loop {
        let list_start = group_from + marker[group_from..].find('[')? + 1;
        let mut quote: Option<u8> = None;
        let mut seg_start = list_start;
        let mut pos = list_start;
        while pos < bytes.len() {
            let b = bytes[pos];
            if let Some(q) = quote {
                if b == q {
                    quote = None;
                }
            } else if b == b'"' || b == b'\'' {
                quote = Some(b);
            } else if b == b',' || b == b']' {
                if let Some(range) = named_attr_value_in(marker, seg_start, pos, key) {
                    return Some(range);
                }
                if b == b']' {
                    break;
                }
                seg_start = pos + 1;
            }
            pos += 1;
        }
        if pos >= bytes.len() {
            return named_attr_value_in(marker, seg_start, bytes.len(), key);
        }
        group_from = pos + 1;
    }
}

/// Check whether `marker[seg_start..seg_end]` is a `key=value` attribute;
/// return the byte range of the value (quotes excluded) if so.
fn named_attr_value_in(
    marker: &str,
    seg_start: usize,
    seg_end: usize,
    key: &str,
) -> Option<(usize, usize)> {
    let seg = &marker[seg_start..seg_end];
    let trimmed = seg.trim_start();
    let key_start = seg_start + (seg.len() - trimmed.len());
    let rest = trimmed.strip_prefix(key)?.strip_prefix('=')?;
    let value_start = key_start + key.len() + 1;
    for quote in ['"', '\''] {
        if let Some(quoted) = rest.strip_prefix(quote) {
            let len = quoted.find(quote)?;
            return Some((value_start + 1, value_start + 1 + len));
        }
    }
    Some((value_start, value_start + rest.trim_end().len()))
}

/// Locate a `<div id="{id}" ...>...</div>` element in rendered HTML by its
/// exact `id=` attribute, tracking nested `<div>`/`</div>` depth so the match
/// spans the whole element even when its content contains further divs
/// (admonitions, nested blocks, etc).
///
/// Searches starting at `from` (callers pass a forward-only cursor, since
/// requirements are visited in document order). A textual match for
/// ` id="..."` is only accepted when it falls inside the *opening tag* of the
/// nearest preceding `<div`, not merely somewhere in that div's body — e.g. an
/// example block whose escaped text happens to spell out the same attribute
/// is body content, not a real id, so the search keeps going past it.
///
/// Returns the byte range of the whole element and the inner HTML (between
/// the opening tag's `>` and the matching `</div>`).
fn extract_div_by_id(html: &str, id: &str, from: usize) -> Option<(Range<usize>, String)> {
    let marker = format!(r#" id="{}""#, id);
    let mut search_from = from;
    let (tag_start, tag_close) = loop {
        let attr_pos = search_from + html[search_from..].find(&marker)?;
        let tag_start = html[..attr_pos].rfind("<div")?;
        let tag_close = html[tag_start..].find('>')? + tag_start + 1;
        if attr_pos < tag_close {
            break (tag_start, tag_close);
        }
        search_from = attr_pos + marker.len();
    };

    let mut depth = 1usize;
    let mut pos = tag_close;
    loop {
        let next_open = html[pos..].find("<div").map(|i| i + pos);
        let next_close = html[pos..].find("</div>").map(|i| i + pos);
        match (next_open, next_close) {
            (Some(o), Some(c)) if o < c => {
                depth += 1;
                pos = o + 4;
            }
            (_, Some(c)) => {
                depth -= 1;
                pos = c + "</div>".len();
                if depth == 0 {
                    let inner = html[tag_close..c].to_string();
                    return Some((tag_start..pos, inner));
                }
            }
            _ => return None,
        }
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}
