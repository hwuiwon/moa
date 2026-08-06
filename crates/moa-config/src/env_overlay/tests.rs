//! Behavior tests for the flat `MOA_*` environment overlay.

use super::*;
use crate::TurbopufferVectorType;

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_string()).collect()
}

/// Regeneration tool for `docs/23-environment-variables.md`.
///
/// Dumps every overlay variable with its config path and default, derived
/// from `EnvOverlay` (serde field enumeration) and `MoaConfig::default()`,
/// so the reference doc is never hand-transcribed. Run with:
/// `cargo test -p moa-config dump_env_var_reference -- --ignored --nocapture`.
#[test]
#[ignore = "dev tool: regenerates the env-var reference doc table"]
fn dump_env_var_reference() {
    let mut schema = serde_json::to_value(MoaConfig::default()).expect("serialize config");
    seed_optional_sections(&mut schema).expect("seed optional sections");
    let overlay = serde_json::to_value(EnvOverlay::default()).expect("serialize overlay");
    let Value::Object(fields) = overlay else {
        panic!("overlay must serialize to an object");
    };

    let mut rows = Vec::new();
    for field in fields.keys() {
        let env = format!("MOA_{}", field.to_uppercase());
        let path = overlay_path(field, &schema).expect("resolve overlay path");
        let default = walk_default(&schema, &path);
        // Secret = actual credential material. Suffix-matched so identifiers
        // and counters that merely contain KEY/TOKEN (`_KEY_ID`, `_TOKENS`,
        // `_TOKEN_ESTIMATES`, `_TTL_SECONDS`) are NOT flagged, while real
        // keys/secrets/passwords/private-key and hex key material are.
        let secret = env.ends_with("_KEY")
            || env.ends_with("_KEY_HEX")
            || env.ends_with("_SECRET")
            || env.ends_with("_SECRET_HEX")
            || env.ends_with("_PASSWORD")
            || env.ends_with("_PRIVATE_KEY_PEM")
            || env.ends_with("_AUTH_TOKEN")
            || env.ends_with("_APP_TOKEN")
            || env.ends_with("_BOT_TOKEN");
        rows.push((path[0].clone(), env, path.join("."), default, secret));
    }
    rows.sort();
    println!("SECTION\tENV_VAR\tCONFIG_PATH\tDEFAULT\tSECRET");
    for (section, env, path, default, secret) in rows {
        println!("{section}\t{env}\t{path}\t{default}\t{secret}");
    }
}

fn walk_default(root: &Value, path: &[String]) -> String {
    let mut cursor = root;
    for segment in path {
        match cursor.get(segment) {
            Some(next) => cursor = next,
            None => return "(unset)".to_string(),
        }
    }
    match cursor {
        Value::Null => "(none)".to_string(),
        Value::String(text) if text.is_empty() => "(empty)".to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[test]
fn registry_flags_unknown_overlay_typo() {
    // Pins: a misspelled overlay var (which envy silently ignores) is detected.
    let unknown = unknown_moa_env_vars(names(&["MOA_MODELS_MIAN"]));
    assert_eq!(unknown, vec!["MOA_MODELS_MIAN".to_string()]);
}

#[test]
fn registry_accepts_known_field_and_allowlisted_specials() {
    // Pins: a real overlay field, a prefix-allowlisted gate, and an exact
    // allowlisted special key are all recognized (no false positives).
    let clean = unknown_moa_env_vars(names(&[
        "MOA_MODELS_MAIN",                // real overlay field
        "MOA_RUN_LIVE_COHERE_TESTS",      // MOA_RUN_ prefix allowlist
        "MOA_SKIP_FGA",                   // exact allowlist
        "MOA_CONFIG_ENV_STRICT",          // this audit's own switch
        "MOA_AUTH_CONTACT_TOKENS_ISSUER", // real overlay field
        "MOA_MCP_SERVERS_JSON",           // JSON MCP server overlay
    ]));
    assert!(clean.is_empty(), "expected no unknown vars, got {clean:?}");
}

#[test]
fn registry_ignores_non_moa_and_lowercase_prefix_boundary() {
    // Pins: only `MOA_`-prefixed names are considered; `MOALITE` and unrelated
    // vars are left alone.
    let unknown = unknown_moa_env_vars(names(&["PATH", "HOME", "MOALITE", "AWS_MOA_X"]));
    assert!(
        unknown.is_empty(),
        "non-MOA_ vars must be ignored, got {unknown:?}"
    );
}

#[test]
fn strict_mode_errors_and_lists_every_unknown() {
    // Pins: strict mode fails startup and names every offending variable.
    let error = EnvOverlay::audit_env_registry(
        names(&["MOA_MODELS_MIAN", "MOA_TOTALLY_MADE_UP", "MOA_MODELS_MAIN"]),
        true,
    )
    .expect_err("strict mode must reject unknown vars");
    let rendered = error.to_string();
    assert!(rendered.contains("MOA_MODELS_MIAN"), "got: {rendered}");
    assert!(rendered.contains("MOA_TOTALLY_MADE_UP"), "got: {rendered}");
    assert!(
        !rendered.contains("MOA_MODELS_MAIN,"),
        "known field must not be listed: {rendered}"
    );
}

#[test]
fn warn_mode_is_ok_for_unknown_vars() {
    // Pins: non-strict mode tolerates unknown vars (returns Ok, logs a warning).
    EnvOverlay::audit_env_registry(names(&["MOA_MODELS_MIAN"]), false)
        .expect("warn mode returns ok");
}

#[test]
fn strict_mode_suggests_the_nearest_known_key() {
    // Pins: a near-miss carries a "did you mean" suggestion for the real field.
    let error = EnvOverlay::audit_env_registry(names(&["MOA_MODELS_MIAN"]), true)
        .expect_err("near-miss should error in strict mode");
    assert!(
        error.to_string().contains("did you mean MOA_MODELS_MAIN"),
        "expected a suggestion, got: {error}"
    );
}

#[test]
fn security_profile_env_selects_the_deployment_posture_and_defaults_local() {
    // Pins: MOA_SECURITY_PROFILE is the single top-level posture switch, it
    // deserializes the snake_case wire names, and an unset overlay leaves the
    // fail-safe Local default in place rather than inferring cloud from other keys.
    let default_config = MoaConfig::default();
    assert_eq!(default_config.security_profile, SecurityProfile::Local);

    for (value, expected) in [
        ("cloud", SecurityProfile::Cloud),
        ("local", SecurityProfile::Local),
    ] {
        let overlay = EnvOverlay::from_iter(env_pairs([
            ("MOA_DATABASE_URL", "postgres://moa:test@db.example/moa"),
            ("MOA_SECURITY_PROFILE", value),
        ]))
        .expect("security profile overlay parses");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("security profile overlay applies");

        assert_eq!(
            config.security_profile, expected,
            "MOA_SECURITY_PROFILE={value}"
        );
    }

    let unset = EnvOverlay::from_iter(env_pairs([(
        "MOA_DATABASE_URL",
        "postgres://moa:test@db.example/moa",
    )]))
    .expect("overlay without the profile key parses");
    let mut config = MoaConfig::default();
    unset.apply_to(&mut config).expect("overlay applies");
    assert_eq!(config.security_profile, SecurityProfile::Local);
}

#[test]
fn unknown_security_profile_value_is_rejected_with_the_offending_value() {
    // Pins: a typo in the deployment posture fails startup instead of silently
    // falling back to the permissive Local default.
    assert_config_error_contains(
        EnvOverlay::from_iter(env_pairs([("MOA_SECURITY_PROFILE", "clould")])),
        "clould",
    );
}

#[test]
fn every_flat_overlay_field_resolves_to_a_config_path() {
    // Pins: adding a flat MOA_* overlay field requires either a matching
    // serialized MoaConfig path or a deliberate alias entry.
    let mut schema =
        serde_json::to_value(MoaConfig::default()).expect("default config should serialize");
    seed_optional_sections(&mut schema).expect("schema seeds should apply");
    let Value::Object(fields) =
        serde_json::to_value(EnvOverlay::default()).expect("default overlay should serialize")
    else {
        panic!("overlay should serialize as an object");
    };

    let unresolved = fields
        .keys()
        .filter_map(|field| {
            overlay_path(field, &schema)
                .err()
                .map(|error| format!("{field}: {error}"))
        })
        .collect::<Vec<_>>();

    assert!(
        unresolved.is_empty(),
        "unmapped overlay fields:\n{}",
        unresolved.join("\n")
    );
}

#[test]
fn from_iter_applies_flat_single_underscore_env() {
    // Pins: flat MOA env names deserialize through envy and update real nested config fields.
    let approval_key_hex = "01".repeat(32);
    let export_key_hex = "02".repeat(32);
    let lineage_key_hex = "03".repeat(32);
    let pii_vault_secret_hex = "04".repeat(32);
    let overlay = EnvOverlay::from_iter(env_pairs([
        ("MOA_DATABASE_URL", "postgres://moa:test@db.example/moa"),
        ("MOA_DATABASE_MAX_CONNECTIONS", "42"),
        ("MOA_DATABASE_BACKGROUND_MAX_CONNECTIONS", "3"),
        ("MOA_SESSION_DIRECT_TURN_EVENT_APPEND", "true"),
        ("MOA_AUTH_PROVIDER", "oidc"),
        ("MOA_AUTHZ_ENGINE", "openfga"),
        ("MOA_AUTHZ_OPENFGA_URL", "http://openfga.example"),
        ("MOA_AUTHZ_OPENFGA_PRESHARED_KEY", "shared-key"),
        ("MOA_AUTHZ_OPENFGA_STORE_ID", "store-1"),
        ("MOA_AUTHZ_OPENFGA_MODEL_ID", "model-1"),
        ("MOA_AUTHZ_OPENFGA_TIMEOUT_MS", "2500"),
        ("MOA_KMS_PROVIDER", "postgres"),
        ("MOA_KMS_ROOT_KEY_DIR", "/var/run/secrets/test-root-keys"),
        ("MOA_KMS_REQUIRED_GENERATION", "generation-2"),
        ("MOA_ASYNC_AUTHZ_PROVIDER", "auth0"),
        ("MOA_ASYNC_AUTHZ_DEFAULT_TIMEOUT_SECS", "120"),
        ("MOA_AUDIT_SECURITY_EMIT_AUTHZ_ALLOWS", "true"),
        (
            "MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX",
            approval_key_hex.as_str(),
        ),
        (
            "MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX",
            export_key_hex.as_str(),
        ),
        ("MOA_PRIVACY_EXPORT_SIGNING_KEY_ID", "privacy-key-v2"),
        (
            "MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX",
            lineage_key_hex.as_str(),
        ),
        ("MOA_LINEAGE_AUDIT_SIGNING_KEY_ID", "lineage-key-v2"),
        ("MOA_PII_VAULT_SECRET_HEX", pii_vault_secret_hex.as_str()),
        ("MOA_LOCAL_DOCKER_ENABLED", "false"),
        ("MOA_LOCAL_SANDBOX_DIR", "/tmp/moa-sandbox"),
        ("MOA_PII_SERVICE_URL", "http://pii.example:8080"),
        ("MOA_MEMORY_EMBEDDING_MODEL", "cohere:embed-v4.0"),
        (
            "MOA_MEMORY_RETRIEVAL_RERANKER_MODEL",
            "zeroentropy:zerank-2",
        ),
        ("MOA_MEMORY_RETRIEVAL_RERANKER_LATENCY", "fast"),
        ("MOA_MEMORY_RETRIEVAL_LINEAGE_ENABLED", "true"),
        ("MOA_MEMORY_DIGEST_ENABLED", "true"),
        ("MOA_MEMORY_DIGEST_MAX_TOKENS", "384"),
        ("MOA_MEMORY_DIGEST_REBUILD_MIN_INTERVAL_HOURS", "12"),
        (
            "MOA_MEMORY_VECTOR_EMBEDDER_NAME",
            "gemini:gemini-embedding-2",
        ),
        ("MOA_MEMORY_VECTOR_EMBEDDER_OUTPUT_DIM", "1536"),
        ("MOA_COHERE_API_KEY", "CUSTOM_COHERE_KEY"),
        ("MOA_GOOGLE_API_KEY", "CUSTOM_GOOGLE_KEY"),
        ("MOA_ZEROENTROPY_API_KEY", "CUSTOM_ZEROENTROPY_KEY"),
        ("MOA_TURBOPUFFER_API_KEY", "CUSTOM_TURBOPUFFER_KEY"),
        ("MOA_TURBOPUFFER_BASE_URL", "https://tpuf.example"),
        ("MOA_TURBOPUFFER_ENVIRONMENT", "prod"),
        ("MOA_TURBOPUFFER_BAA", "true"),
        ("MOA_TURBOPUFFER_VECTOR_TYPE", "f32"),
        ("MOA_MESSAGING_SLACK_TOKEN", "CUSTOM_SLACK_BOT_TOKEN"),
        ("MOA_MESSAGING_SLACK_APP_TOKEN", "CUSTOM_SLACK_APP_TOKEN"),
        (
            "MOA_MESSAGING_POSTMARK_BASE_URL",
            "https://postmark.example",
        ),
        ("MOA_MESSAGING_POSTMARK_MESSAGE_STREAM", "alerts"),
        ("MOA_MESSAGING_EMAIL_FROM", "MOA <moa@example.com>"),
        ("MOA_MESSAGING_EMAIL_REPLY_TO", "support@example.com"),
        ("MOA_MESSAGING_TWILIO_BASE_URL", "https://twilio.example"),
        ("MOA_OPENAI_API_KEY", "CUSTOM_OPENAI_KEY"),
        ("MOA_RESTATE_INGRESS_URL", "http://restate.example:8080"),
        (
            "MOA_RESTATE_LLM_GATEWAY_URL",
            "http://llm-gateway.example:10020",
        ),
        (
            "MOA_OBSERVABILITY_OTLP_ENDPOINT",
            "http://otel.example:4317",
        ),
        ("MOA_OBSERVABILITY_OTLP_PROTOCOL", "http"),
        (
            "MOA_OBSERVABILITY_OTLP_HEADERS",
            "tenant=moa,token=redacted",
        ),
        ("MOA_METRICS_EXPORTER", "prometheus"),
        ("MOA_METRICS_PROMETHEUS_LISTEN", "127.0.0.1:9091"),
        ("MOA_PERMISSIONS_ADMIN_REVIEW", "bash,file_write"),
        ("MOA_PERMISSIONS_DEFAULT_EFFECT", "admin_review"),
    ]))
    .expect("overlay should deserialize");

    let mut config = MoaConfig::default();
    overlay.apply_to(&mut config).expect("overlay should apply");

    assert_eq!(config.database.url, "postgres://moa:test@db.example/moa");
    assert_eq!(config.database.max_connections, 42);
    assert_eq!(config.database.background_max_connections, 3);
    assert!(config.session.direct_turn_event_append);
    assert_eq!(config.auth.provider, AuthProviderKind::Oidc);
    assert_eq!(config.authz.engine, AuthzEngine::Openfga);
    let openfga = config.authz.openfga.expect("openfga config");
    assert_eq!(openfga.url, "http://openfga.example");
    assert_eq!(openfga.preshared_key, "shared-key");
    assert_eq!(openfga.store_id, "store-1");
    assert_eq!(openfga.model_id, "model-1");
    assert_eq!(openfga.timeout_ms, 2500);
    assert_eq!(config.kms.provider, KmsProviderKind::Postgres);
    assert_eq!(
        config.kms.root_key_dir,
        PathBuf::from("/var/run/secrets/test-root-keys")
    );
    assert_eq!(config.kms.required_generation, "generation-2");
    assert_eq!(config.async_authz.provider, AsyncAuthzKind::Auth0);
    assert_eq!(config.async_authz.default_timeout_secs, 120);
    assert!(config.audit_security.emit_authz_allows);
    assert_eq!(
        config.compliance.privacy_approval_public_key_hex.as_deref(),
        Some(approval_key_hex.as_str())
    );
    assert_eq!(
        config.compliance.privacy_export_signing_key_hex.as_deref(),
        Some(export_key_hex.as_str())
    );
    assert_eq!(
        config.compliance.privacy_export_signing_key_id,
        "privacy-key-v2"
    );
    assert_eq!(
        config.compliance.lineage_audit_signing_key_hex.as_deref(),
        Some(lineage_key_hex.as_str())
    );
    assert_eq!(
        config.compliance.lineage_audit_signing_key_id,
        "lineage-key-v2"
    );
    assert_eq!(
        config.compliance.pii_vault_secret_hex.as_deref(),
        Some(pii_vault_secret_hex.as_str())
    );
    assert!(!config.local.docker_enabled);
    assert_eq!(config.local.sandbox_dir, "/tmp/moa-sandbox");
    assert_eq!(
        config.memory.pii_service_url.as_deref(),
        Some("http://pii.example:8080")
    );
    assert_eq!(config.memory.embedding_model, "cohere:embed-v4.0");
    assert_eq!(
        config.memory.retrieval.reranker_model,
        "zeroentropy:zerank-2"
    );
    assert_eq!(
        config.memory.retrieval.reranker_latency.as_deref(),
        Some("fast")
    );
    assert!(config.memory.retrieval.lineage_enabled);
    assert!(config.memory.digest.enabled);
    assert_eq!(config.memory.digest.max_tokens, 384);
    assert_eq!(config.memory.digest.rebuild_min_interval_hours, 12);
    assert_eq!(
        config.memory.vector.embedder.name,
        "gemini:gemini-embedding-2"
    );
    assert_eq!(
        config.memory.vector.turbopuffer.api_key,
        "CUSTOM_TURBOPUFFER_KEY"
    );
    assert_eq!(config.memory.vector.embedder.output_dim, 1536);
    assert_eq!(config.providers.cohere.api_key, "CUSTOM_COHERE_KEY");
    assert_eq!(config.providers.google.api_key, "CUSTOM_GOOGLE_KEY");
    assert_eq!(
        config.providers.zeroentropy.api_key,
        "CUSTOM_ZEROENTROPY_KEY"
    );
    assert_eq!(
        config.memory.vector.turbopuffer.base_url.as_deref(),
        Some("https://tpuf.example")
    );
    assert_eq!(
        config.memory.vector.turbopuffer.environment.as_deref(),
        Some("prod")
    );
    assert!(config.memory.vector.turbopuffer.baa_enabled);
    assert_eq!(
        config.memory.vector.turbopuffer.vector_type,
        TurbopufferVectorType::F32
    );
    assert_eq!(config.messaging.slack_token, "CUSTOM_SLACK_BOT_TOKEN");
    assert_eq!(config.messaging.slack_app_token, "CUSTOM_SLACK_APP_TOKEN");
    assert_eq!(
        config.messaging.postmark_base_url,
        "https://postmark.example"
    );
    assert_eq!(config.messaging.postmark_message_stream, "alerts");
    assert_eq!(config.messaging.email_from, "MOA <moa@example.com>");
    assert_eq!(
        config.messaging.email_reply_to.as_deref(),
        Some("support@example.com")
    );
    assert_eq!(config.messaging.twilio_base_url, "https://twilio.example");
    assert_eq!(config.providers.openai.api_key, "CUSTOM_OPENAI_KEY");
    assert_eq!(
        config.orchestrator.endpoint.as_deref(),
        Some("http://restate.example:8080")
    );
    assert_eq!(
        config.orchestrator.restate_ingress_url.as_deref(),
        Some("http://restate.example:8080")
    );
    assert_eq!(
        config.orchestrator.llm_gateway_url.as_deref(),
        Some("http://llm-gateway.example:10020")
    );
    assert_eq!(
        config.observability.otlp_endpoint.as_deref(),
        Some("http://otel.example:4317")
    );
    assert_eq!(config.observability.otlp_protocol, OtlpProtocol::Http);
    assert_eq!(
        config
            .observability
            .otlp_headers
            .get("tenant")
            .map(String::as_str),
        Some("moa")
    );
    assert_eq!(
        config
            .observability
            .otlp_headers
            .get("token")
            .map(String::as_str),
        Some("redacted")
    );
    assert_eq!(
        config.metrics.exporter,
        moa_core_metrics_exporter_prometheus()
    );
    assert_eq!(
        config.metrics.prometheus_listen.as_deref(),
        Some("127.0.0.1:9091")
    );
    assert_eq!(config.permissions.admin_review, ["bash", "file_write"]);
    assert_eq!(
        config.permissions.default_effect,
        moa_core::types::action_policy::ActionPolicyEffect::AdminReview
    );
}

#[test]
fn from_iter_applies_every_execution_resource_override() {
    // Pins: every execution default has exactly one flat MOA_EXECUTION_* override.
    let overlay = EnvOverlay::from_iter(env_pairs([
        ("MOA_EXECUTION_PLANNER_REPAIR_ATTEMPTS", "2"),
        ("MOA_EXECUTION_REPEATED_FAILURE_LIMIT", "4"),
        ("MOA_EXECUTION_MAX_TASKS", "20000"),
        ("MOA_EXECUTION_MAX_TOKENS", "20000000"),
        ("MOA_EXECUTION_MAX_TOOL_CALLS", "200000"),
        ("MOA_EXECUTION_MAX_RETRIEVED_BYTES", "20000000000"),
        ("MOA_EXECUTION_MAX_COST_MICROUSD", "200000000"),
        ("MOA_EXECUTION_UNATTENDED_MAX_COST_MICROUSD", "6000000"),
        ("MOA_EXECUTION_AGENT_TURN_COST_MICROUSD", "110000"),
        ("MOA_EXECUTION_AGENT_TURN_TOKENS", "9000"),
        ("MOA_EXECUTION_AGENT_TURN_TOOL_CALLS", "9"),
        ("MOA_EXECUTION_AGENT_TURN_RETRIEVED_BYTES", "11000000"),
        ("MOA_EXECUTION_VERIFIER_TURN_COST_MICROUSD", "210000"),
        ("MOA_EXECUTION_VERIFIER_TURN_TOKENS", "17000"),
        ("MOA_EXECUTION_VERIFIER_TURN_TOOL_CALLS", "5"),
        ("MOA_EXECUTION_VERIFIER_TURN_RETRIEVED_BYTES", "2000000"),
    ]))
    .expect("execution overlay should deserialize");

    let mut config = MoaConfig::default();
    overlay
        .apply_to(&mut config)
        .expect("execution overlay should apply");

    assert_eq!(config.execution.planner_repair_attempts, 2);
    assert_eq!(config.execution.repeated_failure_limit, 4);
    assert_eq!(config.execution.max_tasks, 20_000);
    assert_eq!(config.execution.max_tokens, 20_000_000);
    assert_eq!(config.execution.max_tool_calls, 200_000);
    assert_eq!(config.execution.max_retrieved_bytes, 20_000_000_000);
    assert_eq!(config.execution.max_cost_microusd, 200_000_000);
    assert_eq!(config.execution.unattended_max_cost_microusd, 6_000_000);
    assert_eq!(config.execution.agent_turn_cost_microusd, 110_000);
    assert_eq!(config.execution.agent_turn_tokens, 9_000);
    assert_eq!(config.execution.agent_turn_tool_calls, 9);
    assert_eq!(config.execution.agent_turn_retrieved_bytes, 11_000_000);
    assert_eq!(config.execution.verifier_turn_cost_microusd, 210_000);
    assert_eq!(config.execution.verifier_turn_tokens, 17_000);
    assert_eq!(config.execution.verifier_turn_tool_calls, 5);
    assert_eq!(config.execution.verifier_turn_retrieved_bytes, 2_000_000);
}

#[test]
fn from_iter_rejects_invalid_values_for_every_execution_override() {
    // Pins: every MOA_EXECUTION_* numeric field fails explicitly with its canonical env name.
    for name in [
        "MOA_EXECUTION_PLANNER_REPAIR_ATTEMPTS",
        "MOA_EXECUTION_REPEATED_FAILURE_LIMIT",
        "MOA_EXECUTION_MAX_TASKS",
        "MOA_EXECUTION_MAX_TOKENS",
        "MOA_EXECUTION_MAX_TOOL_CALLS",
        "MOA_EXECUTION_MAX_RETRIEVED_BYTES",
        "MOA_EXECUTION_MAX_COST_MICROUSD",
        "MOA_EXECUTION_UNATTENDED_MAX_COST_MICROUSD",
        "MOA_EXECUTION_AGENT_TURN_COST_MICROUSD",
        "MOA_EXECUTION_AGENT_TURN_TOKENS",
        "MOA_EXECUTION_AGENT_TURN_TOOL_CALLS",
        "MOA_EXECUTION_AGENT_TURN_RETRIEVED_BYTES",
        "MOA_EXECUTION_VERIFIER_TURN_COST_MICROUSD",
        "MOA_EXECUTION_VERIFIER_TURN_TOKENS",
        "MOA_EXECUTION_VERIFIER_TURN_TOOL_CALLS",
        "MOA_EXECUTION_VERIFIER_TURN_RETRIEVED_BYTES",
    ] {
        assert_config_error_contains(
            EnvOverlay::from_iter(env_pairs([(name, "not-an-integer")])),
            name,
        );
    }
}

#[test]
fn mcp_servers_json_replaces_configured_servers() {
    // Pins: the production env seam accepts the complete typed MCP server array and replaces,
    // rather than appends to, file-backed server configuration.
    let overlay = EnvOverlay::from_iter(env_pairs([(
        "MOA_MCP_SERVERS_JSON",
        r#"[{"name":"fixture","url":"http://127.0.0.1:4321","trust_tool_annotations":true}]"#,
    )]))
    .expect("MCP JSON overlay should deserialize");
    let mut config = MoaConfig::default();
    config.mcp_servers.push(crate::McpServerConfig {
        required: false,
        discovery: crate::McpDiscoveryMode::Eager,
        name: "file-backed".to_string(),
        url: "http://127.0.0.1:1".to_string(),
        credentials: None,
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
    });

    overlay
        .apply_to(&mut config)
        .expect("MCP JSON overlay should apply");

    assert_eq!(config.mcp_servers.len(), 1);
    assert_eq!(config.mcp_servers[0].name, "fixture");
    assert_eq!(config.mcp_servers[0].url, "http://127.0.0.1:4321");
    assert!(config.mcp_servers[0].trust_tool_annotations);
}

#[test]
fn mcp_servers_json_rejects_malformed_json_through_config_error() {
    // Pins: malformed production MCP JSON fails startup through the ordinary typed config path.
    assert_config_error_contains(
        EnvOverlay::from_iter(env_pairs([("MOA_MCP_SERVERS_JSON", "[{not-json]")])),
        "MOA_MCP_SERVERS_JSON",
    );
}

#[test]
fn mcp_servers_json_rejects_unknown_server_fields() {
    // Pins: retired MCP configuration vocabulary is rejected instead of being
    // silently ignored and leaving an operator with a different security model.
    assert_config_error_contains(
        EnvOverlay::from_iter(env_pairs([(
            "MOA_MCP_SERVERS_JSON",
            r#"[{"name":"fixture","url":"http://127.0.0.1:4321","credential_scope":"deployment_owned"}]"#,
        )])),
        "unknown field",
    );
}

#[test]
fn mcp_servers_json_rejects_retired_oauth_credential_type() {
    // Pins: MCP bearer authentication has one configuration spelling; the
    // behavior-identical OAuth alias is not silently accepted.
    assert_config_error_contains(
        EnvOverlay::from_iter(env_pairs([(
            "MOA_MCP_SERVERS_JSON",
            r#"[{"name":"fixture","url":"http://127.0.0.1:4321","credentials":{"type":"oauth","token_env":"MCP_TOKEN"}}]"#,
        )])),
        "unknown variant",
    );
}

#[test]
fn mcp_servers_json_rejects_retired_transport_field() {
    // Pins: the remote MCP client has one content-type-aware HTTP path, so a
    // no-op transport selector is rejected instead of accepted and ignored.
    assert_config_error_contains(
        EnvOverlay::from_iter(env_pairs([(
            "MOA_MCP_SERVERS_JSON",
            r#"[{"name":"fixture","url":"http://127.0.0.1:4321","transport":"http"}]"#,
        )])),
        "unknown field",
    );
}

#[test]
fn mcp_servers_json_requires_server_url() {
    // Pins: every configured remote MCP server has an endpoint; omission is a
    // typed configuration error rather than a deferred router failure.
    assert_config_error_contains(
        EnvOverlay::from_iter(env_pairs([(
            "MOA_MCP_SERVERS_JSON",
            r#"[{"name":"fixture"}]"#,
        )])),
        "missing field",
    );
}

#[test]
fn provider_coordination_env_overlay_reaches_every_pacing_knob() {
    // Pins: the fleet-coordination knobs are settable the way a deployment
    // actually sets them (flat MOA_* environment variables), and each one lands
    // on its own config field. A wrong path array here is silent in production:
    // the variable is accepted and then ignored.
    let mut config = MoaConfig::default();
    EnvOverlay::from_iter(env_pairs([
        (
            "MOA_PROVIDERS_CONCURRENCY_ON_COORDINATION_FAILURE",
            "fail_closed",
        ),
        ("MOA_PROVIDERS_PACING_SCOPE", "global"),
        ("MOA_PROVIDERS_PACING_STATE_TTL_MS", "120000"),
        ("MOA_PROVIDERS_PACING_MAX_PACING_WAIT_MS", "15000"),
        ("MOA_PROVIDERS_PACING_DEFAULT_COOLDOWN_MS", "7000"),
        ("MOA_PROVIDERS_PACING_MAX_COOLDOWN_MS", "90000"),
        ("MOA_PROVIDERS_PACING_RETRY_BUDGET_WINDOW_MS", "30000"),
        ("MOA_PROVIDERS_PACING_RETRY_BUDGET_PERCENT", "35"),
        ("MOA_PROVIDERS_PACING_RETRY_BUDGET_FLOOR", "3"),
    ]))
    .expect("provider coordination overlay should parse")
    .apply_to(&mut config)
    .expect("provider coordination overlay should apply");

    assert_eq!(
        config.providers.concurrency.on_coordination_failure,
        crate::CoordinationFailurePolicy::FailClosed
    );
    let pacing = &config.providers.pacing;
    assert!(pacing.is_global());
    assert_eq!(pacing.state_ttl_ms, 120_000);
    assert_eq!(pacing.max_pacing_wait_ms, 15_000);
    assert_eq!(pacing.default_cooldown_ms, 7_000);
    assert_eq!(pacing.max_cooldown_ms, 90_000);
    assert_eq!(pacing.retry_budget_window_ms, 30_000);
    assert_eq!(pacing.retry_budget_percent, 35);
    assert_eq!(pacing.retry_budget_floor, 3);
}

/// The Prometheus exporter discriminant, named once so the assertion above reads
/// as a value comparison rather than a path.
fn moa_core_metrics_exporter_prometheus() -> crate::MetricsExporter {
    crate::MetricsExporter::Prometheus
}
