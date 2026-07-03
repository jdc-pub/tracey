//! Integration tests for the `AsciiDocRoles` spec backend
//! (`[role="requirement", id="..."]` open blocks).
//!
//! Engine-level (config-driven) coverage lives in `asciidoc_spec_tests.rs`'s
//! sibling once `SpecConfig.syntax` (Part 2) can route `.adoc` files here;
//! until then these tests call the backend directly via `parse_spec`.

use tracey_core::spec::{SourceSpan, SpecFormat, extract_marker_prefix, id_range_in_marker, parse_spec, parse_weight, rewrite_marker};

// ============================================================================
// Parsing: basic requirement blocks
// ============================================================================

#[tokio::test]
async fn test_roles_basic_block_default_prefix() {
    let src = r#"= Title

[role="requirement", id="auth.login"]
--
Users MUST log in with valid credentials.
--
"#;
    let doc = parse_spec(SpecFormat::AsciiDocRoles, src).await.expect("parse");
    assert_eq!(doc.reqs.len(), 1);
    assert_eq!(doc.reqs[0].id.to_string(), "auth.login");
    assert_eq!(doc.reqs[0].anchor_id, "r--auth.login");
    assert!(doc.reqs[0].raw.contains("valid credentials"));
}

#[tokio::test]
async fn test_roles_explicit_prefix_status_tags() {
    let src = r#"= Title

[role="requirement", id="error.codes", prefix="h2", status="deprecated", tags="legacy,audit"]
--
Some deprecated behavior text.
--
"#;
    let doc = parse_spec(SpecFormat::AsciiDocRoles, src).await.expect("parse");
    assert_eq!(doc.reqs.len(), 1);
    let req = &doc.reqs[0];
    assert_eq!(req.metadata.status, Some(marq::ReqStatus::Deprecated));
    assert_eq!(req.metadata.tags, vec!["legacy".to_string(), "audit".to_string()]);

    let prefix = extract_marker_prefix(SpecFormat::AsciiDocRoles, src, req.marker_span)
        .expect("prefix should resolve");
    assert_eq!(prefix, "h2");
}

#[tokio::test]
async fn test_roles_rich_content_passthrough() {
    let src = r#"= Title

[role="requirement", id="compatibility.facet"]
.Compat title
--
The `facet-git-tree` crate MUST satisfy all requirements.

NOTE: something extra.

See link:#other-id[other].
--
"#;
    let doc = parse_spec(SpecFormat::AsciiDocRoles, src).await.expect("parse");
    let req = &doc.reqs[0];
    assert!(req.html.contains("admonitionblock note"), "NOTE admonition should render, got: {}", req.html);
    assert!(req.html.contains("href=\"#other-id\""), "link should render, got: {}", req.html);
    assert!(req.html.contains("<code>facet-git-tree</code>"), "inline code should render, got: {}", req.html);

    assert!(doc.html.contains("req-container"), "spec HTML should splice a req-container wrapper");
    assert!(doc.html.contains(&req.anchor_id), "spec HTML should contain the req anchor id");
}

#[tokio::test]
async fn test_roles_masks_role_text_inside_listing_block() {
    let src = r#"= Title

----
[role="requirement", id="fake.one"]
--
not a real req
--
----
"#;
    let doc = parse_spec(SpecFormat::AsciiDocRoles, src).await.expect("parse");
    assert!(doc.reqs.is_empty(), "role text inside a listing block must not be extracted");
}

#[tokio::test]
async fn test_roles_masks_role_text_inside_comment() {
    let src = r#"= Title

////
[role="requirement", id="fake.two"]
--
not a real req
--
////
"#;
    let doc = parse_spec(SpecFormat::AsciiDocRoles, src).await.expect("parse");
    assert!(doc.reqs.is_empty(), "role text inside a comment block must not be extracted");
}

#[tokio::test]
async fn test_roles_duplicate_id_errors() {
    let src = r#"= Title

[role="requirement", id="dup.id"]
--
First.
--

[role="requirement", id="dup.id"]
--
Second.
--
"#;
    let result = parse_spec(SpecFormat::AsciiDocRoles, src).await;
    assert!(result.is_err(), "duplicate requirement id should error");
}

#[tokio::test]
async fn test_roles_non_requirement_open_block_ignored() {
    let src = r#"= Title

--
Just a normal open block, no role.
--
"#;
    let doc = parse_spec(SpecFormat::AsciiDocRoles, src).await.expect("parse");
    assert!(doc.reqs.is_empty(), "an open block without role=\"requirement\" is not a req");
}

// ============================================================================
// Bump: id_range_in_marker / rewrite_marker / extract_marker_prefix
// ============================================================================

#[test]
fn test_roles_extract_marker_prefix_defaults_to_r() {
    let content = r#"[role="requirement", id="auth.login"]"#;
    let span = SourceSpan {
        offset: 0,
        length: content.len(),
    };
    let prefix = extract_marker_prefix(SpecFormat::AsciiDocRoles, content, span).expect("prefix");
    assert_eq!(prefix, "r");
}

#[test]
fn test_roles_extract_marker_prefix_explicit() {
    let content = r#"[role="requirement", id="auth.login", prefix="h2"]"#;
    let span = SourceSpan {
        offset: 0,
        length: content.len(),
    };
    let prefix = extract_marker_prefix(SpecFormat::AsciiDocRoles, content, span).expect("prefix");
    assert_eq!(prefix, "h2");
}

#[test]
fn test_roles_id_range_in_marker() {
    let marker = r#"[role="requirement", id="auth.login"]"#;
    let range = id_range_in_marker(SpecFormat::AsciiDocRoles, marker).expect("id_range");
    assert_eq!(&marker[range], "auth.login");
}

#[test]
fn test_roles_rewrite_marker_bumps_version() {
    let marker = r#"[role="requirement", id="auth.login"]"#;
    let range = id_range_in_marker(SpecFormat::AsciiDocRoles, marker).expect("id_range");
    let rewritten = rewrite_marker(marker, range, "auth.login", 2).expect("rewrite");
    assert_eq!(rewritten, r#"[role="requirement", id="auth.login+2"]"#);
}

// ============================================================================
// parse_weight
// ============================================================================

#[test]
fn test_roles_parse_weight_attribute() {
    assert_eq!(parse_weight(SpecFormat::AsciiDocRoles, ":weight: 10\n\n= Title"), 10);
    assert_eq!(parse_weight(SpecFormat::AsciiDocRoles, "= Title\n\nNo weight"), 0);
}
