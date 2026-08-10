//! Durable Daytona tenant-volume ownership, markers, and checkpoint dependencies.

use std::sync::Arc;
use std::{os::unix::fs::PermissionsExt, path::Path};

use base64::Engine;
use futures_util::StreamExt;
use moa_config::DaytonaStorageConfig;
use moa_core::{
    canonical_json::canonical_json_bytes,
    error::{MoaError, Result},
    types::{
        hands::validate_sandbox_file_path,
        identifiers::WorkspaceOperationId,
        sandbox_workspace::{ProviderStorageKind, ProviderStorageRef, WorkspaceStorageOperation},
    },
};
use moa_crypto::{Ciphertext, EncryptionContext, KeyManagementProvider, decrypt, encrypt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

use crate::core::{
    provider_credentials::ProviderHttpAttempt,
    sandbox_workspace::{
        capacity::PostgresWorkspaceCapacityRepository,
        checkpoint::{archive::ArchiveLimits, store::CheckpointObjectStore},
        operations::PostgresWorkspaceOperationRepository,
        repository::PostgresWorkspaceRepository,
        storage_resources::PostgresWorkspaceStorageResourceRepository,
    },
};

const MARKER_CLASS: &str = "sandbox_workspace_marker_v1";

/// All durable owners required by Daytona persistent workspace storage.
#[derive(Clone)]
pub struct DaytonaStorageDependencies {
    /// Operator-authored organization cells, security classes, and headroom.
    pub config: DaytonaStorageConfig,
    /// Portable checkpoint recovery authority.
    pub checkpoint_store: Arc<CheckpointObjectStore>,
    /// Logical workspace lifecycle repository.
    pub workspaces: Arc<PostgresWorkspaceRepository>,
    /// Provider storage-resource ownership repository.
    pub storage_resources: Arc<PostgresWorkspaceStorageResourceRepository>,
    /// Durable provider operation ledger.
    pub operations: Arc<PostgresWorkspaceOperationRepository>,
    /// Atomic capacity and byte reservation owner.
    pub capacity: Arc<PostgresWorkspaceCapacityRepository>,
    /// Durable envelope-key authority for mounted-subpath ownership markers.
    pub kms: Arc<dyn KeyManagementProvider>,
}

impl DaytonaStorageDependencies {
    /// Validates that every configured storage cell is usable before registration.
    pub fn validate(&self) -> Result<()> {
        self.config.validate()?;
        if self.config.accounts.is_empty() {
            return Err(MoaError::ConfigError(
                "Daytona persistent workspaces require at least one storage account cell"
                    .to_string(),
            ));
        }
        if !self.kms.is_durable() {
            return Err(MoaError::ConfigError(
                "Daytona workspace markers require a durable KMS provider".to_string(),
            ));
        }
        Ok(())
    }
}

/// Claims authenticated into each mounted workspace subpath.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaytonaWorkspaceMarkerClaims {
    /// Exact durable operation writing the marker.
    pub operation_id: WorkspaceOperationId,
    /// Canonical create/attach request hash.
    pub request_hash: String,
    /// Canonical hand provisioning spec hash.
    pub spec_hash: String,
    /// Exact writer fencing epoch.
    pub writer_epoch: u64,
    /// Exact compute instance generation.
    pub instance_generation: u64,
}

/// Persistable marker containing only wrapped/encrypted material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedDaytonaWorkspaceMarker {
    /// Versioned envelope ciphertext encoded without plaintext key material.
    pub envelope_base64: String,
}

/// Derives an opaque, provider-enforced subpath from typed ownership only.
#[must_use]
pub fn workspace_subpath(operation: &WorkspaceStorageOperation) -> String {
    let mut digest = Sha256::new();
    digest.update(b"moa/daytona/workspace-subpath/v1\0");
    digest.update(operation.binding.tenant_id.0.as_bytes());
    digest.update(operation.binding.workspace_id.0.as_bytes());
    format!("moa-{}", hex::encode(digest.finalize()))
}

/// Validates a derived Daytona volume subpath before provider I/O.
pub fn validate_workspace_subpath(subpath: &str) -> Result<()> {
    if subpath.len() != 68
        || !subpath.starts_with("moa-")
        || !subpath[4..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(MoaError::ValidationError(
            "Daytona workspace subpath is not a canonical opaque MOA locator".to_string(),
        ));
    }
    Ok(())
}

/// Seals exact ownership/fencing claims with a fresh KMS-generated DEK.
pub async fn seal_workspace_marker(
    kms: &dyn KeyManagementProvider,
    operation: &WorkspaceStorageOperation,
    claims: &DaytonaWorkspaceMarkerClaims,
) -> Result<SealedDaytonaWorkspaceMarker> {
    validate_marker_claims(operation, claims)?;
    let plaintext = canonical_json_bytes(claims).map_err(|error| {
        MoaError::StorageError(format!("serialize Daytona workspace marker: {error}"))
    })?;
    let context = marker_context(operation, claims)?;
    let sealed = encrypt(kms, &plaintext, &context)
        .await
        .map_err(map_crypto_error)?;
    Ok(SealedDaytonaWorkspaceMarker {
        envelope_base64: base64::engine::general_purpose::STANDARD.encode(sealed.to_bytes()),
    })
}

/// Authenticates a mounted marker against current durable ownership and fences.
pub async fn open_workspace_marker(
    kms: &dyn KeyManagementProvider,
    operation: &WorkspaceStorageOperation,
    expected: &DaytonaWorkspaceMarkerClaims,
    marker: &SealedDaytonaWorkspaceMarker,
) -> Result<()> {
    validate_marker_claims(operation, expected)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&marker.envelope_base64)
        .map_err(|_| MoaError::ValidationError("invalid Daytona marker encoding".to_string()))?;
    let ciphertext = Ciphertext::from_bytes(&bytes).map_err(map_crypto_error)?;
    let plaintext = decrypt(kms, &ciphertext, &marker_context(operation, expected)?)
        .await
        .map_err(map_crypto_error)?;
    let claims: DaytonaWorkspaceMarkerClaims =
        serde_json::from_slice(&plaintext).map_err(|_| {
            MoaError::ValidationError("invalid authenticated Daytona marker payload".to_string())
        })?;
    if &claims != expected {
        return Err(MoaError::ValidationError(
            "Daytona marker claims do not match current workspace fences".to_string(),
        ));
    }
    Ok(())
}

/// Builds an exact mutable-storage reference for one tenant volume subpath.
pub fn mutable_storage_reference(
    operation: &WorkspaceStorageOperation,
    volume_id: impl Into<String>,
) -> Result<ProviderStorageRef> {
    let locator = workspace_subpath(operation);
    validate_workspace_subpath(&locator)?;
    let volume_id = volume_id.into();
    if volume_id.trim().is_empty() {
        return Err(MoaError::ValidationError(
            "Daytona mutable storage requires an exact volume id".to_string(),
        ));
    }
    Ok(ProviderStorageRef {
        provider_account_id: operation.binding.provider_account_id,
        provider_account_generation: operation.binding.provider_account_generation,
        kind: ProviderStorageKind::MutableFilesystem,
        resource_id: volume_id,
        workspace_locator: Some(locator),
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaytonaFileInfo {
    path: String,
    is_dir: bool,
    size: u64,
    mode: String,
    #[serde(default)]
    is_symlink: bool,
    #[serde(default)]
    mod_time: Option<String>,
}

/// Downloads one mounted data root through Daytona's toolbox API into a fresh
/// bounded host staging directory for canonical archive construction.
pub async fn materialize_workspace_for_checkpoint(
    attempt: &ProviderHttpAttempt,
    sandbox_id: &str,
    mount_path: &str,
    limits: ArchiveLimits,
) -> Result<TempDir> {
    let mut url = toolbox_file_url(attempt, sandbox_id, "files")?;
    url.query_pairs_mut()
        .append_pair("path", mount_path)
        .append_pair("depth", &limits.max_path_depth.to_string());
    let response = attempt
        .client()
        .get(url)
        .bearer_auth(attempt.credential())
        .send()
        .await
        .map_err(|error| {
            MoaError::ProviderTransport(format!("list Daytona workspace files: {error}"))
        })?;
    let response = response.error_for_status().map_err(|error| {
        MoaError::ProviderError(format!("list Daytona workspace files: {error}"))
    })?;
    let files: Vec<DaytonaFileInfo> = response.json().await.map_err(|error| {
        MoaError::ProviderError(format!("decode Daytona workspace file inventory: {error}"))
    })?;
    if files.len() > limits.max_entries {
        return Err(MoaError::ValidationError(
            "Daytona workspace exceeds checkpoint entry limit".to_string(),
        ));
    }
    let staging = tempfile::tempdir().map_err(|error| {
        MoaError::StorageError(format!("create Daytona checkpoint staging root: {error}"))
    })?;
    let mut logical_bytes = 0_u64;
    for file in files {
        let _ = &file.mod_time;
        if file.is_symlink {
            return Err(MoaError::ValidationError(
                "Daytona workspace file inventory contains an unsupported symbolic link"
                    .to_string(),
            ));
        }
        let relative = remote_relative_path(mount_path, &file.path)?;
        if relative == ".moa-workspace-marker.v1.json" {
            continue;
        }
        validate_sandbox_file_path(&relative)?;
        let local = staging.path().join(&relative);
        if file.is_dir {
            tokio::fs::create_dir_all(&local).await.map_err(|error| {
                MoaError::StorageError(format!("stage Daytona directory: {error}"))
            })?;
        } else {
            logical_bytes = limits.checked_add_file_bytes(
                logical_bytes,
                file.size,
                "Daytona workspace inventory",
            )?;
            if let Some(parent) = local.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|error| {
                    MoaError::StorageError(format!("stage Daytona file parent: {error}"))
                })?;
            }
            download_file_bounded(
                attempt,
                sandbox_id,
                &file.path,
                &local,
                file.size,
                limits.max_file_bytes,
            )
            .await?;
        }
        let mode = parse_daytona_mode(&file.mode)?;
        tokio::fs::set_permissions(&local, std::fs::Permissions::from_mode(mode & 0o7777))
            .await
            .map_err(|error| {
                MoaError::StorageError(format!("apply staged Daytona permissions: {error}"))
            })?;
    }
    Ok(staging)
}

/// Returns whether the exact mounted workspace root is empty.
///
/// Callers use two observations separated by the configured provider
/// consistency window before acknowledging destructive cleanup.
pub async fn workspace_root_is_empty(
    attempt: &ProviderHttpAttempt,
    sandbox_id: &str,
    mount_path: &str,
) -> Result<bool> {
    let mut url = toolbox_file_url(attempt, sandbox_id, "files")?;
    url.query_pairs_mut()
        .append_pair("path", mount_path)
        .append_pair("depth", "1");
    let response = attempt
        .client()
        .get(url)
        .bearer_auth(attempt.credential())
        .send()
        .await
        .map_err(|error| {
            MoaError::ProviderTransport(format!(
                "observe Daytona workspace cleanup inventory: {error}"
            ))
        })?;
    let response = response.error_for_status().map_err(|error| {
        MoaError::ProviderError(format!(
            "observe Daytona workspace cleanup inventory: {error}"
        ))
    })?;
    let files: Vec<DaytonaFileInfo> = response.json().await.map_err(|error| {
        MoaError::ProviderError(format!(
            "decode Daytona workspace cleanup inventory: {error}"
        ))
    })?;
    Ok(files.is_empty())
}

#[derive(Debug)]
struct RestoreEntry {
    relative: String,
    local: std::path::PathBuf,
    is_dir: bool,
    mode: u32,
    size: u64,
}

/// Uploads one verified local restore tree into a fresh Daytona mount.
pub async fn upload_restored_workspace(
    attempt: &ProviderHttpAttempt,
    sandbox_id: &str,
    mount_path: &str,
    local_root: &Path,
    limits: ArchiveLimits,
) -> Result<()> {
    let root = local_root.to_path_buf();
    let entries = tokio::task::spawn_blocking(move || collect_restore_entries(&root, limits))
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("join Daytona restore inventory task: {error}"))
        })??;
    for entry in &entries {
        validate_daytona_volume_restore_mode(entry.is_dir, entry.mode)?;
    }
    for entry in entries.iter().filter(|entry| entry.is_dir) {
        let remote = format!("{}/{}", mount_path.trim_end_matches('/'), entry.relative);
        let mut url = toolbox_file_url(attempt, sandbox_id, "files/folder")?;
        url.query_pairs_mut()
            .append_pair("path", &remote)
            .append_pair("mode", "777");
        attempt
            .client()
            .post(url)
            .bearer_auth(attempt.credential())
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderTransport(format!("create Daytona restore directory: {error}"))
            })?
            .error_for_status()
            .map_err(|error| {
                MoaError::ProviderError(format!("create Daytona restore directory: {error}"))
            })?;
    }
    for entry in entries.iter().filter(|entry| !entry.is_dir) {
        let bytes = tokio::fs::read(&entry.local).await.map_err(|error| {
            MoaError::StorageError(format!("read verified Daytona restore file: {error}"))
        })?;
        if u64::try_from(bytes.len()).ok() != Some(entry.size) {
            return Err(MoaError::StorageError(
                "verified Daytona restore file changed during upload".to_string(),
            ));
        }
        let remote = format!("{}/{}", mount_path.trim_end_matches('/'), entry.relative);
        let mut url = toolbox_file_url(attempt, sandbox_id, "files/upload")?;
        url.query_pairs_mut().append_pair("path", &remote);
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(bytes).file_name("workspace-restore"),
        );
        attempt
            .client()
            .post(url)
            .bearer_auth(attempt.credential())
            .multipart(form)
            .send()
            .await
            .map_err(|error| {
                MoaError::ProviderTransport(format!("upload Daytona restore file: {error}"))
            })?
            .error_for_status()
            .map_err(|error| {
                MoaError::ProviderError(format!("upload Daytona restore file: {error}"))
            })?;
    }
    Ok(())
}

fn validate_daytona_volume_restore_mode(is_dir: bool, mode: u32) -> Result<()> {
    let expected = if is_dir { 0o777 } else { 0o666 };
    if mode & 0o7777 != expected {
        return Err(MoaError::Unsupported(format!(
            "Daytona volume restore cannot represent mode {:o}; mounted directories require 777 and files require 666",
            mode & 0o7777
        )));
    }
    Ok(())
}

fn collect_restore_entries(root: &Path, limits: ArchiveLimits) -> Result<Vec<RestoreEntry>> {
    fn walk(
        root: &Path,
        current: &Path,
        limits: ArchiveLimits,
        entries: &mut Vec<RestoreEntry>,
        logical_bytes: &mut u64,
    ) -> Result<()> {
        for item in std::fs::read_dir(current).map_err(|error| {
            MoaError::StorageError(format!("read verified Daytona restore tree: {error}"))
        })? {
            let item = item.map_err(|error| {
                MoaError::StorageError(format!("read Daytona restore entry: {error}"))
            })?;
            let path = item.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                MoaError::StorageError(format!("stat Daytona restore entry: {error}"))
            })?;
            if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
                return Err(MoaError::ValidationError(
                    "Daytona restore tree contains an unsupported entry".to_string(),
                ));
            }
            let relative = path
                .strip_prefix(root)
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| {
                    MoaError::ValidationError(
                        "Daytona restore path is not normalized UTF-8".to_string(),
                    )
                })?
                .replace(std::path::MAIN_SEPARATOR, "/");
            validate_sandbox_file_path(&relative)?;
            if entries.len() >= limits.max_entries {
                return Err(MoaError::ValidationError(
                    "Daytona restore exceeds entry limit".to_string(),
                ));
            }
            if metadata.is_file() {
                *logical_bytes = limits.checked_add_file_bytes(
                    *logical_bytes,
                    metadata.len(),
                    "Daytona restore",
                )?;
            }
            entries.push(RestoreEntry {
                relative,
                local: path.clone(),
                is_dir: metadata.is_dir(),
                mode: metadata.permissions().mode(),
                size: metadata.len(),
            });
            if metadata.is_dir() {
                walk(root, &path, limits, entries, logical_bytes)?;
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    let mut logical_bytes = 0;
    walk(root, root, limits, &mut entries, &mut logical_bytes)?;
    entries.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(entries)
}

/// Uploads the KMS-sealed ownership marker without exposing key material.
pub async fn upload_workspace_marker(
    attempt: &ProviderHttpAttempt,
    sandbox_id: &str,
    mount_path: &str,
    marker: &SealedDaytonaWorkspaceMarker,
) -> Result<()> {
    let marker_bytes = serde_json::to_vec(marker).map_err(|error| {
        MoaError::StorageError(format!("serialize Daytona marker envelope: {error}"))
    })?;
    let remote_path = format!(
        "{}/.moa-workspace-marker.v1.json",
        mount_path.trim_end_matches('/')
    );
    let mut url = toolbox_file_url(attempt, sandbox_id, "files/upload")?;
    url.query_pairs_mut().append_pair("path", &remote_path);
    let form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(marker_bytes).file_name("workspace-marker.json"),
    );
    attempt
        .client()
        .post(url)
        .bearer_auth(attempt.credential())
        .multipart(form)
        .send()
        .await
        .map_err(|error| {
            MoaError::ProviderTransport(format!("upload Daytona workspace marker: {error}"))
        })?
        .error_for_status()
        .map_err(|error| {
            MoaError::ProviderError(format!("upload Daytona workspace marker: {error}"))
        })?;
    Ok(())
}

/// Downloads the bounded sealed marker envelope for KMS authentication.
pub async fn download_workspace_marker(
    attempt: &ProviderHttpAttempt,
    sandbox_id: &str,
    mount_path: &str,
) -> Result<SealedDaytonaWorkspaceMarker> {
    let remote_path = format!(
        "{}/.moa-workspace-marker.v1.json",
        mount_path.trim_end_matches('/')
    );
    let mut url = toolbox_file_url(attempt, sandbox_id, "files/download")?;
    url.query_pairs_mut().append_pair("path", &remote_path);
    let response = attempt
        .client()
        .get(url)
        .bearer_auth(attempt.credential())
        .send()
        .await
        .map_err(|error| {
            MoaError::ProviderTransport(format!("download Daytona workspace marker: {error}"))
        })?
        .error_for_status()
        .map_err(|error| {
            MoaError::ProviderError(format!("download Daytona workspace marker: {error}"))
        })?;
    if response
        .content_length()
        .is_some_and(|length| length > 64 * 1024)
    {
        return Err(MoaError::ValidationError(
            "Daytona workspace marker exceeds its bounded envelope size".to_string(),
        ));
    }
    let bytes = response.bytes().await.map_err(|error| {
        MoaError::ProviderTransport(format!("read Daytona workspace marker: {error}"))
    })?;
    if bytes.len() > 64 * 1024 {
        return Err(MoaError::ValidationError(
            "Daytona workspace marker exceeds its bounded envelope size".to_string(),
        ));
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        MoaError::ValidationError("invalid Daytona workspace marker envelope".to_string())
    })
}

async fn download_file_bounded(
    attempt: &ProviderHttpAttempt,
    sandbox_id: &str,
    remote_path: &str,
    local_path: &Path,
    declared_size: u64,
    max_file_bytes: u64,
) -> Result<()> {
    let mut url = toolbox_file_url(attempt, sandbox_id, "files/download")?;
    url.query_pairs_mut().append_pair("path", remote_path);
    let response = attempt
        .client()
        .get(url)
        .bearer_auth(attempt.credential())
        .send()
        .await
        .map_err(|error| {
            MoaError::ProviderTransport(format!("download Daytona workspace file: {error}"))
        })?
        .error_for_status()
        .map_err(|error| {
            MoaError::ProviderError(format!("download Daytona workspace file: {error}"))
        })?;
    if response
        .content_length()
        .is_some_and(|length| length != declared_size || length > max_file_bytes)
    {
        return Err(MoaError::ValidationError(
            "Daytona file download length violates the verified inventory bound".to_string(),
        ));
    }
    let mut output = tokio::fs::File::create(local_path)
        .await
        .map_err(|error| MoaError::StorageError(format!("create staged Daytona file: {error}")))?;
    let mut received = 0_u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            MoaError::ProviderTransport(format!("stream Daytona workspace file: {error}"))
        })?;
        received = received
            .checked_add(u64::try_from(chunk.len()).map_err(|_| {
                MoaError::ValidationError("Daytona file chunk length overflows u64".to_string())
            })?)
            .ok_or_else(|| {
                MoaError::ValidationError("Daytona file download length overflow".to_string())
            })?;
        if received > declared_size || received > max_file_bytes {
            return Err(MoaError::ValidationError(
                "Daytona file download exceeded its verified bound".to_string(),
            ));
        }
        output.write_all(&chunk).await.map_err(|error| {
            MoaError::StorageError(format!("write staged Daytona file: {error}"))
        })?;
    }
    if received != declared_size {
        return Err(MoaError::ValidationError(
            "Daytona file download did not match its verified inventory size".to_string(),
        ));
    }
    output
        .flush()
        .await
        .map_err(|error| MoaError::StorageError(format!("flush staged Daytona file: {error}")))?;
    Ok(())
}

fn toolbox_file_url(
    attempt: &ProviderHttpAttempt,
    sandbox_id: &str,
    suffix: &str,
) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(attempt.origin()).map_err(|error| {
        MoaError::ConfigError(format!("invalid Daytona toolbox origin: {error}"))
    })?;
    {
        let mut segments = url.path_segments_mut().map_err(|_| {
            MoaError::ConfigError("Daytona toolbox origin cannot be a base URL".to_string())
        })?;
        segments.pop_if_empty().push("toolbox").push(sandbox_id);
        for segment in suffix.split('/') {
            segments.push(segment);
        }
    }
    Ok(url)
}

fn remote_relative_path(mount_path: &str, remote_path: &str) -> Result<String> {
    let remote = remote_path.trim_start_matches('/');
    let mount = mount_path.trim_matches('/');
    let relative = remote
        .strip_prefix(mount)
        .map(|value| value.trim_start_matches('/'))
        .filter(|value| !value.is_empty())
        .unwrap_or(remote);
    Ok(relative.to_string())
}

fn parse_daytona_mode(mode: &str) -> Result<u32> {
    let octal = mode.strip_prefix("0o").unwrap_or(mode);
    if !octal.is_empty() && octal.bytes().all(|byte| matches!(byte, b'0'..=b'7')) {
        return u32::from_str_radix(octal, 8).map_err(|_| {
            MoaError::ValidationError("Daytona file inventory contains an invalid mode".to_string())
        });
    }

    let bytes = mode.as_bytes();
    if bytes.len() != 10 || !matches!(bytes[0], b'-' | b'd') {
        return Err(MoaError::ValidationError(
            "Daytona file inventory contains an invalid mode".to_string(),
        ));
    }
    let mut parsed = 0_u32;
    for (index, expected, bit) in [
        (1, b'r', 0o400),
        (2, b'w', 0o200),
        (4, b'r', 0o040),
        (5, b'w', 0o020),
        (7, b'r', 0o004),
        (8, b'w', 0o002),
    ] {
        match bytes[index] {
            b'-' => {}
            value if value == expected => parsed |= bit,
            _ => {
                return Err(MoaError::ValidationError(
                    "Daytona file inventory contains an invalid mode".to_string(),
                ));
            }
        }
    }
    for (index, execute_bit, special_bit, special_lower, special_upper) in [
        (3, 0o100, 0o4000, b's', b'S'),
        (6, 0o010, 0o2000, b's', b'S'),
        (9, 0o001, 0o1000, b't', b'T'),
    ] {
        match bytes[index] {
            b'-' => {}
            b'x' => parsed |= execute_bit,
            value if value == special_lower => parsed |= execute_bit | special_bit,
            value if value == special_upper => parsed |= special_bit,
            _ => {
                return Err(MoaError::ValidationError(
                    "Daytona file inventory contains an invalid mode".to_string(),
                ));
            }
        }
    }
    Ok(parsed)
}

fn validate_marker_claims(
    operation: &WorkspaceStorageOperation,
    claims: &DaytonaWorkspaceMarkerClaims,
) -> Result<()> {
    if claims.operation_id != operation.operation_id
        || claims.request_hash != operation.request_hash
        || claims.request_hash.trim().is_empty()
        || claims.spec_hash.trim().is_empty()
        || claims.writer_epoch != operation.binding.writer_epoch
        || claims.instance_generation != operation.binding.instance_generation
    {
        return Err(MoaError::ValidationError(
            "Daytona marker claims do not match the durable workspace operation".to_string(),
        ));
    }
    Ok(())
}

fn marker_context(
    operation: &WorkspaceStorageOperation,
    claims: &DaytonaWorkspaceMarkerClaims,
) -> Result<EncryptionContext> {
    let aad = canonical_json_bytes(&serde_json::json!({
        "tenant_id": operation.binding.tenant_id,
        "workspace_id": operation.binding.workspace_id,
        "provider_account_id": operation.binding.provider_account_id,
        "provider_account_generation": operation.binding.provider_account_generation,
        "writer_epoch": claims.writer_epoch,
        "instance_generation": claims.instance_generation,
        "operation_id": claims.operation_id,
        "request_hash": claims.request_hash,
        "spec_hash": claims.spec_hash,
    }))
    .map_err(|error| MoaError::StorageError(format!("serialize Daytona marker AAD: {error}")))?;
    Ok(EncryptionContext::new(
        operation.binding.tenant_id.0,
        operation.binding.workspace_id.0,
        hex::encode(Sha256::digest(aad)),
        MARKER_CLASS,
    ))
}

fn map_crypto_error(error: moa_crypto::Error) -> MoaError {
    MoaError::StorageError(format!(
        "Daytona workspace marker cryptography failed: {error}"
    ))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::types::{
        identifiers::{
            ProviderAccountId, SandboxWorkspaceId, SessionId, TenantId, WorkspaceCheckpointId,
        },
        sandbox_workspace::{
            DurabilityClass, SandboxWorkspaceScope, WorkspaceBinding, WorkspaceOperationKind,
            WorkspaceRevisionRef,
        },
    };
    use moa_crypto::LocalKmsProvider;
    use uuid::Uuid;

    use super::*;

    fn operation() -> WorkspaceStorageOperation {
        WorkspaceStorageOperation {
            operation_id: WorkspaceOperationId(Uuid::from_u128(5)),
            kind: WorkspaceOperationKind::Attach,
            binding: WorkspaceBinding {
                tenant_id: TenantId(Uuid::from_u128(1)),
                scope: SandboxWorkspaceScope::Worker {
                    session_id: SessionId(Uuid::from_u128(2)),
                    worker_id: "worker".to_string(),
                },
                workspace_id: SandboxWorkspaceId(Uuid::from_u128(3)),
                provider_account_id: ProviderAccountId(Uuid::from_u128(4)),
                provider_account_generation: 2,
                durability_class: DurabilityClass::PortableFilesystem,
                writer_epoch: 7,
                instance_generation: 9,
                current_revision: Some(WorkspaceRevisionRef {
                    checkpoint_id: WorkspaceCheckpointId(Uuid::nil()),
                    generation: 0,
                    format_version: 1,
                }),
            },
            deadline: Utc::now(),
            request_hash: "request-hash".to_string(),
        }
    }

    fn claims() -> DaytonaWorkspaceMarkerClaims {
        DaytonaWorkspaceMarkerClaims {
            operation_id: WorkspaceOperationId(Uuid::from_u128(5)),
            request_hash: "request-hash".to_string(),
            spec_hash: "spec-hash".to_string(),
            writer_epoch: 7,
            instance_generation: 9,
        }
    }

    #[tokio::test]
    async fn marker_is_randomized_and_bound_to_every_workspace_fence_offline() {
        // Pins: API-key rotation cannot affect marker verification, while a
        // different writer/instance/account/operation cannot authenticate it.
        let kms = LocalKmsProvider::new();
        let operation = operation();
        let claims = claims();
        let first = seal_workspace_marker(&kms, &operation, &claims)
            .await
            .expect("first marker should seal");
        let second = seal_workspace_marker(&kms, &operation, &claims)
            .await
            .expect("second marker should seal with a fresh DEK/nonce");
        assert_ne!(first, second);
        open_workspace_marker(&kms, &operation, &claims, &first)
            .await
            .expect("exact claims should authenticate");

        let mut stale = operation.clone();
        stale.binding.writer_epoch += 1;
        let error = open_workspace_marker(&kms, &stale, &claims, &first)
            .await
            .expect_err("stale writer must fail closed");
        assert!(matches!(error, MoaError::ValidationError(_)));
    }

    #[test]
    fn workspace_subpath_is_opaque_stable_and_validated_offline() {
        // Pins: caller paths never influence the provider-enforced mount subpath.
        let subpath = workspace_subpath(&operation());
        assert_eq!(subpath, workspace_subpath(&operation()));
        validate_workspace_subpath(&subpath).expect("derived subpath should validate");
        assert!(!subpath.contains("worker"));
        assert!(validate_workspace_subpath("../tenant").is_err());
    }

    #[test]
    fn daytona_symbolic_and_octal_modes_are_parsed_exactly() {
        assert_eq!(parse_daytona_mode("-rw-rw-rw-").unwrap(), 0o666);
        assert_eq!(parse_daytona_mode("drwxrwxrwx").unwrap(), 0o777);
        assert_eq!(parse_daytona_mode("0o4750").unwrap(), 0o4750);
        assert!(parse_daytona_mode("lrwxrwxrwx").is_err());
    }

    #[test]
    fn daytona_volume_restore_refuses_unrepresentable_modes() {
        validate_daytona_volume_restore_mode(false, 0o666).unwrap();
        validate_daytona_volume_restore_mode(true, 0o777).unwrap();
        assert!(validate_daytona_volume_restore_mode(false, 0o755).is_err());
        assert!(validate_daytona_volume_restore_mode(true, 0o700).is_err());
    }
}
