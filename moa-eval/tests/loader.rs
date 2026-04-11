//! Integration tests for the MOA eval crate scaffold.

use std::path::Path;

use moa_eval::{
    AgentConfig, EvalMetrics, EvalResult, EvalStatus, TestCase, TestSuite, discover_configs,
    discover_suites, load_agent_config, load_suite,
};

#[test]
fn parse_example_suite() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example-suite.toml");
    let suite = load_suite(&path).expect("example suite should parse");
    assert!(!suite.name.is_empty());
    assert!(!suite.cases.is_empty());
    for case in &suite.cases {
        assert!(!case.name.is_empty());
        assert!(!case.input.is_empty());
    }
}

#[test]
fn parse_example_configs() {
    let base_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example-config-baseline.toml");
    let variant_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/example-config-variant.toml");
    let baseline = load_agent_config(&base_path).expect("baseline config should parse");
    let variant = load_agent_config(&variant_path).expect("variant config should parse");
    assert_ne!(baseline.name, variant.name);
}

#[test]
fn minimal_config_has_defaults() {
    let toml = r#"
        [agent]
        name = "minimal"
    "#;
    let config: AgentConfig = toml::from_str(toml).expect("minimal config should parse");
    assert_eq!(config.name, "minimal");
    assert!(config.model.is_none());
    assert!(!config.permissions.auto_approve_all);
    assert!(config.skills.include.is_empty());
    assert!(!config.skills.exclusive);
}

#[test]
fn suite_serialization_roundtrip() {
    let suite = TestSuite {
        name: "test".into(),
        cases: vec![TestCase {
            name: "case1".into(),
            input: "Hello".into(),
            expected_trajectory: Some(vec!["bash".into()]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let toml_str = toml::to_string_pretty(&suite).expect("suite should serialize");
    let parsed: TestSuite = toml::from_str(&toml_str).expect("suite should deserialize");
    assert_eq!(suite.name, parsed.name);
    assert_eq!(suite.cases.len(), parsed.cases.len());
}

#[test]
fn agent_config_serialization_roundtrip() {
    let config = AgentConfig {
        name: "baseline".into(),
        model: Some("claude-sonnet-4-20250514".into()),
        ..Default::default()
    };
    let toml_str = toml::to_string_pretty(&config).expect("config should serialize");
    let parsed: AgentConfig = toml::from_str(&toml_str).expect("config should deserialize");
    assert_eq!(config.name, parsed.name);
    assert_eq!(config.model, parsed.model);
}

#[test]
fn eval_result_captures_metrics() {
    let result = EvalResult {
        test_case: "test".into(),
        agent_config: "baseline".into(),
        status: EvalStatus::Passed,
        metrics: EvalMetrics {
            total_tokens: 1500,
            cost_dollars: 0.012,
            latency_ms: 3200,
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(result.metrics.total_tokens, 1500);
}

#[test]
fn discover_suites_finds_toml_files() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let paths = discover_suites(&dir).expect("example directory should be readable");
    assert!(!paths.is_empty());
    assert!(
        paths
            .iter()
            .all(|path| path.extension().is_some_and(|ext| ext == "toml"))
    );
}

#[test]
fn discover_configs_finds_toml_files() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let paths = discover_configs(&dir).expect("example directory should be readable");
    assert!(!paths.is_empty());
    assert!(
        paths
            .iter()
            .all(|path| path.extension().is_some_and(|ext| ext == "toml"))
    );
}
