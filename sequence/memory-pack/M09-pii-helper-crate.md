# Step M09 — `moa-memory-pii` crate (openai/privacy-filter integration)

_Build a reusable PII classification helper crate around OpenAI's open-weights `openai/privacy-filter` HuggingFace model. Used at ingestion time to set each fact's `pii_class`. Designed so other components (M21 envelope encryption, M23 export, M24 erasure) can call it later without a rewrite._

## 1 What this step is about

The graph schema requires a `pii_class ∈ {none, pii, phi, restricted}` on every node. We classify at ingestion time so the field is set before the row lands. `openai/privacy-filter` (Apache 2.0, released April 22 2026) is a 1.5B-total / 50M-active-parameter MoE token classifier that returns BIOES-tagged spans for 8 PII categories. We map those to MOA's four-tier `pii_class`.

The model can run on CPU (200–400 tok/s, FP32) or GPU (1500 tok/s, FP16). Local-mode docker-compose ships a pre-loaded inference container. Cloud-mode talks to a dedicated GPU pool.

## 2 Files to read

- M00 stack-pin (openai/privacy-filter HuggingFace, Apache 2.0)
- M04 (`PiiClass` enum in node module)

## 3 Goal

1. New crate `moa-memory-pii` exposing `PiiClassifier` trait + `OpenAiPrivacyFilterClassifier` impl.
2. Inference runs against a sidecar HTTP service (`moa-pii-service`) using HuggingFace `text-classification`-style endpoint. We do NOT bind transformers/torch into the Rust crate.
3. Local-mode docker-compose adds the inference container.
4. Helper API: `classify(text) -> PiiClass + spans + confidence`.
5. Operating-point knobs: precision-vs-recall thresholds per category (configurable per workspace later).

## 4 Rules

- **Inference is OUT-OF-PROCESS.** The Rust crate is an HTTP client, not a model runtime. This avoids forcing every consumer to link torch.
- **The classifier is a trait** so we can swap implementations: real model, mock for tests, or future on-device WASM impl.
- **Defaults to `Pii`** when the model abstains or errors — fail-closed.
- **Spans and category are returned**, not just the label. M21 needs spans to know what to encrypt; M24 needs them to know what to redact.
- **Reusable**: this crate is consumed at ingestion only for now; future PRs will add export-time and decrypt-time hooks. The trait must be generic enough to support both.

## 5 Tasks

### 5a Crate scaffold

```toml
# crates/moa-memory-pii/Cargo.toml
[package]
name = "moa-memory-pii"
version.workspace = true
edition.workspace = true

[dependencies]
async-trait = "0.1"
reqwest = { workspace = true, features = ["json", "rustls-tls"] }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = "1"
tracing = "0.1"
moa-core = { path = "../moa-core" }
```

### 5b Public API

`crates/moa-memory-pii/src/lib.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PiiClass { None, Pii, Phi, Restricted }

impl PiiClass {
    pub fn as_str(self) -> &'static str {
        match self { Self::None => "none", Self::Pii => "pii", Self::Phi => "phi", Self::Restricted => "restricted" }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiiSpan {
    pub start: usize, pub end: usize,
    pub category: PiiCategory, pub confidence: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PiiCategory {
    Person, Email, Phone, Address, Ssn, MedicalRecord, FinancialAccount, GovernmentId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiiResult {
    pub class: PiiClass,
    pub spans: Vec<PiiSpan>,
    pub model_version: String,
    pub abstained: bool,
}

#[async_trait::async_trait]
pub trait PiiClassifier: Send + Sync {
    async fn classify(&self, text: &str) -> Result<PiiResult, PiiError>;
}

#[derive(Debug, thiserror::Error)]
pub enum PiiError {
    #[error("inference: {0}")] Inference(String),
    #[error("network: {0}")]   Network(#[from] reqwest::Error),
    #[error("parse: {0}")]     Parse(String),
}
```

### 5c HTTP-backed impl

`crates/moa-memory-pii/src/openai_filter.rs`:

```rust
use crate::*;

pub struct OpenAiPrivacyFilterClassifier {
    client: reqwest::Client,
    base_url: String,           // e.g., http://moa-pii-service:8080
    model_version: String,
    fail_closed_on_error: bool, // default true
}

impl OpenAiPrivacyFilterClassifier {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().unwrap(),
            base_url: base_url.into(),
            model_version: "openai/privacy-filter:v1.0".into(),
            fail_closed_on_error: true,
        }
    }
}

#[async_trait::async_trait]
impl PiiClassifier for OpenAiPrivacyFilterClassifier {
    async fn classify(&self, text: &str) -> Result<PiiResult, PiiError> {
        let resp: serde_json::Value = self.client
            .post(format!("{}/classify", self.base_url))
            .json(&serde_json::json!({ "text": text, "return_spans": true }))
            .send().await?.error_for_status()?.json().await?;

        let spans = resp.get("spans").and_then(|s| serde_json::from_value(s.clone()).ok()).unwrap_or_default();
        let class = resolve_class(&spans);

        Ok(PiiResult {
            class, spans, model_version: self.model_version.clone(),
            abstained: resp.get("abstained").and_then(|a| a.as_bool()).unwrap_or(false),
        })
    }
}

fn resolve_class(spans: &[PiiSpan]) -> PiiClass {
    if spans.iter().any(|s| matches!(s.category,
        PiiCategory::Ssn | PiiCategory::MedicalRecord | PiiCategory::GovernmentId)
        && s.confidence > 0.85) { return PiiClass::Phi; }
    if spans.iter().any(|s| matches!(s.category, PiiCategory::FinancialAccount) && s.confidence > 0.9) {
        return PiiClass::Restricted;
    }
    if spans.iter().any(|s| s.confidence > 0.5) { return PiiClass::Pii; }
    PiiClass::None
}
```

### 5d Mock for tests

```rust
// src/mock.rs
pub struct MockClassifier { pub fixed: PiiResult }

#[async_trait::async_trait]
impl PiiClassifier for MockClassifier {
    async fn classify(&self, _: &str) -> Result<PiiResult, PiiError> { Ok(self.fixed.clone()) }
}
```

### 5e Inference sidecar service

Add to `docker-compose.yml`:

```yaml
moa-pii-service:
  image: moa/pii-service:0.1
  build:
    context: ./services/pii-service
  ports: ["8080:8080"]
  environment:
    MODEL: openai/privacy-filter
    DEVICE: cpu     # or cuda for GPU host
```

`services/pii-service/Dockerfile` is a thin Python FastAPI wrapping `transformers.pipeline("token-classification", model="openai/privacy-filter")`. Build instructions in `docs/ops/pii-service.md`.

### 5f Add to workspace

```toml
# Cargo.toml
[workspace]
members = [..., "crates/moa-memory-pii", ...]
```

## 6 Deliverables

- `crates/moa-memory-pii/Cargo.toml`.
- `crates/moa-memory-pii/src/{lib,openai_filter,mock}.rs`.
- `services/pii-service/{Dockerfile,main.py,requirements.txt}`.
- `docs/ops/pii-service.md` runbook.

## 7 Acceptance criteria

1. `cargo build -p moa-memory-pii` clean.
2. With sidecar up, `OpenAiPrivacyFilterClassifier::classify("My SSN is 123-45-6789")` returns `PiiClass::Phi` and a span.
3. `classify("the auth service uses JWT")` returns `PiiClass::None`.
4. Mock classifier round-trips correctly.
5. Sidecar handles 200 RPS on CPU host (smoke benchmark, not a hard gate).

## 8 Tests

```sh
docker compose up -d moa-pii-service
cargo test -p moa-memory-pii classify_smoke
cargo test -p moa-memory-pii mock_classifier
```

## 9 Cleanup

- No prior PII code to delete (this is greenfield).
- Remove any placeholder `pii_class = 'none'` hardcoded calls in M08 — the ingestion layer will route through the classifier from M10 onward.

## 10 What's next

**M10 — Slow-path ingestion VO in Restate (chunk → extract → contradict → upsert).**
