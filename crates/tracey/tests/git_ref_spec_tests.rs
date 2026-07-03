//! Integration tests for `SpecConfig.git_ref`: a spec pinned to a git ref
//! must be read from that ref's blob instead of the live working tree.
//!
//! Mirrors the git-fixture pattern in `bump_tests.rs`: a real git repo in a
//! temp dir, tagged at one commit, then modified on disk without committing.

use std::fs;
use std::path::Path;
use std::process::Command;

use tracey::config::{Config, Impl, SpecConfig};
use tracey::data::build_dashboard_data;

fn git_init(dir: &Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git not found");
        assert!(status.success(), "git {args:?} failed");
    };

    run(&["init", "--initial-branch=main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    run(&["config", "tag.gpgSign", "false"]);
    run(&["config", "commit.gpgSign", "false"]);
}

fn git_commit_all(dir: &Path, message: &str) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git not found");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["add", "."]);
    run(&["commit", "-m", message]);
}

fn git_tag(dir: &Path, tag: &str) {
    let status = Command::new("git")
        .args(["tag", tag])
        .current_dir(dir)
        .status()
        .expect("git not found");
    assert!(status.success(), "git tag {tag} failed");
}

fn config_with_git_ref(git_ref: Option<&str>) -> Config {
    Config {
        specs: vec![SpecConfig {
            name: "test".to_string(),
            prefix: None,
            source_url: None,
            syntax: None,
            git_ref: git_ref.map(str::to_string),
            include: vec!["spec.md".to_string()],
            format: Default::default(),
            impls: vec![Impl {
                name: "rust".to_string(),
                include: vec!["src/**/*.rs".to_string()],
                exclude: vec![],
                test_include: vec![],
            }],
        }],
    }
}

const SPEC_AT_TAG: &str = "\
# Spec

r[auth.login]
Users MUST provide valid credentials to log in.
";

const SPEC_ON_DISK: &str = "\
# Spec

r[auth.login]
Users MUST provide valid credentials to log in.

r[auth.session]
Sessions MUST expire after 24 hours of inactivity.
";

/// A spec pinned to a git ref must reflect that ref's content, ignoring
/// uncommitted on-disk edits made after the tag.
#[tokio::test]
async fn git_ref_reads_pinned_content_not_working_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    git_init(root);
    fs::write(root.join("spec.md"), SPEC_AT_TAG).unwrap();
    git_commit_all(root, "initial spec");
    git_tag(root, "v1");

    // Modify on disk without committing: a git_ref-pinned spec must not see this.
    fs::write(root.join("spec.md"), SPEC_ON_DISK).unwrap();

    let config = config_with_git_ref(Some("v1"));
    let data = build_dashboard_data(root, &config, 1, true)
        .await
        .expect("build should succeed");

    let forward = data
        .forward_by_impl
        .values()
        .next()
        .expect("one impl configured");
    let rule_ids: Vec<String> = forward.rules.iter().map(|r| r.id.to_string()).collect();
    assert_eq!(
        rule_ids,
        vec!["auth.login"],
        "git_ref-pinned spec must only see the tagged commit's rules, not the uncommitted edit"
    );
}

/// Without `git_ref`, the working tree (including uncommitted edits) is used,
/// same as before this feature existed.
#[tokio::test]
async fn no_git_ref_reads_working_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    git_init(root);
    fs::write(root.join("spec.md"), SPEC_AT_TAG).unwrap();
    git_commit_all(root, "initial spec");
    git_tag(root, "v1");

    fs::write(root.join("spec.md"), SPEC_ON_DISK).unwrap();

    let config = config_with_git_ref(None);
    let data = build_dashboard_data(root, &config, 1, true)
        .await
        .expect("build should succeed");

    let forward = data
        .forward_by_impl
        .values()
        .next()
        .expect("one impl configured");
    let mut rule_ids: Vec<String> = forward.rules.iter().map(|r| r.id.to_string()).collect();
    rule_ids.sort();
    assert_eq!(rule_ids, vec!["auth.login", "auth.session"]);
}
