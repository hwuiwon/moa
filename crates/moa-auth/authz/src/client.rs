//! Production OpenFGA HTTP client used by MOA authorization flows.

use crate::error::AuthzError;
use moa_authz_schema::{TupleKey, TupleOp};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Runtime configuration for the OpenFGA client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FgaConfig {
    /// OpenFGA HTTP API base URL.
    pub url: String,
    /// Preshared key configured in OpenFGA.
    pub preshared_key: String,
    /// OpenFGA store ID.
    pub store_id: String,
    /// Authorization model ID to use for checks and writes.
    pub model_id: String,
    /// Per-request HTTP timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// Reusable OpenFGA client.
#[derive(Clone)]
pub struct FgaClient {
    inner: Arc<FgaInner>,
}

struct FgaInner {
    base: String,
    http: Client,
    token: String,
    store_id: String,
    model_id: String,
}

/// Tuple returned from OpenFGA `Read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FgaTuple {
    /// Wire-format subject, such as `user:<uuid>`.
    pub user: String,
    /// Relation name.
    pub relation: String,
    /// Wire-format object, such as `tenant:<uuid>`.
    pub object: String,
}

fn default_timeout_ms() -> u64 {
    5000
}

impl FgaClient {
    /// Build a new reusable OpenFGA client.
    pub fn new(cfg: FgaConfig) -> Result<Self, AuthzError> {
        if cfg.url.trim().is_empty() {
            return Err(AuthzError::Config("OpenFGA URL is required".to_string()));
        }
        if cfg.preshared_key.trim().is_empty() {
            return Err(AuthzError::Config(
                "OpenFGA preshared key is required".to_string(),
            ));
        }
        if cfg.store_id.trim().is_empty() {
            return Err(AuthzError::Config(
                "OpenFGA store ID is required".to_string(),
            ));
        }
        if cfg.model_id.trim().is_empty() {
            return Err(AuthzError::Config(
                "OpenFGA model ID is required".to_string(),
            ));
        }

        let http = Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .build()
            .map_err(AuthzError::Transport)?;
        Ok(Self {
            inner: Arc::new(FgaInner {
                base: cfg.url.trim_end_matches('/').to_string(),
                http,
                token: cfg.preshared_key,
                store_id: cfg.store_id,
                model_id: cfg.model_id,
            }),
        })
    }

    /// Return the configured OpenFGA store ID.
    #[must_use]
    pub fn store_id(&self) -> &str {
        &self.inner.store_id
    }

    /// Return the configured OpenFGA authorization model ID.
    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.inner.model_id
    }

    /// Check whether `user` has `relation` on `object`.
    #[tracing::instrument(skip(self), fields(store_id = %self.inner.store_id, relation = %relation, object = %object))]
    pub async fn check(
        &self,
        user: &str,
        relation: &str,
        object: &str,
    ) -> Result<bool, AuthzError> {
        let response = self
            .inner
            .http
            .post(format!(
                "{}/stores/{}/check",
                self.inner.base, self.inner.store_id
            ))
            .bearer_auth(&self.inner.token)
            .json(&json!({
                "authorization_model_id": self.inner.model_id,
                "tuple_key": { "user": user, "relation": relation, "object": object },
            }))
            .send()
            .await?;
        let value = self.json_response(response).await?;
        value
            .get("allowed")
            .and_then(|allowed| allowed.as_bool())
            .ok_or_else(|| AuthzError::Ambiguous("Check missing allowed".to_string()))
    }

    /// Check several `(user, relation, object)` tuples in one OpenFGA request.
    #[tracing::instrument(skip(self, items), fields(store_id = %self.inner.store_id, n = items.len()))]
    pub async fn batch_check(
        &self,
        items: &[(String, String, String)],
    ) -> Result<Vec<bool>, AuthzError> {
        let checks: Vec<serde_json::Value> = items
            .iter()
            .enumerate()
            .map(|(index, (user, relation, object))| {
                json!({
                    "tuple_key": { "user": user, "relation": relation, "object": object },
                    "correlation_id": format!("c{index}"),
                })
            })
            .collect();

        let response = self
            .inner
            .http
            .post(format!(
                "{}/stores/{}/batch-check",
                self.inner.base, self.inner.store_id
            ))
            .bearer_auth(&self.inner.token)
            .json(&json!({
                "authorization_model_id": self.inner.model_id,
                "checks": checks,
            }))
            .send()
            .await?;
        let value = self.json_response(response).await?;
        let result_map = value
            .get("result")
            .or_else(|| value.get("results"))
            .and_then(|result| result.as_object())
            .ok_or_else(|| AuthzError::Ambiguous("BatchCheck missing result".to_string()))?;

        let mut out = Vec::with_capacity(items.len());
        for index in 0..items.len() {
            let key = format!("c{index}");
            let entry = result_map
                .get(&key)
                .ok_or_else(|| AuthzError::Ambiguous(format!("BatchCheck missing {key}")))?;
            out.push(
                entry
                    .get("allowed")
                    .and_then(|allowed| allowed.as_bool())
                    .unwrap_or(false),
            );
        }
        Ok(out)
    }

    /// List objects of `object_type` where `user` has `relation`.
    #[tracing::instrument(skip(self), fields(store_id = %self.inner.store_id, object_type = %object_type, relation = %relation))]
    pub async fn list_objects(
        &self,
        object_type: &str,
        relation: &str,
        user: &str,
    ) -> Result<Vec<String>, AuthzError> {
        let response = self
            .inner
            .http
            .post(format!(
                "{}/stores/{}/list-objects",
                self.inner.base, self.inner.store_id
            ))
            .bearer_auth(&self.inner.token)
            .json(&json!({
                "authorization_model_id": self.inner.model_id,
                "type": object_type,
                "relation": relation,
                "user": user,
            }))
            .send()
            .await?;
        let value = self.json_response(response).await?;
        value
            .get("objects")
            .and_then(|objects| objects.as_array())
            .map(|objects| {
                objects
                    .iter()
                    .filter_map(|object| object.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .ok_or_else(|| AuthzError::Ambiguous("ListObjects missing objects".to_string()))
    }

    /// Read matching tuples from OpenFGA.
    #[tracing::instrument(skip(self), fields(store_id = %self.inner.store_id))]
    pub async fn read(
        &self,
        user: Option<&str>,
        relation: Option<&str>,
        object: Option<&str>,
    ) -> Result<Vec<FgaTuple>, AuthzError> {
        let mut tuple_key = serde_json::Map::new();
        if let Some(user) = user {
            tuple_key.insert("user".to_string(), json!(user));
        }
        if let Some(relation) = relation {
            tuple_key.insert("relation".to_string(), json!(relation));
        }
        if let Some(object) = object {
            tuple_key.insert("object".to_string(), json!(object));
        }

        let response = self
            .inner
            .http
            .post(format!(
                "{}/stores/{}/read",
                self.inner.base, self.inner.store_id
            ))
            .bearer_auth(&self.inner.token)
            .json(&json!({
                "authorization_model_id": self.inner.model_id,
                "tuple_key": tuple_key,
            }))
            .send()
            .await?;
        let value = self.json_response(response).await?;
        let tuples = value
            .get("tuples")
            .and_then(|tuples| tuples.as_array())
            .ok_or_else(|| AuthzError::Ambiguous("Read missing tuples".to_string()))?;
        tuples
            .iter()
            .map(|tuple| {
                let key = tuple
                    .get("key")
                    .ok_or_else(|| AuthzError::Ambiguous("Read tuple missing key".to_string()))?;
                let user = key
                    .get("user")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| AuthzError::Ambiguous("Read tuple missing user".to_string()))?;
                let relation = key
                    .get("relation")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        AuthzError::Ambiguous("Read tuple missing relation".to_string())
                    })?;
                let object = key
                    .get("object")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        AuthzError::Ambiguous("Read tuple missing object".to_string())
                    })?;
                Ok(FgaTuple {
                    user: user.to_string(),
                    relation: relation.to_string(),
                    object: object.to_string(),
                })
            })
            .collect()
    }

    /// Apply a single typed tuple operation to OpenFGA.
    #[tracing::instrument(skip(self, tuple), fields(store_id = %self.inner.store_id, op = %op))]
    pub async fn apply(&self, op: TupleOp, tuple: &TupleKey) -> Result<(), AuthzError> {
        let wire = tuple.to_wire();
        let body = body_for_tuple_op(
            self.model_id(),
            op,
            json!({
                "user": wire.user,
                "relation": wire.relation,
                "object": wire.object,
            }),
        );
        self.apply_raw(body).await
    }

    /// Apply a tuple operation and attach the caller's idempotency key to logs.
    #[tracing::instrument(skip(self, tuple), fields(store_id = %self.inner.store_id, op = %op, idempotency_key = %idempotency_key))]
    pub async fn apply_with_idempotency(
        &self,
        op: TupleOp,
        tuple: &TupleKey,
        idempotency_key: &str,
    ) -> Result<(), AuthzError> {
        self.apply(op, tuple).await
    }

    /// Apply a pre-built OpenFGA `/write` request body.
    pub async fn apply_raw(&self, body: serde_json::Value) -> Result<(), AuthzError> {
        let response = self
            .inner
            .http
            .post(format!(
                "{}/stores/{}/write",
                self.inner.base, self.inner.store_id
            ))
            .bearer_auth(&self.inner.token)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        if is_idempotent_tuple_error(status.as_u16(), &body) {
            tracing::debug!(
                status = status.as_u16(),
                "FGA tuple operation was already converged"
            );
            return Ok(());
        }
        Err(AuthzError::HttpError {
            status: status.as_u16(),
            body,
        })
    }

    async fn json_response(
        &self,
        response: reqwest::Response,
    ) -> Result<serde_json::Value, AuthzError> {
        let status = response.status();
        if !status.is_success() {
            return Err(AuthzError::HttpError {
                status: status.as_u16(),
                body: response.text().await.unwrap_or_default(),
            });
        }
        Ok(response.json::<serde_json::Value>().await?)
    }
}

fn body_for_tuple_op(model_id: &str, op: TupleOp, wire: serde_json::Value) -> serde_json::Value {
    match op {
        TupleOp::Write => json!({
            "authorization_model_id": model_id,
            "writes": { "tuple_keys": [wire] },
        }),
        TupleOp::Delete => json!({
            "authorization_model_id": model_id,
            "deletes": { "tuple_keys": [wire] },
        }),
    }
}

fn is_idempotent_tuple_error(status: u16, body: &str) -> bool {
    if status != 400 && status != 409 {
        return false;
    }
    let lowercase = body.to_lowercase();
    lowercase.contains("already exists") || lowercase.contains("does not exist")
}

#[cfg(test)]
mod tests {
    use super::is_idempotent_tuple_error;

    #[test]
    fn idempotent_tuple_error_swallows_converged_write_and_delete_conflicts() {
        // Pins: a duplicate write (400 "already exists") and a missing-tuple delete
        // (409 "does not exist") are treated as already-converged so the outbox can
        // mark the row succeeded; any other status/body is a real failure that must
        // surface, otherwise a failed FGA write would be silently dropped.
        let cases = [
            // Converged: re-applying an existing tuple.
            (400, "cannot write a tuple which already exists", true),
            (409, "cannot write a tuple which already exists", true),
            // Converged: deleting a tuple that is already gone.
            (400, "cannot delete a tuple which does not exist", true),
            (409, "tuple does not exist", true),
            // Case-insensitive matching on the FGA message.
            (400, "Tuple ALREADY EXISTS", true),
            // Right status but a genuinely different error must not be swallowed.
            (400, "invalid authorization model id", false),
            (409, "write conflict on transaction", false),
            // Non-conflict statuses are never idempotent, even with matching text.
            (500, "already exists", false),
            (200, "already exists", false),
            (404, "does not exist", false),
        ];

        for (status, body, expected) in cases {
            assert_eq!(
                is_idempotent_tuple_error(status, body),
                expected,
                "status={status} body={body:?} should map to {expected}"
            );
        }
    }
}
