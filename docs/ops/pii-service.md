# PII Service

`moa-pii-service` is the out-of-process inference sidecar used by `moa-memory-pii`.
It wraps the HuggingFace `openai/privacy-filter` token-classification model behind a
small FastAPI HTTP API so Rust crates do not link Python, transformers, or torch.

## Local Run

```bash
docker compose up -d moa-pii-service
curl -s http://localhost:8080/healthz
```

Classify text:

```bash
curl -s http://localhost:8080/classify \
  -H 'content-type: application/json' \
  -d '{"text":"My SSN is 123-45-6789","return_spans":true}'
```

The response shape is:

```json
{
  "spans": [
    { "start": 10, "end": 21, "category": "SSN", "confidence": 0.97 }
  ],
  "abstained": false,
  "model_version": "openai/privacy-filter:v1.0"
}
```

## Configuration

Environment variables:

- `MODEL`: HuggingFace model id. Default: `openai/privacy-filter`.
- `DEVICE`: `cpu` or `cuda`. Default: `cpu`.

CPU mode is intended for local development. GPU-backed cloud deployments should run the
same image with `DEVICE=cuda` on a node pool that has compatible NVIDIA drivers.

## Rust Client

Use `moa_memory_pii::OpenAiPrivacyFilterClassifier`:

```rust
use moa_memory_pii::{OpenAiPrivacyFilterClassifier, PiiClassifier};

let classifier = OpenAiPrivacyFilterClassifier::new("http://localhost:8080")?;
let result = classifier.classify("the auth service uses JWT").await?;
```

The client fails closed by default. Network, HTTP, or parse failures return
`PiiClass::Pii` with `abstained = true`; callers that need hard errors can use
`with_fail_closed_on_error(false)`.

## Operational Notes

- Keep inference out of MOA Rust binaries. The Rust crate is only an HTTP client.
- Tune thresholds in Rust through `PrivacyFilterThresholds`; future workspace config can
  pass per-category operating points into the classifier constructor.
- The sidecar should be warmed before high-volume ingestion because the first request loads
  model weights.
