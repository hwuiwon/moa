# Builtin approvals

The default `async_authz.provider = "builtin"` configuration uses an in-app
approval workflow. Pending requests are stored in
`builtin_pending_approvals` and shown to users through:

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
the user decides, the approvals service updates the row and resolves that
awakeable with the decision payload.

## Operational notes

- Pending approvals time out after `async_authz.default_timeout_secs`
  (default 900 seconds).
- The approval reaper sweeps every 30 seconds and resolves timed-out
  awakeables with `{"outcome":"timeout"}`.
- Resolved rows remain in Postgres for audit and later OCSF emission.
- The row is the durable source of truth. Awakeable resolution is the one-shot
  wakeup mechanism.
