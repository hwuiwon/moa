//! Key-management composition for serving processes and maintenance commands.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use moa_config::{KmsProviderKind, MoaConfig};
use moa_crypto::KeyManagementProvider;
use moa_kms::{PostgresKmsProvider, RootKeyRing};
use sqlx::PgPool;

/// Shared configured KMS handle used by runtime owners and readiness checks.
#[derive(Clone)]
pub struct KmsRuntime {
    provider: Arc<dyn KeyManagementProvider>,
    postgres: Option<Arc<PostgresKmsProvider>>,
    required_generation: String,
}

/// Summary returned after a root-key rewrap maintenance run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewrapReport {
    /// Root-key generation activated for new and rewrapped KEKs.
    pub active_generation: String,
    /// Number of live KEKs moved to the active generation.
    pub rewrapped: u64,
    /// Number of non-empty bounded transactions committed.
    pub batches: u64,
    /// Generation explicitly retired after rewrap completed, when requested.
    pub retired_generation: Option<String>,
}

impl KmsRuntime {
    /// Builds the KMS used by serving pods and requires live shared state to
    /// match this pod's configured generation before returning.
    pub async fn build_serving(config: &MoaConfig, pool: PgPool) -> Result<Self> {
        let runtime = Self::build(config, pool).await?;
        runtime
            .check_readiness()
            .await
            .context("validate serving KMS compatibility")?;
        Ok(runtime)
    }

    /// Builds a durable KMS for rotation maintenance.
    ///
    /// The current database-active generation and every generation referenced
    /// by a live KEK must be mounted. The active generation may differ from the
    /// configured required generation so this handle can perform the rotation.
    pub async fn build_maintenance(config: &MoaConfig, pool: PgPool) -> Result<Self> {
        if config.kms.provider != KmsProviderKind::Postgres {
            bail!("KMS rewrap maintenance requires kms.provider=postgres");
        }
        let runtime = Self::build(config, pool).await?;
        runtime
            .postgres()?
            .check_mounted_compatibility()
            .await
            .context("validate mounted KMS generations for maintenance")?;
        Ok(runtime)
    }

    /// Returns the provider injected into encryption, retrieval, and erasure owners.
    #[must_use]
    pub fn provider(&self) -> Arc<dyn KeyManagementProvider> {
        self.provider.clone()
    }

    /// Checks whether the live provider remains compatible with serving state.
    pub async fn check_readiness(&self) -> Result<()> {
        if let Some(provider) = &self.postgres {
            provider
                .check_compatibility()
                .await
                .context("Postgres KMS compatibility check failed")?;
        }
        Ok(())
    }

    /// Activates the configured required generation, drains bounded rewrap
    /// batches, and optionally retires one old generation after the drain.
    pub async fn rewrap_to_required(
        &self,
        batch_size: u32,
        retire_generation: Option<&str>,
    ) -> Result<RewrapReport> {
        if batch_size == 0 {
            bail!("KMS rewrap batch size must be greater than zero");
        }
        let provider = self.postgres()?;
        let state = provider
            .activate_generation(&self.required_generation)
            .await
            .with_context(|| {
                format!(
                    "activate required KMS generation {}",
                    self.required_generation
                )
            })?;
        let mut rewrapped = 0_u64;
        let mut batches = 0_u64;
        loop {
            let count = provider
                .rewrap_batch(batch_size)
                .await
                .context("rewrap KMS KEK batch")?;
            if count == 0 {
                break;
            }
            rewrapped = rewrapped
                .checked_add(count)
                .context("KMS rewrap count overflowed u64")?;
            batches = batches
                .checked_add(1)
                .context("KMS rewrap batch count overflowed u64")?;
        }
        let retired_generation = if let Some(generation) = retire_generation {
            provider
                .retire_generation(generation)
                .await
                .with_context(|| format!("retire KMS generation {generation}"))?;
            Some(generation.to_string())
        } else {
            None
        };
        Ok(RewrapReport {
            active_generation: state.active_generation,
            rewrapped,
            batches,
            retired_generation,
        })
    }

    async fn build(config: &MoaConfig, pool: PgPool) -> Result<Self> {
        let required_generation = config.kms.required_generation.clone();
        let (provider, postgres): (
            Arc<dyn KeyManagementProvider>,
            Option<Arc<PostgresKmsProvider>>,
        ) = match config.kms.provider {
            KmsProviderKind::Postgres => {
                let root_keys = load_root_key_ring(&config.kms.root_key_dir, &required_generation)
                    .await
                    .context("load KMS root-key directory for the postgres provider")?;
                tracing::info!(
                    root_key_dir = %root_keys.directory().display(),
                    required_generation = %root_keys.required_generation(),
                    "building persistent Postgres KMS provider"
                );
                let postgres = Arc::new(PostgresKmsProvider::new(pool, root_keys));
                (postgres.clone(), Some(postgres))
            }
            KmsProviderKind::Local => {
                let provider: Arc<dyn KeyManagementProvider> =
                    Arc::new(moa_crypto::LocalKmsProvider::new());
                (provider, None)
            }
        };

        kms_durability_guard(provider.is_durable(), config.kms.allow_ephemeral)
            .map_err(anyhow::Error::msg)?;
        if !provider.is_durable() {
            tracing::warn!(
                "using ephemeral in-process KMS (kms.allow_ephemeral=true); keys are lost on restart"
            );
        }

        Ok(Self {
            provider,
            postgres,
            required_generation,
        })
    }

    fn postgres(&self) -> Result<&PostgresKmsProvider> {
        self.postgres
            .as_deref()
            .context("KMS maintenance requires a Postgres provider")
    }
}

async fn load_root_key_ring(directory: &Path, required_generation: &str) -> Result<RootKeyRing> {
    let mut reader = tokio::fs::read_dir(directory)
        .await
        .with_context(|| format!("read KMS root-key directory {}", directory.display()))?;
    let mut entries = Vec::new();
    while let Some(entry) = reader
        .next_entry()
        .await
        .with_context(|| format!("enumerate KMS root-key directory {}", directory.display()))?
    {
        let generation = entry.file_name().into_string().map_err(|_| {
            anyhow::anyhow!(
                "KMS root-key directory {} contains a non-UTF-8 filename",
                directory.display()
            )
        })?;
        if generation.starts_with("..") {
            continue;
        }
        let path = entry.path();
        let encoded = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("read KMS root-key file {}", path.display()))?;
        entries.push((generation, encoded));
    }
    RootKeyRing::from_directory_entries(directory.to_path_buf(), required_generation, entries)
        .map_err(anyhow::Error::new)
}

fn kms_durability_guard(is_durable: bool, allow_ephemeral: bool) -> Result<(), String> {
    if is_durable || allow_ephemeral {
        return Ok(());
    }
    Err(
        "kms.provider=local uses ephemeral keys that are lost on restart; restricted/PHI data \
         sealed with them would become permanently unrecoverable. Set kms.provider=postgres (or \
         another persistent provider), or set kms.allow_ephemeral=true \
         (MOA_KMS_ALLOW_EPHEMERAL=true) for development and tests only."
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    use super::{kms_durability_guard, load_root_key_ring};

    #[test]
    fn kms_guard_requires_explicit_ephemeral_opt_in_offline() {
        // Pins: durable providers are always accepted while local ephemeral
        // providers require the explicit development/test opt-in.
        assert!(kms_durability_guard(true, false).is_ok());
        assert!(kms_durability_guard(true, true).is_ok());
        assert!(kms_durability_guard(false, true).is_ok());
        let error = kms_durability_guard(false, false)
            .expect_err("ephemeral provider without opt-in must fail closed");
        assert!(error.contains("kms.provider=postgres") && error.contains("allow_ephemeral"));
    }

    #[tokio::test]
    async fn root_key_loader_ignores_kubernetes_metadata_entries_offline() {
        // Pins: Kubernetes Secret metadata entries are not interpreted as key
        // generations while mounted generation files are loaded.
        let directory = tempfile::tempdir().expect("create key directory");
        tokio::fs::write(
            directory.path().join("primary"),
            BASE64.encode([0x5a_u8; 32]),
        )
        .await
        .expect("write primary key");
        tokio::fs::write(directory.path().join("..data"), "ignored")
            .await
            .expect("write metadata entry");

        let ring = load_root_key_ring(directory.path(), "primary")
            .await
            .expect("load root key ring");
        assert_eq!(ring.directory(), directory.path());
        assert_eq!(ring.required_generation(), "primary");
        assert_eq!(ring.generations().collect::<Vec<_>>(), vec!["primary"]);

        let absent = load_root_key_ring(&PathBuf::from("/definitely/missing/moa-kms"), "primary")
            .await
            .expect_err("missing mount must fail closed");
        assert!(absent.to_string().contains("read KMS root-key directory"));
    }
}
