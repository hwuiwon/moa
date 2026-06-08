//! Privacy administration CLI commands.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD};
use chrono::{TimeZone, Utc};
use clap::{Args, Subcommand};
use ed25519_dalek::{Signature, Signer as DalekSigner, SigningKey, Verifier, VerifyingKey};
use flate2::Compression;
use flate2::write::GzEncoder;
use moa_core::{MoaConfig, ScopeContext, ScopedConn, UserId, WorkspaceId};
use moa_lineage_audit::PiiVault;
use moa_memory_graph::{
    AgeGraphStore, ChangelogRecord, write::hard_purge_with_audit, write_and_bump,
};
use moa_session::PostgresSessionStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tar::Builder;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use uuid::Uuid;

pub(super) const APPROVAL_PUBLIC_KEY_ENV: &str = "MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX";
pub(super) const APPROVAL_PUBLIC_KEY_FALLBACK_ENV: &str = "MOA_PRIVACY_APPROVAL_PUBLIC_KEY";
pub(super) const EXPORT_SIGNING_KEY_ENV: &str = "MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX";
pub(super) const EXPORT_SIGNING_KEY_FALLBACK_ENV: &str = "MOA_PRIVACY_EXPORT_SIGNING_KEY";
pub(super) const EXPORT_SIGNING_KEY_ID_ENV: &str = "MOA_PRIVACY_EXPORT_SIGNING_KEY_ID";
pub(super) const PII_VAULT_SECRET_ENV: &str = "MOA_PII_VAULT_WORKSPACE_SECRET";
pub(super) const PII_VAULT_SECRET_HEX_ENV: &str = "MOA_PII_VAULT_WORKSPACE_SECRET_HEX";
pub(super) const ERASE_CHUNK_SIZE: usize = 1000;
pub(super) const ERASE_SAMPLE_LIMIT: usize = 20;

mod auth;
mod erase;
mod export;
#[cfg(test)]
mod tests;

#[cfg(test)]
use auth::ensure_jti_inserted;
use auth::{ApprovalClaims, ApprovalTokenVerifier, Ed25519ManifestSigner, consume_approval_jti};
#[cfg(test)]
use erase::{EraseContext, begin_app_scoped_tx, execute_privacy_erase};
#[cfg(test)]
use export::{ExportContext, finalize_archive, write_export_readme, write_manifest};

/// Privacy administration CLI commands.
#[derive(Debug, Subcommand)]
pub enum PrivacyCommand {
    /// Exports all personal graph memory data for one subject user.
    Export(export::Args),
    /// Hard-purges all graph memory attributable to one subject user in one workspace.
    Erase(erase::Args),
}

/// Runs one privacy CLI command and returns a human-readable report.
pub async fn handle_privacy_command(config: &MoaConfig, command: PrivacyCommand) -> Result<String> {
    match command {
        PrivacyCommand::Export(args) => export::run(config, args).await,
        PrivacyCommand::Erase(args) => erase::run(config, args).await,
    }
}
