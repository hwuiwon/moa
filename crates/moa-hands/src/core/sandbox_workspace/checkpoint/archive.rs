//! Canonical bounded filesystem archives for portable sandbox checkpoints.

use std::collections::VecDeque;
use std::fs::{self, File, Metadata};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use moa_core::{
    error::{MoaError, Result},
    types::hands::validate_sandbox_file_path,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Portable checkpoint archive format version.
pub const CHECKPOINT_ARCHIVE_FORMAT_VERSION: u16 = 1;

/// Default production archive safety and memory bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveLimits {
    /// Maximum number of filesystem entries.
    pub max_entries: usize,
    /// Maximum normalized path depth.
    pub max_path_depth: usize,
    /// Maximum bytes in one regular file.
    pub max_file_bytes: u64,
    /// Maximum logical bytes in the full archive.
    pub max_total_bytes: u64,
    /// Maximum plaintext bytes compressed into one independent chunk.
    pub max_chunk_bytes: usize,
    /// Maximum compressed bytes accepted for one chunk during restore.
    pub max_compressed_chunk_bytes: usize,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_path_depth: 64,
            max_file_bytes: 512 * 1024 * 1024,
            max_total_bytes: 4 * 1024 * 1024 * 1024,
            max_chunk_bytes: 4 * 1024 * 1024,
            max_compressed_chunk_bytes: 8 * 1024 * 1024,
        }
    }
}

impl ArchiveLimits {
    fn validate(self) -> Result<()> {
        if self.max_entries == 0
            || self.max_path_depth == 0
            || self.max_file_bytes == 0
            || self.max_total_bytes == 0
            || self.max_chunk_bytes == 0
            || self.max_compressed_chunk_bytes == 0
        {
            return Err(MoaError::ConfigError(
                "checkpoint archive limits must all be greater than zero".to_string(),
            ));
        }
        Ok(())
    }

    /// Adds one regular file to a bounded logical-byte inventory.
    pub(crate) fn checked_add_file_bytes(
        self,
        current_total: u64,
        file_bytes: u64,
        context: &str,
    ) -> Result<u64> {
        if file_bytes > self.max_file_bytes {
            return Err(MoaError::ValidationError(format!(
                "{context} file exceeds the per-file byte limit"
            )));
        }
        let total = current_total.checked_add(file_bytes).ok_or_else(|| {
            MoaError::ValidationError(format!("{context} logical byte count overflow"))
        })?;
        if total > self.max_total_bytes {
            return Err(MoaError::ValidationError(format!(
                "{context} exceeds the total logical-byte limit"
            )));
        }
        Ok(total)
    }
}

/// Supported portable archive entry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveEntryKind {
    /// Directory.
    Directory,
    /// Regular file.
    File,
    /// Safe relative symbolic link.
    Symlink,
}

/// Canonical metadata for one archived filesystem entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveEntry {
    /// Normalized UTF-8 path relative to the dedicated data root.
    pub path: String,
    /// Supported entry kind.
    pub kind: ArchiveEntryKind,
    /// Unix permission bits, excluding file-type bits.
    pub mode: u32,
    /// Logical file size; zero for directories and symlinks.
    pub size: u64,
    /// Offset into the logical concatenated regular-file byte stream.
    pub offset: u64,
    /// SHA-256 file-content digest; absent for directories and symlinks.
    pub digest_sha256: Option<String>,
    /// Safe relative link target for symlinks.
    pub link_target: Option<String>,
}

/// Integrity metadata for one independently compressed archive chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveChunkDescriptor {
    /// Zero-based chunk position.
    pub index: u32,
    /// Logical bytes in the decompressed chunk.
    pub plaintext_bytes: u64,
    /// Compressed chunk length.
    pub compressed_bytes: u64,
    /// SHA-256 of the logical bytes.
    pub plaintext_sha256: String,
    /// SHA-256 of the compressed bytes.
    pub compressed_sha256: String,
}

/// Canonical portable filesystem manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointArchiveManifest {
    /// Portable archive format version.
    pub format_version: u16,
    /// Logical bytes across regular files.
    pub logical_bytes: u64,
    /// Canonically path-sorted entries.
    pub entries: Vec<ArchiveEntry>,
    /// Chunk descriptors in exact index order.
    pub chunks: Vec<ArchiveChunkDescriptor>,
}

impl CheckpointArchiveManifest {
    /// Serializes the validated manifest into its canonical JSON representation.
    pub fn canonical_bytes(&self, limits: ArchiveLimits) -> Result<Vec<u8>> {
        self.validate(limits)?;
        serde_json::to_vec(self).map_err(|error| {
            MoaError::StorageError(format!("serialize checkpoint archive manifest: {error}"))
        })
    }

    /// Returns the lowercase SHA-256 digest of the canonical manifest.
    pub fn digest_sha256(&self, limits: ArchiveLimits) -> Result<String> {
        Ok(hex::encode(Sha256::digest(self.canonical_bytes(limits)?)))
    }

    /// Validates canonical ordering, structure, bounds, and chunk coverage.
    pub fn validate(&self, limits: ArchiveLimits) -> Result<()> {
        limits.validate()?;
        if self.format_version != CHECKPOINT_ARCHIVE_FORMAT_VERSION {
            return Err(validation("unsupported checkpoint archive format"));
        }
        if self.entries.len() > limits.max_entries {
            return Err(validation("checkpoint archive has too many entries"));
        }
        let mut prior_path: Option<&str> = None;
        let mut symlink_paths = Vec::new();
        let mut expected_offset = 0_u64;
        for entry in &self.entries {
            validate_relative_path(&entry.path, limits.max_path_depth)?;
            if symlink_paths
                .iter()
                .any(|symlink: &String| entry.path.starts_with(&format!("{symlink}/")))
            {
                return Err(validation(
                    "checkpoint entry is nested beneath a symbolic link",
                ));
            }
            if prior_path.is_some_and(|prior| prior >= entry.path.as_str()) {
                return Err(validation(
                    "checkpoint archive entries are not strictly path-sorted",
                ));
            }
            prior_path = Some(&entry.path);
            match entry.kind {
                ArchiveEntryKind::File => {
                    if entry.link_target.is_some()
                        || entry
                            .digest_sha256
                            .as_deref()
                            .is_none_or(|value| !is_sha256_hex(value))
                        || entry.offset != expected_offset
                        || entry.size > limits.max_file_bytes
                    {
                        return Err(validation("invalid checkpoint regular-file metadata"));
                    }
                    expected_offset = expected_offset
                        .checked_add(entry.size)
                        .ok_or_else(|| validation("checkpoint logical size overflow"))?;
                }
                ArchiveEntryKind::Directory => {
                    if entry.size != 0
                        || entry.offset != 0
                        || entry.digest_sha256.is_some()
                        || entry.link_target.is_some()
                    {
                        return Err(validation("invalid checkpoint directory metadata"));
                    }
                }
                ArchiveEntryKind::Symlink => {
                    let target = entry
                        .link_target
                        .as_deref()
                        .ok_or_else(|| validation("checkpoint symlink is missing its target"))?;
                    validate_symlink_target(&entry.path, target, limits.max_path_depth)?;
                    symlink_paths.push(entry.path.clone());
                    if entry.size != 0 || entry.offset != 0 || entry.digest_sha256.is_some() {
                        return Err(validation("invalid checkpoint symlink metadata"));
                    }
                }
            }
        }
        if expected_offset != self.logical_bytes || self.logical_bytes > limits.max_total_bytes {
            return Err(validation(
                "checkpoint logical byte total does not match entries",
            ));
        }
        let mut chunk_total = 0_u64;
        for (position, chunk) in self.chunks.iter().enumerate() {
            if chunk.index as usize != position
                || chunk.plaintext_bytes == 0
                || chunk.plaintext_bytes > limits.max_chunk_bytes as u64
                || chunk.compressed_bytes == 0
                || chunk.compressed_bytes > limits.max_compressed_chunk_bytes as u64
                || !is_sha256_hex(&chunk.plaintext_sha256)
                || !is_sha256_hex(&chunk.compressed_sha256)
            {
                return Err(validation("invalid checkpoint chunk metadata"));
            }
            chunk_total = chunk_total
                .checked_add(chunk.plaintext_bytes)
                .ok_or_else(|| validation("checkpoint chunk size overflow"))?;
        }
        if chunk_total != self.logical_bytes {
            return Err(validation("checkpoint chunks do not cover logical bytes"));
        }
        Ok(())
    }
}

/// Canonical manifest plus independently compressed chunks ready for encryption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointArchive {
    /// Canonical archive manifest.
    pub manifest: CheckpointArchiveManifest,
    /// Independently zstd-compressed chunks in descriptor order.
    pub compressed_chunks: Vec<Vec<u8>>,
}

/// Builds a checkpoint from exactly one dedicated mutable tenant-data root.
pub async fn build_checkpoint_archive(
    data_root: impl AsRef<Path>,
    limits: ArchiveLimits,
) -> Result<CheckpointArchive> {
    let data_root = data_root.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || build_checkpoint_archive_blocking(&data_root, limits))
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("checkpoint archive worker failed: {error}"))
        })?
}

/// Restores a fully verified archive into a path that must not already exist.
///
/// Extraction occurs in a fresh sibling staging directory. Only after every
/// chunk, entry digest, path, and size verifies is that directory renamed to
/// the requested root. Existing data is never replaced.
pub async fn restore_checkpoint_archive(
    archive: CheckpointArchive,
    fresh_data_root: impl AsRef<Path>,
    limits: ArchiveLimits,
) -> Result<()> {
    let fresh_data_root = fresh_data_root.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || {
        restore_checkpoint_archive_blocking(&archive, &fresh_data_root, limits)
    })
    .await
    .map_err(|error| MoaError::StorageError(format!("checkpoint restore worker failed: {error}")))?
}

fn build_checkpoint_archive_blocking(
    root: &Path,
    limits: ArchiveLimits,
) -> Result<CheckpointArchive> {
    limits.validate()?;
    let root_metadata =
        fs::symlink_metadata(root).map_err(storage_io("inspect checkpoint root"))?;
    if !root_metadata.file_type().is_dir() {
        return Err(validation("checkpoint data root must be a directory"));
    }

    let mut paths = Vec::new();
    collect_paths(root, root, &mut paths, limits)?;
    paths.sort_by(|left, right| left.0.cmp(&right.0));

    let mut entries = Vec::with_capacity(paths.len());
    let mut compressed_chunks = Vec::new();
    let mut chunk_descriptors = Vec::new();
    let mut pending_chunk = Vec::with_capacity(limits.max_chunk_bytes);
    let mut logical_bytes = 0_u64;

    for (relative, absolute, metadata) in paths {
        let mode = permission_mode(&metadata);
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            entries.push(ArchiveEntry {
                path: relative,
                kind: ArchiveEntryKind::Directory,
                mode,
                size: 0,
                offset: 0,
                digest_sha256: None,
                link_target: None,
            });
        } else if file_type.is_symlink() {
            let target = fs::read_link(&absolute).map_err(storage_io("read checkpoint symlink"))?;
            let target = target
                .to_str()
                .ok_or_else(|| validation("checkpoint symlink target must be UTF-8"))?
                .to_string();
            validate_symlink_target(&relative, &target, limits.max_path_depth)?;
            entries.push(ArchiveEntry {
                path: relative,
                kind: ArchiveEntryKind::Symlink,
                mode,
                size: 0,
                offset: 0,
                digest_sha256: None,
                link_target: Some(target),
            });
        } else if file_type.is_file() {
            reject_hard_link(&metadata)?;
            let offset = logical_bytes;
            logical_bytes = limits.checked_add_file_bytes(
                logical_bytes,
                metadata.len(),
                "checkpoint archive",
            )?;
            let mut file = File::open(&absolute).map_err(storage_io("open checkpoint file"))?;
            let mut digest = Sha256::new();
            let mut remaining = metadata.len();
            while remaining > 0 {
                let available = limits.max_chunk_bytes - pending_chunk.len();
                let take = usize::try_from(remaining.min(available as u64))
                    .map_err(|_| validation("checkpoint read size overflow"))?;
                let start = pending_chunk.len();
                pending_chunk.resize(start + take, 0);
                file.read_exact(&mut pending_chunk[start..])
                    .map_err(storage_io("read checkpoint file"))?;
                digest.update(&pending_chunk[start..]);
                remaining -= take as u64;
                if pending_chunk.len() == limits.max_chunk_bytes {
                    finish_chunk(
                        &mut pending_chunk,
                        &mut compressed_chunks,
                        &mut chunk_descriptors,
                        limits,
                    )?;
                }
            }
            entries.push(ArchiveEntry {
                path: relative,
                kind: ArchiveEntryKind::File,
                mode,
                size: metadata.len(),
                offset,
                digest_sha256: Some(hex::encode(digest.finalize())),
                link_target: None,
            });
        } else {
            return Err(validation(
                "checkpoint contains a device, FIFO, socket, or unsupported special file",
            ));
        }
    }
    if !pending_chunk.is_empty() {
        finish_chunk(
            &mut pending_chunk,
            &mut compressed_chunks,
            &mut chunk_descriptors,
            limits,
        )?;
    }
    let manifest = CheckpointArchiveManifest {
        format_version: CHECKPOINT_ARCHIVE_FORMAT_VERSION,
        logical_bytes,
        entries,
        chunks: chunk_descriptors,
    };
    manifest.validate(limits)?;
    Ok(CheckpointArchive {
        manifest,
        compressed_chunks,
    })
}

fn collect_paths(
    root: &Path,
    directory: &Path,
    paths: &mut Vec<(String, PathBuf, Metadata)>,
    limits: ArchiveLimits,
) -> Result<()> {
    for item in fs::read_dir(directory).map_err(storage_io("read checkpoint directory"))? {
        let item = item.map_err(storage_io("read checkpoint directory entry"))?;
        let absolute = item.path();
        let relative_path = absolute
            .strip_prefix(root)
            .map_err(|_| validation("checkpoint entry escaped its data root"))?;
        let relative = relative_path
            .to_str()
            .ok_or_else(|| validation("checkpoint paths must be UTF-8"))?
            .replace('\\', "/");
        validate_relative_path(&relative, limits.max_path_depth)?;
        let metadata =
            fs::symlink_metadata(&absolute).map_err(storage_io("inspect checkpoint entry"))?;
        if paths.len() >= limits.max_entries {
            return Err(validation("checkpoint archive has too many entries"));
        }
        let descend = metadata.file_type().is_dir();
        paths.push((relative, absolute.clone(), metadata));
        if descend {
            collect_paths(root, &absolute, paths, limits)?;
        }
    }
    Ok(())
}

fn finish_chunk(
    pending: &mut Vec<u8>,
    compressed_chunks: &mut Vec<Vec<u8>>,
    descriptors: &mut Vec<ArchiveChunkDescriptor>,
    limits: ArchiveLimits,
) -> Result<()> {
    let bytes = std::mem::take(pending);
    let compressed = zstd::bulk::compress(&bytes, 3)
        .map_err(|error| MoaError::StorageError(format!("compress checkpoint chunk: {error}")))?;
    if compressed.len() > limits.max_compressed_chunk_bytes {
        return Err(validation("compressed checkpoint chunk exceeds size limit"));
    }
    let index = u32::try_from(descriptors.len())
        .map_err(|_| validation("checkpoint has too many chunks"))?;
    descriptors.push(ArchiveChunkDescriptor {
        index,
        plaintext_bytes: bytes.len() as u64,
        compressed_bytes: compressed.len() as u64,
        plaintext_sha256: hex::encode(Sha256::digest(&bytes)),
        compressed_sha256: hex::encode(Sha256::digest(&compressed)),
    });
    compressed_chunks.push(compressed);
    *pending = Vec::with_capacity(limits.max_chunk_bytes);
    Ok(())
}

fn restore_checkpoint_archive_blocking(
    archive: &CheckpointArchive,
    final_root: &Path,
    limits: ArchiveLimits,
) -> Result<()> {
    archive.manifest.validate(limits)?;
    if final_root.exists() {
        return Err(validation("checkpoint restore destination must be fresh"));
    }
    if archive.compressed_chunks.len() != archive.manifest.chunks.len() {
        return Err(validation("checkpoint chunk count does not match manifest"));
    }
    let parent = final_root
        .parent()
        .ok_or_else(|| validation("checkpoint restore destination needs a parent"))?;
    fs::create_dir_all(parent).map_err(storage_io("create checkpoint restore parent"))?;
    let file_name = final_root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| validation("checkpoint restore destination must be UTF-8"))?;
    let staging = parent.join(format!(".{file_name}.restore-{}", Uuid::new_v4()));
    fs::create_dir(&staging).map_err(storage_io("create checkpoint restore staging root"))?;

    let result = (|| {
        let chunks = verified_plaintext_chunks(archive, limits)?;
        let mut reader = LogicalChunkReader::new(chunks);
        let mut directory_modes = Vec::new();
        for entry in &archive.manifest.entries {
            let destination = staging.join(&entry.path);
            match entry.kind {
                ArchiveEntryKind::Directory => {
                    fs::create_dir_all(&destination)
                        .map_err(storage_io("create checkpoint directory"))?;
                    directory_modes.push((destination, entry.mode));
                }
                ArchiveEntryKind::File => {
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)
                            .map_err(storage_io("create checkpoint file parent"))?;
                    }
                    let mut file = File::create(&destination)
                        .map_err(storage_io("create restored checkpoint file"))?;
                    let mut digest = Sha256::new();
                    let mut remaining = entry.size;
                    while remaining > 0 {
                        let bytes =
                            reader.take(remaining.min(limits.max_chunk_bytes as u64) as usize)?;
                        file.write_all(&bytes)
                            .map_err(storage_io("write restored checkpoint file"))?;
                        digest.update(&bytes);
                        remaining -= bytes.len() as u64;
                    }
                    file.sync_all()
                        .map_err(storage_io("sync restored checkpoint file"))?;
                    if Some(hex::encode(digest.finalize())) != entry.digest_sha256 {
                        return Err(validation("restored checkpoint file digest mismatch"));
                    }
                    set_permission_mode(&destination, entry.mode)?;
                }
                ArchiveEntryKind::Symlink => {
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)
                            .map_err(storage_io("create checkpoint symlink parent"))?;
                    }
                    create_symlink(
                        entry
                            .link_target
                            .as_deref()
                            .ok_or_else(|| validation("checkpoint symlink target missing"))?,
                        &destination,
                    )?;
                }
            }
        }
        if !reader.is_empty() {
            return Err(validation("checkpoint contains unreferenced logical bytes"));
        }
        for (directory, mode) in directory_modes.into_iter().rev() {
            set_permission_mode(&directory, mode)?;
        }
        fs::rename(&staging, final_root)
            .map_err(storage_io("atomically publish restored checkpoint root"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn verified_plaintext_chunks(
    archive: &CheckpointArchive,
    limits: ArchiveLimits,
) -> Result<VecDeque<Vec<u8>>> {
    let mut chunks = VecDeque::with_capacity(archive.compressed_chunks.len());
    for (descriptor, compressed) in archive
        .manifest
        .chunks
        .iter()
        .zip(&archive.compressed_chunks)
    {
        if compressed.len() != descriptor.compressed_bytes as usize
            || hex::encode(Sha256::digest(compressed)) != descriptor.compressed_sha256
        {
            return Err(validation("checkpoint compressed chunk digest mismatch"));
        }
        let expected = usize::try_from(descriptor.plaintext_bytes)
            .map_err(|_| validation("checkpoint chunk plaintext size overflow"))?;
        if expected > limits.max_chunk_bytes {
            return Err(validation(
                "checkpoint chunk expands beyond configured limit",
            ));
        }
        let plaintext = zstd::bulk::decompress(compressed, expected)
            .map_err(|_| validation("checkpoint chunk decompression failed or exceeded bound"))?;
        if plaintext.len() != expected
            || hex::encode(Sha256::digest(&plaintext)) != descriptor.plaintext_sha256
        {
            return Err(validation("checkpoint plaintext chunk digest mismatch"));
        }
        chunks.push_back(plaintext);
    }
    Ok(chunks)
}

struct LogicalChunkReader {
    chunks: VecDeque<Vec<u8>>,
    position: usize,
}

impl LogicalChunkReader {
    fn new(chunks: VecDeque<Vec<u8>>) -> Self {
        Self {
            chunks,
            position: 0,
        }
    }

    fn take(&mut self, maximum: usize) -> Result<Vec<u8>> {
        let front = self
            .chunks
            .front()
            .ok_or_else(|| validation("checkpoint ended before file content"))?;
        let available = front.len().saturating_sub(self.position);
        let take = available.min(maximum);
        let bytes = front[self.position..self.position + take].to_vec();
        self.position += take;
        if self.position == front.len() {
            self.chunks.pop_front();
            self.position = 0;
        }
        Ok(bytes)
    }

    fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

fn validate_relative_path(path: &str, max_depth: usize) -> Result<()> {
    validate_sandbox_file_path(path).map_err(|_| validation("checkpoint path is invalid"))?;
    if path.split('/').count() > max_depth {
        return Err(validation("checkpoint path depth exceeds configured limit"));
    }
    Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_symlink_target(path: &str, target: &str, max_depth: usize) -> Result<()> {
    if target.is_empty() || target.contains('\0') || target.contains('\\') {
        return Err(validation(
            "checkpoint symlink target is not normalized UTF-8",
        ));
    }
    let target_path = Path::new(target);
    if target_path.is_absolute() {
        return Err(validation("checkpoint symlink target must be relative"));
    }
    let mut depth = Path::new(path)
        .parent()
        .map_or(0, |parent| parent.components().count());
    for component in target_path.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::CurDir => {}
            _ => return Err(validation("checkpoint symlink escapes the data root")),
        }
        if depth > max_depth {
            return Err(validation(
                "checkpoint symlink depth exceeds configured limit",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn reject_hard_link(metadata: &Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() != 1 {
        return Err(validation("checkpoint hard links are not supported"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_hard_link(_metadata: &Metadata) -> Result<()> {
    Err(validation(
        "portable checkpoint creation requires hard-link detection",
    ))
}

#[cfg(unix)]
fn permission_mode(metadata: &Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn permission_mode(_metadata: &Metadata) -> u32 {
    0o600
}

#[cfg(unix)]
fn set_permission_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
        .map_err(storage_io("set restored checkpoint permissions"))
}

#[cfg(not(unix))]
fn set_permission_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &str, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, destination)
        .map_err(storage_io("create restored checkpoint symlink"))
}

#[cfg(not(unix))]
fn create_symlink(_target: &str, _destination: &Path) -> Result<()> {
    Err(validation(
        "portable checkpoint symlink restore is unsupported on this platform",
    ))
}

fn validation(message: &str) -> MoaError {
    MoaError::ValidationError(message.to_string())
}

fn storage_io(context: &'static str) -> impl FnOnce(std::io::Error) -> MoaError {
    move |error| MoaError::StorageError(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn small_limits() -> ArchiveLimits {
        ArchiveLimits {
            max_entries: 32,
            max_path_depth: 8,
            max_file_bytes: 1024,
            max_total_bytes: 2048,
            max_chunk_bytes: 8,
            max_compressed_chunk_bytes: 1024,
        }
    }

    // Pins: a fresh restore reproduces exact file bytes, executable mode, and a
    // safe relative symlink without including anything outside the data root.
    #[tokio::test]
    async fn checkpoint_archive_restores_exact_files_and_modes_offline() {
        let temporary = TempDir::new().expect("temporary archive root");
        let source = temporary.path().join("data");
        fs::create_dir_all(source.join("nested")).expect("create nested source");
        fs::write(source.join("nested/run.sh"), b"#!/bin/sh\necho durable\n")
            .expect("write source marker");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                source.join("nested/run.sh"),
                fs::Permissions::from_mode(0o750),
            )
            .expect("set executable source mode");
            std::os::unix::fs::symlink("nested/run.sh", source.join("current"))
                .expect("create safe source symlink");
        }

        let archive = build_checkpoint_archive(&source, small_limits())
            .await
            .expect("valid data root should archive");
        let restored = temporary.path().join("restored");
        restore_checkpoint_archive(archive, &restored, small_limits())
            .await
            .expect("verified archive should restore into a fresh root");

        assert_eq!(
            fs::read(restored.join("nested/run.sh")).expect("read restored marker"),
            b"#!/bin/sh\necho durable\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(restored.join("nested/run.sh"))
                    .expect("inspect restored mode")
                    .permissions()
                    .mode()
                    & 0o777,
                0o750
            );
            assert_eq!(
                fs::read_link(restored.join("current")).expect("read restored symlink"),
                PathBuf::from("nested/run.sh")
            );
        }
    }

    // Pins: a tampered or expansion-inconsistent chunk cannot publish even an
    // empty replacement over an existing workspace.
    #[tokio::test]
    async fn corrupt_restore_preserves_existing_workspace_offline() {
        let temporary = TempDir::new().expect("temporary archive root");
        let source = temporary.path().join("data");
        fs::create_dir(&source).expect("create source");
        fs::write(source.join("marker"), b"committed").expect("write source marker");
        let mut archive = build_checkpoint_archive(&source, small_limits())
            .await
            .expect("valid data root should archive");
        archive.compressed_chunks[0][0] ^= 1;
        let existing = temporary.path().join("existing");
        fs::create_dir(&existing).expect("create existing workspace");
        fs::write(existing.join("marker"), b"prior").expect("write prior marker");

        let error = restore_checkpoint_archive(archive, &existing, small_limits())
            .await
            .expect_err("restore must refuse any existing destination");

        assert!(matches!(error, MoaError::ValidationError(_)));
        assert_eq!(
            fs::read(existing.join("marker")).expect("read preserved marker"),
            b"prior"
        );
    }

    // Pins: an archive manifest cannot smuggle traversal or an escaping symlink
    // into otherwise valid compressed content.
    #[test]
    fn manifest_rejects_traversal_and_symlink_escape_offline() {
        let traversal = CheckpointArchiveManifest {
            format_version: CHECKPOINT_ARCHIVE_FORMAT_VERSION,
            logical_bytes: 0,
            entries: vec![ArchiveEntry {
                path: "../secret".to_string(),
                kind: ArchiveEntryKind::Directory,
                mode: 0o700,
                size: 0,
                offset: 0,
                digest_sha256: None,
                link_target: None,
            }],
            chunks: Vec::new(),
        };
        let escaping_link = CheckpointArchiveManifest {
            entries: vec![ArchiveEntry {
                path: "link".to_string(),
                kind: ArchiveEntryKind::Symlink,
                mode: 0o777,
                size: 0,
                offset: 0,
                digest_sha256: None,
                link_target: Some("../secret".to_string()),
            }],
            ..traversal.clone()
        };

        assert!(matches!(
            traversal.validate(small_limits()),
            Err(MoaError::ValidationError(_))
        ));
        assert!(matches!(
            escaping_link.validate(small_limits()),
            Err(MoaError::ValidationError(_))
        ));
    }

    #[test]
    fn manifest_rejects_entry_nested_beneath_symlink_offline() {
        // Pins: a canonical manifest cannot use a prior symlink as an
        // extraction trampoline for a later regular file.
        let manifest = CheckpointArchiveManifest {
            format_version: CHECKPOINT_ARCHIVE_FORMAT_VERSION,
            logical_bytes: 0,
            entries: vec![
                ArchiveEntry {
                    path: "alias".to_string(),
                    kind: ArchiveEntryKind::Symlink,
                    mode: 0o777,
                    size: 0,
                    offset: 0,
                    digest_sha256: None,
                    link_target: Some("real".to_string()),
                },
                ArchiveEntry {
                    path: "alias/escaped".to_string(),
                    kind: ArchiveEntryKind::Directory,
                    mode: 0o700,
                    size: 0,
                    offset: 0,
                    digest_sha256: None,
                    link_target: None,
                },
            ],
            chunks: Vec::new(),
        };

        assert!(matches!(
            manifest.validate(small_limits()),
            Err(MoaError::ValidationError(_))
        ));
    }

    // Pins: on Unix a second name for the same inode cannot cross archive
    // extraction semantics or produce ambiguous content accounting.
    #[cfg(unix)]
    #[tokio::test]
    async fn hard_links_are_rejected_offline() {
        let temporary = TempDir::new().expect("temporary archive root");
        let source = temporary.path().join("data");
        fs::create_dir(&source).expect("create source");
        fs::write(source.join("first"), b"content").expect("write first link");
        fs::hard_link(source.join("first"), source.join("second")).expect("create hard link");

        let error = build_checkpoint_archive(&source, small_limits())
            .await
            .expect_err("hard links must be rejected");

        assert!(matches!(error, MoaError::ValidationError(_)));
    }
}
