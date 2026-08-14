# Auth Instructions

Read `docs/03-communication-layer.md`, `docs/08-security.md`, and the auth
placement rules in `docs/15-architecture-policy.md`. Keep schema constants,
authorization checks/outbox, provider identity, credential-vault behavior, and
bootstrap responsibilities in their documented child crates. Protected reads
must follow authz, delegated writes must retain delegation, and deletes must
enqueue inverse tuples transactionally.

Use `fast-pr`, `db-session`, and the serialized `authz-pentest` profile. Local
OpenFGA/Postgres prerequisites are infrastructure checks, not permission to run
external identity-provider or credentialed tests.
