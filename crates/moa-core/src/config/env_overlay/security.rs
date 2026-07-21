//! Authentication, authorization, privacy, and PII overlay validation.

use super::*;

pub(super) fn exact_overlay_path(field: &str) -> Option<Vec<String>> {
    let path = match field {
        "privacy_approval_public_key_hex" => &["compliance", "privacy_approval_public_key_hex"][..],
        "privacy_export_signing_key_hex" => &["compliance", "privacy_export_signing_key_hex"],
        "privacy_export_signing_key_id" => &["compliance", "privacy_export_signing_key_id"],
        "lineage_audit_signing_key_hex" => &["compliance", "lineage_audit_signing_key_hex"],
        "lineage_audit_signing_key_id" => &["compliance", "lineage_audit_signing_key_id"],
        "lineage_audit_root_seed_hex" => &["compliance", "lineage_audit_root_seed_hex"],
        "pii_vault_secret_hex" => &["compliance", "pii_vault_secret_hex"],
        "require_dual_control_for_erasure" => &["compliance", "require_dual_control_for_erasure"],
        "pii_service_url" => &["memory", "pii_service_url"],
        "auth_oauth_clients_json" => &["auth", "oauth", "clients"],
        "token_vault_refresh_json" => &["token_vault", "refresh"],
        _ => return None,
    };
    Some(strings(path))
}

pub(super) fn optional_section_seed(path: &[&str]) -> Option<Value> {
    match path {
        ["auth", "auth0"] => Some(json!({
            "domain": "",
            "audience": "",
            "client_id": "",
            "client_secret": "",
        })),
        ["auth", "oidc"] => Some(json!({
            "issuer": "",
            "audience": "",
            "jwks_url": "",
        })),
        ["authz", "openfga"] => Some(json!({
            "url": "",
            "preshared_key": "",
            "store_id": "",
            "model_id": "",
            "timeout_ms": 2000,
        })),
        _ => None,
    }
}

pub(super) fn validate_urls(overlay: &EnvOverlay) -> Result<()> {
    validate_url("MOA_AUTH_OIDC_ISSUER", &overlay.auth_oidc_issuer)?;
    validate_url("MOA_AUTH_OIDC_JWKS_URL", &overlay.auth_oidc_jwks_url)?;
    validate_url("MOA_AUTH_OAUTH_ISSUER", &overlay.auth_oauth_issuer)?;
    validate_url("MOA_AUTH_OAUTH_RESOURCE", &overlay.auth_oauth_resource)?;
    validate_url("MOA_AUTHZ_OPENFGA_URL", &overlay.authz_openfga_url)?;
    validate_url("MOA_PII_SERVICE_URL", &overlay.pii_service_url)
}

impl EnvOverlay {
    pub(super) fn validate_required_sections(&self, config: &MoaConfig) -> Result<()> {
        self.validate_auth0(config)?;
        self.validate_oidc(config)?;
        self.validate_oauth(config)?;
        self.validate_contact_tokens(config)?;
        self.validate_openfga(config)
    }

    fn validate_auth0(&self, config: &MoaConfig) -> Result<()> {
        if !any_present(&[
            self.auth_auth0_domain.is_some(),
            self.auth_auth0_audience.is_some(),
            self.auth_auth0_client_id.is_some(),
            self.auth_auth0_client_secret.is_some(),
        ]) {
            return Ok(());
        }

        let auth0 = config.auth.auth0.as_ref().ok_or_else(|| {
            MoaError::ConfigError(
                "MOA_AUTH_AUTH0_DOMAIN is required when configuring this section".to_string(),
            )
        })?;
        require_non_empty("MOA_AUTH_AUTH0_DOMAIN", &auth0.domain)?;
        require_non_empty("MOA_AUTH_AUTH0_AUDIENCE", &auth0.audience)?;
        require_non_empty("MOA_AUTH_AUTH0_CLIENT_ID", &auth0.client_id)?;
        require_non_empty("MOA_AUTH_AUTH0_CLIENT_SECRET", &auth0.client_secret)
    }

    fn validate_oidc(&self, config: &MoaConfig) -> Result<()> {
        if !any_present(&[
            self.auth_oidc_issuer.is_some(),
            self.auth_oidc_audience.is_some(),
            self.auth_oidc_jwks_url.is_some(),
        ]) {
            return Ok(());
        }

        let oidc = config.auth.oidc.as_ref().ok_or_else(|| {
            MoaError::ConfigError(
                "MOA_AUTH_OIDC_ISSUER is required when configuring this section".to_string(),
            )
        })?;
        require_non_empty("MOA_AUTH_OIDC_ISSUER", &oidc.issuer)?;
        require_non_empty("MOA_AUTH_OIDC_AUDIENCE", &oidc.audience)?;
        require_non_empty("MOA_AUTH_OIDC_JWKS_URL", &oidc.jwks_url)
    }

    fn validate_contact_tokens(&self, config: &MoaConfig) -> Result<()> {
        if !any_present(&[
            self.auth_contact_tokens_issuer.is_some(),
            self.auth_contact_tokens_audience.is_some(),
            self.auth_contact_tokens_key_id.is_some(),
            self.auth_contact_tokens_private_key_pem.is_some(),
            self.auth_contact_tokens_public_key_pem.is_some(),
            self.auth_contact_tokens_contact_point_hash_key_hex
                .is_some(),
            self.auth_contact_tokens_unverified_ttl_seconds.is_some(),
            self.auth_contact_tokens_verified_ttl_seconds.is_some(),
            self.auth_contact_tokens_verification_ttl_seconds.is_some(),
        ]) {
            return Ok(());
        }

        let contact_tokens = &config.auth.contact_tokens;
        require_non_empty("MOA_AUTH_CONTACT_TOKENS_ISSUER", &contact_tokens.issuer)?;
        require_non_empty("MOA_AUTH_CONTACT_TOKENS_AUDIENCE", &contact_tokens.audience)?;
        require_non_empty("MOA_AUTH_CONTACT_TOKENS_KEY_ID", &contact_tokens.key_id)?;
        require_non_empty(
            "MOA_AUTH_CONTACT_TOKENS_PRIVATE_KEY_PEM",
            &contact_tokens.private_key_pem,
        )?;
        require_non_empty(
            "MOA_AUTH_CONTACT_TOKENS_PUBLIC_KEY_PEM",
            &contact_tokens.public_key_pem,
        )?;
        require_non_empty(
            "MOA_AUTH_CONTACT_TOKENS_CONTACT_POINT_HASH_KEY_HEX",
            &contact_tokens.contact_point_hash_key_hex,
        )
    }

    fn validate_oauth(&self, config: &MoaConfig) -> Result<()> {
        if !any_present(&[
            self.auth_oauth_issuer.is_some(),
            self.auth_oauth_resource.is_some(),
            self.auth_oauth_authorization_request_ttl_seconds.is_some(),
            self.auth_oauth_authorization_code_ttl_seconds.is_some(),
            self.auth_oauth_access_token_ttl_seconds.is_some(),
            self.auth_oauth_refresh_token_ttl_seconds.is_some(),
            self.auth_oauth_clients_json.is_some(),
        ]) {
            return Ok(());
        }
        config
            .auth
            .oauth
            .validate()
            .map_err(|error| MoaError::ConfigError(format!("OAuth configuration: {error}")))
    }

    fn validate_openfga(&self, config: &MoaConfig) -> Result<()> {
        if !any_present(&[
            self.authz_openfga_url.is_some(),
            self.authz_openfga_preshared_key.is_some(),
            self.authz_openfga_store_id.is_some(),
            self.authz_openfga_model_id.is_some(),
            self.authz_openfga_timeout_ms.is_some(),
        ]) {
            return Ok(());
        }

        let openfga = config.authz.openfga.as_ref().ok_or_else(|| {
            MoaError::ConfigError(
                "MOA_AUTHZ_OPENFGA_URL is required when configuring this section".to_string(),
            )
        })?;
        require_non_empty("MOA_AUTHZ_OPENFGA_URL", &openfga.url)?;
        require_non_empty("MOA_AUTHZ_OPENFGA_PRESHARED_KEY", &openfga.preshared_key)?;
        require_non_empty("MOA_AUTHZ_OPENFGA_STORE_ID", &openfga.store_id)?;
        require_non_empty("MOA_AUTHZ_OPENFGA_MODEL_ID", &openfga.model_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_enum_reports_offending_value() {
        // Pins: unsupported enum values are rejected. envy/serde deserialize
        // enums directly, so the message names the rejected variant rather than
        // the `MOA_` env var (see `restore_env_prefix`).
        assert_config_error_contains(
            EnvOverlay::from_iter(env_pairs([("MOA_AUTH_PROVIDER", "saml")])),
            "saml",
        );
    }

    #[test]
    fn invalid_url_reports_env_name() {
        // Pins: URL-shaped parse failures name the canonical env var.
        assert_config_error_contains(
            EnvOverlay::from_iter(env_pairs([("MOA_AUTHZ_OPENFGA_URL", "openfga.internal")])),
            "MOA_AUTHZ_OPENFGA_URL",
        );
    }

    #[test]
    fn partial_openfga_overlay_reports_missing_env_name() {
        // Pins: OpenFGA overlay cannot synthesize a partial nested config.
        let overlay = EnvOverlay::from_iter(env_pairs([(
            "MOA_AUTHZ_OPENFGA_URL",
            "http://openfga.example",
        )]))
        .expect("overlay should parse");
        let mut config = MoaConfig::default();

        assert_config_error_contains(
            overlay.apply_to(&mut config),
            "MOA_AUTHZ_OPENFGA_PRESHARED_KEY",
        );
    }

    #[test]
    fn oauth_overlay_applies_resource_lifetimes_and_clients() {
        // Pins: every authorization-server value needed by independent edge
        // replicas can be declared through flat deployment environment values.
        let overlay = EnvOverlay::from_iter(env_pairs([
            ("MOA_AUTH_OAUTH_ISSUER", "https://auth.example"),
            ("MOA_AUTH_OAUTH_RESOURCE", "https://api.example/mcp"),
            (
                "MOA_AUTH_OAUTH_AUTHORIZATION_REQUEST_TTL_SECONDS",
                "240",
            ),
            (
                "MOA_AUTH_OAUTH_AUTHORIZATION_CODE_TTL_SECONDS",
                "45",
            ),
            ("MOA_AUTH_OAUTH_ACCESS_TOKEN_TTL_SECONDS", "900"),
            ("MOA_AUTH_OAUTH_REFRESH_TOKEN_TTL_SECONDS", "3600"),
            (
                "MOA_AUTH_OAUTH_CLIENTS_JSON",
                r#"[{"client_id":"desktop","client_type":"public","redirect_uris":["http://127.0.0.1/callback"],"scopes":["mcp:read","mcp:write"]}]"#,
            ),
        ]))
        .expect("OAuth overlay parses");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("OAuth overlay applies");

        assert_eq!(config.auth.oauth.issuer, "https://auth.example");
        assert_eq!(config.auth.oauth.resource, "https://api.example/mcp");
        assert_eq!(config.auth.oauth.authorization_request_ttl_seconds, 240);
        assert_eq!(config.auth.oauth.authorization_code_ttl_seconds, 45);
        assert_eq!(config.auth.oauth.access_token_ttl_seconds, 900);
        assert_eq!(config.auth.oauth.refresh_token_ttl_seconds, 3600);
        assert_eq!(config.auth.oauth.clients.len(), 1);
        assert_eq!(config.auth.oauth.clients[0].client_id, "desktop");
        assert_eq!(
            config.auth.oauth.clients[0].scopes,
            vec!["mcp:read".to_string(), "mcp:write".to_string()]
        );
    }

    #[test]
    fn oauth_overlay_rejects_non_mcp_resource() {
        // Pins: deployment cannot accidentally grant OAuth authority over the
        // REST origin by configuring a broad or unrelated protected resource.
        let overlay = EnvOverlay::from_iter(env_pairs([(
            "MOA_AUTH_OAUTH_RESOURCE",
            "https://api.example/v1",
        )]))
        .expect("URL-shaped overlay parses");
        let mut config = MoaConfig::default();

        assert_config_error_contains(overlay.apply_to(&mut config), "invalid OAuth resource URL");
    }

    #[test]
    fn token_vault_refresh_overlay_applies_typed_secrets() {
        // Pins: replicas receive complete refresh credentials directly from one
        // typed JSON environment value without secondary environment lookups.
        let overlay = EnvOverlay::from_iter(env_pairs([(
            "MOA_TOKEN_VAULT_REFRESH_JSON",
            r#"{"github":{"token_endpoint":"https://github.com/login/oauth/access_token","client_id":"client-id","client_secret":"client-secret"}}"#,
        )]))
        .expect("token-vault refresh overlay parses");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("token-vault refresh overlay applies");

        let refresh = config
            .token_vault
            .refresh
            .get("github")
            .expect("github refresh config");
        assert_eq!(refresh.client_id, "client-id");
        assert_eq!(refresh.client_secret.as_deref(), Some("client-secret"));
    }

    #[test]
    fn token_vault_refresh_overlay_rejects_invalid_endpoint() {
        // Pins: invalid refresh endpoints fail startup validation instead of
        // surfacing only after a token expires.
        let overlay = EnvOverlay::from_iter(env_pairs([(
            "MOA_TOKEN_VAULT_REFRESH_JSON",
            r#"{"github":{"token_endpoint":"github.internal/token","client_id":"client-id"}}"#,
        )]))
        .expect("token-vault refresh overlay parses");
        let mut config = MoaConfig::default();

        assert_config_error_contains(
            overlay.apply_to(&mut config),
            "token_vault.refresh.github.token_endpoint",
        );
    }

    #[test]
    fn token_vault_refresh_overlay_rejects_malformed_json() {
        // Pins: malformed JSON names the deployment environment variable.
        assert_config_error_contains(
            EnvOverlay::from_iter(env_pairs([("MOA_TOKEN_VAULT_REFRESH_JSON", "{not-json}")])),
            "MOA_TOKEN_VAULT_REFRESH_JSON",
        );
    }
}
