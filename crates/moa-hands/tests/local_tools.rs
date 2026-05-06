use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use moa_core::{
    Event, HandProvider, HandResources, HandSpec, ModelId, SandboxTier, SessionMeta, SessionStore,
    ToolBudgetConfig, ToolInvocation, UserId, WorkspaceId,
};
use moa_hands::{LocalHandProvider, ToolRouter};
use moa_session::{PostgresSessionStore, testing};
use serde_json::json;
use tempfile::{TempDir, tempdir, tempdir_in};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn docker_mountable_tempdir() -> TempDir {
    let macos_docker_tmp = Path::new("/private/tmp");
    if macos_docker_tmp.exists() {
        return tempdir_in(macos_docker_tmp).expect("create Docker-mountable tempdir");
    }
    tempdir().expect("create tempdir")
}

fn session() -> SessionMeta {
    SessionMeta {
        workspace_id: WorkspaceId::new("workspace"),
        user_id: UserId::new("user"),
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    }
}

fn approximate_tokens(text: &str) -> u32 {
    let chars = text.chars().count() as u32;
    if chars == 0 { 0 } else { chars.div_ceil(4) }
}

async fn test_session_store() -> Arc<PostgresSessionStore> {
    let (store, _database_url, _schema_name) = testing::create_isolated_test_store().await.unwrap();
    Arc::new(store)
}

#[tokio::test]
async fn file_read_reads_written_content() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();
    let session = session();

    router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({ "path": "notes.txt", "content": "hello" }),
            },
        )
        .await
        .unwrap();
    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_read".to_string(),
                input: json!({ "path": "notes.txt" }),
            },
        )
        .await
        .unwrap();

    assert_eq!(output.to_text(), "hello");
}

#[tokio::test]
async fn str_replace_updates_only_the_target_region() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();
    let session = session();

    router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({
                    "path": "src/lib.rs",
                    "content": "fn demo() {\n    alpha();\n    beta();\n}\n",
                }),
            },
        )
        .await
        .unwrap();

    let (_, replace_output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "str_replace".to_string(),
                input: json!({
                    "path": "src/lib.rs",
                    "old_str": "    alpha();\n",
                    "new_str": "    gamma();\n",
                }),
            },
        )
        .await
        .unwrap();
    let rendered = replace_output.to_text();
    assert!(rendered.starts_with("--- a/src/lib.rs\n+++ b/src/lib.rs\n"));
    assert!(rendered.contains("-    alpha();"));
    assert!(rendered.contains("+    gamma();"));
    assert!(!rendered.contains("replaced 1 lines with 1 lines"));

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_read".to_string(),
                input: json!({ "path": "src/lib.rs" }),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        output.to_text(),
        "fn demo() {\n    gamma();\n    beta();\n}"
    );
}

#[tokio::test]
async fn file_write_overwrite_returns_compact_diff() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();
    let session = session();

    router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({
                    "path": "src/demo.rs",
                    "content": (1..=500)
                        .map(|index| format!("{index:03}: {}", "x".repeat(48)))
                        .collect::<Vec<_>>()
                        .join("\n"),
                }),
            },
        )
        .await
        .unwrap();

    let updated = (1..=500)
        .map(|index| match index {
            120 => "120: changed alpha".to_string(),
            121 => "121: changed beta".to_string(),
            122 => "122: changed gamma".to_string(),
            _ => format!("{index:03}: {}", "x".repeat(48)),
        })
        .collect::<Vec<_>>()
        .join("\n");

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({ "path": "src/demo.rs", "content": updated.clone() }),
            },
        )
        .await
        .unwrap();

    let rendered = output.to_text();
    assert!(rendered.starts_with("--- a/src/demo.rs\n+++ b/src/demo.rs\n"));
    assert!(rendered.contains("@@"));
    assert!(rendered.contains("-120:"));
    assert!(rendered.contains("+120: changed alpha"));
    assert!(approximate_tokens(&rendered) * 10 <= approximate_tokens(&updated) * 3);
}

#[tokio::test]
async fn file_search_finds_files_by_glob() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();
    let session = session();

    router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({ "path": "src/lib.rs", "content": "pub fn demo() {}" }),
            },
        )
        .await
        .unwrap();
    router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({ "path": "notes.txt", "content": "ignore me" }),
            },
        )
        .await
        .unwrap();

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_search".to_string(),
                input: json!({ "pattern": "**/*.rs" }),
            },
        )
        .await
        .unwrap();

    let rendered = output.to_text();
    assert!(rendered.contains("src/lib.rs"));
    assert!(!rendered.contains("notes.txt"));
}

#[tokio::test]
async fn file_search_skips_git_directory_contents() {
    let dir = tempdir().unwrap();
    let git_dir = dir.path().join(".git").join("logs");
    tokio::fs::create_dir_all(&git_dir).await.unwrap();
    tokio::fs::write(git_dir.join("HEAD"), "secret history")
        .await
        .unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();
    let session = session();

    router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({ "path": "src/lib.rs", "content": "pub fn demo() {}" }),
            },
        )
        .await
        .unwrap();

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_search".to_string(),
                input: json!({ "pattern": "**/*" }),
            },
        )
        .await
        .unwrap();

    let rendered = output.to_text();
    assert!(rendered.contains("src/lib.rs"));
    assert!(!rendered.contains(".git/logs/HEAD"));
}

#[tokio::test]
async fn file_search_skips_python_virtualenvs_in_remembered_workspace() {
    let dir = tempdir().unwrap();
    let workspace_root = dir.path().join("workspace-root");
    tokio::fs::create_dir_all(workspace_root.join(".venv/lib"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(workspace_root.join("server/core"))
        .await
        .unwrap();
    tokio::fs::write(
        workspace_root.join(".venv/lib/ignored.py"),
        "print('ignore')",
    )
    .await
    .unwrap();
    tokio::fs::write(workspace_root.join("server/core/views.py"), "print('keep')")
        .await
        .unwrap();

    let router = ToolRouter::new_local(dir.path().join("sandboxes"))
        .await
        .unwrap();
    let session = session();
    router
        .remember_workspace_root(session.workspace_id.clone(), workspace_root)
        .await;

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_search".to_string(),
                input: json!({ "pattern": "**/*.py" }),
            },
        )
        .await
        .unwrap();

    let rendered = output.to_text();
    assert!(rendered.contains("server/core/views.py"));
    assert!(!rendered.contains(".venv/lib/ignored.py"));
}

#[tokio::test]
async fn file_search_respects_moaignore_in_remembered_workspace() {
    let dir = tempdir().unwrap();
    let workspace_root = dir.path().join("workspace-root");
    tokio::fs::create_dir_all(workspace_root.join("data"))
        .await
        .unwrap();
    tokio::fs::create_dir_all(workspace_root.join("src"))
        .await
        .unwrap();
    tokio::fs::write(workspace_root.join(".moaignore"), "data\n")
        .await
        .unwrap();
    tokio::fs::write(workspace_root.join("data/fixtures.json"), "{}")
        .await
        .unwrap();
    tokio::fs::write(workspace_root.join("src/lib.rs"), "pub fn demo() {}")
        .await
        .unwrap();

    let router = ToolRouter::new_local(dir.path().join("sandboxes"))
        .await
        .unwrap();
    let session = session();
    router
        .remember_workspace_root(session.workspace_id.clone(), workspace_root)
        .await;

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_search".to_string(),
                input: json!({ "pattern": "**/*" }),
            },
        )
        .await
        .unwrap();

    let rendered = output.to_text();
    assert!(rendered.contains("src/lib.rs"));
    assert!(!rendered.contains("data/fixtures.json"));
}

#[tokio::test]
async fn file_search_truncates_pathological_match_sets() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();
    let session = session();

    for index in 0..1_050 {
        router
            .execute_authorized(
                &session,
                &ToolInvocation {
                    id: None,
                    name: "file_write".to_string(),
                    input: json!({
                        "path": format!("src/file-{index:04}.rs"),
                        "content": "pub fn demo() {}",
                    }),
                },
            )
            .await
            .unwrap();
    }

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_search".to_string(),
                input: json!({ "pattern": "**/*.rs" }),
            },
        )
        .await
        .unwrap();

    let rendered = output.to_text();
    assert!(rendered.contains("[search truncated at 1000 matches"));
    let structured = output.structured.expect("structured file search payload");
    let matches = structured
        .get("matches")
        .and_then(|value| value.as_array())
        .expect("matches array");
    assert_eq!(matches.len(), 1_000);
    assert_eq!(
        structured
            .get("truncated")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
}

#[tokio::test]
async fn default_router_excludes_provider_native_web_tools() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();

    assert!(!router.has_tool("web_search"));
    assert!(!router.has_tool("web_fetch"));
}

#[tokio::test]
async fn file_operations_reject_path_traversal() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();
    let session = session();

    let error = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_read".to_string(),
                input: json!({ "path": "../secret.txt" }),
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(error, moa_core::MoaError::PermissionDenied(_)));
}

#[tokio::test]
async fn approval_prompt_uses_remembered_workspace_root_for_commands() {
    let dir = tempdir().unwrap();
    let workspace_root = dir.path().join("workspace-root");
    tokio::fs::create_dir_all(&workspace_root).await.unwrap();
    let router = ToolRouter::new_local(dir.path().join("sandboxes"))
        .await
        .unwrap();
    let session = session();
    router
        .remember_workspace_root(session.workspace_id.clone(), workspace_root.clone())
        .await;

    let prepared = router
        .prepare_invocation(
            &session,
            &ToolInvocation {
                id: None,
                name: "bash".to_string(),
                input: json!({ "cmd": "pwd" }),
            },
        )
        .await
        .unwrap();
    let prompt = prepared.approval_prompt(uuid::Uuid::now_v7());
    let working_dir = prompt
        .parameters
        .iter()
        .find(|field| field.label == "Working dir")
        .map(|field| field.value.clone());

    assert_eq!(
        working_dir.as_deref(),
        Some(workspace_root.to_str().unwrap())
    );
}

#[tokio::test]
async fn approval_prompt_str_replace_diff_is_surgical() {
    let dir = tempdir().unwrap();
    let workspace_root = dir.path().join("workspace-root");
    tokio::fs::create_dir_all(workspace_root.join("src"))
        .await
        .unwrap();
    tokio::fs::write(
        workspace_root.join("src/lib.rs"),
        concat!(
            "line01\n",
            "line02\n",
            "line03\n",
            "line04\n",
            "line05\n",
            "target_line();\n",
            "line07\n",
            "line08\n",
            "line09\n",
            "line10\n",
            "line11\n",
            "line12\n",
        ),
    )
    .await
    .unwrap();
    let router = ToolRouter::new_local(dir.path().join("sandboxes"))
        .await
        .unwrap();
    let session = session();
    router
        .remember_workspace_root(session.workspace_id.clone(), workspace_root)
        .await;

    let prepared = router
        .prepare_invocation(
            &session,
            &ToolInvocation {
                id: None,
                name: "str_replace".to_string(),
                input: json!({
                    "path": "src/lib.rs",
                    "old_str": "target_line();\n",
                    "new_str": "renamed_line();\n",
                }),
            },
        )
        .await
        .unwrap();
    let prompt = prepared.approval_prompt(uuid::Uuid::now_v7());

    assert_eq!(prompt.file_diffs.len(), 1);
    assert!(prompt.file_diffs[0].before.contains("target_line();"));
    assert!(prompt.file_diffs[0].after.contains("renamed_line();"));
    assert!(!prompt.file_diffs[0].before.contains("line01"));
    assert!(!prompt.file_diffs[0].after.contains("line12"));
}

#[tokio::test]
async fn bash_captures_stdout_and_stderr() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();
    let session = session();

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "bash".to_string(),
                input: json!({ "cmd": "printf 'out'; printf 'err' 1>&2" }),
            },
        )
        .await
        .unwrap();

    assert_eq!(output.process_stdout(), Some("out"));
    assert_eq!(output.process_stderr(), Some("err"));
    assert_eq!(output.process_exit_code(), Some(0));
}

#[tokio::test]
async fn bash_success_output_is_truncated_to_router_budget() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();
    let session = session();

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "bash".to_string(),
                input: json!({
                    "cmd": "python3 -c \"print('x' * 120000)\""
                }),
            },
        )
        .await
        .unwrap();

    let text = output.to_text();
    assert!(output.truncated);
    assert!(output.original_output_tokens.is_some());
    assert!(text.contains("[output truncated from ~"));
    assert!(approximate_tokens(&text) <= 4_000);
}

#[tokio::test]
async fn bash_error_output_is_not_truncated() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();
    let session = session();

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "bash".to_string(),
                input: json!({
                    "cmd": "python3 -c \"import sys; sys.stderr.write('e' * 20000); sys.exit(7)\""
                }),
            },
        )
        .await
        .unwrap();

    let text = output.to_text();
    assert!(output.is_error);
    assert!(!output.truncated);
    assert_eq!(output.original_output_tokens, None);
    assert!(!text.contains("[output truncated from ~"));
    assert!(approximate_tokens(&text) > 4_000);
}

#[tokio::test]
async fn file_read_within_budget_is_not_router_truncated() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();
    let session = session();
    let content = (1..=100)
        .map(|index| format!("{index:03}: {}", "a".repeat(48)))
        .collect::<Vec<_>>()
        .join("\n");

    router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({ "path": "notes.txt", "content": content }),
            },
        )
        .await
        .unwrap();

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_read".to_string(),
                input: json!({ "path": "notes.txt", "start_line": 1, "end_line": 100 }),
            },
        )
        .await
        .unwrap();

    assert!(!output.truncated);
    assert_eq!(output.original_output_tokens, None);
    assert!(!output.to_text().contains("[output truncated from ~"));
}

#[tokio::test]
async fn file_read_budget_override_truncates_large_results() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path())
        .await
        .unwrap()
        .with_tool_budgets(ToolBudgetConfig {
            file_read: 2_000,
            ..ToolBudgetConfig::default()
        });
    let session = session();
    let content = (1..=200)
        .map(|index| format!("{index:03}: {}", "b".repeat(96)))
        .collect::<Vec<_>>()
        .join("\n");

    router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({ "path": "large.txt", "content": content }),
            },
        )
        .await
        .unwrap();

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_read".to_string(),
                input: json!({ "path": "large.txt", "start_line": 1, "end_line": 200 }),
            },
        )
        .await
        .unwrap();

    let text = output.to_text();
    assert!(output.truncated);
    assert!(output.original_output_tokens.is_some());
    assert!(text.contains("[output truncated from ~"));
    assert!(text.contains("to ~2000 tokens]"));
    assert!(approximate_tokens(&text) <= 2_000);
}

#[tokio::test]
async fn bash_respects_timeout() {
    let dir = tempdir().unwrap();
    let router = ToolRouter::new_local(dir.path()).await.unwrap();
    let session = session();

    let error = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "bash".to_string(),
                input: json!({ "cmd": "sleep 10", "timeout_secs": 1 }),
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(error, moa_core::MoaError::ToolError(message) if message.contains("timed out"))
    );
}

#[tokio::test]
async fn session_search_finds_prior_events() {
    let dir = tempdir().unwrap();
    let session_store = test_session_store().await;
    let router = ToolRouter::new_local(dir.path())
        .await
        .unwrap()
        .with_session_store(session_store.clone());
    let session = session();
    let session_id = session_store.create_session(session.clone()).await.unwrap();

    session_store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "deploy failed on port binding".to_string(),
                attachments: vec![],
            },
        )
        .await
        .unwrap();
    session_store
        .emit_event(
            session_id,
            Event::BrainResponse {
                text: "I found the deploy failure".to_string(),
                thought_signature: None,
                model: ModelId::new("claude-sonnet-4-6"),
                model_tier: moa_core::ModelTier::Main,
                input_tokens_uncached: 10,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 5,
                cost_cents: 1,
                duration_ms: 20,
            },
        )
        .await
        .unwrap();

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "session_search".to_string(),
                input: json!({ "query": "port binding", "last_n": 3 }),
            },
        )
        .await
        .unwrap();

    assert!(output.to_text().contains("deploy failed on port binding"));
    assert!(
        output
            .structured
            .as_ref()
            .and_then(|value| value.as_array())
            .is_some_and(|items| !items.is_empty())
    );
}

#[tokio::test]
async fn session_search_filters_error_events() {
    let dir = tempdir().unwrap();
    let session_store = test_session_store().await;
    let router = ToolRouter::new_local(dir.path())
        .await
        .unwrap()
        .with_session_store(session_store.clone());
    let session = session();
    let session_id = session_store.create_session(session.clone()).await.unwrap();

    session_store
        .emit_event(
            session_id,
            Event::Error {
                message: "deploy error".to_string(),
                recoverable: true,
            },
        )
        .await
        .unwrap();
    session_store
        .emit_event(
            session_id,
            Event::BrainResponse {
                text: "deploy completed successfully".to_string(),
                thought_signature: None,
                model: ModelId::new("claude-sonnet-4-6"),
                model_tier: moa_core::ModelTier::Main,
                input_tokens_uncached: 10,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 5,
                cost_cents: 1,
                duration_ms: 20,
            },
        )
        .await
        .unwrap();

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "session_search".to_string(),
                input: json!({ "query": "deploy", "event_type": "error" }),
            },
        )
        .await
        .unwrap();

    let rendered = output.to_text();
    assert!(rendered.contains("deploy error"));
    assert!(!rendered.contains("deploy completed successfully"));
}

#[tokio::test]
async fn local_bash_hard_cancel_kills_running_process() {
    let dir = tempdir().unwrap();
    let router = Arc::new(ToolRouter::new_local(dir.path()).await.unwrap());
    let session = session();
    let cancel_token = CancellationToken::new();
    let started = Instant::now();
    let invocation = ToolInvocation {
        id: None,
        name: "bash".to_string(),
        input: json!({ "cmd": "python3 -c 'import time; time.sleep(60)'" }),
    };

    let task = {
        let router = router.clone();
        let cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            router
                .execute_authorized_with_cancel(&session, &invocation, None, Some(&cancel_token))
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel_token.cancel();

    let error = task.await.unwrap().unwrap_err();
    assert!(matches!(error, moa_core::MoaError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn docker_file_tools_roundtrip_inside_container_workspace() {
    let dir = docker_mountable_tempdir();
    let provider = LocalHandProvider::new(dir.path()).await.unwrap();
    if !provider.docker_available() {
        return;
    }

    let handle = provider
        .provision(HandSpec {
            sandbox_tier: SandboxTier::Container,
            image: None,
            resources: HandResources::default(),
            env: std::collections::HashMap::new(),
            workspace_mount: None,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(300),
        })
        .await
        .unwrap();

    if !matches!(handle, moa_core::HandHandle::Docker { .. }) {
        return;
    }

    let _result = async {
        let write = provider
            .execute(
                &handle,
                "file_write",
                &json!({ "path": "nested/demo.txt", "content": "hello from docker file tool" })
                    .to_string(),
            )
            .await
            .unwrap();
        assert_eq!(
            write.to_text(),
            "[new file created: nested/demo.txt, 1 lines]"
        );
        assert!(
            dir.path().exists(),
            "Docker sandbox temp directory disappeared before file_read"
        );

        let read = provider
            .execute(
                &handle,
                "file_read",
                &json!({ "path": "nested/demo.txt" }).to_string(),
            )
            .await
            .unwrap();
        assert_eq!(read.to_text(), "hello from docker file tool");

        let replace = provider
            .execute(
                &handle,
                "str_replace",
                &json!({
                    "path": "nested/demo.txt",
                    "old_str": "hello from docker file tool",
                    "new_str": "hello from docker str_replace",
                })
                .to_string(),
            )
            .await
            .unwrap();
        assert!(
            replace
                .to_text()
                .contains("--- a/nested/demo.txt\n+++ b/nested/demo.txt\n")
        );

        let replaced = provider
            .execute(
                &handle,
                "file_read",
                &json!({ "path": "nested/demo.txt" }).to_string(),
            )
            .await
            .unwrap();
        assert_eq!(replaced.to_text(), "hello from docker str_replace");

        let search = provider
            .execute(
                &handle,
                "file_search",
                &json!({ "pattern": "**/*.txt" }).to_string(),
            )
            .await
            .unwrap();
        assert!(search.to_text().contains("nested/demo.txt"));

        let bash = provider
            .execute(
                &handle,
                "bash",
                &json!({ "cmd": "cat /workspace/nested/demo.txt" }).to_string(),
            )
            .await
            .unwrap();
        assert!(bash.to_text().contains("hello from docker str_replace"));
    }
    .await;

    let _ = provider.destroy(&handle).await;
    drop(dir);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn docker_bash_hard_cancel_stops_container_exec() {
    let dir = docker_mountable_tempdir();
    let provider = LocalHandProvider::new(dir.path()).await.unwrap();
    if !provider.docker_available() {
        return;
    }

    let handle = provider
        .provision(HandSpec {
            sandbox_tier: SandboxTier::Container,
            image: None,
            resources: HandResources::default(),
            env: std::collections::HashMap::new(),
            workspace_mount: None,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(300),
        })
        .await
        .unwrap();

    if !matches!(handle, moa_core::HandHandle::Docker { .. }) {
        return;
    }

    let cancel_token = CancellationToken::new();
    let started = Instant::now();
    let task = {
        let provider = provider.clone();
        let handle = handle.clone();
        let cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            provider
                .execute_with_cancel(
                    &handle,
                    "bash",
                    &json!({ "cmd": "sleep 60" }).to_string(),
                    Some(&cancel_token),
                )
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel_token.cancel();

    let error = task.await.unwrap().unwrap_err();
    assert!(matches!(error, moa_core::MoaError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(3));

    let _ = provider.destroy(&handle).await;
    drop(dir);
}
