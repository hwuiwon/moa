//! Origin-aware capability admission.
//!
//! Action-policy rules answer "may this tenant run this operation". They cannot
//! answer "may this *runtime* hold this capability at all", because a tenant
//! rule, a deployment default, and a tool's own intrinsic effect are all stated
//! without knowing whether the caller is a real session or an evaluation trial.
//!
//! This module is that second question, and it is deliberately a separate,
//! earlier gate rather than another effect in the same lattice. An experiment
//! trial reaching a production connector is not an action to review or a
//! decision to log as `Deny` — it is inadmissible, so it fails closed with
//! [`MoaError::PermissionDenied`] before rule evaluation, before review
//! rendering, and before any provider call.
//!
//! Admission is decided from the capability's own identity
//! ([`ToolCapabilityId`]) and its declared [`ActionClass`], never from the tool
//! name. A name allowlist would be defeated by the next registered connector;
//! the capability kind cannot be renamed into a different backend.

use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionClass,
    types::action_policy::CallOrigin, types::security::ToolCapabilityId,
};

/// Returns whether one action class stays inside the caller's own run.
///
/// The run-scoped classes are the ones whose entire effect is the caller's own
/// workspace or sandbox: reading, writing local workspace files, and running a
/// command in the execution environment. Every other class — external writes,
/// exports, destruction, permission changes, deployments, and money movement —
/// reaches something that outlives the run and therefore cannot be fixtured.
fn is_run_scoped(action_class: ActionClass) -> bool {
    match action_class {
        ActionClass::Read | ActionClass::LocalWrite | ActionClass::CommandExecution => true,
        ActionClass::ExternalWrite
        | ActionClass::DataExport
        | ActionClass::Destructive
        | ActionClass::PermissionChange
        | ActionClass::Deployment
        | ActionClass::MoneyMovement => false,
    }
}

/// Admits or refuses one capability for the origin that issued the call.
///
/// The rules, per origin:
///
/// - [`CallOrigin::Production`] admits everything; ordinary action policy is
///   the only authority.
/// - [`CallOrigin::Experiment`] refuses every MCP capability, because a
///   connector is a production integration holding tenant credentials and there
///   is no fixture form of it. Of the remaining capabilities it admits only
///   run-scoped ones: a sandbox (`Hand`) capability whose class stays inside the
///   run, and a built-in capability that only reads. A built-in that writes,
///   executes, or exports runs in the MOA process against real tenant state, so
///   it is a side-effecting host tool no matter how the trial is sandboxed.
/// - [`CallOrigin::GeneratedCode`] refuses everything.
///
/// Deliberately not part of these rules: any sandbox egress destination
/// allowlist. The supported sandbox providers refuse per-destination allowlists
/// outright (`docker run` has no such filter, and E2B exposes only an on/off
/// switch), so a control expressed that way would not be enforced anywhere.
pub fn admit_capability_for_origin(
    origin: CallOrigin,
    capability: &ToolCapabilityId,
    action_class: ActionClass,
) -> Result<()> {
    match origin {
        CallOrigin::Production => Ok(()),
        CallOrigin::GeneratedCode => Err(refusal(
            origin,
            capability,
            "generated code holds no MOA capabilities",
        )),
        CallOrigin::Experiment { .. } => match capability {
            ToolCapabilityId::Mcp { .. } => Err(refusal(
                origin,
                capability,
                "experiment trials may not reach production connectors",
            )),
            ToolCapabilityId::Hand { .. } if is_run_scoped(action_class) => Ok(()),
            ToolCapabilityId::BuiltIn { .. } if action_class == ActionClass::Read => Ok(()),
            ToolCapabilityId::Hand { .. } | ToolCapabilityId::BuiltIn { .. } => Err(refusal(
                origin,
                capability,
                "experiment trials admit only run-scoped fixture capabilities",
            )),
        },
    }
}

/// Builds the fail-closed refusal for one inadmissible capability.
///
/// The message carries only the origin label, the capability identity, and a
/// fixed reason — all configuration- or registry-bounded. The invocation input
/// never appears here: a refusal is rendered to the model, and echoing the
/// arguments back would hand an injected payload a second delivery path.
fn refusal(origin: CallOrigin, capability: &ToolCapabilityId, reason: &str) -> MoaError {
    MoaError::PermissionDenied(format!(
        "capability {} is not available to a {} call: {reason}",
        capability.render(),
        origin.as_str()
    ))
}

#[cfg(test)]
mod tests {
    use moa_core::types::action_policy::ActionClass;
    use moa_core::types::action_policy::CallOrigin;
    use moa_core::types::security::ToolCapabilityId;
    use uuid::Uuid;

    use super::admit_capability_for_origin;

    fn experiment() -> CallOrigin {
        CallOrigin::Experiment {
            run_uid: Uuid::from_u128(0x0e1),
            trial_uid: Some(Uuid::from_u128(0x0e2)),
        }
    }

    #[test]
    fn experiment_origin_refuses_every_connector_capability_whatever_its_class() {
        // Pins: an MCP capability is refused for an experiment trial on the
        // strength of being a connector alone. A read-only connector is still a
        // production integration holding tenant credentials, so the low-risk
        // class must not open a path the high-risk one closes.
        for action_class in [
            ActionClass::Read,
            ActionClass::LocalWrite,
            ActionClass::CommandExecution,
            ActionClass::ExternalWrite,
        ] {
            let error = admit_capability_for_origin(
                experiment(),
                &ToolCapabilityId::mcp("crm", "create_deal"),
                action_class,
            )
            .expect_err("an experiment trial must not hold a connector capability");
            assert!(
                matches!(error, moa_core::error::MoaError::PermissionDenied(message)
                    if message.contains("production connectors") && message.contains("crm")),
                "refusal must name the connector and the reason"
            );
        }

        // The same capability is admitted for production traffic, so the
        // refusal is a property of the origin and not a broken connector.
        admit_capability_for_origin(
            CallOrigin::Production,
            &ToolCapabilityId::mcp("crm", "create_deal"),
            ActionClass::ExternalWrite,
        )
        .expect("production traffic keeps its connectors");
    }

    #[test]
    fn experiment_origin_admits_run_scoped_sandbox_work_and_refuses_escaping_classes() {
        // Pins: a trial can still do the work it exists to do inside its own
        // fixture sandbox, while any class whose effect outlives the run is
        // refused. Without the first half the control is unusable; without the
        // second it is decorative.
        for action_class in [
            ActionClass::Read,
            ActionClass::LocalWrite,
            ActionClass::CommandExecution,
        ] {
            admit_capability_for_origin(
                experiment(),
                &ToolCapabilityId::hand("bash"),
                action_class,
            )
            .expect("run-scoped sandbox work stays available to a trial");
        }

        for action_class in [
            ActionClass::ExternalWrite,
            ActionClass::DataExport,
            ActionClass::Destructive,
            ActionClass::PermissionChange,
            ActionClass::Deployment,
            ActionClass::MoneyMovement,
        ] {
            let error = admit_capability_for_origin(
                experiment(),
                &ToolCapabilityId::hand("publish"),
                action_class,
            )
            .expect_err("a class that escapes the run must be refused");
            assert!(
                matches!(error, moa_core::error::MoaError::PermissionDenied(message)
                    if message.contains("run-scoped fixture capabilities"))
            );
        }
    }

    #[test]
    fn experiment_origin_admits_only_reads_among_host_built_ins() {
        // Pins: built-ins execute in the MOA process against real tenant state,
        // so a trial may read but never write, execute, or export through one —
        // the sandbox around the trial does not contain a host-side write.
        admit_capability_for_origin(
            experiment(),
            &ToolCapabilityId::builtin("memory_search"),
            ActionClass::Read,
        )
        .expect("a read-only built-in is run-scoped for a trial");

        for action_class in [
            ActionClass::LocalWrite,
            ActionClass::CommandExecution,
            ActionClass::ExternalWrite,
        ] {
            let error = admit_capability_for_origin(
                experiment(),
                &ToolCapabilityId::builtin("memory_write"),
                action_class,
            )
            .expect_err("a side-effecting host built-in must be refused");
            assert!(
                matches!(error, moa_core::error::MoaError::PermissionDenied(message)
                    if message.contains("run-scoped fixture capabilities"))
            );
        }
    }

    #[test]
    fn generated_code_origin_holds_no_capability_of_any_kind() {
        // Pins: deny-all for generated code is total. Not "no connectors", not
        // "no writes" — no capability at all, including the read-only sandbox
        // ones an experiment trial keeps.
        for capability in [
            ToolCapabilityId::builtin("memory_search"),
            ToolCapabilityId::hand("file_read"),
            ToolCapabilityId::mcp("crm", "lookup"),
        ] {
            let error = admit_capability_for_origin(
                CallOrigin::GeneratedCode,
                &capability,
                ActionClass::Read,
            )
            .expect_err("generated code must hold no capability");
            assert!(
                matches!(error, moa_core::error::MoaError::PermissionDenied(message)
                    if message.contains("generated code holds no MOA capabilities"))
            );
        }
    }
}
