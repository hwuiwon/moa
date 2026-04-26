# Apache AGE Installation

MOA's local Postgres image is built on the Postgres 17 line and pins Apache
AGE to `release/PG17/1.7.0`. The same image also installs pgvector `v0.8.2`
and the Debian `postgresql-17-pgaudit` package so the compose stack matches
the graph, vector, and audit extensions used by migrations.

Build and start the local database:

```sh
docker compose build postgres
docker compose up -d postgres
```

The compose service starts Postgres with:

```text
shared_preload_libraries=age,pgaudit
session_preload_libraries=age
```

AGE still requires transaction-local search path setup for Cypher. MOA's
`ScopedConn` installs `search_path = ag_catalog, "$user", public` alongside
the row-level-security GUCs before tenant queries run.
