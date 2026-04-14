//! Integration test for the data-layer guarantees the settings panel and
//! window-state persistence rely on.
//!
//! These tests intentionally avoid spinning up GPUI or any of the MOA
//! service bridge — they exercise the pure save/load contracts so that a
//! regression in `MoaConfig::save_to_path` or JSON window-state handling
//! would surface here rather than as a silent UX failure at runtime.

use std::fs;
use std::path::PathBuf;

use moa_core::MoaConfig;

/// Creates (and returns) a unique temp subdirectory for each test. Using
/// the process id + a unique counter keeps parallel cargo test runs from
/// clobbering each other without pulling in a new dependency.
fn scratch_dir(label: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "moa-desktop-test-{}-{}-{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&base).expect("mkdir tempdir");
    base
}

/// Changes the settings panel makes (theme switch, default provider change,
/// new auto-approve tool) must round-trip through the config file so that
/// restarting the app sees the new values.
#[test]
fn settings_mutations_round_trip_through_config_file() {
    let tmp = scratch_dir("round-trip");
    let path = tmp.join("config.toml");

    let mut config = MoaConfig::default();
    // Simulate: user toggles theme via General tab.
    config.tui.theme = "light".to_string();
    // Simulate: user switches default provider.
    config.general.default_provider = "anthropic".to_string();
    config.general.default_model = "claude-sonnet-4-6".to_string();
    // Simulate: user adds an auto-approve tool and a deny tool.
    config
        .permissions
        .auto_approve
        .push("file_read".to_string());
    config
        .permissions
        .always_deny
        .push("shell_execute".to_string());
    // Simulate: user switches posture to auto.
    config.permissions.default_posture = "auto".to_string();

    config.save_to_path(&path).expect("save");
    assert!(path.exists());
    let written = fs::read_to_string(&path).expect("read");
    // Sanity-check that the file actually contains user intent (format
    // stability test — not a content snapshot).
    assert!(written.contains("light"));
    assert!(written.contains("anthropic"));
    assert!(written.contains("file_read"));

    let loaded = MoaConfig::load_from_path(&path).expect("load");
    assert_eq!(loaded.tui.theme, "light");
    assert_eq!(loaded.general.default_provider, "anthropic");
    assert_eq!(loaded.general.default_model, "claude-sonnet-4-6");
    assert_eq!(loaded.permissions.default_posture, "auto");
    assert!(
        loaded
            .permissions
            .auto_approve
            .contains(&"file_read".to_string())
    );
    assert!(
        loaded
            .permissions
            .always_deny
            .contains(&"shell_execute".to_string())
    );
}

/// Removing an auto-approve tool (user clicks the "×" on a chip) must
/// persist across restarts too.
#[test]
fn removing_from_auto_approve_persists() {
    let tmp = scratch_dir("remove-tool");
    let path = tmp.join("config.toml");

    let mut config = MoaConfig::default();
    config.permissions.auto_approve = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    config.save_to_path(&path).expect("save");

    // Simulate the UI removing "b".
    let mut loaded = MoaConfig::load_from_path(&path).expect("load");
    loaded.permissions.auto_approve.retain(|n| n != "b");
    loaded.save_to_path(&path).expect("save after remove");

    let final_state = MoaConfig::load_from_path(&path).expect("reload");
    assert_eq!(
        final_state.permissions.auto_approve,
        vec!["a".to_string(), "c".to_string()]
    );
}
