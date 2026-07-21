//! Provenance-aware DLP governance for the LLM egress boundary.
//!
//! The decorator keeps all reversible state inside one completion task. That
//! request-local ownership is safe across Kubernetes replicas because no later
//! request or pod must recover the vault for correctness.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use moa_core::{
    error::{MoaError, Result},
    traits::LLMProvider,
    types::completion::{
        CompletionContent, CompletionRequest, CompletionResponse, CompletionStream,
    },
    types::context::{ContextMessage, MessageRole},
    types::model::ModelCapabilities,
    types::security::SensitivityClass,
    types::tools::ToolContent,
};
use moa_dlp::{
    Error as DlpError, TOKEN_CLOSE, TOKEN_OPEN, TokenDestination, TokenSource, TokenSourceRole,
    TokenVault, TokenVisibility,
};
use moa_memory_pii::{PiiClassifier, PiiResult};
use serde_json::Value;
use tokio::sync::mpsc;

const STREAM_CHANNEL_CAPACITY: usize = 32;
const DEFAULT_CLASSIFIER_CACHE_CAPACITY: usize = 1024;
const CACHE_POLICY_VERSION: &[u8] = b"moa.egress-dlp.v2";

/// Decorates an LLM provider with fail-closed outbound classification and
/// provenance-aware response restoration.
pub struct GovernedLLMProvider {
    inner: Arc<dyn LLMProvider>,
    classifier: Arc<dyn PiiClassifier>,
}

impl GovernedLLMProvider {
    /// Creates a governed provider. Classifier failure or abstention always blocks.
    #[must_use]
    pub fn new(inner: Arc<dyn LLMProvider>, classifier: Arc<dyn PiiClassifier>) -> Self {
        Self { inner, classifier }
    }

    async fn tokenize_message(
        &self,
        message: &mut ContextMessage,
        message_index: usize,
        vault: &mut TokenVault,
    ) -> Result<()> {
        let (role, visibility) = source_context(&message.role);
        let content_field = format!("messages[{message_index}].content");
        if !message.content.is_empty() {
            message.content = self
                .tokenize_text(
                    &message.content,
                    vault,
                    TokenSource::new(visibility, role, content_field),
                )
                .await
                .map_err(dlp_provider_error)?;
        }

        if let Some(tools) = message.tools.take() {
            message.tools = Some(
                self.tokenize_structured_value(
                    tools,
                    vault,
                    role,
                    visibility,
                    format!("messages[{message_index}].tools"),
                )
                .await
                .map_err(dlp_provider_error)?,
            );
        }

        if let Some(blocks) = message.content_blocks.as_mut() {
            for (block_index, block) in blocks.iter_mut().enumerate() {
                let field = format!("messages[{message_index}].content_blocks[{block_index}]");
                match block {
                    ToolContent::Text { text } if !text.is_empty() => {
                        *text = self
                            .tokenize_text(
                                text,
                                vault,
                                TokenSource::new(visibility, role, format!("{field}.text")),
                            )
                            .await
                            .map_err(dlp_provider_error)?;
                    }
                    ToolContent::Json { data } => {
                        *data = self
                            .tokenize_structured_value(
                                std::mem::take(data),
                                vault,
                                role,
                                visibility,
                                format!("{field}.json"),
                            )
                            .await
                            .map_err(dlp_provider_error)?;
                    }
                    ToolContent::Text { .. } => {}
                }
            }
        }

        if let Some(invocation) = message.tool_invocation.as_mut() {
            invocation.input = self
                .tokenize_structured_value(
                    std::mem::take(&mut invocation.input),
                    vault,
                    role,
                    TokenVisibility::Hidden,
                    format!("messages[{message_index}].tool_invocation.input"),
                )
                .await
                .map_err(dlp_provider_error)?;
        }
        Ok(())
    }

    async fn tokenize_text(
        &self,
        text: &str,
        vault: &mut TokenVault,
        source: TokenSource,
    ) -> moa_dlp::Result<String> {
        let result = self.classify(text, source.field()).await?;
        if result.class != SensitivityClass::None && result.spans.is_empty() {
            return Err(DlpError::IncompleteSensitiveSpans {
                field: source.field().to_string(),
            });
        }
        let tokenized = vault.tokenize(text, &result.spans, source.clone())?;
        let residual = vault.classification_view(&tokenized);
        let residual_result = self.classify(&residual, source.field()).await?;
        if residual_result.class != SensitivityClass::None || !residual_result.spans.is_empty() {
            return Err(DlpError::IncompleteSensitiveSpans {
                field: source.field().to_string(),
            });
        }
        Ok(tokenized)
    }

    async fn classify(&self, text: &str, field: &str) -> moa_dlp::Result<PiiResult> {
        let result = self.classifier.classify(text).await.map_err(|source| {
            DlpError::ClassificationFailed {
                field: field.to_string(),
                source,
            }
        })?;
        if result.abstained {
            return Err(DlpError::ClassifierAbstained {
                field: field.to_string(),
            });
        }
        Ok(result)
    }

    async fn tokenize_structured_value(
        &self,
        value: Value,
        vault: &mut TokenVault,
        role: TokenSourceRole,
        visibility: TokenVisibility,
        field: String,
    ) -> moa_dlp::Result<Value> {
        let tokenized = self
            .tokenize_value(value, vault, role, visibility, field.clone())
            .await?;
        let canonical =
            canonical_json(&tokenized).map_err(|_| DlpError::IncompleteSensitiveSpans {
                field: field.clone(),
            })?;
        let residual = vault.classification_view(&canonical);
        let result = self
            .classify(&residual, &format!("{field}.canonical"))
            .await?;
        if result.class != SensitivityClass::None || !result.spans.is_empty() {
            return Err(DlpError::IncompleteSensitiveSpans { field });
        }
        Ok(tokenized)
    }

    fn tokenize_value<'a>(
        &'a self,
        value: Value,
        vault: &'a mut TokenVault,
        role: TokenSourceRole,
        visibility: TokenVisibility,
        field: String,
    ) -> BoxFuture<'a, moa_dlp::Result<Value>> {
        Box::pin(async move {
            match value {
                Value::String(text) if !text.is_empty() => Ok(Value::String(
                    self.tokenize_text(&text, vault, TokenSource::new(visibility, role, field))
                        .await?,
                )),
                Value::Number(number) => {
                    let text = number.to_string();
                    let tokenized = self
                        .tokenize_text(&text, vault, TokenSource::new(visibility, role, field))
                        .await?;
                    if tokenized == text {
                        Ok(Value::Number(number))
                    } else {
                        Ok(Value::String(tokenized))
                    }
                }
                Value::Array(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for (index, item) in items.into_iter().enumerate() {
                        out.push(
                            self.tokenize_value(
                                item,
                                &mut *vault,
                                role,
                                visibility,
                                format!("{field}[{index}]"),
                            )
                            .await?,
                        );
                    }
                    Ok(Value::Array(out))
                }
                Value::Object(map) => {
                    let mut entries = map.into_iter().collect::<Vec<_>>();
                    entries.sort_by(|(left, _), (right, _)| left.cmp(right));
                    let mut out = serde_json::Map::with_capacity(entries.len());
                    for (index, (key, item)) in entries.into_iter().enumerate() {
                        let key_field = format!("{field}.key[{index}]");
                        let tokenized_key = self
                            .tokenize_text(
                                &key,
                                &mut *vault,
                                TokenSource::new(visibility, role, key_field),
                            )
                            .await?;
                        let tokenized_item = self
                            .tokenize_value(
                                item,
                                &mut *vault,
                                role,
                                visibility,
                                format!("{field}.value[{index}]"),
                            )
                            .await?;
                        if out.insert(tokenized_key, tokenized_item).is_some() {
                            return Err(DlpError::StructuredKeyCollision {
                                field: field.clone(),
                            });
                        }
                    }
                    Ok(Value::Object(out))
                }
                other => Ok(other),
            }
        })
    }
}

#[async_trait]
impl LLMProvider for GovernedLLMProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, mut request: CompletionRequest) -> Result<CompletionStream> {
        let mut vault = TokenVault::new().map_err(dlp_provider_error)?;
        for (index, message) in request.messages.iter_mut().enumerate() {
            self.tokenize_message(message, index, &mut vault).await?;
        }
        for (index, tool) in request.tools.iter_mut().enumerate() {
            *tool = self
                .tokenize_structured_value(
                    std::mem::take(tool),
                    &mut vault,
                    TokenSourceRole::System,
                    TokenVisibility::Hidden,
                    format!("request.tools[{index}]"),
                )
                .await
                .map_err(dlp_provider_error)?;
        }
        if let Some(format) = request.response_format.as_mut() {
            format.schema = self
                .tokenize_structured_value(
                    std::mem::take(&mut format.schema),
                    &mut vault,
                    TokenSourceRole::System,
                    TokenVisibility::Hidden,
                    "request.response_format.schema".to_string(),
                )
                .await
                .map_err(dlp_provider_error)?;
        }

        tracing::debug!(
            provider = self.inner.name(),
            distinct_tokens = vault.len(),
            "egress DLP tokenized outbound request"
        );
        let inner = self.inner.complete(request).await?;
        Ok(detokenizing_stream(inner, vault))
    }
}

fn source_context(role: &MessageRole) -> (TokenSourceRole, TokenVisibility) {
    match role {
        MessageRole::System => (TokenSourceRole::System, TokenVisibility::Hidden),
        MessageRole::User => (TokenSourceRole::User, TokenVisibility::Visible),
        MessageRole::Assistant => (TokenSourceRole::Assistant, TokenVisibility::Visible),
        MessageRole::Tool => (TokenSourceRole::Tool, TokenVisibility::Hidden),
    }
}

fn dlp_provider_error(error: DlpError) -> MoaError {
    MoaError::ProviderError(format!("egress DLP blocked: {error}"))
}

fn canonical_json(value: &Value) -> serde_json::Result<String> {
    match value {
        Value::Null => Ok("null".to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        Value::String(value) => serde_json::to_string(value),
        Value::Array(items) => {
            let mut serialized = Vec::with_capacity(items.len());
            for item in items {
                serialized.push(canonical_json(item)?);
            }
            Ok(format!("[{}]", serialized.join(",")))
        }
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            let mut serialized = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                serialized.push(format!(
                    "{}:{}",
                    serde_json::to_string(key)?,
                    canonical_json(value)?
                ));
            }
            Ok(format!("{{{}}}", serialized.join(",")))
        }
    }
}

fn detokenizing_stream(inner: CompletionStream, vault: TokenVault) -> CompletionStream {
    inner.transform(STREAM_CHANNEL_CAPACITY, move |mut inner, tx| async move {
        let mut detokenizer = StreamDetokenizer::new(vault);
        while let Some(item) = inner.next().await {
            let block = item?;
            if !forward_block(&mut detokenizer, block, &tx).await? {
                return Err(MoaError::ProviderError(
                    "completion stream receiver closed during DLP transform".to_string(),
                ));
            }
        }
        let tail = detokenizer.take_pending();
        if !tail.is_empty() && tx.send(Ok(CompletionContent::Text(tail))).await.is_err() {
            return Err(MoaError::ProviderError(
                "completion stream receiver closed during DLP finalization".to_string(),
            ));
        }
        let response = inner.into_response().await?;
        detokenizer
            .detokenize_response(response)
            .map_err(dlp_provider_error)
    })
}

async fn forward_block(
    detokenizer: &mut StreamDetokenizer,
    block: CompletionContent,
    tx: &mpsc::Sender<Result<CompletionContent>>,
) -> Result<bool> {
    let transformed = match block {
        CompletionContent::Text(text) => {
            let emitted = detokenizer.feed(&text).map_err(dlp_provider_error)?;
            if emitted.is_empty() {
                return Ok(true);
            }
            CompletionContent::Text(emitted)
        }
        CompletionContent::ToolCall(mut call) => {
            if !flush_pending(detokenizer, tx).await {
                return Ok(false);
            }
            call.invocation.input = detokenizer
                .detokenize_value(call.invocation.input, TokenDestination::ToolArgument)
                .map_err(dlp_provider_error)?;
            CompletionContent::ToolCall(call)
        }
        CompletionContent::ProviderToolResult { tool_name, summary } => {
            if !flush_pending(detokenizer, tx).await {
                return Ok(false);
            }
            CompletionContent::ProviderToolResult {
                tool_name,
                summary: detokenizer
                    .vault
                    .restore(&summary, TokenDestination::VisibleOutput)
                    .map_err(dlp_provider_error)?,
            }
        }
    };
    Ok(tx.send(Ok(transformed)).await.is_ok())
}

async fn flush_pending(
    detokenizer: &mut StreamDetokenizer,
    tx: &mpsc::Sender<Result<CompletionContent>>,
) -> bool {
    let pending = detokenizer.take_pending();
    pending.is_empty() || tx.send(Ok(CompletionContent::Text(pending))).await.is_ok()
}

struct StreamDetokenizer {
    vault: TokenVault,
    pending: String,
}

impl StreamDetokenizer {
    fn new(vault: TokenVault) -> Self {
        Self {
            vault,
            pending: String::new(),
        }
    }

    fn feed(&mut self, chunk: &str) -> moa_dlp::Result<String> {
        let mut work = std::mem::take(&mut self.pending);
        work.push_str(chunk);
        let mut out = String::with_capacity(work.len());
        let mut rest = work.as_str();
        loop {
            let Some(open) = rest.find(TOKEN_OPEN) else {
                out.push_str(rest);
                break;
            };
            out.push_str(&rest[..open]);
            let tail = &rest[open..];
            match tail.find(TOKEN_CLOSE) {
                Some(close) => {
                    let end = close + TOKEN_CLOSE.len_utf8();
                    out.push_str(
                        &self
                            .vault
                            .restore(&tail[..end], TokenDestination::VisibleOutput)?,
                    );
                    rest = &tail[end..];
                }
                None => {
                    let body = &tail[TOKEN_OPEN.len_utf8()..];
                    if body.chars().all(is_token_body_char) {
                        self.pending.push_str(tail);
                        break;
                    }
                    out.push(TOKEN_OPEN);
                    rest = body;
                }
            }
        }
        Ok(out)
    }

    fn take_pending(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }

    fn detokenize_response(
        &self,
        mut response: CompletionResponse,
    ) -> moa_dlp::Result<CompletionResponse> {
        response.text = self
            .vault
            .restore(&response.text, TokenDestination::VisibleOutput)?;
        response.content = response
            .content
            .into_iter()
            .map(|block| self.detokenize_block(block))
            .collect::<moa_dlp::Result<Vec<_>>>()?;
        Ok(response)
    }

    fn detokenize_block(&self, block: CompletionContent) -> moa_dlp::Result<CompletionContent> {
        match block {
            CompletionContent::Text(text) => Ok(CompletionContent::Text(
                self.vault.restore(&text, TokenDestination::VisibleOutput)?,
            )),
            CompletionContent::ToolCall(mut call) => {
                call.invocation.input =
                    self.detokenize_value(call.invocation.input, TokenDestination::ToolArgument)?;
                Ok(CompletionContent::ToolCall(call))
            }
            CompletionContent::ProviderToolResult { tool_name, summary } => {
                Ok(CompletionContent::ProviderToolResult {
                    tool_name,
                    summary: self
                        .vault
                        .restore(&summary, TokenDestination::VisibleOutput)?,
                })
            }
        }
    }

    fn detokenize_value(
        &self,
        value: Value,
        destination: TokenDestination,
    ) -> moa_dlp::Result<Value> {
        match value {
            Value::String(text) => Ok(Value::String(self.vault.restore(&text, destination)?)),
            Value::Array(items) => Ok(Value::Array(
                items
                    .into_iter()
                    .map(|item| self.detokenize_value(item, destination))
                    .collect::<moa_dlp::Result<Vec<_>>>()?,
            )),
            Value::Object(map) => {
                let mut out = serde_json::Map::with_capacity(map.len());
                for (key, item) in map {
                    let key = self.vault.restore(&key, destination)?;
                    let item = self.detokenize_value(item, destination)?;
                    if out.insert(key, item).is_some() {
                        return Err(DlpError::StructuredKeyCollision {
                            field: "completion.tool_argument".to_string(),
                        });
                    }
                }
                Ok(Value::Object(out))
            }
            other => Ok(other),
        }
    }
}

fn is_token_body_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

/// Bounded cache for classifier results. It is a performance optimization only;
/// no request relies on it for correctness or cross-pod coordination.
pub struct CachingPiiClassifier {
    inner: Arc<dyn PiiClassifier>,
    classifier_namespace: String,
    classifier_model: String,
    cache: Mutex<BoundedResultCache>,
}

struct BoundedResultCache {
    entries: HashMap<[u8; 32], PiiResult>,
    order: VecDeque<[u8; 32]>,
    capacity: usize,
}

impl BoundedResultCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    fn get(&self, key: &[u8; 32]) -> Option<PiiResult> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: [u8; 32], result: PiiResult) {
        if self.entries.insert(key, result).is_none() {
            self.order.push_back(key);
            while self.order.len() > self.capacity {
                if let Some(oldest) = self.order.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
        }
    }
}

impl CachingPiiClassifier {
    /// Creates a cache whose identity includes the classifier deployment and model.
    #[must_use]
    pub fn new(
        inner: Arc<dyn PiiClassifier>,
        classifier_namespace: impl Into<String>,
        classifier_model: impl Into<String>,
    ) -> Self {
        Self::with_capacity(
            inner,
            classifier_namespace,
            classifier_model,
            DEFAULT_CLASSIFIER_CACHE_CAPACITY,
        )
    }

    /// Creates a cache with an explicit maximum entry count.
    #[must_use]
    pub fn with_capacity(
        inner: Arc<dyn PiiClassifier>,
        classifier_namespace: impl Into<String>,
        classifier_model: impl Into<String>,
        capacity: usize,
    ) -> Self {
        Self {
            inner,
            classifier_namespace: classifier_namespace.into(),
            classifier_model: classifier_model.into(),
            cache: Mutex::new(BoundedResultCache::new(capacity)),
        }
    }

    fn cache_key(&self, text: &str) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        update_length_framed(&mut hasher, CACHE_POLICY_VERSION);
        update_length_framed(&mut hasher, self.classifier_namespace.as_bytes());
        update_length_framed(&mut hasher, self.classifier_model.as_bytes());
        update_length_framed(&mut hasher, text.as_bytes());
        *hasher.finalize().as_bytes()
    }

    fn cached(&self, key: &[u8; 32]) -> Option<PiiResult> {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
    }

    fn store(&self, key: [u8; 32], result: PiiResult) {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, result);
    }
}

fn update_length_framed(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

#[async_trait]
impl PiiClassifier for CachingPiiClassifier {
    async fn classify(&self, text: &str) -> moa_memory_pii::Result<PiiResult> {
        let key = self.cache_key(text);
        if let Some(result) = self.cached(&key) {
            return Ok(result);
        }
        let result = self.inner.classify(text).await?;
        self.store(key, result.clone());
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use moa_core::types::completion::{StopReason, TokenUsage, ToolCallContent, ToolInvocation};
    use moa_core::types::identifiers::ModelId;
    use moa_core::types::security::SensitivityClass;
    use moa_memory_pii::{Error, PiiCategory, PiiSpan};
    use serde_json::json;

    use super::*;

    struct FixedClassifier {
        needles: Vec<&'static str>,
        abstain: bool,
    }

    #[async_trait]
    impl PiiClassifier for FixedClassifier {
        async fn classify(&self, text: &str) -> moa_memory_pii::Result<PiiResult> {
            if self.abstain {
                return Ok(PiiResult::fail_closed("test-abstain"));
            }
            let mut spans = Vec::new();
            for needle in &self.needles {
                let mut offset = 0;
                while let Some(index) = text[offset..].find(needle) {
                    let start = offset + index;
                    spans.push(PiiSpan::new(
                        start,
                        start + needle.len(),
                        PiiCategory::Secret,
                        0.99,
                    ));
                    offset = start + needle.len();
                }
            }
            Ok(PiiResult {
                class: if spans.is_empty() {
                    SensitivityClass::None
                } else {
                    SensitivityClass::Restricted
                },
                spans,
                model_version: "test-fixed".to_string(),
                abstained: false,
            })
        }
    }

    fn classifier(needles: Vec<&'static str>) -> Arc<dyn PiiClassifier> {
        Arc::new(FixedClassifier {
            needles,
            abstain: false,
        })
    }

    #[derive(Clone, Copy)]
    enum EchoMode {
        AllText,
        SplitFirst,
        FirstToolArgument,
    }

    struct EchoProvider {
        mode: EchoMode,
        requests: Mutex<Vec<CompletionRequest>>,
    }

    impl EchoProvider {
        fn new(mode: EchoMode) -> Arc<Self> {
            Arc::new(Self {
                mode,
                requests: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl LLMProvider for EchoProvider {
        fn name(&self) -> &str {
            "echo"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
            let rendered = serde_json::to_string(&request.messages)?;
            let tokens = extract_tokens(&rendered);
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(request);
            match self.mode {
                EchoMode::AllText => Ok(CompletionStream::from_response(response(
                    CompletionContent::Text(tokens.join(" ")),
                ))),
                EchoMode::FirstToolArgument => Ok(CompletionStream::from_response(response(
                    CompletionContent::ToolCall(ToolCallContent {
                        invocation: ToolInvocation {
                            id: Some("call-1".to_string()),
                            name: "send".to_string(),
                            input: json!({"value": tokens.first().cloned().unwrap_or_default()}),
                        },
                        provider_metadata: None,
                    }),
                ))),
                EchoMode::SplitFirst => {
                    let token = tokens.first().cloned().unwrap_or_default();
                    let mut split = token.len() / 2;
                    while !token.is_char_boundary(split) {
                        split += 1;
                    }
                    let blocks = vec![
                        CompletionContent::Text(token[..split].to_string()),
                        CompletionContent::Text(token[split..].to_string()),
                    ];
                    let final_response = response(CompletionContent::Text(token));
                    let (tx, rx) = mpsc::channel(2);
                    let completion = tokio::spawn(async move {
                        for block in blocks {
                            tx.send(Ok(block)).await.map_err(|_| {
                                MoaError::ProviderError("test receiver closed".to_string())
                            })?;
                        }
                        Ok(final_response)
                    });
                    Ok(CompletionStream::new(rx, completion))
                }
            }
        }
    }

    fn extract_tokens(text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find(TOKEN_OPEN) {
            let tail = &rest[open..];
            let Some(close) = tail.find(TOKEN_CLOSE) else {
                break;
            };
            let end = close + TOKEN_CLOSE.len_utf8();
            tokens.push(tail[..end].to_string());
            rest = &tail[end..];
        }
        tokens
    }

    fn response(block: CompletionContent) -> CompletionResponse {
        let text = match &block {
            CompletionContent::Text(text) => text.clone(),
            _ => String::new(),
        };
        CompletionResponse {
            text,
            content: vec![block],
            stop_reason: StopReason::EndTurn,
            model: ModelId::new("echo"),
            usage: TokenUsage::default(),
            duration_ms: 1,
            thought_signature: None,
        }
    }

    async fn drain_text(mut stream: CompletionStream) -> Result<(String, CompletionResponse)> {
        let mut text = String::new();
        while let Some(block) = stream.next().await {
            if let CompletionContent::Text(chunk) = block? {
                text.push_str(&chunk);
            }
        }
        let response = stream.into_response().await?;
        Ok((text, response))
    }

    #[tokio::test]
    async fn hidden_and_visible_equal_plaintext_do_not_share_declassification() {
        // Pins: a model repeating both placeholders cannot reveal the hidden system copy.
        let inner = EchoProvider::new(EchoMode::AllText);
        let provider = GovernedLLMProvider::new(inner, classifier(vec!["sk-shared"]));
        let request = CompletionRequest {
            messages: vec![
                ContextMessage::system("internal sk-shared"),
                ContextMessage::user("my key is sk-shared"),
            ],
            ..CompletionRequest::new("ignored")
        };
        let (_, response) = drain_text(provider.complete(request).await.expect("complete"))
            .await
            .expect("drain");
        assert_eq!(response.text, "[REDACTED] sk-shared");
    }

    #[tokio::test]
    async fn split_hidden_token_is_redacted_before_streaming() {
        // Pins: chunk boundaries cannot bypass the hidden-value destination gate.
        let inner = EchoProvider::new(EchoMode::SplitFirst);
        let provider = GovernedLLMProvider::new(inner, classifier(vec!["sk-system"]));
        let stream = provider
            .complete(CompletionRequest {
                messages: vec![ContextMessage::system("secret sk-system")],
                ..CompletionRequest::new("ignored")
            })
            .await
            .expect("complete");
        let (streamed, response) = drain_text(stream).await.expect("drain");
        assert_eq!(streamed, "[REDACTED]");
        assert_eq!(response.text, "[REDACTED]");
    }

    #[tokio::test]
    async fn hidden_token_in_generated_tool_arguments_is_blocked() {
        // Pins: private tool/history placeholders cannot be reconstructed into a generated call.
        let inner = EchoProvider::new(EchoMode::FirstToolArgument);
        let provider = GovernedLLMProvider::new(inner, classifier(vec!["sk-private"]));
        let error = provider
            .complete(CompletionRequest {
                messages: vec![ContextMessage::assistant_tool_call(
                    ToolInvocation {
                        id: Some("private-call".to_string()),
                        name: "private".to_string(),
                        input: json!({"credential": "sk-private"}),
                    },
                    "calling private tool",
                )],
                ..CompletionRequest::new("ignored")
            })
            .await
            .expect("stream")
            .into_response()
            .await
            .expect_err("hidden token must not enter tool arguments");
        assert!(
            matches!(error, MoaError::ProviderError(message) if message.contains("not allowed"))
        );
    }

    #[tokio::test]
    async fn structured_keys_strings_and_numbers_are_classified() {
        // Pins: structured traversal cannot omit keys, string leaves, numeric leaves, or residuals.
        struct RecordingFixed {
            seen: Arc<Mutex<Vec<String>>>,
            inner: FixedClassifier,
        }
        #[async_trait]
        impl PiiClassifier for RecordingFixed {
            async fn classify(&self, text: &str) -> moa_memory_pii::Result<PiiResult> {
                self.seen
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(text.to_string());
                self.inner.classify(text).await
            }
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let inner = EchoProvider::new(EchoMode::AllText);
        let provider = GovernedLLMProvider::new(
            inner.clone(),
            Arc::new(RecordingFixed {
                seen: seen.clone(),
                inner: FixedClassifier {
                    needles: vec!["sk-key", "sk-value", "424242"],
                    abstain: false,
                },
            }),
        );
        let mut message = ContextMessage::tool("result");
        message.content_blocks = Some(vec![ToolContent::Json {
            data: json!({"sk-key": "sk-value", "account": 424242}),
        }]);
        provider
            .complete(CompletionRequest {
                messages: vec![message],
                ..CompletionRequest::new("ignored")
            })
            .await
            .expect("complete")
            .collect()
            .await
            .expect("collect");
        let request = inner
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .first()
            .cloned()
            .expect("recorded request");
        let Some(ToolContent::Json { data }) = request.messages[0]
            .content_blocks
            .as_ref()
            .and_then(|blocks| blocks.first())
        else {
            panic!("recorded structured JSON block");
        };
        let object = data.as_object().expect("tokenized object");
        assert!(!object.contains_key("sk-key"));
        assert!(
            object
                .values()
                .all(|value| value.as_str() != Some("sk-value"))
        );
        assert!(object.values().all(|value| value != &json!(424242)));
        assert!(
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .any(|text| text.starts_with('{') && text.contains("[DLP_TOKEN]")),
            "canonical residual was not classified"
        );
    }

    #[test]
    fn restoring_structured_keys_rejects_collisions() {
        // Pins: two provenance-distinct keys may not silently merge after restoration.
        let mut vault = TokenVault::new().expect("vault");
        let span = [PiiSpan::new(0, 6, PiiCategory::Secret, 0.99)];
        let first = vault
            .tokenize(
                "secret",
                &span,
                TokenSource::new(TokenVisibility::Visible, TokenSourceRole::User, "first.key"),
            )
            .expect("first key");
        let second = vault
            .tokenize(
                "secret",
                &span,
                TokenSource::new(
                    TokenVisibility::Visible,
                    TokenSourceRole::User,
                    "second.key",
                ),
            )
            .expect("second key");
        let detokenizer = StreamDetokenizer::new(vault);
        let mut map = serde_json::Map::new();
        map.insert(first, json!(1));
        map.insert(second, json!(2));
        let value = Value::Object(map);
        assert!(matches!(
            detokenizer.detokenize_value(value, TokenDestination::ToolArgument),
            Err(DlpError::StructuredKeyCollision { .. })
        ));
    }

    #[tokio::test]
    async fn classifier_abstention_always_fails_closed() {
        // Pins: enabled DLP has no cleartext escape hatch for classifier uncertainty.
        let inner = EchoProvider::new(EchoMode::AllText);
        let provider = GovernedLLMProvider::new(
            inner.clone(),
            Arc::new(FixedClassifier {
                needles: Vec::new(),
                abstain: true,
            }),
        );
        let error = provider
            .complete(CompletionRequest::new("unknown sensitivity"))
            .await
            .expect_err("abstention blocks");
        assert!(matches!(error, MoaError::ProviderError(message) if message.contains("abstained")));
        assert!(
            inner
                .requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn sensitive_verdict_without_spans_fails_closed() {
        // Pins: a sensitive aggregate verdict cannot pass cleartext through an incomplete span set.
        struct Incomplete;
        #[async_trait]
        impl PiiClassifier for Incomplete {
            async fn classify(&self, _text: &str) -> moa_memory_pii::Result<PiiResult> {
                Ok(PiiResult {
                    class: SensitivityClass::Restricted,
                    spans: Vec::new(),
                    model_version: "incomplete".to_string(),
                    abstained: false,
                })
            }
        }
        let inner = EchoProvider::new(EchoMode::AllText);
        let provider = GovernedLLMProvider::new(inner.clone(), Arc::new(Incomplete));
        let error = provider
            .complete(CompletionRequest::new("possibly sensitive"))
            .await
            .expect_err("incomplete spans block");
        assert!(
            matches!(error, MoaError::ProviderError(message) if message.contains("without complete spans"))
        );
        assert!(
            inner
                .requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
    }

    struct CountingClassifier {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl PiiClassifier for CountingClassifier {
        async fn classify(&self, _text: &str) -> moa_memory_pii::Result<PiiResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(PiiResult {
                class: SensitivityClass::None,
                spans: Vec::new(),
                model_version: "counting".to_string(),
                abstained: false,
            })
        }
    }

    #[tokio::test]
    async fn cache_identity_includes_namespace_model_and_exact_bytes() {
        // Pins: cache hits cannot cross policy/model namespaces or byte-distinct inputs.
        let counting = Arc::new(CountingClassifier {
            calls: AtomicUsize::new(0),
        });
        let cache = CachingPiiClassifier::new(counting.clone(), "sidecar-a", "model-a");
        cache.classify("same").await.expect("first");
        cache.classify("same").await.expect("cached");
        cache.classify("same\0").await.expect("byte distinct");
        assert_eq!(counting.calls.load(Ordering::SeqCst), 2);

        let other = CachingPiiClassifier::new(counting.clone(), "sidecar-b", "model-a");
        assert_ne!(cache.cache_key("same"), other.cache_key("same"));
        other.classify("same").await.expect("namespace distinct");
        assert_eq!(counting.calls.load(Ordering::SeqCst), 3);

        let other_model = CachingPiiClassifier::new(counting.clone(), "sidecar-a", "model-b");
        assert_ne!(cache.cache_key("same"), other_model.cache_key("same"));
        other_model.classify("same").await.expect("model distinct");
        assert_eq!(counting.calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn classifier_errors_fail_closed_before_provider_call() {
        // Pins: an unavailable classifier never degrades into plaintext egress.
        struct Failing;
        #[async_trait]
        impl PiiClassifier for Failing {
            async fn classify(&self, _text: &str) -> moa_memory_pii::Result<PiiResult> {
                Err(Error::Inference("offline".to_string()))
            }
        }
        let inner = EchoProvider::new(EchoMode::AllText);
        let provider = GovernedLLMProvider::new(inner.clone(), Arc::new(Failing));
        assert!(
            provider
                .complete(CompletionRequest::new("secret"))
                .await
                .is_err()
        );
        assert!(
            inner
                .requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
    }
}
