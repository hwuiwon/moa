# Hardware floor for `perf_gate`

The performance gate (M30) is calibrated against this configuration. Running on
weaker hardware will cause spurious failures; running on stronger hardware will
cause the gate to be too lax and miss regressions.

| Component       | Floor                                                |
| --------------- | ---------------------------------------------------- |
| CPU             | 8 vCPU, x86_64, AVX2 supported                       |
| Memory          | 32 GB                                                |
| Disk            | NVMe SSD, >= 500 MB/s sustained random read          |
| Postgres        | 17.6+, co-located on the same VM, shared_buffers=8GB |
| Network         | Embedder reachable in <= 50ms RTT P50                |
| Tokio runtime   | Multi-thread, default worker count                   |

CI nightly runs on `ubuntu-latest-8-core` which matches this floor (see
`.github/workflows/perf-gate.yml`).

If you are running this locally on a laptop, expect P95 to be 1.5-3x higher than
CI; treat local results as directional only.
