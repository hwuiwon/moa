//! Deterministic scaffolding for sub-agent delegation helper behavior.

mod support {
    pub mod durable_step_recorder;
}

use moa_core::{
    AttachSubAgentResultWaiterInput, AttachSubAgentResultWaiterOutput,
    ConsumeSubAgentChildResultInput, ConsumeSubAgentChildResultOutput, ListSubAgentsInput,
    ReserveSubAgentInput, SpawnSubAgentInput, SubAgentChildRef, SubAgentChildRequest,
    SubAgentResult, SubAgentState, SubAgentStatus, SubAgentTerminalResult, WaitSubAgentInput,
    delegation_tool_schema, is_delegation_tool_name,
};
use serde::Serialize;
use serde_json::json;
use support::durable_step_recorder::{DurableStep, Recorder, canonical_json};

#[test]
fn root_spawn_list_wait_shell_pins_expected_operation_order() {
    // Pins: root-session detached delegation keeps spawn, list, and wait operations ordered.
    assert_required_delegation_tool_names_are_registered();
    let spawn = spawn_input("root-review");

    let (recorded_spawn, trace) = record_root_spawn_list_wait_trace(spawn.clone());

    assert_eq!(recorded_spawn, spawn);
    assert_eq!(
        operation_labels(&trace),
        vec![
            "invoke:Session/child_refs",
            "run:spawn_sub_agent_id",
            "invoke:Session/register_child",
            "invoke:SubAgent/post_message",
            "invoke:SessionStore/append_event",
            "invoke:Session/child_refs",
            "invoke:SubAgent/status",
            "invoke:Session/consume_child_result",
            "invoke:Session/child_refs",
            "invoke:SubAgent/attach_result_waiter",
            "invoke:Session/consume_child_result",
        ]
    );
    assert_run_input(&trace, "spawn_sub_agent_id", &spawn);
}

#[test]
fn nested_spawn_list_wait_shell_pins_expected_operation_order() {
    // Pins: nested detached delegation reserves through the parent sub-agent before list and wait.
    assert_required_delegation_tool_names_are_registered();
    let spawn = spawn_input("nested-review");
    let reserve = reserve_input(&spawn);

    let (recorded_reserve, trace) = record_nested_spawn_list_wait_trace(spawn);

    assert_eq!(recorded_reserve, reserve);
    assert_eq!(
        operation_labels(&trace),
        vec![
            "invoke:SubAgent/reserve_child",
            "invoke:SubAgent/post_message",
            "invoke:SessionStore/append_event",
            "invoke:SubAgent/child_refs",
            "invoke:SubAgent/status",
            "invoke:SubAgent/consume_child_result",
            "invoke:SubAgent/child_refs",
            "invoke:SubAgent/attach_result_waiter",
            "invoke:SubAgent/consume_child_result",
        ]
    );
    assert_invoke_input(&trace, "SubAgent", "reserve_child", &reserve);
}

#[cfg(feature = "integration")]
#[test]
#[ignore = "integration-feature smoke for delegation runtime contract; no live Restate stack required"]
fn integration_feature_spawn_list_wait_uses_result_waiters() {
    // Pins: integration builds keep the v2 spawn/list/wait contract on the waiter path.
    let (_, root_trace) = record_root_spawn_list_wait_trace(spawn_input("root-integration"));
    let (_, nested_trace) = record_nested_spawn_list_wait_trace(spawn_input("nested-integration"));

    for trace in [&root_trace, &nested_trace] {
        let labels = operation_labels(trace);
        assert!(
            labels
                .iter()
                .any(|label| label.ends_with("/attach_result_waiter"))
        );
        assert!(!labels.iter().any(|label| label.ends_with("/result")));
    }
}

fn record_root_spawn_list_wait_trace(
    spawn: SpawnSubAgentInput,
) -> (SpawnSubAgentInput, Vec<DurableStep>) {
    let mut recorder = Recorder::recording();
    let child = child_ref("session-root-child-1");

    recorder.invoke(
        "Session",
        "child_refs",
        &json!({ "session_id": "session-root" }),
        Vec::<SubAgentChildRef>::new,
    );
    let child_id = recorder.run("spawn_sub_agent_id", &spawn, || child.id.clone());
    recorder.invoke("Session", "register_child", &child, || ());
    recorder.invoke(
        "SubAgent",
        "post_message",
        &json!({ "sub_agent_id": child_id.clone(), "message": "initial_task" }),
        || (),
    );
    recorder.invoke(
        "SessionStore",
        "append_event",
        &json!({ "event": "SubAgentSpawned", "sub_agent_id": child.id.clone() }),
        || (),
    );

    let list = ListSubAgentsInput::default();
    recorder.invoke("Session", "child_refs", &list, || vec![child.clone()]);
    recorder.invoke("SubAgent", "status", &status_request(&child.id), || {
        status(SubAgentState::Running)
    });

    let wait = WaitSubAgentInput {
        sub_agent_id: child.id.clone(),
        timeout_ms: 0,
    };
    let consume = consume_input(&child.id);
    let attach = attach_input("root-waiter-1");
    recorder.invoke("Session", "consume_child_result", &consume, || {
        ConsumeSubAgentChildResultOutput { terminal: None }
    });
    recorder.invoke("Session", "child_refs", &wait, || vec![child.clone()]);
    recorder.invoke("SubAgent", "attach_result_waiter", &attach, || {
        AttachSubAgentResultWaiterOutput {
            terminal: Some(terminal_payload(&child.id)),
        }
    });
    recorder.invoke("Session", "consume_child_result", &consume, || {
        ConsumeSubAgentChildResultOutput {
            terminal: Some(terminal_payload(&child.id)),
        }
    });

    (spawn, recorder.finish())
}

fn record_nested_spawn_list_wait_trace(
    spawn: SpawnSubAgentInput,
) -> (ReserveSubAgentInput, Vec<DurableStep>) {
    let mut recorder = Recorder::recording();
    let child = child_ref("parent-sub-agent-child-1");
    let reserve = reserve_input(&spawn);

    recorder.invoke("SubAgent", "reserve_child", &reserve, || child.clone());
    recorder.invoke(
        "SubAgent",
        "post_message",
        &json!({ "sub_agent_id": child.id.clone(), "message": "initial_task" }),
        || (),
    );
    recorder.invoke(
        "SessionStore",
        "append_event",
        &json!({ "event": "SubAgentSpawned", "sub_agent_id": child.id.clone() }),
        || (),
    );

    let list = ListSubAgentsInput::default();
    recorder.invoke("SubAgent", "child_refs", &list, || vec![child.clone()]);
    recorder.invoke("SubAgent", "status", &status_request(&child.id), || {
        status(SubAgentState::Running)
    });

    let wait = WaitSubAgentInput {
        sub_agent_id: child.id.clone(),
        timeout_ms: 0,
    };
    let consume = consume_input(&child.id);
    let attach = attach_input("nested-waiter-1");
    recorder.invoke("SubAgent", "consume_child_result", &consume, || {
        ConsumeSubAgentChildResultOutput { terminal: None }
    });
    recorder.invoke("SubAgent", "child_refs", &wait, || vec![child.clone()]);
    recorder.invoke("SubAgent", "attach_result_waiter", &attach, || {
        AttachSubAgentResultWaiterOutput {
            terminal: Some(terminal_payload(&child.id)),
        }
    });
    recorder.invoke("SubAgent", "consume_child_result", &consume, || {
        ConsumeSubAgentChildResultOutput {
            terminal: Some(terminal_payload(&child.id)),
        }
    });

    (reserve, recorder.finish())
}

fn assert_required_delegation_tool_names_are_registered() {
    for name in ["spawn_sub_agent", "list_sub_agents", "wait_sub_agent"] {
        assert!(is_delegation_tool_name(name));
        assert!(
            delegation_tool_schema(name).is_some(),
            "{name} should have a v2 delegation schema"
        );
    }
}

fn spawn_input(task_name: &str) -> SpawnSubAgentInput {
    SpawnSubAgentInput {
        task: "summarize the delegated work".to_string(),
        task_name: Some(task_name.to_string()),
        tool_subset: vec!["file_search".to_string()],
        budget_tokens: 256,
        max_turns: Some(2),
    }
}

fn reserve_input(spawn: &SpawnSubAgentInput) -> ReserveSubAgentInput {
    ReserveSubAgentInput {
        request: SubAgentChildRequest {
            task: spawn.task.clone(),
            tool_subset: spawn.tool_subset.clone(),
            budget_tokens: spawn.budget_tokens,
            max_turns: spawn.max_turns,
            trusted_sandbox_manifest: None,
        },
        task_name: spawn.task_name.clone(),
    }
}

fn child_ref(id: &str) -> SubAgentChildRef {
    SubAgentChildRef {
        id: id.to_string(),
        task_hash: "stable-task-hash".to_string(),
        budget_tokens: 256,
        terminal: None,
    }
}

fn status(state: SubAgentState) -> SubAgentStatus {
    SubAgentStatus {
        state,
        depth: 1,
        tokens_used: 0,
        budget_remaining: 256,
        active_children: Vec::new(),
    }
}

fn terminal_result(sub_agent_id: &str) -> SubAgentResult {
    SubAgentResult {
        sub_agent_id: sub_agent_id.to_string(),
        success: true,
        output: "done".to_string(),
        tokens_used: 21,
        tools_invoked: 1,
        error: None,
    }
}

fn terminal_payload(sub_agent_id: &str) -> SubAgentTerminalResult {
    SubAgentTerminalResult {
        state: SubAgentState::Completed,
        result: terminal_result(sub_agent_id),
    }
}

fn consume_input(sub_agent_id: &str) -> ConsumeSubAgentChildResultInput {
    ConsumeSubAgentChildResultInput {
        sub_agent_id: sub_agent_id.to_string(),
    }
}

fn attach_input(awakeable_id: &str) -> AttachSubAgentResultWaiterInput {
    AttachSubAgentResultWaiterInput {
        awakeable_id: awakeable_id.to_string(),
    }
}

fn status_request(sub_agent_id: &str) -> serde_json::Value {
    json!({ "sub_agent_id": sub_agent_id })
}

fn operation_labels(trace: &[DurableStep]) -> Vec<String> {
    trace
        .iter()
        .map(|step| match step {
            DurableStep::Run { name, .. } => format!("run:{name}"),
            DurableStep::Invoke {
                service, method, ..
            } => format!("invoke:{service}/{method}"),
        })
        .collect()
}

fn assert_run_input(trace: &[DurableStep], name: &str, expected: &impl Serialize) {
    let expected = canonical_json(expected);
    let Some(DurableStep::Run {
        input_canonical_json,
        ..
    }) = trace
        .iter()
        .find(|step| matches!(step, DurableStep::Run { name: step_name, .. } if step_name == name))
    else {
        panic!("missing run step {name}");
    };
    assert_eq!(input_canonical_json, &expected);
}

fn assert_invoke_input(
    trace: &[DurableStep],
    service: &str,
    method: &str,
    expected: &impl Serialize,
) {
    let expected = canonical_json(expected);
    let Some(DurableStep::Invoke {
        input_canonical_json,
        ..
    }) = trace.iter().find(|step| {
        matches!(
            step,
            DurableStep::Invoke {
                service: step_service,
                method: step_method,
                ..
            } if step_service == service && step_method == method
        )
    })
    else {
        panic!("missing invoke step {service}/{method}");
    };
    assert_eq!(input_canonical_json, &expected);
}
