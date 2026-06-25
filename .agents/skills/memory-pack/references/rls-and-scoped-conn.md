# RLS and ScopedConn

These rules apply to every memory-pack step that touches Postgres. They keep tenant boundaries enforceable from the database itself, not just from application code. Storage keys and GUCs use tenant and storage-partition names.

## Row-Level Security

- Use `FORCE ROW LEVEL SECURITY` on any table that holds tenant-scoped or contact-scoped data. Without `FORCE`, table owners (including the migration role) bypass RLS, which silently breaks tests that run as the owner.
- Do not grant `BYPASSRLS` to the application role. The audit trail and every test depends on the app role being subject to the same policies as production traffic.
- Policies must reference current GUC settings, such as `current_setting('moa.tenant_id', true)` or `current_setting('moa.storage_partition_id', true)`, rather than hardcoded values.
- Every tenant-scoped table needs at least a `SELECT` policy and a `INSERT/UPDATE/DELETE` policy. A missing write policy locks the table; a missing read policy leaks across tenants.

## ScopedConn and ScopeContext

- For any Postgres code that runs inside a brain turn or session lifecycle event, use `ScopedConn` (or whichever current type wraps a connection with tenant/contact storage GUCs). Bare `sqlx::PgPool::acquire()` is not allowed in scoped paths.
- Set GUCs inside the same transaction as the workload. A `SET LOCAL moa.storage_partition_id = 'xxx'` outside a transaction does not bind to the next query reliably across pool checkout.
- Prefer `SET LOCAL` over `SET`. `SET LOCAL` resets at COMMIT/ROLLBACK; `SET` persists to the connection and can leak between borrowers of a pooled connection.
- Read paths and write paths use the same `ScopedConn` shape. There is no read-only escape hatch.

## Common Mistakes to Avoid

- Setting a GUC and then using `pool.acquire()` to grab a different connection. The GUC binds to one connection; the pool may give you another.
- Running migrations as a role that has `BYPASSRLS` and then expecting policies to apply when the same migration test runs as the app role.
- Forgetting `FORCE ROW LEVEL SECURITY` and then debugging why local tests pass but a prod-like environment behaves differently.
- Inserting tenant-scoped rows without setting the storage GUC first. The insert succeeds (no policy denies), but a subsequent `SELECT` inside the same transaction may not see it because the policy now filters by a different GUC.

## Where to Look

- `crates/moa-memory/graph/src/scoped.rs` (or the current location of `ScopedConn`)
- `crates/moa-memory/graph/src/migrations/` for live RLS policy examples
- `docs/08-security.md` for the security model that motivates these rules
