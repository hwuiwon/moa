//! Offline contract tests for the Agents Restate service.

const AGENTS_SERVICE: &str = include_str!("../src/services/agents.rs");
const AGENT_ADMIN: &str = include_str!("../src/identity_admin/agents.rs");

#[test]
fn agents_deactivate_authorizes_before_durable_read_and_mutation() {
    // Pins: denied deactivation exits before tuple capture or DB mutation can be journaled.
    let body = slice_between(
        AGENTS_SERVICE,
        "async fn deactivate(&self, ctx: Context<'_>, id: Json<Uuid>)",
        "async fn grant_can_act_as",
    );

    let authz = body
        .find("require_agent_operator_or_tenant_admin(&identity, agent_id).await?;")
        .expect("deactivate should enforce agent operator or tenant admin authz");
    let tuple_read = body
        .find(".name(\"agents_deactivate_read_can_act_as\")")
        .expect("can_act_as tuple read should be captured in a named durable step");
    let mutation = body
        .find(".name(\"agents_deactivate\")")
        .expect("deactivation mutation should remain in its own durable step");

    assert!(
        authz < tuple_read,
        "deactivation authz must happen before durable tuple capture"
    );
    assert!(
        tuple_read < mutation,
        "captured can_act_as tuples must be journaled before the DB mutation step"
    );
}

#[test]
fn agents_deactivate_reads_can_act_as_inside_named_durable_step() {
    // Pins: replay uses the journaled tuple-read result instead of a fresh OpenFGA read.
    let body = slice_between(
        AGENTS_SERVICE,
        "async fn deactivate(&self, ctx: Context<'_>, id: Json<Uuid>)",
        "async fn grant_can_act_as",
    );

    let read = body
        .find(".read(None, Some(\"can_act_as\"), Some(agent_wire.as_str()))")
        .expect("deactivate should read can_act_as tuples for inverse cleanup");
    let run = body[..read]
        .rfind(".run(|| async move")
        .expect("can_act_as read should be inside ctx.run");
    let step_name = body
        .find(".name(\"agents_deactivate_read_can_act_as\")")
        .expect("can_act_as read should have a stable Restate step name");
    let mutation = body
        .find(".name(\"agents_deactivate\")")
        .expect("deactivation mutation should remain journaled");

    assert!(
        run < read && read < step_name && step_name < mutation,
        "can_act_as read must be inside a named durable step before mutation"
    );
}

#[test]
fn agents_deactivate_still_uses_captured_can_act_as_for_tuple_cleanup() {
    // Pins: deactivation consumes the captured OpenFGA tuples and enqueues inverse deletes.
    let service_body = slice_between(
        AGENTS_SERVICE,
        "async fn deactivate(&self, ctx: Context<'_>, id: Json<Uuid>)",
        "async fn grant_can_act_as",
    );
    assert!(
        service_body
            .contains("agent_admin::deactivate_agent(pool, identity, agent_id, can_act_as).await"),
        "deactivation mutation should receive the journaled can_act_as tuple snapshot"
    );

    let admin_body = slice_between(
        AGENT_ADMIN,
        "pub(crate) async fn deactivate_agent",
        "/// Grant an agent the right to act as a user.",
    );
    let loop_start = admin_body
        .find("for tuple in can_act_as")
        .expect("deactivation should iterate captured can_act_as tuples");
    let loop_body = &admin_body[loop_start..];

    assert!(
        loop_body.contains("TupleOp::Delete"),
        "deactivation should enqueue inverse tuple deletes"
    );
    assert!(
        loop_body.contains("&tuple.user")
            && loop_body.contains("&tuple.relation")
            && loop_body.contains("&tuple.object"),
        "inverse cleanup should preserve each captured tuple key"
    );
}

fn slice_between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start_index = source
        .find(start)
        .expect("source start marker should exist");
    let rest = &source[start_index..];
    let end_index = rest.find(end).expect("source end marker should exist");
    &rest[..end_index]
}
