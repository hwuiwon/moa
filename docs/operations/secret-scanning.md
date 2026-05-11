# Secret Scanning

MOA local API keys use this public detection regex for GitHub secret-scanning
partner registration:

```text
moa_(live|prod|stg|dev)_[A-Za-z0-9]{32}_[a-f0-9]{8}
```

The edge route `POST /v1/security/secret-scanning/github` is reserved for the
GitHub partner webhook. Until partner registration is complete, the route
returns `501` with `X-Moa-Reason:
not-yet-implemented-pending-github-partner-registration`.
