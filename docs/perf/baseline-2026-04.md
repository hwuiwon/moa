# Perf gate baseline - 2026-04

First green run on CI hardware floor (8 vCPU / 32 GB / NVMe).

| Metric                  | Value  |
| ----------------------- | ------ |
| Total P50               | 18 ms  |
| Total P95               | 64 ms  |
| Total P99               | 142 ms |
| Cache hit rate          | 0.78   |
| RLS leaks               | 0      |
| Embedder P95            | 22 ms  |
| Graph leg P95           | 11 ms  |
| Vector leg P95          | 9 ms   |
| Lexical leg P95         | 6 ms   |
| RRF + rerank P95        | 7 ms   |

Update this file when intentional architectural changes shift the baseline; do
not update it to absorb regressions.
