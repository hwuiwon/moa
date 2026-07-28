//! Live provider smoke tests for external knowledge providers.

use std::{collections::HashMap, path::PathBuf};

use moa_core::types::credentials::RedactedSecret;
use moa_core::types::identifiers::TenantId;
use moa_knowledge::{
    domain::{
        ConnectionStatus, CreateLinkTokenRequest, FetchRecordContentRequest, KnowledgeConnection,
        KnowledgeObject, ListChangedRecordsRequest, ObjectStatus, ParseInput,
    },
    ingestion::parse_input_from_record,
    parser::{DocumentParser, native::NativeDocumentParser},
    providers::{LinkedIntegrationProvider, merge::MergeProvider, nango::NangoProvider},
};
use serde_json::{Value, json};
use uuid::Uuid;

const LIVE_FLAG: &str = "MOA_RUN_LIVE_KNOWLEDGE_PROVIDER_TESTS";
const NANGO_BASE_URL: &str = "https://api.nango.dev";

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_KNOWLEDGE_PROVIDER_TESTS=1 and MERGE_API_KEY"]
async fn merge_live_creates_link_token() {
    // Pins: the Merge adapter can create a hosted link token against the live API.
    require_live_flag();
    let api_key = required_secret("MERGE_API_KEY");
    let provider = MergeProvider::new("https://api.merge.dev", api_key)
        .expect("merge provider should initialize");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let account_id = format!("moa-live-{}", Uuid::now_v7());

    let token = provider
        .create_link_token(CreateLinkTokenRequest {
            tenant_id,
            connector: "crm".to_string(),
            external_account_id: Some(account_id.clone()),
            end_user_email_address: Some(format!("{account_id}@example.com")),
            redirect_url: Some("https://example.com/merge/callback".to_string()),
            source_selection: json!({}),
        })
        .await
        .expect("merge live link token creation should succeed");

    assert_eq!(token.provider, "merge");
    assert!(!token.token.trim().is_empty());
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_KNOWLEDGE_PROVIDER_TESTS=1, NANGO_API_KEY, and a configured Nango provider config key"]
async fn nango_live_creates_link_token() {
    // Pins: the Nango adapter can create a connect-session link token against the live API.
    require_live_flag();
    let api_key = required_secret("NANGO_API_KEY");
    let connector =
        optional_secret("NANGO_PROVIDER_CONFIG_KEY").unwrap_or_else(|| "google-drive".to_string());
    let provider = NangoProvider::new("https://api.nango.dev", api_key)
        .expect("nango provider should initialize");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let account_id = format!("moa-live-{}", Uuid::now_v7());

    let token = provider
        .create_link_token(CreateLinkTokenRequest {
            tenant_id,
            connector,
            external_account_id: Some(account_id),
            end_user_email_address: None,
            redirect_url: Some("https://example.com/nango/callback".to_string()),
            source_selection: json!({
                "metadata": {
                    "selected_folder_ids": ["moa-live-smoke"]
                }
            }),
        })
        .await
        .expect("nango live link token creation should succeed");

    assert_eq!(token.provider, "nango");
    assert!(!token.token.trim().is_empty());
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_KNOWLEDGE_PROVIDER_TESTS=1, NANGO_API_KEY, and a live google-drive connection"]
async fn nango_live_google_drive_lists_records_with_content() {
    // Pins: against a live Nango google-drive connection, list_changed_records
    // returns records and parse_input_from_record materializes inline content or
    // a fetchable source_url for at least one record, proving Drive content
    // reaches the ingestion pipeline rather than metadata-only stubs.
    require_live_flag();
    let api_key = required_secret("NANGO_API_KEY");
    let connector =
        optional_secret("NANGO_PROVIDER_CONFIG_KEY").unwrap_or_else(|| "google-drive".to_string());
    let base_url =
        optional_secret("NANGO_API_BASE_URL").unwrap_or_else(|| NANGO_BASE_URL.to_string());
    let http = reqwest::Client::new();

    let connection_id = match optional_secret("NANGO_CONNECTION_ID") {
        Some(id) => id,
        None => discover_connection_id(&http, &base_url, &api_key, &connector)
            .await
            .unwrap_or_else(|| {
                panic!(
                    "no google-drive connection found: set NANGO_CONNECTION_ID or ensure a \
                     connection with provider_config_key `{connector}` exists in the Nango \
                     environment"
                )
            }),
    };

    let model = match optional_secret("NANGO_SYNC_MODEL") {
        Some(model) => model,
        None => discover_sync_model(&http, &base_url, &api_key, &connector, &connection_id)
            .await
            .unwrap_or_else(|| {
                panic!(
                    "could not determine a sync model for provider_config_key `{connector}`: \
                     GET /sync/status reported no syncs with a model, which usually means no \
                     sync is configured/deployed on the connection yet. Configure and run a \
                     google-drive sync in the Nango dashboard, then set NANGO_SYNC_MODEL to its \
                     model name (must match /^[A-Z][a-zA-Z0-9_-]+$/)"
                )
            }),
    };

    let provider =
        NangoProvider::new(&base_url, api_key).expect("nango provider should initialize");
    let connection = live_connection(&connector, &connection_id, &model);

    let page = provider
        .list_changed_records(ListChangedRecordsRequest {
            acl_key: std::sync::Arc::new(moa_knowledge::acl_key::SourceAclKey::new(1, vec![7; 32])),
            credential: test_credential(),
            connection,
            cursor: None,
            modified_after: None,
            limit: Some(25),
            variant: None,
        })
        .await
        .expect("live Nango /records listing should succeed");

    assert!(
        !page.records.is_empty(),
        "live google-drive sync returned zero records for model `{model}`; \
         trigger the sync or pick a model that has synced records"
    );

    let mut materialized = 0usize;
    let mut with_text = 0usize;
    let mut with_url = 0usize;
    let mut carried_fields: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for record in &page.records {
        if let Value::Object(map) = &record.payload {
            for key in map.keys() {
                carried_fields.insert(key.clone());
            }
        }
        if let Ok(input) = parse_input_from_record("nango", live_object(), record) {
            let has_text = input
                .text
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty());
            let has_url = input.source_url.is_some();
            if has_text {
                with_text += 1;
            }
            if has_url {
                with_url += 1;
            }
            if has_text || has_url {
                materialized += 1;
            }
        }
    }

    // Field NAMES only (never values) so we can report what to configure without
    // leaking any Drive content or presigned-URL tokens. Printed on success too
    // (with `--no-capture`) as a non-secret summary of what the sync carries.
    eprintln!(
        "nango google-drive live: model=`{model}` records={} materialized={materialized} \
         with_text={with_text} with_url={with_url} payload_field_names={:?}",
        page.records.len(),
        carried_fields
    );
    assert!(
        materialized > 0,
        "no record materialized content: {} records for model `{model}`, {with_text} with inline \
         text, {with_url} with a source_url; payload field names present: {:?}",
        page.records.len(),
        carried_fields
    );
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_KNOWLEDGE_PROVIDER_TESTS=1 and NANGO_API_KEY"]
async fn nango_live_lists_integrations_including_google_drive() {
    // Pins: list_integrations reads the live Nango /integrations catalog and
    // returns the configured google-drive integration with the connector id used
    // in the link flow.
    require_live_flag();
    let api_key = required_secret("NANGO_API_KEY");
    let connector =
        optional_secret("NANGO_PROVIDER_CONFIG_KEY").unwrap_or_else(|| "google-drive".to_string());
    let base_url =
        optional_secret("NANGO_API_BASE_URL").unwrap_or_else(|| NANGO_BASE_URL.to_string());
    let provider =
        NangoProvider::new(&base_url, api_key).expect("nango provider should initialize");

    let integrations = provider
        .list_integrations()
        .await
        .expect("live Nango /integrations listing should succeed");

    assert!(
        integrations
            .iter()
            .any(|integration| integration.id == connector),
        "live Nango catalog should include the `{connector}` integration; got ids {:?}",
        integrations
            .iter()
            .map(|integration| integration.id.as_str())
            .collect::<Vec<_>>()
    );
    // Integration ids are non-secret provider_config_keys.
    eprintln!(
        "nango live integrations: count={} ids={:?}",
        integrations.len(),
        integrations
            .iter()
            .map(|integration| integration.id.as_str())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_KNOWLEDGE_PROVIDER_TESTS=1, NANGO_API_KEY, and a live google-drive connection"]
async fn nango_live_google_drive_fetches_record_content() {
    // Pins: against a live Nango google-drive connection, fetch_record_content
    // downloads real byte content through the proxy for a metadata-only record,
    // and text-MIME content parses to non-empty text via the native parser,
    // proving Drive file content (not just metadata) reaches ingestion.
    require_live_flag();
    let api_key = required_secret("NANGO_API_KEY");
    let connector =
        optional_secret("NANGO_PROVIDER_CONFIG_KEY").unwrap_or_else(|| "google-drive".to_string());
    let base_url =
        optional_secret("NANGO_API_BASE_URL").unwrap_or_else(|| NANGO_BASE_URL.to_string());
    let http = reqwest::Client::new();

    let connection_id = match optional_secret("NANGO_CONNECTION_ID") {
        Some(id) => id,
        None => discover_connection_id(&http, &base_url, &api_key, &connector)
            .await
            .unwrap_or_else(|| {
                panic!(
                    "no google-drive connection found: set NANGO_CONNECTION_ID or ensure a \
                     connection with provider_config_key `{connector}` exists"
                )
            }),
    };
    let model = match optional_secret("NANGO_SYNC_MODEL") {
        Some(model) => model,
        None => discover_sync_model(&http, &base_url, &api_key, &connector, &connection_id)
            .await
            .unwrap_or_else(|| {
                panic!("could not determine a sync model for provider_config_key `{connector}`")
            }),
    };

    let provider =
        NangoProvider::new(&base_url, api_key).expect("nango provider should initialize");
    let connection = live_connection(&connector, &connection_id, &model);
    let page = provider
        .list_changed_records(ListChangedRecordsRequest {
            acl_key: std::sync::Arc::new(moa_knowledge::acl_key::SourceAclKey::new(1, vec![7; 32])),
            credential: test_credential(),
            connection: connection.clone(),
            cursor: None,
            modified_after: None,
            limit: Some(25),
            variant: None,
        })
        .await
        .expect("live Nango /records listing should succeed");
    assert!(
        !page.records.is_empty(),
        "live google-drive sync returned zero records for model `{model}`"
    );

    // Try records until one yields content. Drive listings mix folders and other
    // non-exportable types that legitimately return None; a Doc/Sheet/Slide or a
    // binary file should download. Google-apps editor files come first so text
    // exports are exercised when present.
    let mut ranked: Vec<_> = page.records.iter().collect();
    ranked.sort_by_key(|record| {
        let is_editor = record
            .payload
            .get("mimeType")
            .and_then(Value::as_str)
            .is_some_and(|mime| mime.starts_with("application/vnd.google-apps."));
        u8::from(!is_editor)
    });

    let mut fetched = None;
    for record in ranked {
        let record_mime = record
            .payload
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match provider
            .fetch_record_content(FetchRecordContentRequest {
                credential: test_credential(),
                connection: connection.clone(),
                record: record.clone(),
            })
            .await
        {
            Ok(Some(content)) if !content.bytes.is_empty() => {
                fetched = Some((record, record_mime, content));
                break;
            }
            Ok(_) => continue,
            Err(error) => panic!("live content fetch errored for `{record_mime}`: {error}"),
        }
    }
    let (record, record_mime, content) = fetched.expect(
        "at least one live google-drive record should yield fetchable content \
         (a Doc, Sheet, Slide, or binary file)",
    );

    let fetched_mime = content.mime_type.clone().unwrap_or_default();
    let input = ParseInput {
        object: live_object(),
        file_name: record.title.clone(),
        mime_type: content.mime_type.clone(),
        source_url: None,
        bytes: Some(content.bytes.clone()),
        text: None,
        options: json!({}),
    };
    let mut parsed_text_len = 0usize;
    if fetched_mime.starts_with("text/") {
        let parsed = NativeDocumentParser::new()
            .parse(input)
            .await
            .expect("native parser should parse text content");
        parsed_text_len = parsed.text.trim().len();
        assert!(
            parsed_text_len > 0,
            "text-MIME fetched content should parse to non-empty text"
        );
    }

    // Byte/text lengths and MIME only; never document content, tokens, or PII.
    eprintln!(
        "nango google-drive live fetch: model=`{model}` source_mime=`{record_mime}` \
         fetched_mime=`{fetched_mime}` fetched_bytes={} parsed_text_len={parsed_text_len}",
        content.bytes.len()
    );
}

/// Discovers a connection id for `connector` by listing Nango connections and
/// matching on `provider_config_key` (falling back to `provider`). Returns the
/// first match, or `None` when discovery is unavailable or finds nothing.
async fn discover_connection_id(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    connector: &str,
) -> Option<String> {
    let response = http
        .get(format!("{}/connection", base_url.trim_end_matches('/')))
        .bearer_auth(api_key)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    let connections = body.get("connections")?.as_array()?;
    connections
        .iter()
        .find(|connection| {
            connection
                .get("provider_config_key")
                .and_then(Value::as_str)
                .or_else(|| connection.get("provider").and_then(Value::as_str))
                == Some(connector)
        })
        .and_then(|connection| {
            connection
                .get("connection_id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
}

/// Discovers a sync model for the connection via GET /sync/status, preferring a
/// model that already has synced records (`recordCount` > 0) and otherwise any
/// model name the syncs report. Returns `None` when discovery is unavailable.
async fn discover_sync_model(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    connector: &str,
    connection_id: &str,
) -> Option<String> {
    let mut url =
        reqwest::Url::parse(&format!("{}/sync/status", base_url.trim_end_matches('/'))).ok()?;
    url.query_pairs_mut()
        .append_pair("provider_config_key", connector)
        .append_pair("syncs", "*")
        .append_pair("connection_id", connection_id);
    let response = http.get(url).bearer_auth(api_key).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    let syncs = body.get("syncs")?.as_array()?;
    let mut fallback: Option<String> = None;
    for sync in syncs {
        for field in ["recordCount", "latestResult"] {
            if let Some(models) = sync.get(field).and_then(Value::as_object) {
                for (model, value) in models {
                    fallback.get_or_insert_with(|| model.clone());
                    let has_records = value.as_u64().is_some_and(|count| count > 0)
                        || value
                            .as_object()
                            .and_then(|entry| entry.get("added").or_else(|| entry.get("count")))
                            .and_then(Value::as_u64)
                            .is_some_and(|count| count > 0);
                    if has_records {
                        return Some(model.clone());
                    }
                }
            }
        }
    }
    fallback
}

/// Builds a live connection targeting `connector`/`connection_id` with `model`
/// selected as the Nango sync model.
fn live_connection(connector: &str, connection_id: &str, model: &str) -> KnowledgeConnection {
    let now = moa_test_support::fixtures::pg_now();
    KnowledgeConnection {
        acl_mode: moa_knowledge::domain::ConnectionAclMode::TenantPublic,
        connection_uid: Uuid::now_v7(),
        tenant_id: TenantId::from(Uuid::now_v7()),
        provider: "nango".to_string(),
        connector: connector.to_string(),
        provider_account_id: connection_id.to_string(),
        credential_ref: format!("nango:{connection_id}"),
        status: ConnectionStatus::Active,
        metadata: json!({}),
        source_selection: json!({ "model": model }),
        information_barrier: None,
        created_at: now,
        updated_at: now,
        last_synced_at: None,
    }
}

/// Builds a throwaway knowledge object for materialization checks.
fn live_object() -> KnowledgeObject {
    KnowledgeObject {
        acl: moa_knowledge::domain::ObjectAcl::incomplete(),
        object_uid: Uuid::now_v7(),
        tenant_id: TenantId::from(Uuid::now_v7()),
        connection_uid: Uuid::now_v7(),
        object_type: "document".to_string(),
        source_id: "live-probe".to_string(),
        parent_source_id: None,
        source_uri: None,
        title: None,
        change_token: None,
        metadata: json!({}),
        status: ObjectStatus::Pending,
        source_updated_at: None,
        deleted_at: None,
    }
}

/// Returns `true` when `name` is set to a common truthy value (`1`, `true`,
/// `yes`, or `on`, case-insensitively after trimming), matching how live-test
/// flags are written in a developer's `.env`.
fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn require_live_flag() {
    assert!(
        env_flag_enabled(LIVE_FLAG),
        "{LIVE_FLAG}=1 is required for live provider tests"
    );
}

fn required_secret(name: &str) -> String {
    optional_secret(name).unwrap_or_else(|| panic!("{name} must be set when {LIVE_FLAG}=1"))
}

fn optional_secret(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .and_then(non_empty)
        .or_else(|| dotenv_values().remove(name).and_then(non_empty))
}

fn dotenv_values() -> HashMap<String, String> {
    let path = repo_root().join(".env");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    contents.lines().filter_map(parse_dotenv_line).collect()
}

fn parse_dotenv_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (key, value) = trimmed.split_once('=')?;
    Some((key.trim().to_string(), unquote(value.trim()).to_string()))
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("crate should live under crates/moa-knowledge")
        .to_path_buf()
}

/// Builds the resolved credential a provider request carries.
///
/// Provider requests take a non-serializable redacted secret, so tests build one
/// explicitly instead of smuggling material through the connection.
fn test_credential() -> RedactedSecret {
    RedactedSecret::new("test-provider-credential".to_string())
}
