//! Postgres container bootstrap for orchestrator service fixtures.

use super::*;

pub(super) async fn start_postgres_container() -> Result<ContainerAsync<GenericImage>> {
    GenericImage::new(POSTGRES_IMAGE, POSTGRES_TAG)
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::seconds(1))
        .with_env_var("POSTGRES_DB", POSTGRES_DB)
        .with_env_var("POSTGRES_USER", POSTGRES_USER)
        .with_env_var("POSTGRES_PASSWORD", POSTGRES_PASSWORD)
        .with_cmd([
            "postgres",
            "-c",
            "shared_preload_libraries=age,pgaudit",
            "-c",
            "session_preload_libraries=age",
            "-c",
            "wal_level=logical",
            "-c",
            "max_replication_slots=10",
            "-c",
            "max_wal_senders=10",
        ])
        .start()
        .await
        .context("start Postgres testcontainer")
}

pub(super) async fn ensure_postgres_image(repo_root: &Path) -> Result<()> {
    let image = format!("{POSTGRES_IMAGE}:{POSTGRES_TAG}");
    let inspect_status = tokio::process::Command::new("docker")
        .args(["image", "inspect", &image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("inspect Postgres test image with Docker")?;
    if inspect_status.success() {
        return Ok(());
    }

    let build_status = tokio::process::Command::new("docker")
        .current_dir(repo_root)
        .args([
            "build",
            "-f",
            "docker/postgres/Dockerfile",
            "--build-arg",
            "AGE_REF=release/PG17/1.7.0",
            "--build-arg",
            "PGVECTOR_REF=v0.8.2",
            "-t",
            &image,
            ".",
        ])
        .status()
        .await
        .context("build Postgres test image with Docker")?;
    if !build_status.success() {
        bail!("docker build for {image} failed with status {build_status}");
    }
    Ok(())
}

pub(super) async fn wait_for_postgres(postgres_url: &str) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match PgPoolOptions::new()
            .max_connections(1)
            .connect(postgres_url)
            .await
        {
            Ok(pool) => {
                pool.close().await;
                return Ok(());
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for Postgres testcontainer");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) => return Err(error).context("Postgres testcontainer did not become ready"),
        }
    }
}
