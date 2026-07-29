# Secret Scanning

MOA local API keys use this public detection regex for GitHub secret-scanning
partner registration:

```text
moa_(live|prod|stg|dev)_[A-Za-z0-9]{32}_[a-f0-9]{8}
```

The regex is the whole of MOA's side of the contract today: GitHub's scanning
service matches leaked keys against it, and revocation is handled out of band.

No partner webhook endpoint is registered. `POST
/v1/security/secret-scanning/github` previously existed and answered every
request with `501`, which advertised a contract nothing implemented — a caller
could not distinguish "registered but unbuilt" from "wrong URL", and the route
appeared in the public ladder as if it were part of the API. It returns `404`
like any other unknown path. When partner registration completes, register the
handler then, alongside the payload-signature verification GitHub requires.
