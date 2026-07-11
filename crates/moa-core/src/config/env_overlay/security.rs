//! Authentication, authorization, privacy, and PII overlay validation.

use super::*;

pub(super) fn exact_overlay_path(field: &str) -> Option<Vec<String>> {
    let path = match field {
        "privacy_approval_public_key_hex" => &["compliance", "privacy_approval_public_key_hex"][..],
        "privacy_export_signing_key_hex" => &["compliance", "privacy_export_signing_key_hex"],
        "privacy_export_signing_key_id" => &["compliance", "privacy_export_signing_key_id"],
        "lineage_audit_signing_key_hex" => &["compliance", "lineage_audit_signing_key_hex"],
        "lineage_audit_signing_key_id" => &["compliance", "lineage_audit_signing_key_id"],
        "pii_vault_secret_hex" => &["compliance", "pii_vault_secret_hex"],
        "pii_service_url" => &["memory", "pii_service_url"],
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

pub(super) fn validate_urls(overlay: &MoaEnvOverlay) -> Result<()> {
    validate_url("MOA_AUTH_OIDC_ISSUER", &overlay.auth_oidc_issuer)?;
    validate_url("MOA_AUTH_OIDC_JWKS_URL", &overlay.auth_oidc_jwks_url)?;
    validate_url("MOA_AUTHZ_OPENFGA_URL", &overlay.authz_openfga_url)?;
    validate_url("MOA_PII_SERVICE_URL", &overlay.pii_service_url)
}

impl MoaEnvOverlay {
    pub(super) fn validate_required_sections(&self, config: &MoaConfig) -> Result<()> {
        self.validate_auth0(config)?;
        self.validate_oidc(config)?;
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
            MoaEnvOverlay::from_iter(env_pairs([("MOA_AUTH_PROVIDER", "saml")])),
            "saml",
        );
    }

    #[test]
    fn invalid_url_reports_env_name() {
        // Pins: URL-shaped parse failures name the canonical env var.
        assert_config_error_contains(
            MoaEnvOverlay::from_iter(env_pairs([("MOA_AUTHZ_OPENFGA_URL", "openfga.internal")])),
            "MOA_AUTHZ_OPENFGA_URL",
        );
    }

    #[test]
    fn partial_openfga_overlay_reports_missing_env_name() {
        // Pins: OpenFGA overlay cannot synthesize a partial nested config.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([(
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
}
