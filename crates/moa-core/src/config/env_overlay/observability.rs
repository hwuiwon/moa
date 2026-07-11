//! Observability and ClickHouse overlay parsing.

use super::*;

pub(super) fn optional_section_seed(path: &[&str]) -> Option<Value> {
    match path {
        ["clickhouse"] => Some(json!({
            "url": "",
            "database": "moa",
            "user": null,
            "password": null,
            "lineage_ttl_days": 30,
            "export_poll_secs": 15,
            "export_batch_rows": 5000,
        })),
        _ => None,
    }
}

pub(super) fn deserialize_optional_nonempty<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
}

pub(super) fn deserialize_optional_headers<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<HashMap<String, String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    parse_headers(&raw)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

fn parse_headers(value: &str) -> std::result::Result<HashMap<String, String>, String> {
    let mut headers = HashMap::new();
    if value.trim().is_empty() {
        return Ok(headers);
    }
    for entry in value.split(',') {
        let (key, header_value) = entry
            .split_once('=')
            .ok_or_else(|| format!("header entry `{entry}` must use key=value"))?;
        let key = key.trim();
        if key.is_empty() {
            return Err("header entry contains an empty header name".to_string());
        }
        headers.insert(key.to_string(), header_value.trim().to_string());
    }
    Ok(headers)
}

pub(super) fn validate_urls(overlay: &MoaEnvOverlay) -> Result<()> {
    validate_url(
        "MOA_OBSERVABILITY_OTLP_ENDPOINT",
        &overlay.observability_otlp_endpoint,
    )?;
    validate_url("MOA_CLICKHOUSE_URL", &overlay.clickhouse_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clickhouse_env_creates_optional_section() {
        // Pins: flat Kubernetes env can enable the ClickHouse analytics store
        // without a TOML section; unset knobs keep their seeded defaults.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_DATABASE_URL", "postgres://moa:test@db.example/moa"),
            ("MOA_CLICKHOUSE_URL", "http://clickhouse.example:8123"),
            ("MOA_CLICKHOUSE_PASSWORD", "redacted"),
            ("MOA_CLICKHOUSE_EXPORT_POLL_SECS", "30"),
            ("MOA_CLICKHOUSE_EXPORT_BATCH_ROWS", "2500"),
        ]))
        .expect("overlay should deserialize");
        let mut config = MoaConfig::default();
        assert!(config.clickhouse.is_none());

        overlay.apply_to(&mut config).expect("overlay should apply");

        let clickhouse = config.clickhouse.expect("clickhouse config");
        assert_eq!(clickhouse.url, "http://clickhouse.example:8123");
        assert_eq!(clickhouse.database, "moa");
        assert_eq!(clickhouse.user, None);
        assert_eq!(clickhouse.password.as_deref(), Some("redacted"));
        assert_eq!(clickhouse.lineage_ttl_days, 30);
        assert_eq!(clickhouse.export_poll_secs, 30);
        assert_eq!(clickhouse.export_batch_rows, 2500);
    }

    #[test]
    fn empty_clickhouse_env_values_mean_unset() {
        // Pins: compose files can pass `MOA_CLICKHOUSE_URL: ${MOA_CLICKHOUSE_URL:-}`;
        // empty values leave the section absent so Postgres stays the backend.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_DATABASE_URL", "postgres://moa:test@db.example/moa"),
            ("MOA_CLICKHOUSE_URL", ""),
            ("MOA_CLICKHOUSE_USER", " "),
            ("MOA_CLICKHOUSE_PASSWORD", ""),
        ]))
        .expect("overlay should deserialize");
        let mut config = MoaConfig::default();

        overlay.apply_to(&mut config).expect("overlay should apply");

        assert!(config.clickhouse.is_none());
    }

    #[test]
    fn clickhouse_env_without_url_is_rejected() {
        // Pins: a partially configured ClickHouse section fails startup instead
        // of silently falling back to Postgres while credentials are set.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_DATABASE_URL", "postgres://moa:test@db.example/moa"),
            ("MOA_CLICKHOUSE_PASSWORD", "redacted"),
        ]))
        .expect("overlay should deserialize");
        let mut config = MoaConfig::default();

        assert_config_error_contains(overlay.apply_to(&mut config), "clickhouse.url");
    }
}
