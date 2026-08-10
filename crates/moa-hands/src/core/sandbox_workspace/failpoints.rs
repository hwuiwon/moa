//! Feature-gated crash barriers for sandbox-workspace service fixtures.

#[cfg(feature = "sandbox-workspace-failpoints")]
use moa_core::error::MoaError;
use moa_core::error::Result;

/// Records entry into one external purge phase for crash-replay assertions.
#[cfg(feature = "sandbox-workspace-failpoints")]
pub(crate) async fn record_purge_external_phase(operation_id: &str, phase: &str) {
    use tokio::io::AsyncWriteExt as _;

    let Ok(path) = std::env::var("MOA_SANDBOX_WORKSPACE_PURGE_PHASE_LOG") else {
        return;
    };
    let result = async {
        let mut log = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        log.write_all(operation_id.as_bytes()).await?;
        log.write_all(b"\t").await?;
        log.write_all(phase.as_bytes()).await?;
        log.write_all(b"\n").await?;
        log.flush().await
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(operation_id, phase, %error, "failed to record test-only purge phase");
    }
}

/// No-op phase recorder in ordinary production builds.
#[cfg(not(feature = "sandbox-workspace-failpoints"))]
pub(crate) async fn record_purge_external_phase(_operation_id: &str, _phase: &str) {}

/// Pauses at one named workspace crash barrier when the fixture selects it.
///
/// Production builds compile this to a no-op. Feature-qualified fixture builds
/// signal arrival through one file and resume only after the matching release
/// file appears, allowing the parent process to terminate the orchestrator at
/// an exact durable boundary.
#[cfg(feature = "sandbox-workspace-failpoints")]
pub(crate) async fn hit(name: &str) -> Result<()> {
    if std::env::var("MOA_SANDBOX_WORKSPACE_FAILPOINT").as_deref() != Ok(name) {
        return Ok(());
    }
    let signal_dir = std::env::var("MOA_SANDBOX_WORKSPACE_FAILPOINT_SIGNAL_DIR").map_err(|_| {
        MoaError::ConfigError(
            "workspace failpoint requires MOA_SANDBOX_WORKSPACE_FAILPOINT_SIGNAL_DIR".to_string(),
        )
    })?;
    let release_dir =
        std::env::var("MOA_SANDBOX_WORKSPACE_FAILPOINT_RELEASE_DIR").map_err(|_| {
            MoaError::ConfigError(
                "workspace failpoint requires MOA_SANDBOX_WORKSPACE_FAILPOINT_RELEASE_DIR"
                    .to_string(),
            )
        })?;
    let signal = std::path::Path::new(&signal_dir).join(name);
    let release = std::path::Path::new(&release_dir).join(name);
    tokio::fs::create_dir_all(&signal_dir).await?;
    tokio::fs::write(&signal, b"reached").await?;
    loop {
        if tokio::fs::try_exists(&release).await? {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// No-op crash barrier in ordinary production builds.
#[cfg(not(feature = "sandbox-workspace-failpoints"))]
pub(crate) async fn hit(_name: &str) -> Result<()> {
    Ok(())
}
