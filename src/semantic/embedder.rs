use crate::{
    config::{KatokConfig, DEFAULT_EMBEDDER_MODEL},
    Error, Result,
};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};

pub(crate) trait SemanticEmbedder {
    fn id(&self) -> &str;
    fn embed(&mut self, texts: &[String], batch_size: usize) -> Result<Vec<Vec<f32>>>;
    fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        let embeddings = self.embed(&[query.to_string()], 1)?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| Error::Embedding("embedder returned no query vector".to_string()))
    }
}

pub(crate) fn create_embedder(config: &KatokConfig) -> Result<Box<dyn SemanticEmbedder>> {
    let config = config.with_env_overrides();
    match config.embedder_family() {
        "mock" => Ok(Box::new(DeterministicEmbedder::new(usize::from(
            config.vector_dimension,
        )))),
        "openai-compatible" => Ok(Box::new(OpenAiCompatibleEmbedder::new(&config)?)),
        _ => Ok(Box::new(FastEmbedder::new(&config)?)),
    }
}

struct FastEmbedder {
    inner: TextEmbedding,
}

impl FastEmbedder {
    fn new(config: &KatokConfig) -> Result<Self> {
        if config.embedder_model != DEFAULT_EMBEDDER_MODEL {
            return Err(Error::Embedding(format!(
                "unsupported local embedder model: {}",
                config.embedder_model
            )));
        }
        let options = TextInitOptions::new(EmbeddingModel::EmbeddingGemma300MQ4)
            .with_show_download_progress(false)
            .with_cache_dir(fastembed_cache_dir());
        let inner = TextEmbedding::try_new(options).map_err(to_embedding_error)?;
        Ok(Self { inner })
    }
}

/// Resolve a writable cache directory for the fastembed model files.
///
/// The previous relative `.fastembed_cache` resolved against the process working
/// directory, so a run from `/` failed with `EROFS`. Prefer an explicit override,
/// then the platform cache dir, then the temp dir.
fn fastembed_cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("KATOK_FASTEMBED_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("lazykatok")
        .join("fastembed")
}

impl SemanticEmbedder for FastEmbedder {
    fn id(&self) -> &str {
        "embeddinggemma/local"
    }

    fn embed(&mut self, texts: &[String], batch_size: usize) -> Result<Vec<Vec<f32>>> {
        let documents = texts
            .iter()
            .map(|text| format!("passage: {text}"))
            .collect::<Vec<_>>();
        self.inner
            .embed(documents, Some(batch_size.max(1)))
            .map_err(to_embedding_error)
    }

    fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        let embeddings = self
            .inner
            .embed([format!("query: {}", query.trim())], Some(1))
            .map_err(to_embedding_error)?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| Error::Embedding("embedder returned no query vector".to_string()))
    }
}

/// An embedder backed by an OpenAI-compatible `POST /v1/embeddings` endpoint.
///
/// Works with the OpenAI API itself and with local servers (llama.cpp, Ollama,
/// TEI-via-openai, vLLM) that speak the same shape, including models whose
/// dimension is not the built-in 768 — set `vector_dimension`/`KATOK_EMBED_DIM`
/// to match (for example 1024 for `Qwen3-Embedding-0.6B`).
#[derive(Debug)]
struct OpenAiCompatibleEmbedder {
    agent: ureq::Agent,
    endpoint: String,
    model: String,
    dimension: usize,
    api_key: Option<String>,
    query_prefix: Option<String>,
    passage_prefix: Option<String>,
    id: String,
}

impl OpenAiCompatibleEmbedder {
    fn new(config: &KatokConfig) -> Result<Self> {
        let url = config.embedder_url.as_deref().ok_or_else(|| {
            Error::Embedding(
                "the openai-compatible embedder needs embedder_url or KATOK_EMBED_URL".to_string(),
            )
        })?;
        let dimension = usize::from(config.vector_dimension);
        if dimension == 0 {
            return Err(Error::Embedding(
                "embedding dimension must be nonzero".to_string(),
            ));
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .build()
            .into();
        Ok(Self {
            agent,
            endpoint: resolve_embeddings_endpoint(url),
            model: config.embedder_model.clone(),
            dimension,
            api_key: config.embedder_api_key.clone(),
            query_prefix: config.query_prefix.clone(),
            passage_prefix: config.passage_prefix.clone(),
            id: format!("openai/{}", config.embedder_model),
        })
    }

    fn request(
        &self,
        texts: &[String],
        batch_size: usize,
        prefix: Option<&str>,
    ) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let batch_size = batch_size.max(1);
        let mut vectors = Vec::with_capacity(texts.len());
        for batch in texts.chunks(batch_size) {
            let inputs = batch
                .iter()
                .map(|text| match prefix {
                    Some(prefix) => format!("{prefix}{text}"),
                    None => text.clone(),
                })
                .collect::<Vec<_>>();
            vectors.extend(self.post_batch(&inputs)?);
        }
        Ok(vectors)
    }

    fn post_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        let body = serde_json::json!({ "model": self.model, "input": inputs });
        let mut request = self
            .agent
            .post(&self.endpoint)
            .header("Content-Type", "application/json");
        if let Some(api_key) = &self.api_key {
            request = request.header("Authorization", &format!("Bearer {api_key}"));
        }
        let mut response = request.send_json(&body).map_err(|error| {
            Error::Embedding(format!(
                "embedding request to {} failed: {error}",
                self.endpoint
            ))
        })?;
        let parsed: EmbeddingsResponse = response.body_mut().read_json().map_err(|error| {
            Error::Embedding(format!(
                "cannot parse embeddings response from {}: {error}",
                self.endpoint
            ))
        })?;
        let mut items = parsed.data;
        if items.iter().any(|item| item.index.is_some()) {
            items.sort_by_key(|item| item.index.unwrap_or(0));
        }
        if items.len() != inputs.len() {
            return Err(Error::Embedding(format!(
                "expected {} embeddings, got {}",
                inputs.len(),
                items.len()
            )));
        }
        let mut vectors = Vec::with_capacity(items.len());
        for item in items {
            if item.embedding.len() != self.dimension {
                return Err(Error::Embedding(format!(
                    "expected {} dimensions, got {}",
                    self.dimension,
                    item.embedding.len()
                )));
            }
            let mut vector = item.embedding;
            normalize(&mut vector);
            vectors.push(vector);
        }
        Ok(vectors)
    }
}

impl SemanticEmbedder for OpenAiCompatibleEmbedder {
    fn id(&self) -> &str {
        &self.id
    }

    fn embed(&mut self, texts: &[String], batch_size: usize) -> Result<Vec<Vec<f32>>> {
        self.request(texts, batch_size, self.passage_prefix.as_deref())
    }

    fn embed_query(&mut self, query: &str) -> Result<Vec<f32>> {
        let vectors = self.request(&[query.trim().to_string()], 1, self.query_prefix.as_deref())?;
        vectors
            .into_iter()
            .next()
            .ok_or_else(|| Error::Embedding("embedder returned no query vector".to_string()))
    }
}

#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingItem {
    embedding: Vec<f32>,
    #[serde(default)]
    index: Option<usize>,
}

/// Normalize a base URL into the embeddings endpoint.
///
/// `.../embeddings` is used as-is; `.../v1` gains `/embeddings`; anything else
/// gains `/v1/embeddings`. Trailing slashes are trimmed first.
fn resolve_embeddings_endpoint(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.ends_with("/embeddings") {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}/embeddings")
    } else {
        format!("{trimmed}/v1/embeddings")
    }
}

struct DeterministicEmbedder {
    dimension: usize,
}

impl DeterministicEmbedder {
    const fn new(dimension: usize) -> Self {
        Self { dimension }
    }
}

impl SemanticEmbedder for DeterministicEmbedder {
    fn id(&self) -> &str {
        "embeddinggemma/local-test"
    }

    fn embed(&mut self, texts: &[String], _batch_size: usize) -> Result<Vec<Vec<f32>>> {
        texts
            .iter()
            .map(|text| deterministic_vector(text, self.dimension))
            .collect()
    }
}

fn deterministic_vector(text: &str, dimension: usize) -> Result<Vec<f32>> {
    if dimension == 0 {
        return Err(Error::Embedding(
            "embedding dimension must be nonzero".to_string(),
        ));
    }
    let mut vector = vec![0.0_f32; dimension];
    for term in text.split_whitespace() {
        let hash = Sha256::digest(term.as_bytes());
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(&hash[..8]);
        let dimension =
            u64::try_from(dimension).map_err(|error| Error::Embedding(error.to_string()))?;
        let index = usize::try_from(u64::from_le_bytes(bytes) % dimension)
            .map_err(|error| Error::Embedding(error.to_string()))?;
        vector[index] += 1.0;
    }
    normalize(&mut vector);
    Ok(vector)
}

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm == 0.0 {
        return;
    }
    for value in vector {
        *value /= norm;
    }
}

fn to_embedding_error(error: impl std::fmt::Display) -> Error {
    Error::Embedding(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_resolution_covers_common_url_shapes() {
        assert_eq!(
            resolve_embeddings_endpoint("http://host:8080"),
            "http://host:8080/v1/embeddings"
        );
        assert_eq!(
            resolve_embeddings_endpoint("http://host:8080/"),
            "http://host:8080/v1/embeddings"
        );
        assert_eq!(
            resolve_embeddings_endpoint("http://host:8080/v1"),
            "http://host:8080/v1/embeddings"
        );
        assert_eq!(
            resolve_embeddings_endpoint("http://host:8080/v1/"),
            "http://host:8080/v1/embeddings"
        );
        assert_eq!(
            resolve_embeddings_endpoint("http://host:8080/v1/embeddings"),
            "http://host:8080/v1/embeddings"
        );
        assert_eq!(
            resolve_embeddings_endpoint("https://api.openai.com/v1/embeddings/"),
            "https://api.openai.com/v1/embeddings"
        );
    }

    #[test]
    fn openai_embedder_requires_a_url() {
        let config = KatokConfig::default();
        let error = OpenAiCompatibleEmbedder::new(&config).expect_err("url is required");
        assert!(matches!(error, Error::Embedding(_)));
    }

    #[test]
    fn openai_embedder_uses_model_prefixed_id() {
        let config = KatokConfig {
            embedder_url: Some("http://host/v1".to_string()),
            embedder_model: "Qwen3-Embedding-0.6B".to_string(),
            vector_dimension: 1024,
            ..KatokConfig::default()
        };
        let embedder = OpenAiCompatibleEmbedder::new(&config).expect("build embedder");
        assert_eq!(embedder.id(), "openai/Qwen3-Embedding-0.6B");
        assert_eq!(embedder.endpoint, "http://host/v1/embeddings");
        assert_eq!(embedder.dimension, 1024);
    }

    #[test]
    fn empty_input_yields_no_request() {
        let config = KatokConfig {
            embedder_url: Some("http://127.0.0.1:9/v1".to_string()),
            ..KatokConfig::default()
        };
        let mut embedder = OpenAiCompatibleEmbedder::new(&config).expect("build embedder");
        let vectors = embedder.embed(&[], 8).expect("empty embed");
        assert!(vectors.is_empty());
    }
}
