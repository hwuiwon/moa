include!("local_tools_support/docker.rs");

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
