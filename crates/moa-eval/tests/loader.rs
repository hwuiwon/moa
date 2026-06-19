//! Integration tests for the MOA eval crate scaffold.

use std::path::Path;

use moa_eval_core::{Result, discover_configs, discover_suites, load_agent_config, load_suite};

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
fn discover_eval_inputs_finds_toml_files() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    for (label, discover) in [
        (
            "suite",
            discover_suites as fn(&Path) -> Result<Vec<std::path::PathBuf>>,
        ),
        (
            "config",
            discover_configs as fn(&Path) -> Result<Vec<std::path::PathBuf>>,
        ),
    ] {
        let paths = discover(&dir).unwrap_or_else(|error| {
            panic!("example directory should be readable while discovering {label}s: {error}")
        });
        assert!(!paths.is_empty(), "expected at least one {label} TOML file");
        assert!(
            paths
                .iter()
                .all(|path| path.extension().is_some_and(|ext| ext == "toml")),
            "{label} discovery returned a non-TOML path: {paths:?}"
        );
    }
}
