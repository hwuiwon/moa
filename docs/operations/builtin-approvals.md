# Builtin Async-Authz Challenges And Action Reviews

The default `async_authz.provider = "builtin"` configuration uses an in-app
challenge workflow. Builtin async-authz requests are stored in
`builtin_pending_approvals` and are separate from tenant action reviews.

```sh
curl -H "Authorization: Bearer <key>" http://localhost:10080/v1/authz-challenges
curl -X POST -H "Authorization: Bearer <key>" \
  -H "Content-Type: application/json" \
  http://localhost:10080/v1/authz-challenges/<id>/decision \
  --data '{"outcome":"approved","reason":null}'
```

The workflow waiting on a builtin async-authz challenge is suspended on a
Restate awakeable. When the user decides the row, the authz challenge service
updates the row and resolves that awakeable with the decision payload.

Tenant tool actions use action policy instead. `AdminReview` creates a row
in `tenant_action_reviews`, returns a pending-review tool result to the
model, and lets the turn continue. Tenant admins list and decide those rows
through:

```sh
curl -H "Authorization: Bearer <key>" \
  http://localhost:10080/v1/action-reviews
curl -X POST -H "Authorization: Bearer <key>" \
  -H "Content-Type: application/json" \
  http://localhost:10080/v1/action-reviews/<review_id>/decision \
  --data '{"decision":"cleared","reason":null}'
```

## Operational Notes

- Pending builtin async-authz challenges time out after
  `async_authz.default_timeout_secs` (default 900 seconds).
- The authz challenge reaper sweeps builtin async-authz rows every 30 seconds
  and resolves timed-out awakeables with `{"outcome":"timeout"}`.
- Resolved builtin async-authz rows remain in Postgres for audit and later OCSF
  emission. Tenant action-review audit state is the review row plus session
  `ActionReviewRequested` and `ActionReviewDecided` events.
- Privacy export and erasure approvals use separate Ed25519 approval JWTs with
  a `tenant_id` claim matching the request tenant; they do not use workspace
  scope claims.
