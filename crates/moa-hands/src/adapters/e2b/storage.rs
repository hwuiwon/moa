//! Portable filesystem checkpoint transfer for E2B sandboxes.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};

use futures_util::StreamExt as _;
use moa_core::error::{MoaError, Result};
use serde::{Deserialize, Deserializer, de::Error as _};
use sha2::{Digest as _, Sha256};
use tempfile::{Builder, TempDir};
use tokio::io::AsyncWriteExt as _;

use crate::adapters::http_util::{build_url, expect_success_json, http_error};
use crate::core::provider_credentials::ProviderSandboxAttempt;
use crate::core::sandbox_workspace::checkpoint::archive::{
    ArchiveEntryKind, ArchiveLimits, CheckpointArchive, build_checkpoint_archive,
};
use crate::core::sandbox_workspace::checkpoint::store::{
    CheckpointObjectStore, CheckpointStoreContext,
};

use super::{
    CONNECT_PROTOCOL_VERSION, ConnectedSandbox, E2BHandProvider, envd_headers, shell_escape,
};

/// Dedicated mutable tenant-data root inside every E2B sandbox.
pub(super) const E2B_DATA_ROOT: &str = "/workspace";

/// Builds the bounded shell command that makes the mutable root caller-owned.
///
/// Restore additionally requires an empty root so template or stale bytes can
/// never be folded into a checkpoint recovery.
pub(super) fn prepare_data_root_command(require_empty: bool) -> String {
    let root = shell_escape(E2B_DATA_ROOT);
    let empty_guard = if require_empty {
        format!(
            "if test -e {root}; then test -d {root} && test -z \"$(find {root} -mindepth 1 -maxdepth 1 -print -quit)\"; fi && "
        )
    } else {
        String::new()
    };
    format!(
        "{empty_guard}if install -d -m 700 {root} 2>/dev/null; then :; else sudo -n install -d -m 700 -o \"$(id -u)\" -g \"$(id -g)\" {root}; fi"
    )
}

#[derive(Debug, Deserialize)]
struct ListDirectoryResponse {
    entries: Vec<RemoteEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteEntry {
    path: String,
    #[serde(rename = "type")]
    entry_type: String,
    #[serde(default, deserialize_with = "deserialize_u64")]
    size: u64,
    #[serde(default)]
    mode: u32,
    #[serde(default)]
    symlink_target: Option<String>,
}

fn deserialize_u64<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum WireU64 {
        Number(u64),
        String(String),
    }

    match WireU64::deserialize(deserializer)? {
        WireU64::Number(value) => Ok(value),
        WireU64::String(value) => value.parse().map_err(D::Error::custom),
    }
}

/// Returns the provider-resource-specific prefix used for operation scratch directories.
pub(super) fn operation_temp_prefix(purpose: &str, sandbox_id: &str) -> String {
    let discriminator = format!("{:x}", Sha256::digest(sandbox_id.as_bytes()));
    format!(".moa-e2b-{purpose}-{}-", &discriminator[..16])
}

async fn create_operation_temp_dir(purpose: &str, sandbox_id: &str) -> Result<TempDir> {
    let prefix = operation_temp_prefix(purpose, sandbox_id);
    let directory = Builder::new().prefix(&prefix).tempdir().map_err(|error| {
        MoaError::StorageError(format!("create E2B operation temp directory: {error}"))
    })?;
    tokio::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("restrict E2B operation temp directory: {error}"))
        })?;
    Ok(directory)
}

async fn cleanup_operation_temp_dir(directory: TempDir) -> Result<()> {
    // Keep the owner alive through the explicit wipe so `TempDir::drop` can
    // still make a best-effort retry if the reported cleanup fails.
    match tokio::fs::remove_dir_all(directory.path()).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(MoaError::StorageError(format!(
            "wipe E2B operation temp directory: {error}"
        ))),
    }
}

/// Downloads only the dedicated mutable root and rebuilds it through the
/// canonical archive validator before publication.
pub(super) async fn export_data_root(
    provider: &E2BHandProvider,
    attempt: &ProviderSandboxAttempt,
    sandbox_id: &str,
    sandbox: &ConnectedSandbox,
    limits: ArchiveLimits,
) -> Result<CheckpointArchive> {
    let temporary = create_operation_temp_dir("export", sandbox_id).await?;
    let result = export_into_temp(provider, attempt, sandbox_id, sandbox, &temporary, limits).await;
    let cleanup = cleanup_operation_temp_dir(temporary).await;
    match (result, cleanup) {
        (Ok(archive), Ok(())) => Ok(archive),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

async fn export_into_temp(
    provider: &E2BHandProvider,
    attempt: &ProviderSandboxAttempt,
    sandbox_id: &str,
    sandbox: &ConnectedSandbox,
    temporary: &TempDir,
    limits: ArchiveLimits,
) -> Result<CheckpointArchive> {
    let root = temporary.path().join("data");
    tokio::fs::create_dir(&root).await?;
    let hard_links = provider
        .execute_bash(
            attempt,
            sandbox_id,
            sandbox,
            &format!(
                "find {root} -xdev -type f -links +1 -print -quit",
                root = shell_escape(E2B_DATA_ROOT)
            ),
            super::DEFAULT_COMMAND_TIMEOUT,
        )
        .await?;
    if hard_links.is_error
        || hard_links
            .process_stdout()
            .is_some_and(|stdout| !stdout.trim().is_empty())
    {
        return Err(MoaError::ValidationError(format!(
            "E2B export hard-link probe failed or found a hard-linked file: {}",
            hard_links.to_text()
        )));
    }
    let entries = list_data_root(attempt, sandbox_id, sandbox, limits.max_path_depth).await?;
    if entries.len() > limits.max_entries {
        return Err(MoaError::ValidationError(
            "E2B export contains too many filesystem entries".to_string(),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut announced_total = 0_u64;
    let mut directory_modes = Vec::new();
    for entry in entries {
        let relative = remote_relative_path(&entry.path)?;
        if !seen.insert(relative.clone()) {
            return Err(MoaError::ValidationError(
                "E2B export returned a duplicate filesystem path".to_string(),
            ));
        }
        let destination = root.join(&relative);
        match remote_entry_kind(&entry.entry_type)? {
            ArchiveEntryKind::Directory => {
                tokio::fs::create_dir_all(&destination).await?;
                directory_modes.push((destination, entry.mode));
            }
            ArchiveEntryKind::File => {
                announced_total =
                    limits.checked_add_file_bytes(announced_total, entry.size, "E2B export")?;
                if let Some(parent) = destination.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                download_file(
                    attempt,
                    sandbox_id,
                    sandbox,
                    &entry.path,
                    &destination,
                    entry.size,
                    limits.max_file_bytes,
                )
                .await?;
                set_mode(&destination, entry.mode).await?;
            }
            ArchiveEntryKind::Symlink => {
                let target = entry.symlink_target.as_deref().ok_or_else(|| {
                    MoaError::ValidationError("E2B export symlink omitted its target".to_string())
                })?;
                if let Some(parent) = destination.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::symlink(target, &destination).await?;
            }
        }
    }
    directory_modes.sort_unstable_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, mode) in directory_modes {
        set_mode(&path, mode).await?;
    }
    let archive = build_checkpoint_archive(&root, limits).await?;
    if archive.manifest.logical_bytes != announced_total {
        return Err(MoaError::ValidationError(
            "E2B export changed while its checkpoint was being collected".to_string(),
        ));
    }
    provider
        .run_checked_command(attempt, sandbox_id, sandbox, "export_sync", "sync")
        .await?;
    Ok(archive)
}

/// Restores and revalidates a canonical host root, then uploads only the safe
/// entries into a supplied fresh E2B sandbox.
pub(super) async fn import_data_root(
    provider: &E2BHandProvider,
    attempt: &ProviderSandboxAttempt,
    sandbox_id: &str,
    sandbox: &ConnectedSandbox,
    restored_root: &Path,
    limits: ArchiveLimits,
) -> Result<()> {
    let archive = build_checkpoint_archive(restored_root, limits).await?;
    let mut directory_modes = Vec::new();
    provider
        .run_checked_command(
            attempt,
            sandbox_id,
            sandbox,
            "restore_empty_root",
            &prepare_data_root_command(true),
        )
        .await?;
    for entry in &archive.manifest.entries {
        let local = restored_root.join(&entry.path);
        let remote = format!("{E2B_DATA_ROOT}/{}", entry.path);
        match entry.kind {
            ArchiveEntryKind::Directory => {
                provider
                    .run_checked_command(
                        attempt,
                        sandbox_id,
                        sandbox,
                        "restore_directory",
                        &format!("install -d -m 700 {path}", path = shell_escape(&remote)),
                    )
                    .await?;
                directory_modes.push((remote, entry.mode));
            }
            ArchiveEntryKind::File => {
                upload_file(attempt, sandbox_id, sandbox, &remote, &local, entry.size).await?;
                provider
                    .run_checked_command(
                        attempt,
                        sandbox_id,
                        sandbox,
                        "restore_file_mode",
                        &format!(
                            "chmod {mode:o} {path}",
                            mode = entry.mode,
                            path = shell_escape(&remote)
                        ),
                    )
                    .await?;
            }
            ArchiveEntryKind::Symlink => {
                let target = entry.link_target.as_deref().ok_or_else(|| {
                    MoaError::ValidationError(
                        "canonical E2B restore symlink omitted its target".to_string(),
                    )
                })?;
                provider
                    .run_checked_command(
                        attempt,
                        sandbox_id,
                        sandbox,
                        "restore_symlink",
                        &format!(
                            "ln -s -- {target} {path}",
                            target = shell_escape(target),
                            path = shell_escape(&remote)
                        ),
                    )
                    .await?;
            }
        }
    }
    directory_modes
        .sort_unstable_by_key(|(path, _)| std::cmp::Reverse(Path::new(path).components().count()));
    for (path, mode) in directory_modes {
        provider
            .run_checked_command(
                attempt,
                sandbox_id,
                sandbox,
                "restore_directory_mode",
                &format!("chmod {mode:o} {path}", path = shell_escape(&path)),
            )
            .await?;
    }
    provider
        .run_checked_command(attempt, sandbox_id, sandbox, "restore_sync", "sync")
        .await
}

/// Decrypts a portable checkpoint into a fresh operation root, validates it a
/// second time, uploads it into fresh compute, and wipes plaintext afterward.
pub(super) async fn restore_checkpoint_data_root(
    provider: &E2BHandProvider,
    store: &CheckpointObjectStore,
    context: CheckpointStoreContext,
    attempt: &ProviderSandboxAttempt,
    sandbox_id: &str,
    sandbox: &ConnectedSandbox,
    limits: ArchiveLimits,
) -> Result<()> {
    let temporary = create_operation_temp_dir("restore", sandbox_id).await?;
    let restored_root = temporary.path().join("data");
    let result = async {
        store.restore(context, &restored_root).await?;
        import_data_root(
            provider,
            attempt,
            sandbox_id,
            sandbox,
            &restored_root,
            limits,
        )
        .await
    }
    .await;
    let cleanup = cleanup_operation_temp_dir(temporary).await;
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
    }
}

async fn list_data_root(
    attempt: &ProviderSandboxAttempt,
    sandbox_id: &str,
    sandbox: &ConnectedSandbox,
    max_depth: usize,
) -> Result<Vec<RemoteEntry>> {
    let response = attempt
        .client()
        .post(format!(
            "{}/filesystem.Filesystem/ListDir",
            attempt.origin()
        ))
        .headers(envd_headers(sandbox_id, sandbox)?)
        .header("Connect-Protocol-Version", CONNECT_PROTOCOL_VERSION)
        .json(&serde_json::json!({
            "path": E2B_DATA_ROOT,
            "depth": max_depth,
        }))
        .send()
        .await
        .map_err(|error| {
            MoaError::ProviderError(format!("failed to list E2B data root: {error}"))
        })?;
    let value = expect_success_json(response, "E2B").await?;
    serde_json::from_value::<ListDirectoryResponse>(value)
        .map(|response| response.entries)
        .map_err(|error| MoaError::ProviderError(format!("invalid E2B directory listing: {error}")))
}

async fn download_file(
    attempt: &ProviderSandboxAttempt,
    sandbox_id: &str,
    sandbox: &ConnectedSandbox,
    remote: &str,
    destination: &Path,
    expected_size: u64,
    max_file_bytes: u64,
) -> Result<()> {
    let url = build_url(
        &format!("{}/files", attempt.origin()),
        &[("path", remote)],
        "E2B",
    )?;
    let response = attempt
        .client()
        .get(url)
        .headers(envd_headers(sandbox_id, sandbox)?)
        .send()
        .await
        .map_err(|error| {
            MoaError::ProviderError(format!("failed to download E2B file: {error}"))
        })?;
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    let mut file = tokio::fs::File::create(destination).await?;
    let mut observed = 0_u64;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| {
            MoaError::ProviderError(format!("failed to stream E2B file: {error}"))
        })?;
        observed = observed.checked_add(chunk.len() as u64).ok_or_else(|| {
            MoaError::ValidationError("E2B download byte count overflow".to_string())
        })?;
        if observed > max_file_bytes || observed > expected_size {
            return Err(MoaError::ValidationError(
                "E2B file exceeded its announced or configured size".to_string(),
            ));
        }
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    if observed != expected_size {
        return Err(MoaError::ValidationError(
            "E2B file size changed during checkpoint export".to_string(),
        ));
    }
    Ok(())
}

async fn upload_file(
    attempt: &ProviderSandboxAttempt,
    sandbox_id: &str,
    sandbox: &ConnectedSandbox,
    remote: &str,
    local: &Path,
    expected_size: u64,
) -> Result<()> {
    let metadata = tokio::fs::metadata(local).await?;
    if !metadata.is_file() || metadata.len() != expected_size {
        return Err(MoaError::ValidationError(
            "canonical E2B restore file changed before upload".to_string(),
        ));
    }
    let file = tokio::fs::File::open(local).await?;
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut buffer = vec![0_u8; 64 * 1024];
        let read = tokio::io::AsyncReadExt::read(&mut file, &mut buffer).await?;
        if read == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        buffer.truncate(read);
        Ok(Some((buffer, file)))
    });
    let url = build_url(
        &format!("{}/files", attempt.origin()),
        &[("path", remote)],
        "E2B",
    )?;
    let response = attempt
        .client()
        .post(url)
        .headers(envd_headers(sandbox_id, sandbox)?)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .map_err(|error| MoaError::ProviderError(format!("failed to upload E2B file: {error}")))?;
    let _ = expect_success_json(response, "E2B").await?;
    Ok(())
}

fn remote_relative_path(remote: &str) -> Result<PathBuf> {
    let path = Path::new(remote);
    let relative = path.strip_prefix(E2B_DATA_ROOT).map_err(|_| {
        MoaError::ValidationError("E2B export path escaped the mutable data root".to_string())
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(MoaError::ValidationError(
            "E2B export returned a non-normalized path".to_string(),
        ));
    }
    Ok(relative.to_path_buf())
}

fn remote_entry_kind(value: &str) -> Result<ArchiveEntryKind> {
    match value.to_ascii_lowercase().as_str() {
        "file" | "file_type_file" => Ok(ArchiveEntryKind::File),
        "dir" | "directory" | "file_type_directory" => Ok(ArchiveEntryKind::Directory),
        "symlink" | "file_type_symlink" => Ok(ArchiveEntryKind::Symlink),
        _ => Err(MoaError::ValidationError(
            "E2B export contains a device, FIFO, socket, or unsupported special file".to_string(),
        )),
    }
}

async fn set_mode(path: &Path, mode: u32) -> Result<()> {
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
        .await
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins: provider directory listings can never select a host destination
    // outside the operation-scoped temporary root.
    #[test]
    fn export_path_must_be_strictly_beneath_reserved_root() {
        assert_eq!(
            remote_relative_path("/workspace/nested/file").expect("valid path"),
            PathBuf::from("nested/file")
        );
        for invalid in [
            "/workspace",
            "/workspace/../secret",
            "/home/user/other/file",
        ] {
            assert!(
                remote_relative_path(invalid).is_err(),
                "path should be rejected: {invalid}"
            );
        }
    }

    // Pins: an E2B listing cannot smuggle a special file into Task 6's
    // canonical archive builder.
    #[test]
    fn export_refuses_unknown_remote_entry_kinds() {
        assert_eq!(
            remote_entry_kind("FILE_TYPE_FILE").expect("file kind"),
            ArchiveEntryKind::File
        );
        assert!(remote_entry_kind("FILE_TYPE_SOCKET").is_err());
    }

    // Pins: announced remote sizes are rejected before a response body can
    // allocate or write beyond the configured checkpoint bounds.
    #[test]
    fn export_rejects_exact_limit_plus_one_before_download() {
        let limits = ArchiveLimits {
            max_entries: 4,
            max_path_depth: 4,
            max_file_bytes: 8,
            max_total_bytes: 12,
            max_chunk_bytes: 4,
            max_compressed_chunk_bytes: 16,
        };
        assert_eq!(
            limits
                .checked_add_file_bytes(0, 8, "E2B export")
                .expect("at limit"),
            8
        );
        assert!(limits.checked_add_file_bytes(0, 9, "E2B export").is_err());
        assert!(limits.checked_add_file_bytes(8, 5, "E2B export").is_err());
    }
}
