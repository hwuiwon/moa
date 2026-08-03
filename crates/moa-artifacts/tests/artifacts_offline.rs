//! Consolidated offline artifact integration tests.

#[path = "artifacts_offline/behavior_lab_offline.rs"]
mod behavior_lab_offline;
#[path = "artifacts_offline/connector_definition.rs"]
mod connector_definition;
#[path = "artifacts_offline/connector_governance.rs"]
mod connector_governance;
#[path = "artifacts_offline/definition_roundtrip.rs"]
mod definition_roundtrip;
#[path = "artifacts_offline/execution_plan_validation.rs"]
mod execution_plan_validation;
