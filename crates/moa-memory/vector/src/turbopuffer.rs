//! Turbopuffer-backed graph-memory vector store.

use std::{collections::HashMap, time::Duration};

use async_trait::async_trait;
use backon::{ExponentialBuilder, Retryable};
use reqwest::{Client, Method};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    Error, Result, VECTOR_DIMENSION, VectorItem, VectorMatch, VectorQuery, VectorStore, pii_rank,
    validate_dimension,
};

const DEFAULT_BASE_URL: &str = "https://api.turbopuffer.com";
const BACKEND: &str = "turbopuffer";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_MIN_DELAY: Duration = Duration::from_millis(100);
const RETRY_MAX_DELAY: Duration = Duration::from_millis(800);
const RETRY_TOTAL_DELAY: Duration = Duration::from_secs(3);
const RETRY_ATTEMPTS: usize = 5;

/// Turbopuffer implementation of [`VectorStore`].
#[derive(Clone)]
pub struct TurbopufferStore {
    client: Client,
    base_url: String,
    api_key: SecretString,
    env: String,
    baa_enabled: bool,
}

impl TurbopufferStore {
    /// Creates a Turbopuffer store from process environment.
    ///
    /// Required: `TURBOPUFFER_API_KEY`.
    /// Optional: `TURBOPUFFER_BASE_URL`, `MOA_ENV`, `TURBOPUFFER_BAA`.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("TURBOPUFFER_API_KEY")
            .map_err(|_| Error::TurbopufferConfig("TURBOPUFFER_API_KEY is required".to_string()))?;
        if api_key.trim().is_empty() {
            return Err(Error::TurbopufferConfig(
                "TURBOPUFFER_API_KEY is empty".to_string(),
            ));
        }

        let base_url =
            std::env::var("TURBOPUFFER_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let env = std::env::var("MOA_ENV")
            .or_else(|_| std::env::var("MOA_ENVIRONMENT"))
            .unwrap_or_else(|_| "dev".to_string());
        let baa_enabled = std::env::var("TURBOPUFFER_BAA")
            .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false);

        Self::new(base_url, SecretString::from(api_key), env, baa_enabled)
    }

    /// Creates a Turbopuffer store with explicit configuration.
    pub fn new(
        base_url: impl Into<String>,
        api_key: SecretString,
        env: impl Into<String>,
        baa_enabled: bool,
    ) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(Error::TurbopufferConfig(
                "base URL must not be empty".to_string(),
            ));
        }

        let env = namespace_segment(&env.into());
        if env.is_empty() {
            return Err(Error::TurbopufferConfig(
                "environment namespace segment must not be empty".to_string(),
            ));
        }

        let client = Client::builder()
            .http2_keep_alive_interval(HTTP2_KEEPALIVE_INTERVAL)
            .http2_keep_alive_timeout(HTTP2_KEEPALIVE_TIMEOUT)
            .http2_keep_alive_while_idle(true)
            .gzip(true)
            .timeout(REQUEST_TIMEOUT)
            .build()?;

        Ok(Self {
            client,
            base_url,
            api_key,
            env,
            baa_enabled,
        })
    }

    /// Returns whether this client may be used for HIPAA-tier workspaces.
    #[must_use]
    pub fn has_baa(&self) -> bool {
        self.baa_enabled
    }

    /// Returns the Turbopuffer namespace for a workspace.
    pub fn namespace_for_workspace(&self, workspace_id: &str) -> Result<String> {
        let workspace_segment = namespace_segment(workspace_id);
        if workspace_segment.is_empty() {
            return Err(Error::TurbopufferConfig(
                "workspace namespace segment must not be empty".to_string(),
            ));
        }

        let namespace = format!("moa-{}-{}", self.env, workspace_segment);
        if namespace.len() > 128 {
            return Err(Error::TurbopufferConfig(format!(
                "namespace `{namespace}` exceeds Turbopuffer's 128-byte limit"
            )));
        }
        Ok(namespace)
    }

    /// Validates a namespace before issuing a write.
    ///
    /// Turbopuffer creates namespaces implicitly on first write. This method exists to keep
    /// the call site explicit and to reject names that cannot be represented by the API.
    pub async fn ensure_namespace(&self, namespace: &str) -> Result<()> {
        if namespace.is_empty() || namespace.len() > 128 {
            return Err(Error::TurbopufferConfig(format!(
                "invalid Turbopuffer namespace `{namespace}`"
            )));
        }
        if !namespace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(Error::TurbopufferConfig(format!(
                "invalid Turbopuffer namespace `{namespace}`"
            )));
        }
        Ok(())
    }

    /// Deletes embeddings in one explicit workspace namespace.
    pub async fn delete_in_workspace(&self, workspace_id: &str, uids: &[Uuid]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let namespace = self.namespace_for_workspace(workspace_id)?;
        self.ensure_namespace(&namespace).await?;
        let body = json!({
            "deletes": uids.iter().map(Uuid::to_string).collect::<Vec<_>>(),
        });
        self.request_value(Method::POST, write_path(&namespace), body)
            .await?;
        Ok(())
    }

    async fn request_value(&self, method: Method, path: String, body: Value) -> Result<Value> {
        let backoff = ExponentialBuilder::default()
            .with_min_delay(RETRY_MIN_DELAY)
            .with_max_delay(RETRY_MAX_DELAY)
            .with_total_delay(Some(RETRY_TOTAL_DELAY))
            .with_max_times(RETRY_ATTEMPTS)
            .with_jitter();
        let url = format!("{}{}", self.base_url, path);

        (|| async {
            self.request_value_once(method.clone(), &url, body.clone())
                .await
        })
        .retry(backoff)
        .when(is_retryable)
        .await
    }

    async fn request_value_once(&self, method: Method, url: &str, body: Value) -> Result<Value> {
        let response = self
            .client
            .request(method, url)
            .bearer_auth(self.api_key.expose_secret())
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            return Err(Error::VectorProviderStatus {
                provider: BACKEND,
                status: status.as_u16(),
                body: text,
            });
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text)?)
    }
}

#[async_trait]
impl VectorStore for TurbopufferStore {
    fn backend(&self) -> &'static str {
        BACKEND
    }

    fn dimension(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn upsert(&self, items: &[VectorItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }

        let mut groups: HashMap<&str, Vec<&VectorItem>> = HashMap::new();
        for item in items {
            validate_dimension(&item.embedding)?;
            pii_rank(&item.pii_class)?;
            let workspace_id = item
                .workspace_id
                .as_deref()
                .ok_or(Error::WorkspaceRequired {
                    backend: BACKEND,
                    operation: "upsert",
                })?;
            groups.entry(workspace_id).or_default().push(item);
        }

        for (workspace_id, batch) in groups {
            let namespace = self.namespace_for_workspace(workspace_id)?;
            self.ensure_namespace(&namespace).await?;
            let upsert_rows = batch
                .into_iter()
                .map(upsert_row)
                .collect::<Result<Vec<_>>>()?;
            let body = json!({
                "upsert_rows": upsert_rows,
                "distance_metric": "cosine_distance",
            });
            self.request_value(Method::POST, write_path(&namespace), body)
                .await?;
        }

        Ok(())
    }

    async fn knn(&self, query: &VectorQuery) -> Result<Vec<VectorMatch>> {
        validate_dimension(&query.embedding)?;
        if query.k == 0 {
            return Ok(Vec::new());
        }
        if query.k > 10_000 {
            return Err(Error::QueryLimitTooLarge(query.k));
        }
        let workspace_id = query
            .workspace_id
            .as_deref()
            .ok_or(Error::WorkspaceRequired {
                backend: BACKEND,
                operation: "knn",
            })?;
        let namespace = self.namespace_for_workspace(workspace_id)?;
        self.ensure_namespace(&namespace).await?;

        let mut body = json!({
            "rank_by": ["vector", "ANN", query.embedding],
            "top_k": query.k,
            "include_attributes": ["id"],
        });
        if let Some(filters) = filter_expr(query)? {
            body["filters"] = filters;
        }

        let value = self
            .request_value(Method::POST, query_path(&namespace), body)
            .await?;
        parse_matches(value)
    }

    async fn delete(&self, uids: &[Uuid]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        Err(Error::WorkspaceRequired {
            backend: BACKEND,
            operation: "delete",
        })
    }
}

fn upsert_row(item: &VectorItem) -> Result<Value> {
    let pii_rank = pii_rank(&item.pii_class)?;
    let scope = if item.user_id.is_some() {
        "user"
    } else {
        "workspace"
    };
    Ok(json!({
        "id": item.uid.to_string(),
        "vector": item.embedding,
        "label": item.label,
        "pii_class": item.pii_class,
        "pii_rank": pii_rank,
        "valid_to": item
            .valid_to
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| "open".to_string()),
        "scope": scope,
        "embedding_model": item.embedding_model,
        "embedding_model_version": item.embedding_model_version,
    }))
}

fn filter_expr(query: &VectorQuery) -> Result<Option<Value>> {
    let mut terms = vec![
        json!(["pii_rank", "Lte", pii_rank(&query.max_pii_class)?]),
        json!(["valid_to", "Eq", "open"]),
    ];

    if !query.include_global {
        terms.push(json!(["scope", "NotEq", "global"]));
    }
    if let Some(labels) = query
        .label_filter
        .as_ref()
        .filter(|labels| !labels.is_empty())
    {
        if labels.len() == 1 {
            terms.push(json!(["label", "Eq", labels[0]]));
        } else {
            terms.push(json!([
                "Or",
                labels
                    .iter()
                    .map(|label| json!(["label", "Eq", label]))
                    .collect::<Vec<_>>()
            ]));
        }
    }

    Ok(match terms.len() {
        0 => None,
        1 => terms.pop(),
        _ => Some(json!(["And", terms])),
    })
}

fn parse_matches(value: Value) -> Result<Vec<VectorMatch>> {
    let response: QueryResponse = serde_json::from_value(value)?;
    response
        .rows
        .into_iter()
        .map(|row| {
            let uid = match row.id {
                Value::String(value) => Uuid::parse_str(&value).map_err(|error| {
                    Error::TurbopufferResponse(format!("invalid UUID id `{value}`: {error}"))
                })?,
                other => {
                    return Err(Error::TurbopufferResponse(format!(
                        "expected string UUID id, got {other}"
                    )));
                }
            };
            let distance = row.distance.unwrap_or(0.0);
            Ok(VectorMatch {
                uid,
                score: (1.0 - distance).clamp(-1.0, 1.0),
            })
        })
        .collect()
}

fn is_retryable(error: &Error) -> bool {
    match error {
        Error::VectorProviderStatus { status, .. } => {
            *status == 429 || (500..=599).contains(status)
        }
        Error::Reqwest(error) => error.is_connect() || error.is_timeout(),
        _ => false,
    }
}

fn write_path(namespace: &str) -> String {
    format!("/v2/namespaces/{namespace}")
}

fn query_path(namespace: &str) -> String {
    format!("/v2/namespaces/{namespace}/query")
}

fn namespace_segment(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[derive(Debug, Deserialize)]
struct QueryResponse {
    #[serde(default)]
    rows: Vec<QueryRow>,
}

#[derive(Debug, Deserialize)]
struct QueryRow {
    id: Value,
    #[serde(rename = "$dist")]
    distance: Option<f32>,
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
    };

    use super::*;

    #[tokio::test]
    async fn turbopuffer_namespace_auto_create() {
        let server = MockServer::start(vec![MockResponse::json(
            200,
            r#"{"rows_affected":1,"rows_upserted":1}"#,
        )])
        .await;
        let store = TurbopufferStore::new(
            server.base_url(),
            SecretString::from("test-key".to_string()),
            "test",
            false,
        )
        .expect("store");
        let workspace_id = Uuid::now_v7().to_string();
        store
            .upsert(&[test_item(Uuid::now_v7(), &workspace_id)])
            .await
            .expect("upsert");

        let requests = server.requests().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].path,
            format!("/v2/namespaces/moa-test-{workspace_id}")
        );
        assert!(requests[0].body.contains("\"upsert_rows\""));
        assert!(requests[0].body.contains("\"valid_to\":\"open\""));
    }

    #[tokio::test]
    async fn turbopuffer_429_retry() {
        let uid = Uuid::now_v7();
        let server = MockServer::start(vec![
            MockResponse::json(429, r#"{"status":"error","error":"rate limited"}"#),
            MockResponse::json(
                200,
                &format!(r#"{{"rows":[{{"id":"{uid}","$dist":0.2}}]}}"#),
            ),
        ])
        .await;
        let store = TurbopufferStore::new(
            server.base_url(),
            SecretString::from("test-key".to_string()),
            "test",
            false,
        )
        .expect("store");
        let matches = store
            .knn(&VectorQuery {
                workspace_id: Some(Uuid::now_v7().to_string()),
                embedding: basis_vector(0),
                k: 1,
                label_filter: None,
                max_pii_class: "restricted".to_string(),
                include_global: false,
            })
            .await
            .expect("query");

        assert_eq!(matches, vec![VectorMatch { uid, score: 0.8 }]);
        assert_eq!(server.requests().await.len(), 2);
    }

    #[tokio::test]
    async fn turbopuffer_round_trip() {
        let uid = Uuid::now_v7();
        let server = MockServer::start(vec![
            MockResponse::json(200, r#"{"rows_affected":1,"rows_upserted":1}"#),
            MockResponse::json(
                200,
                &format!(r#"{{"rows":[{{"id":"{uid}","$dist":0.0}}]}}"#),
            ),
        ])
        .await;
        let store = TurbopufferStore::new(
            server.base_url(),
            SecretString::from("test-key".to_string()),
            "test",
            false,
        )
        .expect("store");
        let workspace_id = Uuid::now_v7().to_string();
        store
            .upsert(&[test_item(uid, &workspace_id)])
            .await
            .expect("upsert");
        let matches = store
            .knn(&VectorQuery {
                workspace_id: Some(workspace_id),
                embedding: basis_vector(0),
                k: 10,
                label_filter: Some(vec!["Fact".to_string()]),
                max_pii_class: "restricted".to_string(),
                include_global: false,
            })
            .await
            .expect("query");

        assert_eq!(matches, vec![VectorMatch { uid, score: 1.0 }]);
        let requests = server.requests().await;
        assert!(requests[1].body.contains("\"valid_to\",\"Eq\",\"open\""));
    }

    fn test_item(uid: Uuid, workspace_id: &str) -> VectorItem {
        VectorItem {
            uid,
            workspace_id: Some(workspace_id.to_string()),
            user_id: None,
            label: "Fact".to_string(),
            pii_class: "none".to_string(),
            embedding: basis_vector(0),
            embedding_model: "test-embed".to_string(),
            embedding_model_version: 1,
            valid_to: None,
        }
    }

    fn basis_vector(index: usize) -> Vec<f32> {
        let mut embedding = vec![0.0; VECTOR_DIMENSION];
        embedding[index % VECTOR_DIMENSION] = 1.0;
        embedding
    }

    #[derive(Clone)]
    struct MockResponse {
        status: u16,
        body: String,
    }

    impl MockResponse {
        fn json(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.to_string(),
            }
        }
    }

    #[derive(Clone, Debug)]
    struct RecordedRequest {
        path: String,
        body: String,
    }

    struct MockServer {
        addr: std::net::SocketAddr,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    impl MockServer {
        async fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("local addr");
            let responses = Arc::new(responses);
            let requests = Arc::new(Mutex::new(Vec::new()));
            let attempts = Arc::new(AtomicUsize::new(0));
            let request_log = Arc::clone(&requests);

            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let index = attempts.fetch_add(1, Ordering::SeqCst);
                    let response = responses
                        .get(index)
                        .cloned()
                        .or_else(|| responses.last().cloned())
                        .expect("at least one response");
                    let request_log = Arc::clone(&request_log);

                    tokio::spawn(async move {
                        let request = read_request(&mut socket).await;
                        request_log.lock().await.push(request);
                        let response = format!(
                            "HTTP/1.1 {} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            response.status,
                            response.body.len(),
                            response.body
                        );
                        socket
                            .write_all(response.as_bytes())
                            .await
                            .expect("write response");
                    });
                }
            });

            Self { addr, requests }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }

        async fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().await.clone()
        }
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> RecordedRequest {
        let mut buf = Vec::new();
        let mut temp = [0_u8; 1024];
        let header_end = loop {
            let read = socket.read(&mut temp).await.expect("read request");
            if read == 0 {
                break buf.len();
            }
            buf.extend_from_slice(&temp[..read]);
            if let Some(pos) = find_header_end(&buf) {
                break pos;
            }
        };

        let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let body_start = header_end + 4;
        while buf.len().saturating_sub(body_start) < content_length {
            let read = socket.read(&mut temp).await.expect("read body");
            if read == 0 {
                break;
            }
            buf.extend_from_slice(&temp[..read]);
        }

        let request_line = headers.lines().next().unwrap_or_default();
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string();
        let body =
            String::from_utf8_lossy(&buf[body_start..body_start + content_length]).to_string();
        RecordedRequest { path, body }
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|window| window == b"\r\n\r\n")
    }
}
