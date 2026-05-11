# OIDC group mapping

`OidcGroupSync` is an optional additive sync from identity-provider group names
to OpenFGA tuples. P1.7 writes missing tuples through the authz outbox and does
not delete tuples when a user leaves a group. Full removal semantics land with
SCIM reconciliation.

## Naming convention

Use colon-delimited group names:

```text
tenant:<tenant_uuid>:admin
tenant:<tenant_uuid>:member
tenant:<tenant_uuid>:workspace:<workspace_uuid>:admin
tenant:<tenant_uuid>:workspace:<workspace_uuid>:member
```

The sync maps those to:

```text
user:<user_uuid> admin tenant:<tenant_uuid>
user:<user_uuid> member tenant:<tenant_uuid>
user:<user_uuid> admin workspace:<workspace_uuid>
user:<user_uuid> member workspace:<workspace_uuid>
```

The `tenant:<tenant_uuid>` prefix is included so one IdP can serve multiple
MOA tenants without group-name collisions.

## Operational note

This sync is best-effort and additive in P1.7. Removing a user from an IdP
group does not remove the corresponding OpenFGA tuple yet, so operators should
not use it as the only revocation path for production access.
