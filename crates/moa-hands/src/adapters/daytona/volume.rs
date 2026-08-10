//! Exact Daytona volume REST shapes and typed lifecycle outcomes.

use moa_core::error::{MoaError, Result};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::{
    adapters::http_util::{expect_success, expect_success_json, http_error},
    core::provider_credentials::ProviderHttpAttempt,
};

/// Daytona's documented volume lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaytonaVolumeState {
    /// Provider create is running.
    Creating,
    /// Volume is mountable.
    Ready,
    /// Create is queued.
    PendingCreate,
    /// Delete is queued.
    PendingDelete,
    /// Provider delete is running.
    Deleting,
    /// Provider reports the resource deleted.
    Deleted,
    /// Provider reports a terminal error.
    Error,
}

/// Exact public Daytona `VolumeDto` response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DaytonaVolume {
    /// Opaque provider volume ID.
    pub id: String,
    /// Deterministic MOA create name.
    pub name: String,
    /// Daytona organization owner.
    pub organization_id: String,
    /// Current provider lifecycle state.
    pub state: DaytonaVolumeState,
    /// Provider creation timestamp.
    pub created_at: String,
    /// Provider update timestamp.
    pub updated_at: String,
    /// Last mount/use timestamp, when supplied.
    pub last_used_at: Option<String>,
    /// Provider terminal error, when any.
    pub error_reason: Option<String>,
}

#[derive(Serialize)]
struct CreateVolumeRequest<'a> {
    name: &'a str,
}

/// One exact sandbox-create volume mount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaytonaSandboxVolumeMount<'a> {
    /// Exact verified volume ID.
    pub volume_id: &'a str,
    /// Trusted absolute mount path.
    pub mount_path: &'a str,
    /// Opaque provider-enforced tenant/workspace subpath.
    pub subpath: &'a str,
}

/// Typed result of a Daytona asynchronous volume delete request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaytonaVolumeDeleteOutcome {
    /// Delete was accepted and absence must be polled.
    Accepted,
    /// The exact resource is already absent.
    Absent,
    /// Daytona refused because at least one sandbox still mounts the volume.
    MountedConflict,
}

/// Creates a deterministic tenant volume.
pub async fn create_volume(attempt: &ProviderHttpAttempt, name: &str) -> Result<DaytonaVolume> {
    validate_name(name)?;
    let response = attempt
        .client()
        .post(format!("{}/api/volumes", attempt.origin()))
        .bearer_auth(attempt.credential())
        .json(&CreateVolumeRequest { name })
        .send()
        .await
        .map_err(provider_transport)?;
    typed_volume_response(response, "create").await
}

/// Lists the complete current organization inventory as a bare DTO array.
pub async fn list_volumes(attempt: &ProviderHttpAttempt) -> Result<Vec<DaytonaVolume>> {
    let response = attempt
        .client()
        .get(format!(
            "{}/api/volumes?includeDeleted=false",
            attempt.origin()
        ))
        .bearer_auth(attempt.credential())
        .send()
        .await
        .map_err(provider_transport)?;
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        return Err(rate_limited(response));
    }
    let value = expect_success_json(response, "Daytona volumes").await?;
    serde_json::from_value(value).map_err(|error| {
        MoaError::ProviderError(format!("invalid Daytona volume inventory: {error}"))
    })
}

/// Gets one exact volume by provider ID; `None` means confirmed HTTP 404.
pub async fn get_volume(
    attempt: &ProviderHttpAttempt,
    volume_id: &str,
) -> Result<Option<DaytonaVolume>> {
    exact_get(attempt, exact_url(attempt, &["api", "volumes", volume_id])?).await
}

/// Gets one exact volume by deterministic create name.
pub async fn get_volume_by_name(
    attempt: &ProviderHttpAttempt,
    name: &str,
) -> Result<Option<DaytonaVolume>> {
    validate_name(name)?;
    exact_get(
        attempt,
        exact_url(attempt, &["api", "volumes", "by-name", name])?,
    )
    .await
}

/// Requests asynchronous deletion of one exact volume.
pub async fn delete_volume(
    attempt: &ProviderHttpAttempt,
    volume_id: &str,
) -> Result<DaytonaVolumeDeleteOutcome> {
    let response = attempt
        .client()
        .delete(exact_url(attempt, &["api", "volumes", volume_id])?)
        .bearer_auth(attempt.credential())
        .send()
        .await
        .map_err(provider_transport)?;
    match response.status() {
        StatusCode::NOT_FOUND => Ok(DaytonaVolumeDeleteOutcome::Absent),
        StatusCode::CONFLICT => Ok(DaytonaVolumeDeleteOutcome::MountedConflict),
        StatusCode::TOO_MANY_REQUESTS => Err(rate_limited(response)),
        status if status.is_success() => {
            expect_success(response).await?;
            Ok(DaytonaVolumeDeleteOutcome::Accepted)
        }
        _ => Err(http_error(response).await),
    }
}

async fn exact_get(
    attempt: &ProviderHttpAttempt,
    url: reqwest::Url,
) -> Result<Option<DaytonaVolume>> {
    let response = attempt
        .client()
        .get(url)
        .bearer_auth(attempt.credential())
        .send()
        .await
        .map_err(provider_transport)?;
    match response.status() {
        StatusCode::NOT_FOUND => Ok(None),
        StatusCode::TOO_MANY_REQUESTS => Err(rate_limited(response)),
        _ => typed_volume_response(response, "get").await.map(Some),
    }
}

fn exact_url(attempt: &ProviderHttpAttempt, segments: &[&str]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(attempt.origin()).map_err(|error| {
        MoaError::ConfigError(format!("invalid Daytona volume API origin: {error}"))
    })?;
    {
        let mut path = url.path_segments_mut().map_err(|_| {
            MoaError::ConfigError("Daytona volume API origin cannot be a base URL".to_string())
        })?;
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
    }
    Ok(url)
}

async fn typed_volume_response(
    response: reqwest::Response,
    operation: &str,
) -> Result<DaytonaVolume> {
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        return Err(rate_limited(response));
    }
    let value = expect_success_json(response, "Daytona volume").await?;
    serde_json::from_value(value).map_err(|error| {
        MoaError::ProviderError(format!(
            "invalid Daytona volume {operation} response: {error}"
        ))
    })
}

fn rate_limited(response: reqwest::Response) -> MoaError {
    let retry_after = response
        .headers()
        .iter()
        .find(|(name, _)| {
            name.as_str()
                .to_ascii_lowercase()
                .starts_with("retry-after")
        })
        .and_then(|(_, value)| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(std::time::Duration::from_secs);
    MoaError::HttpStatus {
        status: StatusCode::TOO_MANY_REQUESTS.as_u16(),
        retry_after,
        message: "Daytona volume API rate limit exceeded".to_string(),
    }
}

fn provider_transport(error: reqwest::Error) -> MoaError {
    MoaError::ProviderTransport(format!("Daytona volume request failed: {error}"))
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(MoaError::ValidationError(
            "Daytona volume name must be a bounded opaque identifier".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DaytonaSandboxVolumeMount, DaytonaVolume, DaytonaVolumeState, validate_name};

    #[test]
    fn exact_volume_dto_and_mount_shapes_decode_offline() {
        // Pins: Daytona's bare DTO fields and camelCase mount request stay exact.
        let dto: DaytonaVolume = serde_json::from_value(serde_json::json!({
            "id": "vol-1",
            "name": "moa-tenant-1",
            "organizationId": "org-1",
            "state": "ready",
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:01Z",
            "lastUsedAt": null,
            "errorReason": null
        }))
        .expect("documented VolumeDto should decode");
        assert_eq!(dto.state, DaytonaVolumeState::Ready);
        let mount = serde_json::to_value(DaytonaSandboxVolumeMount {
            volume_id: "vol-1",
            mount_path: "/workspace",
            subpath: "moa-deadbeef",
        })
        .expect("mount should encode");
        assert_eq!(
            mount,
            serde_json::json!({
                "volumeId": "vol-1",
                "mountPath": "/workspace",
                "subpath": "moa-deadbeef"
            })
        );
        assert!(validate_name("../tenant").is_err());
    }
}
