//! Redis/Valkey container bootstrap for orchestrator service fixtures.
//!
//! The spawned `moa-orchestrator-bin` is built with the `redis` runtime-cache backend, so the
//! internal (self-hosted) fixture path must point it at a running Redis-compatible server. This
//! boots a throwaway Valkey container mirroring the compose stack's `valkey/valkey:8-alpine`, so
//! the cost lanes are hermetic instead of depending on ambient `MOA_RUNTIME_CACHE_REDIS_URL`.

use super::*;

pub(super) async fn start_redis_container() -> Result<ContainerAsync<GenericImage>> {
    start_redis_container_on_port(None).await
}

pub(super) async fn start_redis_container_on_port(
    host_port: Option<u16>,
) -> Result<ContainerAsync<GenericImage>> {
    let image = GenericImage::new(REDIS_IMAGE, REDIS_TAG)
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"));
    let image = match host_port {
        Some(host_port) => image.with_mapped_port(host_port, 6379.tcp()),
        None => image.into(),
    };
    image.start().await.context("start Valkey testcontainer")
}

pub(super) async fn wait_for_redis(redis_url: &str) -> Result<()> {
    let addr = redis_host_port(redis_url)?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match tokio::net::TcpStream::connect(&addr).await {
            Ok(_) => return Ok(()),
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for Redis testcontainer");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error).context("Redis testcontainer did not become ready"),
        }
    }
}

fn redis_host_port(redis_url: &str) -> Result<String> {
    let parsed =
        url::Url::parse(redis_url).with_context(|| format!("parse Redis URL {redis_url}"))?;
    let host = parsed.host_str().context("Redis URL missing host")?;
    let port = parsed.port().unwrap_or(6379);
    Ok(format!("{host}:{port}"))
}
