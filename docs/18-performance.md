# 18 - Performance

_Performance gate hardware floor and current baseline._

## Hardware Floor

The `perf_gate` is calibrated against this floor. Weaker hardware causes
spurious failures; stronger hardware can hide regressions.

| Component | Floor |
|---|---|
| CPU | 8 vCPU, x86_64, AVX2 supported |
| Memory | 32 GB |
| Disk | NVMe SSD, >= 500 MB/s sustained random read |
| Postgres | 17.6+, co-located on the same VM, `shared_buffers=8GB` |
| Network | Embedder reachable in <= 50 ms RTT P50 |
| Tokio runtime | Multi-thread, default worker count |

CI nightly runs on `ubuntu-latest-8-core`, which matches this floor. Laptop
results are directional only; expect P95 to be 1.5-3x higher than CI.

## Baseline - 2026-04

First green run on CI hardware floor:

| Metric | Value |
|---|---:|
| Total P50 | 18 ms |
| Total P95 | 64 ms |
| Total P99 | 142 ms |
| Cache hit rate | 0.78 |
| RLS leaks | 0 |
| Embedder P95 | 22 ms |
| Graph leg P95 | 11 ms |
| Vector leg P95 | 9 ms |
| Lexical leg P95 | 6 ms |
| RRF + rerank P95 | 7 ms |

Update the baseline only for intentional architectural shifts. Do not update it
to absorb regressions.
