# R10 — Kubernetes Manifests

## Purpose

Package `moa-orchestrator` as a container, deploy a 3-node `RestateCluster` via the `restate-operator`, wire the orchestrator as a `RestateDeployment`, and configure HPA, PDB, and graceful shutdown. First production-shape deployment — this is where sub-phase A ends and Kubernetes is proven.

End state: a Kubernetes namespace `moa-system` contains a 3-replica Restate cluster on NVMe PVCs, a 6-replica orchestrator behind HPA, and a smoke test confirms end-to-end handler invocation routes through Kubernetes networking.

## Prerequisites

- R01–R09 complete. All handlers work on local `restate-server`.
- Kubernetes cluster available (production target; use kind/minikube for dev).
- `kubectl` and `helm` installed.
- Container registry accessible (GHCR, ECR, or equivalent).
- `fast-ssd` StorageClass exists (or create one for NVMe-backed PVs).

## Read before starting

- `docs/12-restate-architecture.md` — "Kubernetes deployment" section
- Restate operator docs: https://docs.restate.dev/deploy/kubernetes
- `docs/11-v2-architecture.md` — Kubernetes topology overview

## Steps

### 1. Dockerfile

`Dockerfile` at workspace root:

```dockerfile
# syntax=docker/dockerfile:1.6

FROM rust:1.82 AS builder
WORKDIR /build
COPY . .
RUN cargo build --release -p moa-orchestrator

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /build/target/release/moa-orchestrator /usr/local/bin/moa-orchestrator
COPY --from=builder /build/moa-orchestrator/migrations /migrations
USER nonroot
ENTRYPOINT ["/usr/local/bin/moa-orchestrator"]
CMD ["--port", "9080"]
```

Build and push:

```bash
docker build -t ghcr.io/hwuiwon/moa-orchestrator:$(git rev-parse --short HEAD) .
docker push ghcr.io/hwuiwon/moa-orchestrator:$(git rev-parse --short HEAD)
```

### 2. Namespace and operator

`k8s/00-namespace.yaml`:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: moa-system
  labels:
    name: moa-system
```

Install the operator (once per cluster, not per env):

```bash
helm repo add restate https://restatedev.github.io/charts
helm upgrade --install restate-operator restate/restate-operator \
  --namespace restate-system \
  --create-namespace
```

Wait for CRDs to be Established:

```bash
kubectl wait --for condition=Established \
  crd/restateclusters.restate.dev \
  crd/restatedeployments.restate.dev \
  --timeout=120s
```

### 3. StorageClass (if not already present)

`k8s/01-storage-class.yaml`:

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: fast-ssd
provisioner: kubernetes.io/aws-ebs   # or gce-pd, azure-disk, csi driver
parameters:
  type: gp3
  iops: "6000"
  throughput: "500"
allowVolumeExpansion: true
volumeBindingMode: WaitForFirstConsumer
reclaimPolicy: Retain
```

Adjust provisioner + params per cloud. For on-prem / local NVMe, use a CSI driver like `topolvm` or `openebs-lvm`.

### 4. `RestateCluster` CRD

`k8s/10-restate-cluster.yaml`:

```yaml
apiVersion: restate.dev/v1
kind: RestateCluster
metadata:
  name: moa-restate
  namespace: moa-system
spec:
  replicas: 3
  storage:
    storageClassName: fast-ssd
    size: 200Gi
  resources:
    requests:
      cpu: "2"
      memory: 8Gi
    limits:
      cpu: "4"
      memory: 16Gi
  metrics:
    enabled: true
    port: 5122
  tracing:
    otlpEndpoint: http://alloy.observability.svc.cluster.local:4317
  retention:
    default: 48h        # overridden per-handler in code
    completionDefault: 1h
  image: restatedev/restate:1.5.0
```

Apply and wait:

```bash
kubectl apply -f k8s/10-restate-cluster.yaml
kubectl -n moa-system wait --for=condition=Ready restatecluster/moa-restate --timeout=600s
```

### 5. Postgres connection secret

The orchestrator needs `POSTGRES_URL`. Use a Kubernetes Secret populated from the Neon connection string:

```bash
kubectl -n moa-system create secret generic moa-postgres \
  --from-literal=url="postgres://user:pass@host/db?sslmode=require"
```

LLM provider keys similarly (for direct-to-provider Phase 1-2; later replaced by gateway with AWS Secrets Manager):

```bash
kubectl -n moa-system create secret generic moa-llm-keys \
  --from-literal=anthropic=sk-ant-... \
  --from-literal=openai=sk-... \
  --from-literal=openrouter=sk-or-...
```

### 6. `RestateDeployment` CRD

`k8s/20-orchestrator-deployment.yaml`:

```yaml
apiVersion: restate.dev/v1
kind: RestateDeployment
metadata:
  name: moa-orchestrator
  namespace: moa-system
spec:
  image: ghcr.io/hwuiwon/moa-orchestrator:REPLACE_WITH_SHA
  replicas: 6
  restateCluster: moa-restate
  serviceEndpoints:
    - uri: http://moa-orchestrator.moa-system.svc.cluster.local:9080
      services:
        - Session
        - SubAgent
        - Workspace
        - Consolidate
        - IngestSource
        - SessionStore
        - LLMGateway
        - ToolExecutor
        - MemoryStore
        - Health
  podTemplate:
    spec:
      terminationGracePeriodSeconds: 600
      containers:
        - name: orchestrator
          image: ghcr.io/hwuiwon/moa-orchestrator:REPLACE_WITH_SHA
          ports:
            - containerPort: 9080
              name: restate
          readinessProbe:
            httpGet:
              path: /_health/ready
              port: 9080
            initialDelaySeconds: 5
            periodSeconds: 5
          livenessProbe:
            httpGet:
              path: /_health/live
              port: 9080
            initialDelaySeconds: 30
            periodSeconds: 15
          env:
            - name: RUST_LOG
              value: info
            - name: POSTGRES_URL
              valueFrom:
                secretKeyRef:
                  name: moa-postgres
                  key: url
            - name: ANTHROPIC_API_KEY
              valueFrom:
                secretKeyRef:
                  name: moa-llm-keys
                  key: anthropic
            - name: OPENAI_API_KEY
              valueFrom:
                secretKeyRef:
                  name: moa-llm-keys
                  key: openai
            - name: RESTATE_ADMIN_URL
              value: http://moa-restate.moa-system.svc.cluster.local:9070
            - name: OTEL_EXPORTER_OTLP_ENDPOINT
              value: http://alloy.observability.svc.cluster.local:4317
          resources:
            requests:
              cpu: "500m"
              memory: 1Gi
            limits:
              cpu: "2"
              memory: 4Gi
```

Add `/_health/ready` and `/_health/live` HTTP endpoints to `moa-orchestrator` main.rs:
- `ready`: returns 200 when DB pool is healthy AND Restate admin is reachable AND handlers are registered
- `live`: returns 200 if the process is running (basic liveness)

### 7. HPA

`k8s/30-orchestrator-hpa.yaml`:

```yaml
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: moa-orchestrator
  namespace: moa-system
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: moa-orchestrator
  minReplicas: 2
  maxReplicas: 50
  metrics:
    - type: Resource
      resource:
        name: cpu
        target:
          type: Utilization
          averageUtilization: 60
  behavior:
    scaleUp:
      policies:
        - type: Percent
          value: 100
          periodSeconds: 30
    scaleDown:
      stabilizationWindowSeconds: 300
      policies:
        - type: Percent
          value: 10
          periodSeconds: 60
```

No KEDA. Plain CPU HPA per Phase 1–2 decisions.

### 8. PodDisruptionBudget

`k8s/40-orchestrator-pdb.yaml`:

```yaml
apiVersion: policy/v1
kind: PodDisruptionBudget
metadata:
  name: moa-orchestrator
  namespace: moa-system
spec:
  minAvailable: 2
  selector:
    matchLabels:
      app.kubernetes.io/name: moa-orchestrator
```

### 9. Graceful shutdown implementation

`moa-orchestrator/src/main.rs` — extend with signal handling:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ... existing init

    let shutdown = shutdown_signal();
    tokio::select! {
        res = HttpServer::new(endpoint).listen_and_serve(addr) => res?,
        _ = shutdown => {
            tracing::info!("shutdown signal received, draining");
            // Steps from R01 notes:
            // 1. Flip readiness to false (the readiness handler reads a flag).
            set_ready(false);
            // 2. Deregister from Restate admin.
            deregister_from_restate().await?;
            // 3. Wait for in-flight invocations (Restate handles this via drain).
            tokio::time::sleep(Duration::from_secs(5)).await;
            // 4. Exit.
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.ok(); };
    let sigterm = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = sigterm => {},
    }
}
```

### 10. Postgres partitioning via `pg_partman`

Add a migration for production that enables `pg_partman` on the events table:

`moa-orchestrator/migrations/002_partitioning.sql`:

```sql
CREATE EXTENSION IF NOT EXISTS pg_partman;

SELECT partman.create_parent(
    p_parent_table := 'public.events',
    p_control := 'timestamp',
    p_interval := '1 month',
    p_premake := 3
);

UPDATE partman.part_config
SET retention = '13 months',
    retention_keep_table = false,
    retention_keep_index = false
WHERE parent_table = 'public.events';
```

Run the `partman_maintenance` function on a cron or via a small sidecar. This is optional for R10; the existing default partition handles low volumes. Enable when event count exceeds a few million.

### 11. CI pipeline

`.github/workflows/deploy.yml`:

- On tag/release: build Docker image, push to GHCR with tag = git SHA.
- On merge to main: apply k8s manifests via `kubectl apply -f k8s/` with the new image SHA substituted.
- For production: manual approval step before applying.

Use `kustomize` or a templating tool to substitute the image SHA across manifests.

### 12. Smoke test

`k8s/scripts/smoke.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

NAMESPACE=${NAMESPACE:-moa-system}

echo "Waiting for orchestrator rollout..."
kubectl -n $NAMESPACE rollout status deployment/moa-orchestrator --timeout=600s

echo "Port-forward Restate ingress..."
kubectl -n $NAMESPACE port-forward svc/moa-restate 8080:8080 &
PF_PID=$!
trap "kill $PF_PID" EXIT
sleep 3

echo "Calling Health/ping..."
curl -sf -X POST http://localhost:8080/Health/ping \
  -H "Content-Type: application/json" \
  -d '{}' | tee /dev/stderr | grep -q "pong"

echo "Creating test session..."
SESSION_ID=$(uuidgen)
# ... similar curls for SessionStore/create_session, Session/post_message, etc.

echo "OK"
```

## Files to create or modify

- `Dockerfile` — new
- `k8s/00-namespace.yaml` — new
- `k8s/01-storage-class.yaml` — new (skip if cluster already has fast-ssd)
- `k8s/10-restate-cluster.yaml` — new
- `k8s/20-orchestrator-deployment.yaml` — new
- `k8s/30-orchestrator-hpa.yaml` — new
- `k8s/40-orchestrator-pdb.yaml` — new
- `k8s/scripts/smoke.sh` — new
- `.github/workflows/deploy.yml` — new or extend
- `moa-orchestrator/src/main.rs` — add `/_health/ready`, `/_health/live`, signal handling
- `moa-orchestrator/migrations/002_partitioning.sql` — new (optional for R10)

## Acceptance criteria

- [ ] `docker build` succeeds, image <200 MB.
- [ ] `kubectl apply -k k8s/` (or equivalent) applies all manifests without errors.
- [ ] `kubectl -n moa-system get restatecluster/moa-restate` reports Ready.
- [ ] `kubectl -n moa-system get pods` shows 3 Restate replicas + 6 orchestrator replicas (or whatever HPA settled at).
- [ ] Smoke test passes: Health/ping, session create, post_message round-trip.
- [ ] Rolling an orchestrator deployment (change image tag): zero session failures during the roll. Verify via event log — all sessions that were running during the roll still completed successfully.
- [ ] Killing a single Restate pod: cluster retains quorum, no session stalls.
- [ ] `kubectl -n moa-system top pods` — orchestrator pods under 2 CPU / 4 Gi each at idle.
- [ ] Scaling test: send 100 concurrent sessions, HPA scales up within 2 minutes.

## Notes

- **No KEDA** in this prompt. The architecture decision holds for Phase 1–2. Revisit at ~10k tenants when plain CPU HPA stops being enough.
- **Distroless base image**: smaller, fewer CVEs, no shell. If you need `exec` access for debugging, use a separate `-debug` variant or `kubectl debug`.
- **Local NVMe vs. network-attached volumes**: Restate's journal writes are hot-path. A local NVMe PV via CSI (e.g., topolvm, openebs-lvm) gives 10x better latency than EBS gp3. For cloud, consider local-storage provisioner with instance-store volumes; acceptable tradeoff if cluster has instance-level redundancy.
- **Namespace `moa-system`**: reserved for orchestration. Tenant workloads (sandboxes) go in separate namespaces as per `docs/06-hands-and-mcp.md`.
- **`terminationGracePeriodSeconds: 600`** is long. Needed because a pod may hold in-flight invocations for minutes (e.g., waiting on an LLM or tool). Kubernetes defaults (30s) would cut these off; Restate handles re-routing but it still produces replay noise.
- **Ingress**: the Restate cluster's ingress (port 8080) is not exposed externally. The gateway calls it from within the cluster via service DNS. Public ingress for the gateway comes from a separate Service/Ingress, out of scope for R10.

## What R11 expects

- Kubernetes deployment works end-to-end.
- OTel endpoint (`alloy.observability.svc.cluster.local:4317`) is referenced but not yet receiving; R11 stands up the receiver and dashboards.
- Graceful shutdown works; tested by rolling deployments with active sessions.
