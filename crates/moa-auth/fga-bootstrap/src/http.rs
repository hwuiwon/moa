//! Minimal OpenFGA HTTP client used only by the bootstrap binary.
//!
//! The production client wrapper lands in `moa-authz` in P1.2. This module is
//! intentionally small and explicit so bootstrap does not depend on later
//! authz crate work.

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write;
use std::process::Stdio;

/// Bootstrap-only OpenFGA HTTP client.
pub(crate) struct FgaClient {
    base: String,
    http: Client,
    token: String,
}

impl FgaClient {
    /// Build a client for an OpenFGA HTTP base URL and preshared key.
    pub(crate) fn new(base: &str, token: &str) -> Result<Self> {
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            http: Client::builder().build()?,
            token: token.to_string(),
        })
    }

    /// Find the first store with the given name.
    pub(crate) async fn find_store_by_name(&self, name: &str) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct StoresResponse {
            stores: Vec<Store>,
            #[serde(default)]
            continuation_token: String,
        }

        #[derive(Deserialize)]
        struct Store {
            id: String,
            name: String,
        }

        let mut continuation = String::new();
        loop {
            let stores_url = if continuation.is_empty() {
                format!("{}/stores", self.base)
            } else {
                format!("{}/stores?continuation_token={}", self.base, continuation)
            };
            let request = self.http.get(stores_url).bearer_auth(&self.token);

            let response = request
                .send()
                .await?
                .error_for_status()?
                .json::<StoresResponse>()
                .await?;

            if let Some(store) = response.stores.into_iter().find(|store| store.name == name) {
                return Ok(Some(store.id));
            }

            if response.continuation_token.is_empty() {
                return Ok(None);
            }
            continuation = response.continuation_token;
        }
    }

    /// Create a store and return its generated store ID.
    pub(crate) async fn create_store(&self, name: &str) -> Result<String> {
        let response = self
            .http
            .post(format!("{}/stores", self.base))
            .bearer_auth(&self.token)
            .json(&json!({ "name": name }))
            .send()
            .await?;
        let value = response_json_or_error(response, "CreateStore").await?;
        value
            .get("id")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned)
            .context("CreateStore response missing id")
    }

    /// Transform DSL to JSON via the `fga` CLI, then write the model.
    pub(crate) async fn write_authorization_model_from_dsl(
        &self,
        store_id: &str,
        dsl: &str,
    ) -> Result<String> {
        let json_model = transform_dsl_to_json(dsl).await?;
        let response = self
            .http
            .post(format!(
                "{}/stores/{}/authorization-models",
                self.base, store_id
            ))
            .bearer_auth(&self.token)
            .json(&json_model)
            .send()
            .await?;
        let value = response_json_or_error(response, "WriteAuthorizationModel").await?;
        value
            .get("authorization_model_id")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned)
            .context("WriteAuthorizationModel response missing authorization_model_id")
    }

    /// Write raw tuple keys to OpenFGA.
    pub(crate) async fn write_tuples_raw(
        &self,
        store_id: &str,
        model_id: &str,
        tuples: &[serde_json::Value],
    ) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/stores/{}/write", self.base, store_id))
            .bearer_auth(&self.token)
            .json(&json!({
                "authorization_model_id": model_id,
                "writes": { "tuple_keys": tuples },
            }))
            .send()
            .await?;
        response_unit_or_error(response, "Write").await
    }

    /// Delete raw tuple keys from OpenFGA.
    pub(crate) async fn delete_tuples_raw(
        &self,
        store_id: &str,
        model_id: &str,
        tuples: &[serde_json::Value],
    ) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/stores/{}/write", self.base, store_id))
            .bearer_auth(&self.token)
            .json(&json!({
                "authorization_model_id": model_id,
                "deletes": { "tuple_keys": tuples },
            }))
            .send()
            .await?;
        response_unit_or_error(response, "Delete").await
    }

    /// Best-effort smoke cleanup for stale tuples from a previous failed run.
    pub(crate) async fn delete_tuples_raw_best_effort(
        &self,
        store_id: &str,
        model_id: &str,
        tuples: &[serde_json::Value],
    ) {
        if let Err(error) = self.delete_tuples_raw(store_id, model_id, tuples).await {
            tracing::debug!(%error, "ignoring best-effort smoke tuple cleanup failure");
        }
    }

    /// Run a single OpenFGA Check request.
    pub(crate) async fn check(
        &self,
        store_id: &str,
        model_id: &str,
        user: &str,
        relation: &str,
        object: &str,
    ) -> Result<bool> {
        let response = self
            .http
            .post(format!("{}/stores/{}/check", self.base, store_id))
            .bearer_auth(&self.token)
            .json(&json!({
                "authorization_model_id": model_id,
                "tuple_key": { "user": user, "relation": relation, "object": object },
            }))
            .send()
            .await?;
        let value = response_json_or_error(response, "Check").await?;
        Ok(value
            .get("allowed")
            .and_then(|value| value.as_bool())
            .unwrap_or(false))
    }

    /// Run a ListObjects request for one user/relation/object type.
    pub(crate) async fn list_objects(
        &self,
        store_id: &str,
        model_id: &str,
        object_type: &str,
        relation: &str,
        user: &str,
    ) -> Result<Vec<String>> {
        let response = self
            .http
            .post(format!("{}/stores/{}/list-objects", self.base, store_id))
            .bearer_auth(&self.token)
            .json(&json!({
                "authorization_model_id": model_id,
                "type": object_type,
                "relation": relation,
                "user": user,
            }))
            .send()
            .await?;
        let value = response_json_or_error(response, "ListObjects").await?;
        Ok(value
            .get("objects")
            .and_then(|value| value.as_array())
            .map(|objects| {
                objects
                    .iter()
                    .filter_map(|object| object.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Run a BatchCheck request, preserving input order in the returned bools.
    pub(crate) async fn batch_check(
        &self,
        store_id: &str,
        model_id: &str,
        items: &[(String, String, String)],
    ) -> Result<Vec<bool>> {
        let checks = items
            .iter()
            .enumerate()
            .map(|(index, (user, relation, object))| {
                json!({
                    "tuple_key": {
                        "user": user,
                        "relation": relation,
                        "object": object,
                    },
                    "correlation_id": format!("c{index}"),
                })
            })
            .collect::<Vec<_>>();

        let response = self
            .http
            .post(format!("{}/stores/{}/batch-check", self.base, store_id))
            .bearer_auth(&self.token)
            .json(&json!({
                "authorization_model_id": model_id,
                "checks": checks,
            }))
            .send()
            .await?;
        let value = response_json_or_error(response, "BatchCheck").await?;
        parse_batch_check_allowed(&value, items.len())
    }
}

async fn response_json_or_error(
    response: reqwest::Response,
    operation: &str,
) -> Result<serde_json::Value> {
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("{operation} failed {status}: {body}");
    }
    response
        .json::<serde_json::Value>()
        .await
        .with_context(|| format!("{operation} response JSON"))
}

async fn response_unit_or_error(response: reqwest::Response, operation: &str) -> Result<()> {
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("{operation} failed {status}: {body}");
    }
    Ok(())
}

fn parse_batch_check_allowed(value: &serde_json::Value, count: usize) -> Result<Vec<bool>> {
    if let Some(results) = value
        .get("result")
        .or_else(|| value.get("results"))
        .and_then(|value| value.as_object())
    {
        return batch_allowed_from_map(results, count);
    }

    if let Some(results) = value
        .get("result")
        .or_else(|| value.get("results"))
        .and_then(|value| value.as_array())
    {
        let mut by_correlation = BTreeMap::new();
        for result in results {
            let correlation_id = result
                .get("correlation_id")
                .or_else(|| result.get("correlationId"))
                .and_then(|value| value.as_str())
                .context("BatchCheck array entry missing correlation id")?;
            by_correlation.insert(
                correlation_id.to_string(),
                result
                    .get("allowed")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
            );
        }
        return batch_allowed_from_bool_map(&by_correlation, count);
    }

    bail!("BatchCheck response missing result/results: {value}");
}

fn batch_allowed_from_map(
    results: &serde_json::Map<String, serde_json::Value>,
    count: usize,
) -> Result<Vec<bool>> {
    let mut allowed = Vec::with_capacity(count);
    for index in 0..count {
        let key = format!("c{index}");
        let entry = results
            .get(&key)
            .with_context(|| format!("BatchCheck missing correlation entry {key}"))?;
        allowed.push(
            entry
                .get("allowed")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
        );
    }
    Ok(allowed)
}

fn batch_allowed_from_bool_map(
    results: &BTreeMap<String, bool>,
    count: usize,
) -> Result<Vec<bool>> {
    let mut allowed = Vec::with_capacity(count);
    for index in 0..count {
        let key = format!("c{index}");
        allowed.push(
            *results
                .get(&key)
                .with_context(|| format!("BatchCheck missing correlation entry {key}"))?,
        );
    }
    Ok(allowed)
}

async fn transform_dsl_to_json(dsl: &str) -> Result<serde_json::Value> {
    let mut temp = tempfile::Builder::new()
        .prefix("moa-fga-model-")
        .suffix(".fga")
        .tempfile()
        .context("create temporary OpenFGA model file")?;
    temp.write_all(dsl.as_bytes())
        .context("write temporary OpenFGA model file")?;
    let path = temp.path();
    let path_str = path
        .to_str()
        .with_context(|| format!("temporary path is not UTF-8: {}", path.display()))?;

    let output = tokio::process::Command::new(OsStr::new("fga"))
        .args([
            "model",
            "transform",
            "--file",
            path_str,
            "--input-format",
            "fga",
            "--output-format",
            "json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("invoke fga CLI; run `make fga-install` if it is missing")?;
    if !output.status.success() {
        bail!(
            "fga model transform failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    serde_json::from_slice(&output.stdout).context("parse fga model transform output")
}
