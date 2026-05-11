# Builtin approvals

The default `async_authz.provider = "builtin"` configuration uses an in-app
approval workflow. Pending requests are stored in
`builtin_pending_approvals` and shown to users through:

```sh
moa approvals list
moa approvals approve <id>
moa approvals deny <id> --reason "wrong tool"
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
