//! Restart-stable KMS, object-store, sandbox-root, and crash-barrier fixtures.

use std::fs::OpenOptions;

use moa_crypto::{EncryptionContext, KeyHandle, KeyManagementProvider as _, WrappedDek};
use moa_kms::{PostgresKmsProvider, ROOT_KEY_LEN, RootKeyRing};
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};

use super::*;

const ROOT_KEY_GENERATION: &str = "primary";

/// Durable resources retained while only the orchestrator child is restarted.
pub struct SandboxWorkspaceFixture {
    root_key_dir: TempDir,
    sandbox_root: TempDir,
    purge_log_dir: TempDir,
    rustfs: RustFsFixture,
}

impl SandboxWorkspaceFixture {
    /// Creates a permission-restricted KMS mount, durable sandbox root, and RustFS namespace.
    pub async fn start() -> Result<Self> {
        let root_key_dir = restricted_tempdir("moa-kms-root-keys-")?;
        write_root_key(root_key_dir.path())?;
        let sandbox_root = restricted_tempdir("moa-sandbox-workspace-")?;
        let purge_log_dir = restricted_tempdir("moa-sandbox-purge-log-")?;
        let rustfs = RustFsFixture::start().await?;
        Ok(Self {
            root_key_dir,
            sandbox_root,
            purge_log_dir,
            rustfs,
        })
    }

    /// Returns the directory mounted as the local sandbox root across child restarts.
    #[must_use]
    pub fn sandbox_root(&self) -> &Path {
        self.sandbox_root.path()
    }

    /// Returns the fixture's authenticated RustFS namespace.
    #[must_use]
    pub fn rustfs(&self) -> &RustFsFixture {
        &self.rustfs
    }

    /// Counts entries into the exact external purge phase across child restarts.
    pub async fn purge_external_phase_count(&self, operation_id: &str) -> Result<usize> {
        Ok(self
            .purge_external_phase_trace(operation_id)
            .await?
            .into_iter()
            .filter(|phase| phase == "entered")
            .count())
    }

    /// Loads the ordered external-purge phase trace for one operation.
    pub async fn purge_external_phase_trace(&self, operation_id: &str) -> Result<Vec<String>> {
        let path = self.purge_log_dir.path().join("external-phase.log");
        let contents = match tokio::fs::read_to_string(path).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).context("read sandbox purge phase log"),
        };
        Ok(contents
            .lines()
            .filter_map(|entry| entry.split_once('\t'))
            .filter(|(entry_operation, _)| *entry_operation == operation_id)
            .map(|(_, phase)| phase.to_string())
            .collect())
    }

    /// Persists a KMS-wrapped probe, exact checkpoint bytes, and a sandbox marker.
    ///
    /// The returned value is opaque so tests cannot accidentally serialize key
    /// handles or use plaintext DEK bytes as assertion fixtures.
    pub async fn prepare_restart_probe(
        &self,
        postgres_url: &str,
        checkpoint_bytes: &[u8],
    ) -> Result<WorkspaceRestartProbe> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(postgres_url)
            .await
            .context("connect restart-probe Postgres")?;
        let provider = PostgresKmsProvider::new(pool.clone(), self.load_root_key_ring().await?);
        provider
            .check_compatibility()
            .await
            .context("initialize restart-stable Postgres KMS")?;
        let context = EncryptionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            format!("workspace-fixture-{}", Uuid::now_v7()),
            "sandbox_workspace_checkpoint",
        );
        let generated = provider
            .generate_data_key(&context)
            .await
            .context("generate restart-probe data key")?;
        let plaintext_sha256 = Sha256::digest(generated.plaintext.expose()).into();
        let checkpoint_key = self
            .rustfs
            .put_probe(
                &format!("restart-probe-{}", Uuid::now_v7()),
                checkpoint_bytes,
            )
            .await?;
        let marker_path = self
            .sandbox_root
            .path()
            .join(format!("restart-probe-{}.bin", Uuid::now_v7()));
        tokio::fs::write(&marker_path, checkpoint_bytes)
            .await
            .with_context(|| format!("write sandbox restart probe {}", marker_path.display()))?;
        let checkpoint_sha256 = Sha256::digest(checkpoint_bytes).into();
        let root_key_sha256 = Sha256::digest(
            tokio::fs::read(self.root_key_path())
                .await
                .context("read fixture root key for restart fingerprint")?,
        )
        .into();
        pool.close().await;

        Ok(WorkspaceRestartProbe {
            context,
            wrapped: generated.wrapped,
            handle: generated.handle,
            plaintext_sha256,
            checkpoint_key,
            checkpoint_sha256,
            marker_path,
            root_key_sha256,
        })
    }

    /// Verifies KMS unwrap plus exact RustFS and sandbox bytes after a hard restart.
    pub async fn verify_restart_probe(
        &self,
        postgres_url: &str,
        probe: &WorkspaceRestartProbe,
    ) -> Result<()> {
        let root_key_bytes = tokio::fs::read(self.root_key_path())
            .await
            .context("read fixture root key after restart")?;
        if <[u8; 32]>::from(Sha256::digest(root_key_bytes)) != probe.root_key_sha256 {
            bail!("sandbox workspace fixture root-key generation changed across restart");
        }

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(postgres_url)
            .await
            .context("reconnect restart-probe Postgres")?;
        let provider = PostgresKmsProvider::new(pool.clone(), self.load_root_key_ring().await?);
        provider
            .check_compatibility()
            .await
            .context("check restarted Postgres KMS compatibility")?;
        let plaintext = provider
            .decrypt_data_key(&probe.wrapped, &probe.handle, &probe.context)
            .await
            .context("unwrap restart-probe data key after child restart")?;
        if <[u8; 32]>::from(Sha256::digest(plaintext.expose())) != probe.plaintext_sha256 {
            bail!("Postgres KMS unwrapped different data-key bytes after restart");
        }
        pool.close().await;

        self.rustfs.assert_available().await?;
        let checkpoint_bytes = self.rustfs.get_bytes(&probe.checkpoint_key).await?;
        if <[u8; 32]>::from(Sha256::digest(&checkpoint_bytes)) != probe.checkpoint_sha256 {
            bail!("RustFS checkpoint bytes changed across orchestrator restart");
        }
        let marker_bytes = tokio::fs::read(&probe.marker_path).await.with_context(|| {
            format!("read sandbox restart probe {}", probe.marker_path.display())
        })?;
        if <[u8; 32]>::from(Sha256::digest(marker_bytes)) != probe.checkpoint_sha256 {
            bail!("sandbox root bytes changed across orchestrator restart");
        }
        Ok(())
    }

    /// Deletes only this fixture's checkpoint prefix and unique bucket.
    pub async fn cleanup_namespace(&self) -> Result<()> {
        self.rustfs.cleanup_namespace().await
    }

    /// Returns the restart-stable KMS, object-store, and sandbox environment.
    #[must_use]
    pub fn orchestrator_env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("MOA_KMS_PROVIDER".to_string(), "postgres".to_string()),
            (
                "MOA_KMS_ROOT_KEY_DIR".to_string(),
                self.root_key_dir.path().display().to_string(),
            ),
            (
                "MOA_KMS_REQUIRED_GENERATION".to_string(),
                ROOT_KEY_GENERATION.to_string(),
            ),
            ("MOA_KMS_ALLOW_EPHEMERAL".to_string(), "false".to_string()),
            (
                "MOA_LOCAL_SANDBOX_DIR".to_string(),
                self.sandbox_root.path().display().to_string(),
            ),
            (
                "MOA_SANDBOX_WORKSPACE_PURGE_PHASE_LOG".to_string(),
                self.purge_log_dir
                    .path()
                    .join("external-phase.log")
                    .display()
                    .to_string(),
            ),
        ];
        env.extend(self.rustfs.orchestrator_env());
        env
    }

    async fn load_root_key_ring(&self) -> Result<RootKeyRing> {
        let encoded = tokio::fs::read_to_string(self.root_key_path())
            .await
            .context("read fixture KMS root-key file")?;
        RootKeyRing::from_directory_entries(
            self.root_key_dir.path().to_path_buf(),
            ROOT_KEY_GENERATION,
            [(ROOT_KEY_GENERATION, encoded)],
        )
        .context("decode fixture KMS root-key ring")
    }

    fn root_key_path(&self) -> PathBuf {
        self.root_key_dir.path().join(ROOT_KEY_GENERATION)
    }
}

/// Opaque durable-owner probe prepared before an orchestrator hard restart.
pub struct WorkspaceRestartProbe {
    context: EncryptionContext,
    wrapped: WrappedDek,
    handle: KeyHandle,
    plaintext_sha256: [u8; 32],
    checkpoint_key: String,
    checkpoint_sha256: [u8; 32],
    marker_path: PathBuf,
    root_key_sha256: [u8; 32],
}

/// One of the six durable windows available to workspace recovery tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxWorkspaceCrashBarrier {
    /// Capacity is reserved but provider creation has not started.
    PostReservationPreProviderCreate,
    /// Provider creation returned but durable activation has not committed.
    PostProviderCreatePreActivation,
    /// A mutating command returned but checkpoint publication has not started.
    PostCommandPreCheckpointPublication,
    /// Checkpoint bytes are verified but the authoritative head CAS has not committed.
    PostCheckpointReadyPreHeadCas,
    /// Provider deletion returned but durable absence is not confirmed.
    PostProviderDeletePreDurableConfirmation,
    /// Absence is confirmed but the capacity reservation has not been released.
    PostAbsenceConfirmationPreReservationRelease,
}

impl SandboxWorkspaceCrashBarrier {
    /// Returns the canonical failpoint value consumed by feature-gated production hooks.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PostReservationPreProviderCreate => "post_reservation_pre_provider_create",
            Self::PostProviderCreatePreActivation => "post_provider_create_pre_activation",
            Self::PostCommandPreCheckpointPublication => "post_command_pre_checkpoint_publication",
            Self::PostCheckpointReadyPreHeadCas => "post_checkpoint_ready_pre_head_cas",
            Self::PostProviderDeletePreDurableConfirmation => {
                "post_provider_delete_pre_durable_confirmation"
            }
            Self::PostAbsenceConfirmationPreReservationRelease => {
                "post_absence_confirmation_pre_reservation_release"
            }
        }
    }

    /// Returns all six barriers in lifecycle order.
    #[must_use]
    pub const fn all() -> [Self; 6] {
        [
            Self::PostReservationPreProviderCreate,
            Self::PostProviderCreatePreActivation,
            Self::PostCommandPreCheckpointPublication,
            Self::PostCheckpointReadyPreHeadCas,
            Self::PostProviderDeletePreDurableConfirmation,
            Self::PostAbsenceConfirmationPreReservationRelease,
        ]
    }
}

/// Filesystem control plane used to observe and release one armed crash barrier.
pub struct SandboxWorkspaceCrashControl {
    barrier: SandboxWorkspaceCrashBarrier,
    signal_dir: TempDir,
    release_dir: TempDir,
}

impl SandboxWorkspaceCrashControl {
    /// Creates an isolated signal/release pair for one exact barrier.
    pub fn new(barrier: SandboxWorkspaceCrashBarrier) -> Result<Self> {
        Ok(Self {
            barrier,
            signal_dir: restricted_tempdir("moa-workspace-failpoint-signal-")?,
            release_dir: restricted_tempdir("moa-workspace-failpoint-release-")?,
        })
    }

    /// Returns the exact test-only child environment that arms this barrier.
    #[must_use]
    pub fn orchestrator_env(&self) -> Vec<(String, String)> {
        vec![
            (
                "MOA_SANDBOX_WORKSPACE_FAILPOINT".to_string(),
                self.barrier.as_str().to_string(),
            ),
            (
                "MOA_SANDBOX_WORKSPACE_FAILPOINT_SIGNAL_DIR".to_string(),
                self.signal_dir.path().display().to_string(),
            ),
            (
                "MOA_SANDBOX_WORKSPACE_FAILPOINT_RELEASE_DIR".to_string(),
                self.release_dir.path().display().to_string(),
            ),
        ]
    }

    /// Waits until the child has durably signaled the exact armed barrier.
    pub async fn wait_until_reached(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.signal_path().is_file() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "workspace failpoint `{}` was not reached within {timeout:?}",
                    self.barrier.as_str()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Releases a child that is waiting at this barrier without crashing it.
    pub async fn release(&self) -> Result<()> {
        tokio::fs::write(self.release_path(), [])
            .await
            .with_context(|| format!("release workspace failpoint `{}`", self.barrier.as_str()))
    }

    fn signal_path(&self) -> PathBuf {
        self.signal_dir.path().join(self.barrier.as_str())
    }

    fn release_path(&self) -> PathBuf {
        self.release_dir.path().join(self.barrier.as_str())
    }
}

fn write_root_key(directory: &Path) -> Result<()> {
    let mut key = [0_u8; ROOT_KEY_LEN];
    rand::rngs::OsRng.fill_bytes(&mut key);
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key);
    let path = directory.join(ROOT_KEY_GENERATION);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("create fixture root-key file {}", path.display()))?;
    std::io::Write::write_all(&mut file, encoded.as_bytes())
        .with_context(|| format!("write fixture root-key file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync fixture root-key file {}", path.display()))
}

fn restricted_tempdir(prefix: &str) -> Result<TempDir> {
    let directory = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .with_context(|| format!("create restricted fixture directory with prefix {prefix}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .with_context(|| {
                format!(
                    "restrict fixture directory permissions {}",
                    directory.path().display()
                )
            })?;
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_barriers_have_exact_unique_lifecycle_names() {
        // Pins: production failpoint sites and the recovery matrix share exactly
        // six stable names; a typo must fail before a child process is launched.
        assert_eq!(
            SandboxWorkspaceCrashBarrier::all().map(SandboxWorkspaceCrashBarrier::as_str),
            [
                "post_reservation_pre_provider_create",
                "post_provider_create_pre_activation",
                "post_command_pre_checkpoint_publication",
                "post_checkpoint_ready_pre_head_cas",
                "post_provider_delete_pre_durable_confirmation",
                "post_absence_confirmation_pre_reservation_release",
            ]
        );
    }

    #[test]
    fn crash_control_uses_isolated_exact_child_environment() {
        // Pins: a recovery case arms one barrier through filesystem controls,
        // never through a public service endpoint or process-global mutable state.
        let control = SandboxWorkspaceCrashControl::new(
            SandboxWorkspaceCrashBarrier::PostCheckpointReadyPreHeadCas,
        )
        .expect("create crash control");
        let env = control.orchestrator_env();
        assert_eq!(env.len(), 3);
        assert_eq!(
            env[0],
            (
                "MOA_SANDBOX_WORKSPACE_FAILPOINT".to_string(),
                "post_checkpoint_ready_pre_head_cas".to_string()
            )
        );
        assert_ne!(env[1].1, env[2].1, "signal and release roots must differ");
    }

    #[cfg(unix)]
    #[test]
    fn generated_root_key_permissions_are_owner_only() {
        // Pins: the restart-stable KMS key is generated into an owner-only
        // directory and file rather than inherited from a developer machine.
        use std::os::unix::fs::PermissionsExt as _;

        let directory =
            restricted_tempdir("moa-kms-permissions-").expect("create restricted key directory");
        write_root_key(directory.path()).expect("write root key");
        let directory_mode = std::fs::metadata(directory.path())
            .expect("read directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(directory.path().join(ROOT_KEY_GENERATION))
            .expect("read key metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[tokio::test]
    #[ignore = "requires local Docker; uses disposable Postgres and RustFS testcontainers"]
    async fn restart_probe_reopens_kms_and_exact_storage_docker() -> Result<()> {
        // Pins: reconstructing every process-local client over the same durable
        // owners preserves the Postgres-wrapped key and exact checkpoint/root bytes.
        let repository = repo_root();
        ensure_postgres_image(&repository).await?;
        let postgres = start_postgres_container().await?;
        let port = fixture_host_port_ipv4(&postgres, "restart probe Postgres", 5432.tcp()).await?;
        let postgres_url = format!(
            "postgres://{POSTGRES_USER}:{POSTGRES_PASSWORD}@127.0.0.1:{port}/{POSTGRES_DB}"
        );
        wait_for_postgres(&postgres_url).await?;
        moa_migrations::run(&postgres_url).await?;

        let fixture = SandboxWorkspaceFixture::start().await?;
        let expected = b"exact portable checkpoint bytes across hard restart";
        let probe = fixture
            .prepare_restart_probe(&postgres_url, expected)
            .await?;
        fixture.verify_restart_probe(&postgres_url, &probe).await?;
        fixture.cleanup_namespace().await?;
        drop(postgres);
        Ok(())
    }
}
