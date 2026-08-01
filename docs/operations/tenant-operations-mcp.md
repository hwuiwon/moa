# Tenant-Operations MCP

MOA exposes a stateless Streamable HTTP MCP protected resource at `/mcp` for
tenant admins and operators. It is an inbound product-control surface in
`moa-edge`, not the outbound MCP client used by agent hands.

## Connection And Trust Boundary

Today, clients use the same API-key or OIDC bearer credentials accepted by
`moa-edge`. Each HTTP message is authenticated before JSON-RPC parsing,
contacts and agents are rejected, and OpenFGA must allow
`tenant:<identity.tenant_id>#operator`. Tenant scope is implicit and no tool
accepts a target-tenant override. Set exact comma-delimited values in
`MOA_EDGE_MCP_ALLOWED_HOSTS` and `MOA_EDGE_MCP_ALLOWED_ORIGINS`; wildcards,
empty entries, and origins containing paths are rejected at startup.

Access tokens stop at the edge. The internal Restate proxy removes
`Authorization` and caller-supplied `X-Moa-*` headers, then adds verified
identity headers. OpenFGA and the owning service retain resource-level and
operation-specific checks, including stricter agent-principal admin checks.

The endpoint is stateless JSON mode: clients do not receive an MCP session ID.
Use `tools/list` for discovery and persist returned domain run IDs in the
client. List limits are clamped to `1..=200`, analytics query results are
bounded to `1..=1000`, and session/event cursors are opaque URL-safe tokens.

## Model-Facing Contract

`tools/list` is the canonical contract. Every advertised tool includes:

- a selection description with `Use when:`, explicit side effects, `Returns:`,
  and a recommended `Next:` action;
- MCP annotations for read-only, destructive, idempotent, and open-world
  behavior;
- an input schema whose fields explain where IDs and documents come from,
  distinguish inline content from paths or URIs, and state retry semantics;
- JSON Schema enums for closed choices, including analytics aggregations,
  filter operators and sort directions, artifact kind/status/source format,
  and execution review decisions;
- numeric schema bounds matching runtime behavior, including list pagination
  and analytics row limits; and
- an `outputSchema` for the stable success envelope and a tool-specific
  description of the typed response under `data`.

Successful calls always return concise text plus this structured content:

```json
{
  "summary": "Validated artifact source.",
  "data": {
    "valid": true,
    "validation_report": {
      "errors": []
    }
  }
}
```

`summary` is for a human or short model observation. `data` is the complete
typed response from the owning MOA service and is the source for IDs, cursors,
statuses, scores, and subsequent tool arguments. A tool execution failure is
not a JSON-RPC transport failure: the result sets `isError: true` and returns
structured content shaped as `{"error":"..."}`. Callers must branch on
`isError` before reading `data`.

Tenant IDs, reviewer subjects, and internal dispatch tokens never appear in
input schemas. The edge injects verified tenant and reviewer identity. Models
should not guess UUIDs, statuses, artifact references, variant keys, cursors,
or evaluator names; obtain them from a preceding list/get/catalog/plan call.

## Tools

Read-only observation tools are `analytics_catalog`, `analytics_query`,
`sessions_list`, `session_get`, `session_events_list`, `lineage_explain`, and
`learning_candidates_list`. Analytics accepts only catalog-backed fields and
operators, never SQL. Session events contain redacted timeline summaries, not
raw event payloads.

Artifact and learning tools are `artifacts_list`, `artifact_export`,
`artifact_validate`, `artifact_import`, `artifact_publish`,
`learning_candidate_get`, `learning_candidate_accept_skill`, and
`learning_candidate_reject`. `artifact_import` creates a draft. For non-skill
artifacts, `artifact_publish` is a separate destructive confirmation. A skill
draft never activates through generic publish: only
`learning_candidate_accept_skill` may activate a learned candidate after its
regression review. Skills still use the generic artifact format for draft
authoring and inspection, so MCP does not duplicate legacy skill import paths.

Execution tools are `capabilities_list`, `execution_runs_list`,
`execution_run_status`, `execution_run_start`, `execution_run_cancel`,
`execution_review_decide`, and `execution_signal`. Start accepts either a
an activated `skill://...` reference with its exact revision, an objective, and
structured input; it enters the same origin-bound session admission path and
accepts neither a compiled-plan identifier nor raw plan JSON. Status includes immutable goal coverage,
reserved/actual budget, plan revision/provenance, aggregate progress, completion
checks, and explicit terminal gaps. Full task results use a separate bounded
listing response.

There are no eval tools. The platform regression harness (`moa-eval`) is a
CI/CLI/`xtask` surface and is not reachable from `/mcp`. Behavior Lab is the
only tenant evaluation product exposed here.

Experiment tools are `experiment_plan_generate`, `experiments_list`,
`experiment_run`, `experiment_status`, `experiment_trials_list`,
`experiment_trial_status`, `experiment_cancel`, `experiment_scores`,
`experiment_compare`, and `experiment_propose_improvements`. Generation,
execution, cancellation, and improvement proposal remain separate calls;
provider-backed generation and live runs are open-world operations.

Configurable-agent tools are `agent_definitions_list`,
`agent_installations_list`, `agent_definition_install`,
`agent_deployments_list`, `agent_definition_deploy`, `agent_revision_compare`,
`agent_revision_simulate`, and `agent_revision_simulation_compare`.
Agent-principal lifecycle tools are separately named
`agent_principal_register`, `agent_principals_list`, `agent_principal_get`,
`agent_principal_deactivate`, `agent_principal_grant_act_as`, and
`agent_principal_revoke_act_as`.

Tool annotations mark reads and validation as read-only/idempotent, draft
creation as additive, publish/deploy/cancel/review/deactivation/permission
changes as destructive, and provider or production execution as open-world.

## Tool Selection Guide

The following sequences cover the intended decision points. The detailed
field constraints and exact response description remain in `tools/list` so MCP
clients do not need a second hard-coded schema catalog.

| Goal | Inspect or plan | Mutate or execute | Verify or compare |
|---|---|---|---|
| Understand aggregate performance | `analytics_catalog` → `analytics_query` | — | Narrow with `sessions_list` |
| Diagnose one session | `sessions_list` → `session_get` → `session_events_list` | — | `lineage_explain` |
| Author or edit a skill draft without activating it | `artifacts_list` → `artifact_export` → `artifact_validate` | `artifact_import` | Draft remains inactive; only a separately generated learned candidate can activate |
| Edit a non-skill artifact: agent, connector, action, or experiment plan | `artifacts_list` → `artifact_export` → `artifact_validate` | `artifact_import` → `artifact_publish` | Run the relevant experiment |
| Review learned improvements | `learning_candidates_list` → `learning_candidate_get` | `learning_candidate_accept_skill` or `learning_candidate_reject` | `artifacts_list` plus an experiment |
| Run durable typed work | `capabilities_list` and `execution_runs_list` | `execution_run_start`; when waiting, `execution_review_decide` or `execution_signal`; if necessary, `execution_run_cancel` | Poll `execution_run_status` |
| Run a Behavior Lab experiment | `experiment_plan_generate` or artifact authoring | `experiment_run`; if necessary, `experiment_cancel` | `experiment_status`, `experiment_trials_list`, `experiment_trial_status`, `experiment_scores`, `experiment_compare` |
| Turn experiment evidence into proposals | `experiment_compare` | `experiment_propose_improvements` | Review through the learning-candidate tools |
| Install or upgrade a configurable agent | `agent_definitions_list`, `agent_installations_list`, `agent_deployments_list`, `agent_revision_compare` | `agent_definition_install` or `agent_definition_deploy` | `agent_revision_simulate` → `agent_revision_simulation_compare` before deployment when risk warrants it |
| Manage agent identities | `agent_principals_list` → `agent_principal_get` | `agent_principal_register`, `agent_principal_grant_act_as`, `agent_principal_revoke_act_as`, or `agent_principal_deactivate` | Re-read the principal/list; delegation checks remain service-owned |

Long-running calls return domain IDs, not MCP task IDs. Poll with the paired
status tool: experiment or simulation `run_uid` with `experiment_status`;
execution `run_uid` with
`execution_run_status`. Read scores only after a terminal completed status;
partial, blocked, and unsupported runs remain terminal but are not completed.

## Operator Workflows

1. Diagnose performance with the analytics catalog/query, then inspect bounded
   sessions, redacted events, and lineage.
2. Triage a learning summary, load its full candidate, and explicitly accept or
   reject it.
3. Export an artifact, edit locally, validate, import a draft, evaluate it, and
   publish only after review.
4. Inspect capabilities, start an execution run, poll status, and deliver review,
   signal, or cancellation input when required.
5. Generate or import an experiment plan draft, publish it, run trials, compare
   evidence, and separately propose reviewable improvements.
6. Author agents as generic artifacts, compare exact revisions, optionally
   simulate them, then explicitly install or deploy; manage agent principals
   through the distinct principal tools.

## Future Dashboard OAuth

The canonical protected-resource URI remains `/mcp`. A future customer
dashboard will provide login and consent as the authorization server, publish
RFC 9728 protected-resource and RFC 8414/OIDC authorization-server metadata,
use Authorization Code with PKCE plus RFC 8707 `resource`, and issue short-lived
audience-bound tokens. Workspace admins must select exactly one target tenant
during consent; the resulting token maps through the existing `AuthProvider`
to an `Identity` bound to that tenant. OAuth does not add a second authorization
stack or permit tool-level tenant selection.
