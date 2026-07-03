//! `AsciiDocRoles` spec backend — native `[role="requirement", id="..."]`
//! open blocks, asciidork-parser based.
//!
//! Unlike [`super::asciidoc::Asciidoc`] (which marks requirements with a
//! leading `r[id]` token that asciidork treats as ordinary paragraph text),
//! a role-marked open block is a real, distinct AsciiDoc construct — asciidork
//! renders it as its own `<div id="..." class="openblock requirement">`. So
//! instead of re-rendering the requirement body by hand, this backend locates
//! that div (by its authored `id=`) in asciidork's own HTML output and splices
//! the `req-container`/badge wrapper around it.

mod ast_walk;

use std::ops::Range;

use asciidork_core::JobSettings;
use asciidork_parser::prelude::*;
use bumpalo::Bump;
use marq::{ReqDefinition, SourceSpan};

use super::{
    BadgeFn, NoConfig, REQ_CONTAINER_CLOSE, RenderInput, RenderOutput, RenderedSection,
    SlugAllocator, SpecBackend, SpecDoc, SpecFormat, adoc_parse_weight, html_escape,
};

/// AsciiDoc backend using native role/attribute-list requirement blocks.
#[derive(Default)]
pub struct AsciiDocRoles;

#[async_trait::async_trait]
impl SpecBackend for AsciiDocRoles {
    type Config = NoConfig;

    fn format(&self) -> SpecFormat {
        SpecFormat::AsciiDocRoles
    }
    fn name(&self) -> &'static str {
        "asciidoc-roles"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["adoc", "asciidoc"]
    }

    async fn parse(&self, content: &str) -> eyre::Result<SpecDoc> {
        let req_renderer = |req: &ReqDefinition| {
            let anchor = req.anchor_id.clone();
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
        adoc_parse_weight(content)
    }

    fn extract_marker_prefix(&self, content: &str, span: SourceSpan) -> Option<String> {
        let start = span.offset;
        let end = start.checked_add(span.length)?;
        let marker = content.get(start..end)?;
        Some(
            find_named_attr(marker, "prefix")
                .map(|(s, e)| marker[s..e].to_string())
                .unwrap_or_else(|| "r".to_string()),
        )
    }

    fn id_range_in_marker(&self, marker: &str) -> eyre::Result<Range<usize>> {
        find_named_attr(marker, "id")
            .map(|(s, e)| s..e)
            .ok_or_else(|| eyre::eyre!("malformed asciidoc-roles marker: {}", marker))
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

/// Find a `key="value"` (or `key=value`) attribute in a `[...]` bracket-list
/// marker string. Returns the byte range of the value, quotes excluded.
///
/// Skips false matches where `key` is a substring of a longer attribute name
/// (e.g. searching for `id` must not match inside `prefix`) by requiring a
/// list-boundary character (`[`, `,`, ` `) immediately before the match and a
/// literal `=` immediately after it.
fn find_named_attr(marker: &str, key: &str) -> Option<(usize, usize)> {
    let mut search_from = 0;
    loop {
        let rel = marker[search_from..].find(key)?;
        let key_start = search_from + rel;
        let after_key = key_start + key.len();

        let boundary_ok = marker[..key_start]
            .chars()
            .next_back()
            .is_none_or(|c| matches!(c, '[' | ',' | ' '));

        let rest = &marker[after_key..];
        if !boundary_ok || !rest.starts_with('=') {
            search_from = after_key;
            continue;
        }

        let value_part = &rest[1..];
        let (quoted, value_part) = match value_part.strip_prefix('"') {
            Some(v) => (true, v),
            None => (false, value_part),
        };
        let value_len = if quoted {
            value_part.find('"')?
        } else {
            value_part.find([',', ']']).unwrap_or(value_part.len())
        };
        let value_start = after_key + 1 + usize::from(quoted);
        let value_end = value_start + value_len;
        return Some((value_start, value_end));
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
    let content_html = post_process_html(content_html, &mut walk.reqs, &walk.section_id_map, req_renderer);

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
    reqs: &mut [ReqDefinition],
    section_id_map: &[(String, String)],
    req_renderer: &dyn Fn(&ReqDefinition) -> (String, String),
) -> String {
    let mut html = content_html.to_string();

    for req in reqs.iter_mut() {
        let doc_id = req.id.to_string();
        if let Some((range, inner)) = extract_div_by_id(&html, &doc_id) {
            req.html = inner.clone();
            let (open_html, close_html) = req_renderer(req);
            let replacement = format!("{open_html}{inner}{close_html}");
            html.replace_range(range, &replacement);
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

    html
}

/// Locate a `<div id="{id}" ...>...</div>` element in rendered HTML by its
/// exact `id=` attribute, tracking nested `<div>`/`</div>` depth so the match
/// spans the whole element even when its content contains further divs
/// (admonitions, nested blocks, etc).
///
/// Returns the byte range of the whole element and the inner HTML (between
/// the opening tag's `>` and the matching `</div>`).
fn extract_div_by_id(html: &str, id: &str) -> Option<(Range<usize>, String)> {
    let marker = format!(r#" id="{}""#, id);
    let attr_pos = html.find(&marker)?;
    let tag_start = html[..attr_pos].rfind("<div")?;
    let tag_close = html[tag_start..].find('>')? + tag_start + 1;

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
