# Local observability

The local overlay runs one ephemeral `grafana/otel-lgtm:0.29.2` pod in
`moa-system`. It is a development-only Phase 0 measurement backend; it is not a
replacement for the production Alloy and Grafana Cloud topology.

The service exposes these cluster-local ports:

| Port | Purpose |
|---:|---|
| 3000 | Grafana UI/API (`admin` / `admin`, local only) |
| 3100 | Loki query API |
| 3200 | Tempo query API |
| 4317 | OTLP/gRPC ingest |
| 4318 | OTLP/HTTP ingest |
| 9090 | Prometheus query API |

MOA edge and orchestrator export metrics, traces, and structured logs directly
over OTLP/gRPC. Setting `MOA_OBSERVABILITY_OTLP_ENDPOINT` is sufficient to
enable the three MOA signals; transport and metric-export cadence use the MOA
defaults. The local sample rate and environment remain explicit deployment
overrides.

Restate 1.7.2 exports traces over its supported `tracing-endpoint` OTLP/gRPC
setting; the local patch deliberately does not invent unsupported Restate
sampler variables. A small secondary config extends the pinned LGTM collector
with a per-pod Restate Prometheus scrape and a local-only filelog pipeline. The
LGTM pod is co-located with the overlay's single Restate pod and mounts
`/var/log/pods` read-only; it reads only `moa-restate` CRI paths, removes the CRI
envelope, and sends Restate's original JSON body to Loki with
`service.name=restate`. Postgres, OpenFGA, Valkey, and other infrastructure pod
logs remain outside this contract. Production uses Alloy and never uses this
host mount. Local Postgres lineage is enabled with `MOA_LINEAGE_SINK=postgres`.

All LGTM data uses `emptyDir` volumes and disappears when the pod is replaced.

Apply and inspect only the pinned local context:

```bash
kubectl --context kind-moa-local apply -k k8s/overlays/local
kubectl --context kind-moa-local -n moa-system rollout status deployment/moa-lgtm
kubectl --context kind-moa-local -n moa-system port-forward service/moa-lgtm \
  3000:3000 3100:3100 3200:3200 4317:4317 4318:4318 9090:9090
```

Open `http://localhost:3000` for Grafana. The local Postgres datasource is
provisioned at startup. Dashboards remain single-sourced under
`dashboards/grafana`; the Phase 0 report helper imports all of them through the
canonical `scripts/observability/sync-grafana-dashboards.sh` path after Grafana
starts. They are not copied into this Kustomize overlay.

After generating representative local traffic, collect the Phase 0 inventory:

```bash
bash k8s/overlays/local/phase0-observability-report.sh
```

The helper hard-codes `kubectl --context kind-moa-local`, establishes its
own query-port forwarding, creates a temporary local Grafana service account,
uses the canonical sync script to import every dashboard (including newly added
ones), deletes that account on exit, and reports active MOA metric series by
service and metric name, local Restate scrape targets and metric families, JSON
Restate log records, collector OTLP acceptance and export-failure counters,
direct MOA log lines and uncompressed bodies by service, completed spans and
Tempo search results by service, and the dashboard inventory. Successful empty
queries are labeled `no_data`; API or query errors stop the report with the
measurement label instead of being rendered as zero.
It never reads or changes the current kubectl context.

For a deliberately blocked product turn, correlate the session and active turn
with Restate SQL, the exact Tempo trace, and Loki records before requesting
product-level cancellation; see [the Restate operations
runbook](../../../docs/operations/restate-operations.md).
