# Builtin approvals

The default `async_authz.provider = "builtin"` configuration uses an in-app
approval workflow. Builtin async-authz requests are stored in
`builtin_pending_approvals`. Normal tool approvals are stored in the session
event log as `ApprovalRequested` events and resumed through the owning
`Session` or `SubAgent` awakeable. Both sources are shown to users through:

```sh
curl -H "Authorization: Bearer <key>" http://localhost:10080/v1/approvals
curl -X POST -H "Authorization: Bearer <key>" \
  -H "Content-Type: application/json" \
  http://localhost:10080/v1/approvals/<id>/decision \
  --data '{"outcome":"approved","reason":null}'
curl -X POST -H "Authorization: Bearer <key>" \
  -H "Content-Type: application/json" \
  http://localhost:10080/v1/approvals/<id>/decision \
  --data '{"outcome":"denied","reason":"wrong tool"}'
```

The workflow waiting on the approval is suspended on a Restate awakeable. When
the user decides a builtin async-authz row, the approvals service updates the
row and resolves that awakeable with the decision payload. When the user decides
a tool approval, the approvals service routes the decision to `Session/approve`
or `SubAgent/approve`; the turn workflow then appends `ApprovalDecided`.

## Operational notes

- Pending approvals time out after `async_authz.default_timeout_secs`
  (default 900 seconds) for builtin async-authz, and after
  `MOA_APPROVAL_TIMEOUT_SECS` (default 1800 seconds) for tool approvals.
- The approval reaper sweeps builtin async-authz rows every 30 seconds and
  resolves timed-out awakeables with `{"outcome":"timeout"}`. Tool approval
  timeouts are handled by the waiting turn workflow.
- Resolved builtin async-authz rows remain in Postgres for audit and later OCSF
  emission. Tool approval audit state is the session event log.
