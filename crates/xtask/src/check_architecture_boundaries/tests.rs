//! Unit tests for the architecture-boundary checker.

use std::path::{Path, PathBuf};

use super::budgets::{
    ReverseDependencyBudget, SymbolBudget, configured_paths, count_moa_core_root_exports,
    count_pub_use_exports, missing_configured_paths, moa_core_root_export_allowlist_finding,
    reverse_dependency_budget_reports, symbol_budget_finding, validate_configured_paths,
};
use super::graph::{
    DependencyKind, FORBIDDEN_DEPENDENCY_RULES, PackageGraph, forbidden_dependency_findings,
    parse_package_graph,
};
use super::report::Rule;
use super::restate_rules::{handler_authz_safety_findings, restate_service_traits_from_source};
use super::source_rules::{
    ALLOWANCES, classify_line, event_wildcard_match_arms, is_repository_code_path,
    matching_allowance, scan_release_serving_writes, scan_source,
};

const ENV_OVERLAY_OWNER: &str = "moa-config env overlay LOC budget";
const ENV_OVERLAY_PATH: &str = "crates/moa-config/src/env_overlay/mod.rs";

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("workspace root should resolve from the xtask manifest directory")
}

#[test]
fn release_serving_tables_are_writeable_only_through_the_database_seam() {
    // Pins: a production raw write is rejected even when SQL is split across
    // a Rust line continuation, while an inline negative test may still
    // exercise PostgreSQL's permission denial.
    let root = tempfile::TempDir::new().expect("temp dir");
    let source = root.path().join("owner.rs");
    let production_write = [
        "UPDATE",
        " \\\n    ",
        "moa.artifact_serving_pointer",
        " SET revision_uid = gen_random_uuid()",
    ]
    .concat();
    let fixture = format!(
        r#"
fn bypass() {{
let _ = "{production_write}";
}}

#[cfg(test)]
mod tests {{
const DENIED: &str = "DELETE FROM moa.artifact_activation_audit";
}}
"#
    );
    std::fs::write(&source, fixture).expect("write scanner fixture");

    let findings = scan_release_serving_writes(root.path()).expect("scan fixture");
    assert_eq!(findings.len(), 1, "raw production pointer write must fail");
    assert_eq!(findings[0].rule, Rule::ReleaseServingWriteBoundary);
}

#[test]
fn configured_paths_name_their_owner_and_exist_in_the_real_tree() {
    // Pins: every path the architecture policy is configured against exists,
    // and the env-overlay LOC budget owner points at its current moa-config
    // owner rather than the removed moa-core location.
    let configured = configured_paths();

    let env_overlay = configured
        .iter()
        .find(|entry| entry.owner == ENV_OVERLAY_OWNER)
        .unwrap_or_else(|| {
            panic!("configured paths should include an owner named `{ENV_OVERLAY_OWNER}`")
        });
    assert_eq!(
        env_overlay.path, ENV_OVERLAY_PATH,
        "the env-overlay LOC budget must be owned by moa-config"
    );

    let missing = missing_configured_paths(&repository_root(), &configured);
    assert!(
        missing.is_empty(),
        "configured architecture paths must exist; missing: {missing:?}"
    );
}

#[test]
fn missing_configured_path_reports_its_owner_and_exact_path() {
    // Pins: a configured owner whose file was moved or deleted fails the
    // pre-scan with both the owner label and the exact path, instead of
    // aborting a later rule with an opaque read error.
    let root = tempfile::TempDir::new().expect("temp dir");
    let configured = configured_paths();
    for entry in &configured {
        if entry.path == ENV_OVERLAY_PATH {
            continue;
        }
        std::fs::create_dir_all(root.path().join(&entry.path))
            .expect("materialize configured path");
    }

    let missing = missing_configured_paths(root.path(), &configured);
    assert_eq!(
        missing.len(),
        1,
        "only the removed env-overlay owner should be missing; saw {missing:?}"
    );
    assert_eq!(missing[0].owner, ENV_OVERLAY_OWNER);
    assert_eq!(missing[0].path, ENV_OVERLAY_PATH);

    let error = validate_configured_paths(root.path())
        .expect_err("a missing configured path must fail the pre-scan")
        .to_string();
    assert!(
        error.contains(ENV_OVERLAY_OWNER),
        "error must name the configured owner; got {error}"
    );
    assert!(
        error.contains(ENV_OVERLAY_PATH),
        "error must name the exact missing path; got {error}"
    );
}

#[test]
fn classifies_direct_sql() {
    assert_eq!(
        classify_line("let rows = sqlx::query_scalar::<_, String>(\"SELECT 1\");"),
        Some(Rule::DirectSql)
    );
    assert_eq!(
        classify_line("let row = sqlx::query!(\"SELECT 1 as one\");"),
        Some(Rule::DirectSql)
    );
    assert_eq!(
        classify_line("let row = query_as!(Row, \"SELECT 1 as one\");"),
        Some(Rule::DirectSql)
    );
    assert_eq!(
        classify_line("let mut query = QueryBuilder::<Postgres>::new(\"SELECT 1\");"),
        Some(Rule::DirectSql)
    );
}

#[test]
fn classifies_raw_context_access() {
    assert_eq!(
        classify_line("let pool = OrchestratorCtx::current_graph_pool();"),
        Some(Rule::RuntimeContext)
    );
    assert_eq!(
        classify_line("let store = runtime.session_store();"),
        Some(Rule::RuntimeContext)
    );
    assert_eq!(
        classify_line("let providers = runtime.provider_registry();"),
        Some(Rule::RuntimeContext)
    );
    assert_eq!(
        classify_line("let providers = runtime.auth_providers();"),
        Some(Rule::RuntimeContext)
    );
    assert_eq!(
        classify_line("let providers = OrchestratorCtx::current_provider_registry();"),
        Some(Rule::RuntimeContext)
    );
    assert_eq!(
        classify_line("let config = OrchestratorCtx::current_config().clone();"),
        Some(Rule::RuntimeContext)
    );
    assert_eq!(
        classify_line("let embedder = runtime.embedding_provider();"),
        Some(Rule::RuntimeContext)
    );
    assert_eq!(
        classify_line("OrchestratorCtx::current_lineage().record(json);"),
        Some(Rule::RuntimeContext)
    );
}

#[test]
fn repository_paths_are_classified_by_exact_file_or_directory_component() {
    // Pins: repositories may own SQL, while similarly named handlers remain scanned.
    assert!(is_repository_code_path(
        "crates/moa-orchestrator/src/services/privacy/repository.rs"
    ));
    assert!(is_repository_code_path(
        "crates/moa-orchestrator/src/services/privacy/repository/erase.rs"
    ));
    assert!(!is_repository_code_path(
        "crates/moa-orchestrator/src/services/privacy/repository_helpers.rs"
    ));
    assert!(!is_repository_code_path(
        "crates/moa-orchestrator/src/services/privacy/my_repository/erase.rs"
    ));

    let service_traits = std::collections::BTreeSet::new();
    let mut allowance_uses = vec![0usize; ALLOWANCES.len()];
    let mut repository_findings = Vec::new();
    scan_source(
        "crates/moa-orchestrator/src/services/privacy/repository/erase.rs",
        "let row = sqlx::query(\"SELECT 1\");",
        &service_traits,
        &mut allowance_uses,
        &mut repository_findings,
    );
    assert!(repository_findings.is_empty());

    let mut helper_findings = Vec::new();
    scan_source(
        "crates/moa-orchestrator/src/services/privacy/repository_helpers.rs",
        "let row = sqlx::query(\"SELECT 1\");",
        &service_traits,
        &mut allowance_uses,
        &mut helper_findings,
    );
    assert_eq!(helper_findings.len(), 1);
    assert_eq!(helper_findings[0].rule, Rule::DirectSql);
}

#[test]
fn rejects_same_needle_on_unallowlisted_path() {
    assert_eq!(
        matching_allowance(
            Rule::DirectSql,
            "crates/moa-orchestrator/src/services/new_handler.rs",
            "let rows = sqlx::query(\"SELECT 1\");",
        ),
        None
    );
}

#[test]
fn rejects_upward_dependency_on_orchestrator() {
    // Pins: domain crates cannot depend upward on the Restate adapter boundary.
    let graph = PackageGraph::for_tests(
        &["moa-core", "moa-orchestrator", "moa-providers"],
        &["moa-core", "moa-orchestrator", "moa-providers"],
        &[("moa-providers", "moa-orchestrator")],
    );

    let findings = forbidden_dependency_findings(&graph, FORBIDDEN_DEPENDENCY_RULES);

    assert_eq!(findings.len(), 1, "one upward dependency should fail");
    assert_eq!(findings[0].rule, Rule::ForbiddenDependency);
    assert!(
        findings[0]
            .detail
            .contains("moa-providers -> moa-orchestrator"),
        "finding should name the rejected edge"
    );
}

#[test]
fn connector_and_knowledge_dependency_walls_reject_normal_and_dev_edges() {
    // Pins: every concrete connector/knowledge ownership wall documented in
    // docs/15 is enforced for both production and test-only dependencies.
    let forbidden_edges = [
        ("moa-connectors", "moa-hands"),
        ("moa-connectors", "moa-knowledge"),
        ("moa-connectors", "moa-wire"),
        ("moa-connectors", "moa-edge"),
        ("moa-connectors", "moa-orchestrator"),
        ("moa-knowledge", "moa-connectors"),
        ("moa-knowledge", "moa-artifacts"),
    ];

    for (source, target) in forbidden_edges {
        for (kind, normal_edges, dev_edges) in [
            (DependencyKind::NormalBuild, vec![(source, target)], vec![]),
            (DependencyKind::Dev, vec![], vec![(source, target)]),
        ] {
            let graph = PackageGraph::for_tests_with_kinds(
                &[source, target],
                &[source, target],
                &normal_edges,
                &dev_edges,
            );
            let findings = forbidden_dependency_findings(&graph, FORBIDDEN_DEPENDENCY_RULES);

            assert_eq!(
                findings.len(),
                1,
                "{kind} edge {source} -> {target} must be rejected exactly once"
            );
            assert_eq!(findings[0].rule, Rule::ForbiddenDependency);
            assert!(
                findings[0]
                    .detail
                    .contains(&format!("{source} -> {target}")),
                "finding should name rejected {kind} edge; got {:?}",
                findings[0]
            );
        }
    }
}

#[test]
fn dependency_kind_fixture_separates_production_and_dev_edges() {
    // Pins: production reverse budgets exclude dev-only workspace dependencies.
    let metadata = br#"{
        "packages": [
            {"name":"moa-core","id":"core","dependencies":[]},
            {"name":"moa-brain","id":"brain","dependencies":[{"name":"moa-core","kind":null}]},
            {"name":"moa-edge","id":"edge","dependencies":[{"name":"moa-core","kind":"build"}]},
            {"name":"moa-devtool","id":"devtool","dependencies":[{"name":"moa-core","kind":"dev"}]}
        ],
        "workspace_members":["core","brain","edge","devtool"],
        "workspace_default_members":["core","brain","edge","devtool"]
    }"#;

    let graph = parse_package_graph(metadata).expect("fixture metadata should parse");

    assert_eq!(
        graph.direct_reverse_dependencies("moa-core"),
        ["moa-brain".to_string(), "moa-edge".to_string()].into()
    );
    assert_eq!(
        graph.dev_only_edges(),
        vec![("moa-devtool".to_string(), "moa-core".to_string())]
    );
    assert!(
        graph
            .dependencies(DependencyKind::Dev)
            .get("moa-devtool")
            .is_some_and(|dependencies| dependencies.contains("moa-core"))
    );
}

#[test]
fn test_support_cannot_depend_on_orchestrator_even_for_dev() {
    // Pins: test utilities launch the orchestrator binary without importing the Restate adapter.
    let dev_graph = PackageGraph::for_tests_with_kinds(
        &["moa-test-support", "moa-orchestrator"],
        &["moa-test-support", "moa-orchestrator"],
        &[],
        &[("moa-test-support", "moa-orchestrator")],
    );
    let findings = forbidden_dependency_findings(&dev_graph, FORBIDDEN_DEPENDENCY_RULES);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].detail.contains("dev"));

    let production_graph = PackageGraph::for_tests(
        &["moa-test-support", "moa-orchestrator"],
        &["moa-test-support", "moa-orchestrator"],
        &[("moa-test-support", "moa-orchestrator")],
    );
    let findings = forbidden_dependency_findings(&production_graph, FORBIDDEN_DEPENDENCY_RULES);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].detail.contains("normal/build"));
}

#[test]
fn rejects_moa_core_fan_in_over_budget() {
    // Pins: new direct moa-core reverse dependencies require an intentional budget update.
    let graph = PackageGraph::for_tests(
        &["moa-brain", "moa-core", "moa-session"],
        &["moa-brain", "moa-core", "moa-session"],
        &[("moa-brain", "moa-core"), ("moa-session", "moa-core")],
    );
    let budgets = [ReverseDependencyBudget {
        package: "moa-core",
        max_direct: 1,
        max_transitive: 2,
        reason: "synthetic fan-in budget",
    }];

    let (reports, findings) = reverse_dependency_budget_reports(&graph, &budgets);

    assert_eq!(reports[0].direct_count, 2);
    assert_eq!(reports[0].transitive_count, 2);
    assert_eq!(findings.len(), 1, "direct fan-in over budget should fail");
    assert_eq!(findings[0].rule, Rule::ReverseDependencyBudget);
}

#[test]
fn rejects_moa_core_re_export_budget_growth() {
    // Pins: the moa-core top-level re-export wall cannot grow silently.
    let source = r#"
pub use analytics::{CacheDailyMetric, SessionAnalyticsSummary};
pub use error::MoaError;
"#;
    let count = count_pub_use_exports(source);
    let budget = SymbolBudget {
        label: "synthetic moa-core exports",
        path: "crates/moa-core/src/lib.rs",
        max_count: 2,
        reason: "synthetic re-export budget",
    };

    let finding = symbol_budget_finding(budget, count)
        .expect("three re-exported symbols should exceed a budget of two");

    assert_eq!(count, 3);
    assert_eq!(finding.rule, Rule::SymbolBudget);
    assert!(
        finding.detail.contains("expected at most 2, saw 3"),
        "finding should include exact re-export counts"
    );
}

#[test]
fn moa_core_root_export_allowlist_accepts_only_universal_symbols() {
    // Pins: the final root facade contains exactly the three documented universal symbols.
    let root_source = r#"
pub use error::{MoaError, Result};
pub use workspace::WORKSPACE_ID;
"#;

    assert_eq!(count_moa_core_root_exports(root_source, ""), 3);
    assert!(moa_core_root_export_allowlist_finding(root_source).is_none());
}

#[test]
fn moa_core_root_export_allowlist_rejects_wildcards() {
    // Pins: wildcard exports cannot silently rebuild a flattened facade.
    let root_source = r#"
pub use error::{MoaError, Result};
pub use types::*;
pub use workspace::WORKSPACE_ID;
"#;

    let finding = moa_core_root_export_allowlist_finding(root_source)
        .expect("a root wildcard must violate the exact allowlist");
    assert!(finding.detail.contains("wildcard export"));
}

#[test]
fn moa_core_root_export_allowlist_rejects_same_count_substitution() {
    // Pins: an equal-sized replacement cannot evade the semantic allowlist.
    let root_source = r#"
pub use error::{MoaError, Result};
pub use events::Event;
"#;

    let finding = moa_core_root_export_allowlist_finding(root_source)
        .expect("Event is not a universal root export");
    assert!(finding.detail.contains("Event"));
}

#[test]
fn ordinary_wildcard_export_counts_as_one_without_known_module_expansion() {
    // Pins: semantic expansion is limited to moa-core's known types module.
    assert_eq!(count_pub_use_exports("pub use generated::*;"), 1);
}

#[test]
fn existing_orchestrator_allowances_are_counted_exactly() {
    // Pins: counted orchestrator exceptions consume their allowance and remain stale-proof.
    let index = matching_allowance(
        Rule::DirectSql,
        "crates/moa-orchestrator/src/objects/tenant.rs",
        "sqlx::query_scalar",
    )
    .expect("tenant direct-SQL allowance should exist");
    let mut allowance_uses = vec![0usize; ALLOWANCES.len()];
    let mut findings = Vec::new();
    let service_traits = std::collections::BTreeSet::new();

    scan_source(
        "crates/moa-orchestrator/src/objects/tenant.rs",
        "sqlx::query_scalar(\"SELECT COUNT(*)\")",
        &service_traits,
        &mut allowance_uses,
        &mut findings,
    );

    assert!(
        findings.is_empty(),
        "one exact direct-SQL allowance should not fail"
    );
    assert_eq!(
        allowance_uses[index], 1,
        "tenant direct-SQL allowance count"
    );
}

#[test]
fn exact_count_allowance_rejects_the_next_matching_use() {
    // Pins: a counted exception cannot silently grow within an allowed file.
    let mut allowance_uses = vec![0usize; ALLOWANCES.len()];
    let mut findings = Vec::new();
    let service_traits = std::collections::BTreeSet::new();

    scan_source(
        "crates/moa-orchestrator/src/objects/tenant.rs",
        &"sqlx::query_scalar(\"SELECT COUNT(*)\");\n".repeat(2),
        &service_traits,
        &mut allowance_uses,
        &mut findings,
    );

    assert_eq!(findings.len(), 1);
    assert!(findings[0].detail.contains("expected 1, saw at least 2"));
}

#[test]
fn rejects_wildcard_event_match_arms_in_sensitive_consumers() {
    // Pins: sensitive Event consumers cannot hide new variants behind catch-all previews.
    let source = r#"
fn snippet(event: &Event) -> String {
match event {
    Event::UserMessage { text, .. } => text.clone(),
    other => format!("{other:?}"),
}
}

fn json(value: &Value) -> Value {
match value {
    Value::String(text) => Value::String(text.clone()),
    _ => value.clone(),
}
}
"#;

    let arms = event_wildcard_match_arms(source);

    assert_eq!(arms.len(), 1);
    assert_eq!(arms[0].line, 5);
    assert!(
        arms[0].source.contains("other =>"),
        "finding should point at the wildcard Event arm"
    );
}

#[test]
fn accepts_exhaustive_event_match_arms_in_sensitive_consumers() {
    // Pins: explicit Event variant arms are accepted even when the same file has non-Event wildcards.
    let source = r#"
fn snippet(event: &Event) -> String {
match event {
    Event::UserMessage { text, .. } => text.clone(),
    Event::Warning { message } => message.clone(),
}
}

fn json(value: &Value) -> Value {
match value {
    Value::String(text) => Value::String(text.clone()),
    _ => value.clone(),
}
}
"#;

    assert!(event_wildcard_match_arms(source).is_empty());
}

#[test]
fn restate_handler_without_authz_or_safety_is_flagged() {
    // Pins: a new Restate service handler cannot read or mutate caller-owned data without an explicit authz boundary marker.
    let source = r#"#[restate_sdk::service]
pub trait Example {
async fn read() -> Result<(), HandlerError>;
}
pub struct ExampleImpl;
impl Example for ExampleImpl {
async fn read(&self, _ctx: Context<'_>) -> Result<(), HandlerError> {
    Ok(())
}
}
"#;
    let service_traits = restate_service_traits_from_source(source);
    let findings = handler_authz_safety_findings(
        "crates/moa-orchestrator/src/services/example.rs",
        source,
        &service_traits,
    );

    assert_eq!(
        findings.len(),
        1,
        "missing marker should produce one finding"
    );
    assert_eq!(findings[0].rule, Rule::HandlerAuthzSafety);
    assert_eq!(findings[0].line, Some(7));
}

#[test]
fn restate_handler_with_immediate_safety_comment_is_allowed() {
    // Pins: intentionally internal or informational handlers document why resource authz is not applied.
    let source = r#"#[restate_sdk::service]
pub trait Example {
async fn read() -> Result<(), HandlerError>;
}
pub struct ExampleImpl;
impl Example for ExampleImpl {
#[tracing::instrument(skip(self, _ctx))]
// SAFETY: informational status endpoint with no caller-owned data.
async fn read(&self, _ctx: Context<'_>) -> Result<(), HandlerError> {
    Ok(())
}
}
"#;
    let service_traits = restate_service_traits_from_source(source);
    let findings = handler_authz_safety_findings(
        "crates/moa-orchestrator/src/services/example.rs",
        source,
        &service_traits,
    );

    assert!(findings.is_empty(), "immediate SAFETY marker should pass");
}

#[test]
fn restate_handler_with_multiline_safety_comment_is_allowed() {
    // Pins: a `// SAFETY:` rationale that spans several comment lines above
    // `async fn` is recognized even though a continuation line, not the
    // marker itself, sits directly above the handler signature.
    let source = r#"#[restate_sdk::service]
pub trait Example {
async fn read() -> Result<(), HandlerError>;
}
pub struct ExampleImpl;
impl Example for ExampleImpl {
#[tracing::instrument(skip(self, _ctx))]
// SAFETY: internal teardown dispatched by the owning VO's own cleanup path.
// It reclaims only that owner's own scope and reads no caller-owned data back.
async fn read(&self, _ctx: Context<'_>) -> Result<(), HandlerError> {
    Ok(())
}
}
"#;
    let service_traits = restate_service_traits_from_source(source);
    let findings = handler_authz_safety_findings(
        "crates/moa-orchestrator/src/services/example.rs",
        source,
        &service_traits,
    );

    assert!(
        findings.is_empty(),
        "multi-line SAFETY marker should pass; got {findings:?}"
    );
}

#[test]
fn restate_handler_with_visible_authz_helper_is_allowed() {
    // Pins: local authorization helper calls in handler bodies count as the behavior-boundary check.
    let source = r#"#[restate_sdk::service]
pub trait Example {
async fn read() -> Result<(), HandlerError>;
}
pub struct ExampleImpl;
impl Example for ExampleImpl {
async fn read(&self, ctx: Context<'_>) -> Result<(), HandlerError> {
    authorize_tenant(&ctx).await?;
    Ok(())
}
}
"#;
    let service_traits = restate_service_traits_from_source(source);
    let findings = handler_authz_safety_findings(
        "crates/moa-orchestrator/src/services/example.rs",
        source,
        &service_traits,
    );

    assert!(findings.is_empty(), "visible authz helper should pass");
}

#[test]
fn same_file_wrapper_is_resolved_and_read_rather_than_trusted_by_name() {
    // Pins: a handler delegating to a wrapper defined in the same file passes only
    // when that wrapper's own body checks authz. Accepting any helper whose name
    // merely looks authoritative would turn this rule into a rubber stamp — the
    // second half of this test is the one that matters.
    let checked = r#"#[restate_sdk::service]
pub trait Example {
async fn read() -> Result<(), HandlerError>;
}
pub struct ExampleImpl;
impl Example for ExampleImpl {
async fn read(&self, ctx: Context<'_>) -> Result<(), HandlerError> {
    require_rebuild_authority(&ctx).await?;
    Ok(())
}
}

async fn require_rebuild_authority(ctx: &Context<'_>) -> Result<(), HandlerError> {
require_authz_with_delegation(ctx, ObjectType::Tenant, Relation::Admin).await
}
"#;
    let findings = handler_authz_safety_findings(
        "crates/moa-orchestrator/src/services/example.rs",
        checked,
        &restate_service_traits_from_source(checked),
    );
    assert!(
        findings.is_empty(),
        "a wrapper that really checks authz satisfies its callers; got {findings:?}"
    );

    let unchecked = checked.replace(
        "require_authz_with_delegation(ctx, ObjectType::Tenant, Relation::Admin).await",
        "Ok(())",
    );
    let findings = handler_authz_safety_findings(
        "crates/moa-orchestrator/src/services/example.rs",
        &unchecked,
        &restate_service_traits_from_source(&unchecked),
    );
    assert_eq!(
        findings.len(),
        1,
        "an authoritative-sounding wrapper that checks nothing must not clear its \
         callers; got {findings:?}"
    );
}

#[test]
fn allowlist_reasons_are_not_empty() {
    for allowance in ALLOWANCES {
        assert!(
            !allowance.reason.trim().is_empty(),
            "allowlist entry for {} must carry a reason",
            allowance.path
        );
        assert!(
            allowance.removal_task().is_some(),
            "allowlist entry for {} must name a removal task",
            allowance.path
        );
    }
}

#[test]
fn allowlist_entries_are_unique() {
    let mut seen = std::collections::BTreeMap::new();
    for allowance in ALLOWANCES {
        let key = (allowance.rule, allowance.path, allowance.needle);
        let previous = seen.insert(key, allowance.expected_count);
        assert!(
            previous.is_none(),
            "duplicate allowlist entry for {} / {}",
            allowance.path,
            allowance.needle
        );
    }
}
