include!("local_tools_support/sandbox_profile.rs");

use moa_core::{traits::HandProvider, types::hands::SandboxTier};
use moa_hands::LocalHandProvider;
use tempfile::tempdir;

#[tokio::test]
async fn docker_container_runs_with_hardening() {
    let dir = tempdir().unwrap();
    let provider = LocalHandProvider::new(dir.path()).await.unwrap();
    assert!(
        provider.docker_available(),
        "this test is Docker-selected by its `_docker` suffix; a run without Docker \
         must fail loudly rather than silently pass"
    );

    let handle = provider
        .provision(hand_spec_with_profile(
            SandboxTier::Container,
            deny_all_egress_profile(),
            "test-capabilities-v1",
        ))
        .await
        .unwrap();

    assert!(
        matches!(handle, moa_core::types::hands::HandHandle::Docker { .. }),
        "the container tier must produce a Docker handle, not a silently downgraded one"
    );

    let _result = async {
        let output = provider
            .execute(
                &handle,
                "bash",
                r#"{
                    "cmd": "echo \"uid=$(id -u)\"; echo \"gid=$(id -g)\"; cat /proc/self/status; echo '---MOUNTS---'; awk '$2==\"/\"{print $4}' /proc/mounts; echo '---NET---'; (wget -q -T 2 -O- http://169.254.169.254 >/dev/null 2>&1 && echo metadata=reachable) || echo metadata=blocked"
                }"#,
            )
            .await
            .unwrap();

        let rendered = output.to_text();
        assert!(
            !rendered.contains("uid=0"),
            "tier-1 Docker sandbox must not run as root:\n{rendered}"
        );
        assert!(
            rendered.contains("gid="),
            "Docker sandbox should report its effective group:\n{rendered}"
        );
        assert!(rendered.contains("NoNewPrivs:\t1"));
        assert!(rendered.contains("Seccomp:\t2"));
        let mounts = rendered
            .split("---MOUNTS---")
            .nth(1)
            .and_then(|section| section.split("---NET---").next())
            .unwrap_or_default();
        assert!(mounts.contains("ro"));
        assert!(rendered.contains("metadata=blocked"));
    }
    .await;

    let _ = provider.destroy(&handle).await;
}

/// The hardened container profile: no outbound network at all, everything else
/// deliberately unbounded so this test isolates the egress translation.
fn deny_all_egress_profile() -> moa_core::types::hands::SandboxProfile {
    use moa_core::types::hands::{
        CpuLimit, DiskLimit, EgressPolicy, LifetimeLimit, MemoryLimit, SandboxProfile,
    };
    SandboxProfile::new(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::DenyAll,
        LifetimeLimit::Unbounded,
        LifetimeLimit::Unbounded,
    )
    .expect("deny-all profile should validate")
}
