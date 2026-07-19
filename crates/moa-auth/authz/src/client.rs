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
    /// Wire-format subject, such as `operator:<uuid>`.
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
        if is_idempotent_tuple_error(&body) {
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

/// Structured OpenFGA REST error body.
///
/// OpenFGA reports write failures as a JSON object carrying a machine `code`
/// and a human `message`. Both fields are required for classification, so a
/// body missing either fails to deserialize and is treated as a real failure.
#[derive(Debug, Deserialize)]
struct FgaErrorBody {
    /// Machine-readable OpenFGA error code.
    code: String,
    /// Human-readable OpenFGA error message.
    message: String,
}

/// OpenFGA error code returned for tuple `Write` failures.
///
/// OpenFGA (through at least v1.x) reuses this single code for the idempotent
/// no-ops we can safely swallow — re-writing an existing tuple and deleting a
/// missing tuple — *and* for genuinely invalid writes such as an unknown type
/// or relation. It exposes no finer-grained code to distinguish them, so a
/// match on the code alone is insufficient and the message must be inspected.
const FGA_INVALID_WRITE_INPUT_CODE: &str = "write_failed_due_to_invalid_input";

/// Returns true when an OpenFGA `/write` error body means the store already
/// matches the requested state, so the outbox row can be marked converged.
///
/// Classification is driven entirely by the structured error `code` and
/// message, never by the HTTP status or loose substring matching: only a
/// [`FGA_INVALID_WRITE_INPUT_CODE`] error whose message is one of OpenFGA's two
/// stable idempotent phrases counts. A structured "this tuple already exists" /
/// "this tuple does not exist" signal means the store is in the desired state
/// whatever envelope carried it, so keying off the body alone also avoids the
/// outbox looping forever should OpenFGA ever move these errors off HTTP 400.
/// Any parse failure, unexpected code, or unrecognized message fails closed
/// (treated as a real failure) so a dropped tuple write is never swallowed.
fn is_idempotent_tuple_error(body: &str) -> bool {
    let Ok(error) = serde_json::from_str::<FgaErrorBody>(body) else {
        return false;
    };
    if error.code != FGA_INVALID_WRITE_INPUT_CODE {
        return false;
    }
    // Narrow within the shared invalid-input code on OpenFGA's stable message
    // prefixes; genuinely invalid writes reuse the same code with a different
    // message and must not be swallowed.
    let message = error.message.to_lowercase();
    message.contains("cannot write a tuple which already exists")
        || message.contains("cannot delete a tuple which does not exist")
}

#[cfg(test)]
mod tests {
    use super::is_idempotent_tuple_error;

    #[test]
    fn idempotent_tuple_error_swallows_converged_write_and_delete_conflicts() {
        // Pins: OpenFGA's duplicate-write and delete-nonexistent errors are the
        // only bodies treated as already-converged so the outbox can mark the row
        // succeeded. Classification is structural — the machine `code` plus the
        // stable idempotent message — so a genuinely invalid write that reuses the
        // same code, an unexpected code, a missing field, or an unparseable body
        // all surface as real failures. Otherwise a dropped FGA write would be
        // silently swallowed on a wording change.

        // Real OpenFGA duplicate-write body: converged, safe to swallow.
        let duplicate = r#"{"code":"write_failed_due_to_invalid_input","message":"cannot write a tuple which already exists: user: 'operator:a', relation: 'member', object: 'tenant:b': invalid write input"}"#;
        // Real OpenFGA delete-nonexistent body: converged, safe to swallow.
        let delete_missing = r#"{"code":"write_failed_due_to_invalid_input","message":"cannot delete a tuple which does not exist: user: 'operator:a', relation: 'member', object: 'tenant:b': invalid write input"}"#;
        // Same invalid-input code, but a genuinely invalid write (unknown type):
        // must NOT be swallowed even though the code matches.
        let invalid_write = r#"{"code":"write_failed_due_to_invalid_input","message":"type 'nonexistent' not found"}"#;
        // A different structured code is never idempotent.
        let other_code =
            r#"{"code":"validation_error","message":"invalid authorization model id"}"#;

        let cases = [
            (duplicate, true),
            (delete_missing, true),
            // Same code, non-idempotent message: real failure.
            (invalid_write, false),
            // Different code: real failure.
            (other_code, false),
            // Fail closed on bodies that cannot be parsed as a structured error.
            ("cannot write a tuple which already exists", false),
            // Missing the `code` field: cannot classify, fail closed.
            (
                r#"{"message":"cannot write a tuple which already exists"}"#,
                false,
            ),
            // Matching code but missing the `message` field: fail closed.
            (r#"{"code":"write_failed_due_to_invalid_input"}"#, false),
            ("", false),
        ];

        for (body, expected) in cases {
            assert_eq!(
                is_idempotent_tuple_error(body),
                expected,
                "body={body:?} should map to {expected}"
            );
        }
    }
}
