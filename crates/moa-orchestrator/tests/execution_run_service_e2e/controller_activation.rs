//! Service-E2E coverage for the bounded execution-run controller boundary.

use anyhow::Result;
use moa_core::types::identifiers::TenantId;
use moa_orchestrator::objects::execution_run_controller::{
    ExecutionRunAdvanceRequest, ExecutionRunAdvanceResponse,
};
use moa_test_support::OrchestratorTestFixture;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires local Restate, Postgres, OpenFGA, and the service-e2e feature lane"]
async fn controller_rejects_a_payload_for_another_virtual_object_key() -> Result<()> {
    // Pins: the deployed Restate object validates its key before any Postgres lookup, so a
    // dispatch payload cannot redirect an activation to a different tenant-owned run.
    let fixture = OrchestratorTestFixture::shared().await?;
    let test = fixture.isolated().await;
    let object_run_uid = Uuid::now_v7();
    let payload_run_uid = Uuid::now_v7();
    let result = test
        .client()
        .post_call::<_, ExecutionRunAdvanceResponse>(
            &format!("/ExecutionRunController/{object_run_uid}/advance"),
            &ExecutionRunAdvanceRequest {
                dispatch_uid: Uuid::now_v7(),
                tenant_id: TenantId::new(),
                run_uid: payload_run_uid,
                controller_generation: 1,
                wake_epoch: 1,
            },
        )
        .await;

    match result {
        Ok(response) => anyhow::bail!(
            "mismatched controller key unexpectedly returned a response: {response:?}"
        ),
        Err(error) => {
            assert!(
                error.to_string().contains("does not match run_uid"),
                "unexpected controller rejection: {error:#}"
            );
            Ok(())
        }
    }
}
