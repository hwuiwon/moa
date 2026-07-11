//! Database overlay URL validation and database-focused behavior tests.

use super::*;

pub(super) fn validate_urls(overlay: &MoaEnvOverlay) -> Result<()> {
    validate_url("MOA_DATABASE_URL", &overlay.database_url)?;
    validate_url("MOA_DATABASE_ADMIN_URL", &overlay.database_admin_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_integer_reports_env_name() {
        // Pins: integer parse failures name the canonical env var.
        assert_config_error_contains(
            MoaEnvOverlay::from_iter(env_pairs([("MOA_DATABASE_MAX_CONNECTIONS", "many")])),
            "MOA_DATABASE_MAX_CONNECTIONS",
        );
    }

    #[test]
    fn invalid_background_pool_size_reports_env_name() {
        // Pins: background-pool parse failures identify the deployment knob that
        // owns the isolated maintenance connection budget.
        assert_config_error_contains(
            MoaEnvOverlay::from_iter(env_pairs([(
                "MOA_DATABASE_BACKGROUND_MAX_CONNECTIONS",
                "many",
            )])),
            "MOA_DATABASE_BACKGROUND_MAX_CONNECTIONS",
        );
    }
}
