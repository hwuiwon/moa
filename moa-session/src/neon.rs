//! Neon API-backed checkpoint branch management for MOA.

use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_core::{
    BranchManager, CheckpointHandle, CheckpointInfo, MoaConfig, MoaError, Result, SessionId,
};
use reqwest::{Client, Method, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

const MOA_CHECKPOINT_PREFIX: &str = "moa-checkpoint-";
const NEON_INCLUDED_BRANCH_WARNING_THRESHOLD: usize = 8;

/// Neon REST API-backed checkpoint branch manager.
#[derive(Clone)]
pub struct NeonBranchManager {
    api_key: String,
    project_id: String,
    parent_branch_id: String,
    database_url: String,
    http_client: Client,
    base_url: Url,
    max_branches: usize,
    ttl: Duration,
    pooled: bool,
    suspend_timeout_seconds: u64,
}

struct NeonBranchManagerOptions {
    api_key: String,
    project_id: String,
    parent_branch_id: String,
    database_url: String,
    max_branches: usize,
    ttl: Duration,
    pooled: bool,
    suspend_timeout_seconds: u64,
    base_url: Url,
}

impl NeonBranchManager {
    /// Creates a Neon branch manager from the loaded MOA config.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        if !config.database.neon.enabled {
            return Err(MoaError::ConfigError(
                "database.neon.enabled must be true to use Neon checkpoints".to_string(),
            ));
        }
        if config.database.neon.max_checkpoints == 0 {
            return Err(MoaError::ConfigError(
                "database.neon.max_checkpoints must be greater than zero".to_string(),
            ));
        }
        if config.database.neon.project_id.trim().is_empty() {
            return Err(MoaError::ConfigError(
                "database.neon.project_id is required when Neon checkpointing is enabled"
                    .to_string(),
            ));
        }
        if config.database.neon.parent_branch_id.trim().is_empty() {
            return Err(MoaError::ConfigError(
                "database.neon.parent_branch_id is required when Neon checkpointing is enabled"
                    .to_string(),
            ));
        }
        let api_key_env = config.database.neon.api_key_env.trim();
        if api_key_env.is_empty() {
            return Err(MoaError::ConfigError(
                "database.neon.api_key_env is required when Neon checkpointing is enabled"
                    .to_string(),
            ));
        }
        let api_key = std::env::var(api_key_env)
            .map_err(|_| MoaError::MissingEnvironmentVariable(api_key_env.to_string()))?;
        let base_url = Url::parse("https://console.neon.tech/api/v2/")
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;

        Self::new_with_options(NeonBranchManagerOptions {
            api_key,
            project_id: config.database.neon.project_id.clone(),
            parent_branch_id: config.database.neon.parent_branch_id.clone(),
            database_url: config.database.url.clone(),
            max_branches: config.database.neon.max_checkpoints,
            ttl: Duration::from_secs(config.database.neon.checkpoint_ttl_hours * 60 * 60),
            pooled: config.database.neon.pooled,
            suspend_timeout_seconds: config.database.neon.suspend_timeout_seconds,
            base_url,
        })
    }

    /// Returns `Some(manager)` when Neon checkpointing is enabled in config.
    pub fn maybe_from_config(config: &MoaConfig) -> Result<Option<Self>> {
        if config.database.neon.enabled {
            return Self::from_config(config).map(Some);
        }

        Ok(None)
    }

    fn new_with_options(options: NeonBranchManagerOptions) -> Result<Self> {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        Ok(Self {
            api_key: options.api_key,
            project_id: options.project_id,
            parent_branch_id: options.parent_branch_id,
            database_url: options.database_url,
            http_client,
            base_url: options.base_url,
            max_branches: options.max_branches,
            ttl: options.ttl,
            pooled: options.pooled,
            suspend_timeout_seconds: options.suspend_timeout_seconds,
        })
    }

    /// Fetches one checkpoint by branch id, if it exists and belongs to MOA.
    pub async fn get_checkpoint(&self, branch_id: &str) -> Result<Option<CheckpointInfo>> {
        let branch = self.fetch_branch(branch_id).await?;
        if !is_moa_checkpoint_branch(&branch.name) {
            return Ok(None);
        }

        let connection_url = self.fetch_connection_uri(branch_id).await?;
        Ok(Some(checkpoint_info_from_branch(
            branch,
            connection_url,
            None,
        )))
    }

    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<T> {
        let mut url = self
            .base_url
            .join(path)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        let mut request = self
            .http_client
            .request(method, url)
            .bearer_auth(&self.api_key)
            .header("Accept", "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| format!("neon api request failed with status {status}"));
            return Err(MoaError::HttpStatus {
                status: status.as_u16(),
                message,
            });
        }

        response
            .json::<T>()
            .await
            .map_err(|error| MoaError::SerializationError(error.to_string()))
    }

    async fn delete_branch(&self, branch_id: &str) -> Result<()> {
        let path = format!("projects/{}/branches/{branch_id}", self.project_id);
        let url = self
            .base_url
            .join(&path)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;
        let response = self
            .http_client
            .delete(url)
            .bearer_auth(&self.api_key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        if !response.status().is_success() {
            let status = response.status();
            let message = response
                .text()
                .await
                .unwrap_or_else(|_| format!("neon api request failed with status {status}"));
            return Err(MoaError::HttpStatus {
                status: status.as_u16(),
                message,
            });
        }
        Ok(())
    }

    async fn list_all_branches(&self) -> Result<Vec<NeonBranch>> {
        let mut branches = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut query = vec![
                ("limit", "100".to_string()),
                ("sort_by", "created_at".to_string()),
                ("sort_order", "desc".to_string()),
            ];
            if let Some(next) = cursor.clone() {
                query.push(("cursor", next));
            }
            let path = format!("projects/{}/branches", self.project_id);
            let response: NeonBranchListResponse =
                self.request(Method::GET, &path, &query, None).await?;
            branches.extend(response.branches);
            cursor = response.pagination.and_then(|pagination| pagination.next);
            if cursor.is_none() {
                break;
            }
        }
        Ok(branches)
    }

    async fn list_checkpoint_branches(&self) -> Result<Vec<NeonBranch>> {
        Ok(self
            .list_all_branches()
            .await?
            .into_iter()
            .filter(|branch| is_moa_checkpoint_branch(&branch.name))
            .collect())
    }

    async fn fetch_branch(&self, branch_id: &str) -> Result<NeonBranch> {
        let path = format!("projects/{}/branches/{branch_id}", self.project_id);
        let response: NeonBranchResponse = self.request(Method::GET, &path, &[], None).await?;
        Ok(response.branch)
    }

    async fn wait_for_branch_ready(&self, branch_id: &str) -> Result<NeonBranch> {
        for attempt in 0..20 {
            let branch = self.fetch_branch(branch_id).await?;
            if branch.current_state.eq_ignore_ascii_case("ready") {
                return Ok(branch);
            }
            tokio::time::sleep(Duration::from_millis(500 + attempt * 100)).await;
        }

        Err(MoaError::ProviderError(format!(
            "checkpoint branch {branch_id} did not become ready in time"
        )))
    }

    async fn resolve_parent_branch_id(&self) -> Result<String> {
        if self.parent_branch_id.starts_with("br-") {
            return Ok(self.parent_branch_id.clone());
        }

        let branches = self.list_all_branches().await?;
        branches
            .into_iter()
            .find(|branch| {
                branch.id == self.parent_branch_id || branch.name == self.parent_branch_id
            })
            .map(|branch| branch.id)
            .ok_or_else(|| {
                MoaError::ConfigError(format!(
                    "database.neon.parent_branch_id `{}` did not match a Neon branch id or name",
                    self.parent_branch_id
                ))
            })
    }

    async fn fetch_connection_uri(&self, branch_id: &str) -> Result<String> {
        let (database_name, role_name) = parse_connection_identity(&self.database_url)?;
        let path = format!("projects/{}/connection_uri", self.project_id);
        let query = vec![
            ("branch_id", branch_id.to_string()),
            ("database_name", database_name),
            ("role_name", role_name),
            ("pooled", self.pooled.to_string()),
        ];
        let response: NeonConnectionUriResponse =
            self.request(Method::GET, &path, &query, None).await?;
        Ok(response.uri)
    }
}

impl BranchManager for NeonBranchManager {
    /// Creates an ephemeral Neon checkpoint branch from the configured parent branch.
    async fn create_checkpoint(
        &self,
        label: &str,
        session_id: Option<SessionId>,
    ) -> Result<CheckpointHandle> {
        let mut active_branches = self.list_checkpoint_branches().await?;
        if active_branches.len() >= self.max_branches {
            let deleted = self.cleanup_expired().await?;
            if deleted > 0 {
                active_branches = self.list_checkpoint_branches().await?;
            }
        }
        if active_branches.len() >= self.max_branches {
            return Err(MoaError::ProviderError(format!(
                "refusing to create Neon checkpoint: {} active checkpoints already exist (max {})",
                active_branches.len(),
                self.max_branches
            )));
        }

        let parent_branch_id = self.resolve_parent_branch_id().await?;
        let branch_name = format_checkpoint_branch_name(label);
        let path = format!("projects/{}/branches", self.project_id);
        let body = json!({
            "branch": {
                "parent_id": parent_branch_id,
                "name": branch_name,
            },
            "endpoints": [{
                "type": "read_write",
                "suspend_timeout_seconds": self.suspend_timeout_seconds,
            }],
        });
        let created: NeonCreatedBranchResponse =
            self.request(Method::POST, &path, &[], Some(body)).await?;
        let branch = self.wait_for_branch_ready(&created.branch.id).await?;
        let connection_url = self.fetch_connection_uri(&branch.id).await?;
        let handle = CheckpointHandle {
            id: branch.id.clone(),
            label: label.to_string(),
            connection_url,
            created_at: branch.created_at,
            session_id,
        };
        let active_after = active_branches.len() + 1;
        info!(
            branch_id = %handle.id,
            label = %handle.label,
            active = active_after,
            max = self.max_branches,
            "created Neon checkpoint branch ({} of {} active; Neon child branches cost roughly $1.50/month each beyond the included quota)",
            active_after,
            self.max_branches
        );
        if active_after >= NEON_INCLUDED_BRANCH_WARNING_THRESHOLD {
            warn!(
                active = active_after,
                "approaching Neon included branch quota; keep checkpoint cleanup healthy"
            );
        }
        Ok(handle)
    }

    /// Marks the provided checkpoint branch as the rollback target.
    async fn rollback_to(&self, handle: &CheckpointHandle) -> Result<()> {
        let branch = self.fetch_branch(&handle.id).await?;
        if !is_moa_checkpoint_branch(&branch.name) {
            return Err(MoaError::ProviderError(format!(
                "branch {} is not a MOA-managed checkpoint",
                handle.id
            )));
        }
        info!(
            branch_id = %handle.id,
            "rollback selected checkpoint branch; callers must reconnect using the checkpoint connection URL"
        );
        Ok(())
    }

    /// Deletes the provided checkpoint branch.
    async fn discard_checkpoint(&self, handle: &CheckpointHandle) -> Result<()> {
        self.delete_branch(&handle.id).await?;
        info!(branch_id = %handle.id, "discarded Neon checkpoint branch");
        Ok(())
    }

    /// Lists active MOA checkpoint branches.
    async fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>> {
        let mut checkpoints = Vec::new();
        for branch in self.list_checkpoint_branches().await? {
            let connection_url = self.fetch_connection_uri(&branch.id).await?;
            checkpoints.push(checkpoint_info_from_branch(branch, connection_url, None));
        }
        Ok(checkpoints)
    }

    /// Deletes expired MOA checkpoint branches older than the configured TTL.
    async fn cleanup_expired(&self) -> Result<u32> {
        let cutoff = chrono::Duration::from_std(self.ttl)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;
        let now = Utc::now();
        let mut deleted = 0_u32;
        for branch in self.list_checkpoint_branches().await? {
            if now - branch.created_at < cutoff {
                continue;
            }
            self.delete_branch(&branch.id).await?;
            deleted += 1;
        }
        if deleted > 0 {
            info!(deleted, "cleaned up expired Neon checkpoint branches");
        }
        Ok(deleted)
    }
}

fn parse_connection_identity(database_url: &str) -> Result<(String, String)> {
    let url = Url::parse(database_url)
        .map_err(|error| MoaError::ConfigError(format!("invalid database.url: {error}")))?;
    let role_name = url.username().trim();
    if role_name.is_empty() {
        return Err(MoaError::ConfigError(
            "database.url must include a PostgreSQL role name to derive Neon checkpoint connection URLs"
                .to_string(),
        ));
    }
    let database_name = url.path().trim_start_matches('/').trim();
    if database_name.is_empty() {
        return Err(MoaError::ConfigError(
            "database.url must include a PostgreSQL database name to derive Neon checkpoint connection URLs"
                .to_string(),
        ));
    }
    Ok((database_name.to_string(), role_name.to_string()))
}

fn is_moa_checkpoint_branch(name: &str) -> bool {
    name.starts_with(MOA_CHECKPOINT_PREFIX)
}

fn format_checkpoint_branch_name(label: &str) -> String {
    let sanitized = label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let label = if sanitized.is_empty() {
        "checkpoint".to_string()
    } else {
        sanitized.chars().take(40).collect()
    };
    format!(
        "{MOA_CHECKPOINT_PREFIX}{label}-{}",
        Utc::now().format("%Y%m%dt%H%M%Sz")
    )
}

fn checkpoint_label_from_name(name: &str) -> String {
    let trimmed = name.strip_prefix(MOA_CHECKPOINT_PREFIX).unwrap_or(name);
    match trimmed.rsplit_once('-') {
        Some((label, suffix))
            if suffix.len() == 16
                && suffix.starts_with('2')
                && suffix.contains('t')
                && suffix.ends_with('z') =>
        {
            label.to_string()
        }
        _ => trimmed.to_string(),
    }
}

fn checkpoint_info_from_branch(
    branch: NeonBranch,
    connection_url: String,
    session_id: Option<SessionId>,
) -> CheckpointInfo {
    let handle = CheckpointHandle {
        id: branch.id.clone(),
        label: checkpoint_label_from_name(&branch.name),
        connection_url,
        created_at: branch.created_at,
        session_id,
    };
    CheckpointInfo {
        handle,
        size_bytes: branch.logical_size,
        parent_branch: branch.parent_id.unwrap_or_default(),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct NeonPagination {
    next: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct NeonBranchListResponse {
    branches: Vec<NeonBranch>,
    pagination: Option<NeonPagination>,
}

#[derive(Debug, Clone, Deserialize)]
struct NeonBranchResponse {
    branch: NeonBranch,
}

#[derive(Debug, Clone, Deserialize)]
struct NeonCreatedBranchResponse {
    branch: NeonBranch,
}

#[derive(Debug, Clone, Deserialize)]
struct NeonConnectionUriResponse {
    uri: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct NeonBranch {
    id: String,
    name: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    logical_size: Option<u64>,
    created_at: DateTime<Utc>,
    #[serde(default)]
    current_state: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::{
        Json, Router,
        extract::{Path, Query, State},
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    #[derive(Clone, Default)]
    struct TestState {
        branches: Arc<Mutex<Vec<NeonBranch>>>,
        deleted: Arc<Mutex<Vec<String>>>,
        create_requests: Arc<Mutex<Vec<Value>>>,
        connection_uris: Arc<Mutex<HashMap<String, String>>>,
    }

    #[tokio::test]
    async fn create_checkpoint_sends_expected_request_and_returns_handle() {
        let state = TestState::default();
        seed_parent_branch(&state).await;
        let server = spawn_test_server(state.clone()).await;
        let manager = manager_for_test(server, 5, 24).expect("manager");

        let handle = manager
            .create_checkpoint("before deploy", Some(SessionId::new()))
            .await
            .expect("create checkpoint");

        assert!(handle.id.starts_with("br-created"));
        assert!(handle.label.contains("before deploy"));
        assert_eq!(
            handle.connection_url,
            "postgres://postgres:postgres@ep-created.us-east-2.aws.neon.tech/moa_test?sslmode=require"
        );
        let requests = state.create_requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]["branch"]["parent_id"],
            Value::String("br-main".to_string())
        );
        assert_eq!(
            requests[0]["endpoints"][0]["suspend_timeout_seconds"],
            Value::Number(300_u64.into())
        );
    }

    #[tokio::test]
    async fn create_checkpoint_refuses_to_exceed_capacity() {
        let state = TestState::default();
        seed_parent_branch(&state).await;
        for index in 0..2 {
            push_branch(
                &state,
                test_checkpoint_branch(
                    &format!("br-existing-{index}"),
                    &format!("moa-checkpoint-existing-{index}-20260410t120000z"),
                    hours_ago(1),
                ),
            )
            .await;
        }
        let server = spawn_test_server(state.clone()).await;
        let manager = manager_for_test(server, 2, 24).expect("manager");

        let error = manager
            .create_checkpoint("overflow", None)
            .await
            .expect_err("capacity error");

        assert!(
            error
                .to_string()
                .contains("active checkpoints already exist")
        );
    }

    #[tokio::test]
    async fn cleanup_expired_deletes_only_old_moa_branches() {
        let state = TestState::default();
        seed_parent_branch(&state).await;
        push_branch(
            &state,
            test_checkpoint_branch(
                "br-old",
                "moa-checkpoint-old-20260409t120000z",
                hours_ago(30),
            ),
        )
        .await;
        push_branch(
            &state,
            test_checkpoint_branch(
                "br-new",
                "moa-checkpoint-new-20260410t120000z",
                hours_ago(1),
            ),
        )
        .await;
        push_branch(
            &state,
            NeonBranch {
                id: "br-user".to_string(),
                name: "feature-work".to_string(),
                parent_id: Some("br-main".to_string()),
                logical_size: Some(123),
                created_at: hours_ago(40),
                current_state: "ready".to_string(),
            },
        )
        .await;
        let server = spawn_test_server(state.clone()).await;
        let manager = manager_for_test(server, 5, 24).expect("manager");

        let deleted = manager.cleanup_expired().await.expect("cleanup");

        assert_eq!(deleted, 1);
        let deleted_ids = state.deleted.lock().await;
        assert_eq!(deleted_ids.as_slice(), &["br-old".to_string()]);
    }

    #[tokio::test]
    async fn discard_checkpoint_calls_delete_endpoint() {
        let state = TestState::default();
        seed_parent_branch(&state).await;
        let server = spawn_test_server(state.clone()).await;
        let manager = manager_for_test(server, 5, 24).expect("manager");
        let handle = CheckpointHandle {
            id: "br-delete-me".to_string(),
            label: "delete-me".to_string(),
            connection_url: "postgres://example".to_string(),
            created_at: Utc::now(),
            session_id: None,
        };

        manager
            .discard_checkpoint(&handle)
            .await
            .expect("discard checkpoint");

        let deleted = state.deleted.lock().await;
        assert_eq!(deleted.as_slice(), &["br-delete-me".to_string()]);
    }

    #[test]
    fn checkpoint_branch_names_follow_moa_prefix() {
        let name = format_checkpoint_branch_name("Before Deploy!");
        assert!(name.starts_with("moa-checkpoint-before-deploy-"));
    }

    fn manager_for_test(
        base_url: Url,
        max_branches: usize,
        ttl_hours: u64,
    ) -> Result<NeonBranchManager> {
        NeonBranchManager::new_with_options(NeonBranchManagerOptions {
            api_key: "token".to_string(),
            project_id: "project-1".to_string(),
            parent_branch_id: "main".to_string(),
            database_url:
                "postgres://postgres:postgres@ep-main.us-east-2.aws.neon.tech/moa_test?sslmode=require"
                    .to_string(),
            max_branches,
            ttl: Duration::from_secs(ttl_hours * 60 * 60),
            pooled: true,
            suspend_timeout_seconds: 300,
            base_url,
        })
    }

    async fn spawn_test_server(state: TestState) -> Url {
        let router = Router::new()
            .route(
                "/api/v2/projects/{project_id}/branches",
                get(list_branches).post(create_branch),
            )
            .route(
                "/api/v2/projects/{project_id}/branches/{branch_id}",
                get(get_branch).delete(delete_branch_handler),
            )
            .route(
                "/api/v2/projects/{project_id}/connection_uri",
                get(get_connection_uri),
            )
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        Url::parse(&format!("http://{addr}/api/v2/")).expect("url")
    }

    async fn list_branches(
        State(state): State<TestState>,
        headers: HeaderMap,
    ) -> std::result::Result<Json<Value>, StatusCode> {
        assert!(headers.get("authorization").is_some());
        let branches = state.branches.lock().await.clone();
        Ok(Json(
            json!({ "branches": branches, "pagination": { "next": null } }),
        ))
    }

    async fn create_branch(
        State(state): State<TestState>,
        Json(body): Json<Value>,
    ) -> std::result::Result<Json<Value>, StatusCode> {
        state.create_requests.lock().await.push(body.clone());
        let branch = NeonBranch {
            id: "br-created".to_string(),
            name: body["branch"]["name"]
                .as_str()
                .unwrap_or("moa-checkpoint-created")
                .to_string(),
            parent_id: Some("br-main".to_string()),
            logical_size: Some(128),
            created_at: Utc::now(),
            current_state: "ready".to_string(),
        };
        state.branches.lock().await.push(branch.clone());
        state.connection_uris.lock().await.insert(
            branch.id.clone(),
            "postgres://postgres:postgres@ep-created.us-east-2.aws.neon.tech/moa_test?sslmode=require".to_string(),
        );
        Ok(Json(json!({ "branch": branch, "endpoints": [] })))
    }

    async fn get_branch(
        State(state): State<TestState>,
        Path((_project_id, branch_id)): Path<(String, String)>,
    ) -> std::result::Result<Json<Value>, StatusCode> {
        let branch = state
            .branches
            .lock()
            .await
            .iter()
            .find(|branch| branch.id == branch_id)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?;
        Ok(Json(json!({ "branch": branch })))
    }

    async fn delete_branch_handler(
        State(state): State<TestState>,
        Path((_project_id, branch_id)): Path<(String, String)>,
    ) -> StatusCode {
        state.deleted.lock().await.push(branch_id.clone());
        state
            .branches
            .lock()
            .await
            .retain(|branch| branch.id != branch_id);
        StatusCode::OK
    }

    async fn get_connection_uri(
        State(state): State<TestState>,
        Query(query): Query<HashMap<String, String>>,
    ) -> std::result::Result<Json<Value>, StatusCode> {
        let branch_id = query.get("branch_id").ok_or(StatusCode::BAD_REQUEST)?;
        let uri = state
            .connection_uris
            .lock()
            .await
            .get(branch_id)
            .cloned()
            .unwrap_or_else(|| {
                format!(
                    "postgres://postgres:postgres@{branch_id}.us-east-2.aws.neon.tech/moa_test?sslmode=require"
                )
            });
        Ok(Json(json!({ "uri": uri })))
    }

    async fn seed_parent_branch(state: &TestState) {
        push_branch(
            state,
            NeonBranch {
                id: "br-main".to_string(),
                name: "main".to_string(),
                parent_id: None,
                logical_size: Some(512),
                created_at: hours_ago(48),
                current_state: "ready".to_string(),
            },
        )
        .await;
        state.connection_uris.lock().await.insert(
            "br-main".to_string(),
            "postgres://postgres:postgres@ep-main.us-east-2.aws.neon.tech/moa_test?sslmode=require"
                .to_string(),
        );
    }

    async fn push_branch(state: &TestState, branch: NeonBranch) {
        state.branches.lock().await.push(branch);
    }

    fn test_checkpoint_branch(id: &str, name: &str, created_at: DateTime<Utc>) -> NeonBranch {
        NeonBranch {
            id: id.to_string(),
            name: name.to_string(),
            parent_id: Some("br-main".to_string()),
            logical_size: Some(64),
            created_at,
            current_state: "ready".to_string(),
        }
    }

    fn hours_ago(hours: i64) -> DateTime<Utc> {
        Utc::now() - chrono::Duration::hours(hours)
    }
}
