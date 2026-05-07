//! Daemon tests.

use std::net::TcpListener as StdTcpListener;
use std::time::Duration;

use anyhow::Result;
use moa_session::testing;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uuid::Uuid;

use super::{daemon_info, request, run_daemon_server, stop_daemon, wait_for_daemon};
use moa_core::{
    DaemonCommand, DaemonReply, MoaConfig, Platform, SessionFilter, SessionId, StartSessionRequest,
    UserId, UserMessage, WorkspaceId,
};

static DAEMON_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn spawn_daemon_server(config: MoaConfig) -> tokio::task::JoinHandle<Result<()>> {
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(run_daemon_server(config))
    })
}

fn test_config() -> Option<MoaConfig> {
    if !live_provider_tests_enabled() {
        return None;
    }

    let dir = tempdir().ok()?;
    let base = dir.keep();
    let mut config = MoaConfig::default();
    config.database.url = testing::test_database_url();
    config.local.memory_dir = base.join("memory").display().to_string();
    config.local.sandbox_dir = base.join("sandbox").display().to_string();
    config.daemon.socket_path = base.join("daemon.sock").display().to_string();
    config.daemon.pid_file = base.join("daemon.pid").display().to_string();
    config.daemon.log_file = base.join("daemon.log").display().to_string();
    config.daemon.auto_connect = false;
    if let Some(fly) = config.cloud.flyio.as_mut() {
        fly.graceful_shutdown_timeout_secs = 1;
    }

    if std::env::var(&config.providers.openai.api_key_env).is_ok() {
        return Some(config);
    }
    if std::env::var(&config.providers.anthropic.api_key_env).is_ok() {
        config.set_main_model("anthropic", "claude-sonnet-4-6");
        return Some(config);
    }
    if std::env::var(&config.providers.google.api_key_env).is_ok() {
        config.set_main_model("google", "gemini-3.1-pro-preview");
        return Some(config);
    }

    None
}

fn live_provider_tests_enabled() -> bool {
    std::env::var("MOA_RUN_LIVE_PROVIDER_TESTS")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn random_port() -> u16 {
    StdTcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

#[tokio::test]
async fn daemon_ping_create_and_shutdown_roundtrip() -> Result<()> {
    let _guard = DAEMON_TEST_LOCK.lock().await;
    let Some(config) = test_config() else {
        return Ok(());
    };
    let server = spawn_daemon_server(config.clone());
    wait_for_daemon(&config, std::time::Duration::from_secs(5)).await?;

    let info = daemon_info(&config).await?;
    assert!(info.pid > 0);

    let session_id = match request(
        &config,
        &DaemonCommand::CreateSession {
            request: StartSessionRequest {
                workspace_id: WorkspaceId::new("default"),
                user_id: UserId::new("tester"),
                platform: Platform::Cli,
                model: config.general.default_model.clone().into(),
                initial_message: Some(UserMessage {
                    text: "start".to_string(),
                    attachments: Vec::new(),
                }),
                title: None,
                parent_session_id: None,
            },
        },
    )
    .await?
    {
        DaemonReply::SessionId(session_id) => session_id,
        other => panic!("unexpected create-session reply: {other:?}"),
    };
    assert_ne!(session_id, SessionId::default());

    stop_daemon(&config).await?;
    server.await.expect("daemon task join")?;
    Ok(())
}

#[tokio::test]
async fn daemon_lists_session_previews() -> Result<()> {
    let _guard = DAEMON_TEST_LOCK.lock().await;
    let Some(config) = test_config() else {
        return Ok(());
    };
    let workspace_id = WorkspaceId::new(format!("preview-{}", Uuid::now_v7()));
    let server = spawn_daemon_server(config.clone());
    wait_for_daemon(&config, std::time::Duration::from_secs(5)).await?;

    let empty_previews = match request(
        &config,
        &DaemonCommand::ListSessionPreviews {
            filter: SessionFilter {
                workspace_id: Some(workspace_id.clone()),
                ..SessionFilter::default()
            },
        },
    )
    .await?
    {
        DaemonReply::SessionPreviews(previews) => previews,
        other => panic!("unexpected preview reply: {other:?}"),
    };
    assert!(empty_previews.is_empty());

    let _ = request(
        &config,
        &DaemonCommand::CreateSession {
            request: StartSessionRequest {
                workspace_id: workspace_id.clone(),
                user_id: UserId::new("tester"),
                platform: Platform::Cli,
                model: config.general.default_model.clone().into(),
                initial_message: Some(UserMessage {
                    text: "preview".to_string(),
                    attachments: Vec::new(),
                }),
                title: None,
                parent_session_id: None,
            },
        },
    )
    .await?;
    let previews = match request(
        &config,
        &DaemonCommand::ListSessionPreviews {
            filter: SessionFilter {
                workspace_id: Some(workspace_id),
                ..SessionFilter::default()
            },
        },
    )
    .await?
    {
        DaemonReply::SessionPreviews(previews) => previews,
        other => panic!("unexpected preview reply: {other:?}"),
    };
    assert!(!previews.is_empty());

    stop_daemon(&config).await?;
    server.await.expect("daemon task join")?;
    Ok(())
}

#[tokio::test]
async fn daemon_create_session_uses_explicit_client_scope() -> Result<()> {
    let _guard = DAEMON_TEST_LOCK.lock().await;
    let Some(config) = test_config() else {
        return Ok(());
    };
    let scope_suffix = Uuid::now_v7().simple().to_string();
    let alpha_workspace = WorkspaceId::new(format!("alpha-{scope_suffix}"));
    let beta_workspace = WorkspaceId::new(format!("beta-{scope_suffix}"));
    let server = spawn_daemon_server(config.clone());
    wait_for_daemon(&config, std::time::Duration::from_secs(5)).await?;

    for workspace_id in [alpha_workspace.clone(), beta_workspace.clone()] {
        let reply = request(
            &config,
            &DaemonCommand::CreateSession {
                request: StartSessionRequest {
                    workspace_id,
                    user_id: UserId::new("tester"),
                    platform: Platform::Cli,
                    model: config.general.default_model.clone().into(),
                    initial_message: Some(UserMessage {
                        text: "scoped".to_string(),
                        attachments: Vec::new(),
                    }),
                    title: None,
                    parent_session_id: None,
                },
            },
        )
        .await?;
        assert!(matches!(reply, DaemonReply::SessionId(_)));
    }

    let alpha_sessions = match request(
        &config,
        &DaemonCommand::ListSessions {
            filter: SessionFilter {
                workspace_id: Some(alpha_workspace.clone()),
                ..SessionFilter::default()
            },
        },
    )
    .await?
    {
        DaemonReply::Sessions(sessions) => sessions,
        other => panic!("unexpected sessions reply: {other:?}"),
    };
    let beta_sessions = match request(
        &config,
        &DaemonCommand::ListSessions {
            filter: SessionFilter {
                workspace_id: Some(beta_workspace.clone()),
                ..SessionFilter::default()
            },
        },
    )
    .await?
    {
        DaemonReply::Sessions(sessions) => sessions,
        other => panic!("unexpected sessions reply: {other:?}"),
    };

    assert_eq!(alpha_sessions.len(), 1);
    assert_eq!(beta_sessions.len(), 1);
    assert_eq!(alpha_sessions[0].workspace_id, alpha_workspace);
    assert_eq!(beta_sessions[0].workspace_id, beta_workspace);

    stop_daemon(&config).await?;
    server.await.expect("daemon task join")?;
    Ok(())
}

#[tokio::test]
async fn daemon_health_endpoint_responds_when_cloud_enabled() -> Result<()> {
    let _guard = DAEMON_TEST_LOCK.lock().await;
    let Some(mut config) = test_config() else {
        return Ok(());
    };
    config.cloud.enabled = true;
    config.cloud.hands = None;
    if let Some(fly) = config.cloud.flyio.as_mut() {
        fly.health_bind = "127.0.0.1".to_string();
        fly.internal_port = random_port();
        fly.graceful_shutdown_timeout_secs = 1;
    }

    let port = config
        .cloud
        .flyio
        .as_ref()
        .expect("fly config")
        .internal_port;
    let server = spawn_daemon_server(config.clone());
    wait_for_daemon(&config, Duration::from_secs(5)).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");

    stop_daemon(&config).await?;
    server.await.expect("daemon task join")?;
    Ok(())
}
