//! Cached embedding fixtures and hermetic embedding provider support.

use std::collections::{BTreeMap, HashMap, hash_map::Entry};
use std::path::Path;

use async_trait::async_trait;
use moa_core::{MoaError, traits::EmbeddingProvider};
use moa_memory_vector::VECTOR_DIMENSION;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::generator::EmbeddingInput;
use moa_eval_core::{EvalError, Result};

/// Deterministic fixture model name used by generated PR memory-eval corpora.
pub const CACHED_EMBEDDING_MODEL: &str = "memory-eval-deterministic-sha256-v1";

/// One cached embedding record from `embeddings.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedEmbeddingFixture {
    /// SHA-256 hash of the normalized source text.
    pub text_hash: String,
    /// Embedding model identifier that produced this vector.
    pub model: String,
    /// Fixed vector dimension.
    pub dimension: usize,
    /// Cached embedding vector.
    pub vector: Vec<f32>,
}

impl CachedEmbeddingFixture {
    /// Builds a deterministic cached embedding fixture for one source text.
    pub fn for_text(text: &str) -> Self {
        Self {
            text_hash: embedding_text_hash(text),
            model: CACHED_EMBEDDING_MODEL.to_string(),
            dimension: VECTOR_DIMENSION,
            vector: deterministic_embedding_vector(text),
        }
    }

    /// Validates this fixture before it is written or loaded.
    pub fn validate(&self) -> Result<()> {
        let normalized_hash = normalize_text_hash(&self.text_hash);
        ensure_non_empty("cached embedding text_hash", &normalized_hash)?;
        ensure_non_empty("cached embedding model", &self.model)?;
        if self.dimension != VECTOR_DIMENSION {
            return invalid_config(format!(
                "cached embedding {} has dimension {}; expected {}",
                normalized_hash, self.dimension, VECTOR_DIMENSION
            ));
        }
        if self.vector.len() != self.dimension {
            return invalid_config(format!(
                "cached embedding {} vector length {} does not match dimension {}",
                normalized_hash,
                self.vector.len(),
                self.dimension
            ));
        }
        if self.vector.iter().any(|value| !value.is_finite()) {
            return invalid_config(format!(
                "cached embedding {} contains a non-finite value",
                normalized_hash
            ));
        }
        Ok(())
    }
}

/// Embedding provider backed only by cached fixture vectors.
#[derive(Debug, Clone)]
pub struct CachedEmbeddingProvider {
    model: String,
    vectors: HashMap<String, Vec<f32>>,
}

impl CachedEmbeddingProvider {
    /// Loads and validates an embedding provider from `embeddings.jsonl`.
    pub async fn from_jsonl(path: &Path) -> Result<Self> {
        let fixtures = read_embeddings_jsonl(path).await?;
        Self::from_fixtures(fixtures)
    }

    /// Builds an embedding provider from preloaded cached fixtures.
    pub fn from_fixtures(fixtures: Vec<CachedEmbeddingFixture>) -> Result<Self> {
        if fixtures.is_empty() {
            return invalid_config("cached embedding fixture set must not be empty");
        }

        let mut model = None::<String>;
        let mut vectors = HashMap::new();
        for fixture in fixtures {
            fixture.validate()?;
            match &model {
                Some(existing) if existing != &fixture.model => {
                    return invalid_config(format!(
                        "cached embeddings mix models {} and {}",
                        existing, fixture.model
                    ));
                }
                Some(_) => {}
                None => model = Some(fixture.model.clone()),
            }

            let text_hash = normalize_text_hash(&fixture.text_hash);
            match vectors.entry(text_hash.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(fixture.vector);
                }
                Entry::Occupied(entry) if entry.get() == &fixture.vector => {}
                Entry::Occupied(_) => {
                    return invalid_config(format!(
                        "cached embedding text_hash {} has conflicting vectors",
                        text_hash
                    ));
                }
            }
        }

        let Some(model) = model else {
            return invalid_config("cached embedding fixture set must not be empty");
        };
        Ok(Self { model, vectors })
    }
}

#[async_trait]
impl EmbeddingProvider for CachedEmbeddingProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
        let mut embeddings = Vec::with_capacity(inputs.len());
        for input in inputs {
            let text_hash = embedding_text_hash(input);
            let Some(vector) = self.vectors.get(&text_hash) else {
                return Err(MoaError::ProviderError(format!(
                    "missing cached embedding fixture for text_hash {text_hash}"
                )));
            };
            embeddings.push(vector.clone());
        }
        Ok(embeddings)
    }
}

/// Returns the stable SHA-256 hash for a text after token normalization.
pub fn embedding_text_hash(text: &str) -> String {
    let normalized = normalize_embedding_text(text);
    let digest = Sha256::digest(normalized.as_bytes());
    format!("{digest:x}")
}

/// Builds deterministic fixture records from generated embedding inputs.
pub fn build_cached_embedding_fixtures(
    inputs: &[EmbeddingInput],
) -> Result<Vec<CachedEmbeddingFixture>> {
    if inputs.is_empty() {
        return invalid_config("cached embedding inputs must not be empty");
    }

    let mut fixtures_by_hash = BTreeMap::new();
    for input in inputs {
        input.validate()?;
        let fixture = CachedEmbeddingFixture::for_text(&input.text);
        fixtures_by_hash
            .entry(fixture.text_hash.clone())
            .or_insert(fixture);
    }
    let fixtures = fixtures_by_hash.into_values().collect::<Vec<_>>();
    validate_embedding_fixtures(&fixtures)?;
    Ok(fixtures)
}

/// Reads and validates `embeddings.jsonl`.
pub async fn read_embeddings_jsonl(path: &Path) -> Result<Vec<CachedEmbeddingFixture>> {
    let fixtures = read_jsonl(path).await?;
    validate_embedding_fixtures(&fixtures)?;
    Ok(fixtures)
}

/// Writes and validates `embeddings.jsonl`.
pub async fn write_embeddings_jsonl(
    path: &Path,
    fixtures: &[CachedEmbeddingFixture],
) -> Result<()> {
    validate_embedding_fixtures(fixtures)?;
    write_jsonl(path, fixtures).await
}

/// Validates a cached embedding fixture set.
pub fn validate_embedding_fixtures(fixtures: &[CachedEmbeddingFixture]) -> Result<()> {
    if fixtures.is_empty() {
        return invalid_config("cached embedding fixture set must not be empty");
    }

    let mut vectors_by_hash = HashMap::<String, &Vec<f32>>::new();
    let mut model = None::<&str>;
    for fixture in fixtures {
        fixture.validate()?;
        match model {
            Some(existing) if existing != fixture.model => {
                return invalid_config(format!(
                    "cached embeddings mix models {} and {}",
                    existing, fixture.model
                ));
            }
            Some(_) => {}
            None => model = Some(&fixture.model),
        }

        let text_hash = normalize_text_hash(&fixture.text_hash);
        match vectors_by_hash.entry(text_hash.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(&fixture.vector);
            }
            Entry::Occupied(entry) if entry.get() == &&fixture.vector => {}
            Entry::Occupied(_) => {
                return invalid_config(format!(
                    "cached embedding text_hash {} has conflicting vectors",
                    text_hash
                ));
            }
        }
    }
    Ok(())
}

fn deterministic_embedding_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0_f32; VECTOR_DIMENSION];
    let tokens = normalized_tokens(text);
    let tokens = if tokens.is_empty() {
        vec![normalize_embedding_text(text)]
    } else {
        tokens
    };

    for (position, token) in tokens.iter().enumerate() {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        hasher.update(position.to_le_bytes());
        let digest = hasher.finalize();
        let mut index_bytes = [0_u8; 8];
        index_bytes.copy_from_slice(&digest[..8]);
        let index = (u64::from_le_bytes(index_bytes) % VECTOR_DIMENSION as u64) as usize;
        let sign = if digest[8] % 2 == 0 { 1.0 } else { -1.0 };
        let weight = 1.0 + (f32::from(digest[9]) / 255.0);
        vector[index] += sign * weight;
    }

    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value = (f64::from(*value) / norm) as f32;
        }
    } else if let Some(first) = vector.first_mut() {
        *first = 1.0;
    }
    vector
}

fn normalize_embedding_text(text: &str) -> String {
    normalized_tokens(text).join(" ")
}

fn normalized_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for character in text.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            current.push(character);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn normalize_text_hash(text_hash: &str) -> String {
    text_hash.trim().to_ascii_lowercase()
}

async fn read_jsonl<T>(path: &Path) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    let file = File::open(path)
        .await
        .map_err(|source| io_error(path, source))?;
    let mut lines = BufReader::new(file).lines();
    let mut records = Vec::new();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|source| io_error(path, source))?
    {
        if line.trim().is_empty() {
            continue;
        }
        records.push(serde_json::from_str(&line)?);
    }
    Ok(records)
}

async fn write_jsonl<T>(path: &Path, records: &[T]) -> Result<()>
where
    T: Serialize,
{
    ensure_parent_dir(path).await?;
    let mut file = File::create(path)
        .await
        .map_err(|source| io_error(path, source))?;
    for record in records {
        let line = serde_json::to_vec(record)?;
        file.write_all(&line)
            .await
            .map_err(|source| io_error(path, source))?;
        file.write_all(b"\n")
            .await
            .map_err(|source| io_error(path, source))?;
    }
    file.flush().await.map_err(|source| io_error(path, source))
}

async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| io_error(parent, source))?;
    }
    Ok(())
}

fn ensure_non_empty(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return invalid_config(format!("{label} must not be empty"));
    }
    Ok(())
}

fn invalid_config<T>(message: impl Into<String>) -> Result<T> {
    Err(EvalError::InvalidConfig(message.into()))
}

fn io_error(path: &Path, source: std::io::Error) -> EvalError {
    EvalError::Io {
        path: path.to_path_buf(),
        source,
    }
}
