//! Engine-level (config-driven) integration tests for the `AsciiDocRoles`
//! backend, exercised via `SpecConfig.syntax = "asciidoc-roles"` — the Part 2
//! override that disambiguates it from the marker-based `Asciidoc` backend
//! (both claim `.adoc`/`.asciidoc`).

use std::path::PathBuf;
use std::sync::Arc;

use tracey_core::parse_rule_id;

mod common;

fn rpc<T, E: std::fmt::Debug>(res: Result<T, vox::VoxError<E>>) -> T {
    res.expect("RPC call failed")
}

fn rid(id: &str) -> tracey_core::RuleId {
    parse_rule_id(id).expect("valid rule id")
}

fn fixtures_asciidoc_roles() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures-asciidoc-roles")
}

async fn create_roles_engine() -> Arc<tracey::daemon::Engine> {
    let project_root = fixtures_asciidoc_roles();
    let config_path = project_root.join("config.styx");
    Arc::new(
        tracey::daemon::Engine::new(project_root, config_path)
            .await
            .expect("Failed to create AsciiDocRoles test engine"),
    )
}

async fn create_roles_service() -> common::RpcTestService {
    let engine = create_roles_engine().await;
    let service = tracey::daemon::TraceyService::new(engine);
    common::create_test_rpc_service(service).await
}

#[tokio::test]
async fn test_roles_config_syntax_selects_backend() {
    let service = create_roles_service().await;
    let status = rpc(service.client.status().await);

    let test_impl = status
        .impls
        .iter()
        .find(|i| i.spec == "test" && i.impl_name == "rust")
        .expect("test/rust impl should exist");

    assert!(
        test_impl.total_rules >= 8,
        "Expected at least 8 rules from spec.adoc via the asciidoc-roles syntax override, got {}",
        test_impl.total_rules
    );
}

#[tokio::test]
async fn test_roles_masking_excludes_fake_reqs() {
    let service = create_roles_service().await;
    let status = rpc(service.client.status().await);

    let test_impl = status
        .impls
        .iter()
        .find(|i| i.spec == "test" && i.impl_name == "rust")
        .expect("test/rust impl should exist");

    assert!(
        test_impl.total_rules < 15,
        "Listing/comment block masking failed — too many rules: {}",
        test_impl.total_rules
    );
}

#[tokio::test]
async fn test_roles_rule_lookup() {
    let service = create_roles_service().await;
    let rule = rpc(service.client.rule(rid("auth.login")).await);

    assert!(rule.is_some(), "auth.login rule should exist");
    let info = rule.unwrap();
    assert_eq!(info.id, rid("auth.login"));
    assert!(
        info.raw.contains("valid credentials"),
        "Rule body should contain spec text, got: {:?}",
        info.raw
    );
}

#[tokio::test]
async fn test_roles_deprecated_status_rule_lookup() {
    let service = create_roles_service().await;
    // error.codes is declared with status="deprecated" and tags in the
    // fixture, exercising named-attribute metadata parsing end-to-end.
    let rule = rpc(service.client.rule(rid("error.codes")).await);
    assert!(rule.is_some(), "error.codes rule should exist");
}
