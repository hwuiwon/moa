use std::collections::HashMap;
use std::time::Duration;

use moa_core::{HandProvider, HandResources, HandSpec, SandboxTier};
use moa_hands::LocalHandProvider;
use tempfile::tempdir;

#[tokio::test]
async fn docker_container_runs_with_hardening() {
    let dir = tempdir().unwrap();
    let provider = LocalHandProvider::new(dir.path()).await.unwrap();
    if !provider.docker_available() {
        return;
    }

    let handle = provider
        .provision(HandSpec {
            sandbox_tier: SandboxTier::Container,
            image: None,
            resources: HandResources::default(),
            env: HashMap::new(),
            workspace_mount: None,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(300),
        })
        .await
        .unwrap();

    if !matches!(handle, moa_core::HandHandle::Docker { .. }) {
        return;
    }

    let result = async {
        let output = provider
            .execute(
                &handle,
                "bash",
                r#"{
                    "cmd": "cat /proc/self/status; echo '---MOUNTS---'; awk '$2==\"/\"{print $4}' /proc/mounts; echo '---NET---'; (wget -q -T 2 -O- http://169.254.169.254 >/dev/null 2>&1 && echo metadata=reachable) || echo metadata=blocked"
                }"#,
            )
            .await
            .unwrap();

        assert!(output.stdout.contains("NoNewPrivs:\t1"));
        assert!(output.stdout.contains("Seccomp:\t2"));
        let mounts = output
            .stdout
            .split("---MOUNTS---")
            .nth(1)
            .and_then(|section| section.split("---NET---").next())
            .unwrap_or_default();
        assert!(mounts.contains("ro"));
        assert!(output.stdout.contains("metadata=blocked"));

        provider.pause(&handle).await.unwrap();
        provider.resume(&handle).await.unwrap();
    }
    .await;

    let _ = provider.destroy(&handle).await;
    result
}
