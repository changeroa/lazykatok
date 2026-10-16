use crate::{Error, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_EMBEDDER_MODEL: &str = "embeddinggemma-300m-q4";
pub const DEFAULT_VECTOR_DIMENSION: u16 = 768;
pub const DEFAULT_EMBEDDER_PROVIDER: &str = "local";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KatokConfig {
    pub source_adapter: String,
    pub chunk_gap_group_seconds: i64,
    pub chunk_gap_direct_seconds: i64,
    pub semantic_dir: PathBuf,
    pub embedder_model: String,
    pub embedding_batch_size: usize,
    pub vector_dimension: u16,
    pub snippet_length: usize,
    /// Embedder backend: `local` (fastembed), `mock`/`local-test` (deterministic),
    /// or an OpenAI-compatible HTTP endpoint (`openai`, `openai-compatible`, `http`,
    /// `loopback-http`).
    pub embedder_provider: String,
    /// Base URL of the OpenAI-compatible embedding endpoint. `embedder_endpoint` is
    /// accepted as an alias so either spelling works in `katok.toml`.
    #[serde(alias = "embedder_endpoint")]
    pub embedder_url: Option<String>,
    /// Name of an environment variable holding the bearer token for the endpoint.
    pub embedder_api_key_env: Option<String>,
    /// Text prepended to every search query before embedding.
    pub query_prefix: Option<String>,
    /// Text prepended to every indexed passage before embedding.
    pub passage_prefix: Option<String>,
    /// Resolved bearer token for the endpoint. Never read from `katok.toml`; it is
    /// populated from the environment by [`KatokConfig::apply_env_overrides`].
    #[serde(skip)]
    pub embedder_api_key: Option<String>,
}

impl Default for KatokConfig {
    fn default() -> Self {
        Self {
            source_adapter: "fixture".to_string(),
            chunk_gap_group_seconds: 600,
            chunk_gap_direct_seconds: 1_800,
            semantic_dir: PathBuf::from("semantic"),
            embedder_model: DEFAULT_EMBEDDER_MODEL.to_string(),
            embedding_batch_size: 64,
            vector_dimension: DEFAULT_VECTOR_DIMENSION,
            snippet_length: 80,
            embedder_provider: DEFAULT_EMBEDDER_PROVIDER.to_string(),
            embedder_url: None,
            embedder_api_key_env: None,
            query_prefix: None,
            passage_prefix: None,
            embedder_api_key: None,
        }
    }
}

impl KatokConfig {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let content = std::fs::read_to_string(path).map_err(Error::Io)?;
        toml::from_str(&content).map_err(Error::Config)
    }

    /// Return a copy with `KATOK_*` / `LAZYKATOK_*` environment overrides applied.
    ///
    /// `@autorag/librarian`'s `LazykatokClient` exports both prefixes, so both are honored.
    #[must_use]
    pub fn with_env_overrides(&self) -> Self {
        let mut config = self.clone();
        config.apply_env_overrides();
        config
    }

    /// Apply `KATOK_*` / `LAZYKATOK_*` environment overrides in place.
    pub fn apply_env_overrides(&mut self) {
        self.apply_env_lookup(|name| std::env::var(name).ok().filter(|value| !value.is_empty()));
    }

    /// Apply overrides using an explicit variable lookup. Separated from
    /// [`KatokConfig::apply_env_overrides`] so the mapping is unit-testable without
    /// mutating the process environment.
    pub fn apply_env_lookup(&mut self, lookup: impl Fn(&str) -> Option<String>) {
        let mode = first_env(&lookup, &["KATOK_EMBEDDER", "LAZYKATOK_EMBEDDER"]);
        if let Some(mode) = mode.as_deref() {
            self.embedder_provider = mode.to_string();
        }
        if let Some(url) = first_env(&lookup, &["KATOK_EMBED_URL", "LAZYKATOK_EMBED_URL"]) {
            self.embedder_url = Some(url);
            // A URL selects the HTTP embedder unless the caller explicitly asked for a
            // non-network mode.
            if !matches!(mode.as_deref(), Some("mock" | "local-test")) {
                self.embedder_provider = "openai-compatible".to_string();
            }
        }
        if let Some(model) = first_env(&lookup, &["KATOK_EMBED_MODEL", "LAZYKATOK_EMBED_MODEL"]) {
            self.embedder_model = model;
        }
        if let Some(dimension) = first_env(&lookup, &["KATOK_EMBED_DIM", "LAZYKATOK_EMBED_DIM"]) {
            match dimension.parse::<u16>() {
                Ok(dimension) => self.vector_dimension = dimension,
                Err(error) => {
                    eprintln!("ignoring invalid KATOK_EMBED_DIM={dimension}: {error}");
                }
            }
        }
        if let Some(key) = first_env(&lookup, &["KATOK_EMBED_API_KEY", "LAZYKATOK_EMBED_API_KEY"]) {
            self.embedder_api_key = Some(key);
        } else if let Some(name) = &self.embedder_api_key_env {
            if let Some(key) = lookup(name).filter(|value| !value.is_empty()) {
                self.embedder_api_key = Some(key);
            }
        }
    }

    /// Collapse the configured provider into the embedder family the indexer selects:
    /// `local`, `mock`, or `openai-compatible`.
    #[must_use]
    pub fn embedder_family(&self) -> &'static str {
        match self.embedder_provider.as_str() {
            "mock" | "local-test" => "mock",
            "openai" | "openai-compatible" | "http" | "loopback-http" => "openai-compatible",
            _ if self.embedder_url.is_some() => "openai-compatible",
            _ => "local",
        }
    }
}

fn first_env(lookup: &impl Fn(&str) -> Option<String>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| lookup(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<HashMap<_, _>>();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn defaults_are_local_with_no_endpoint() {
        let config = KatokConfig::default();
        assert_eq!(config.embedder_provider, DEFAULT_EMBEDDER_PROVIDER);
        assert_eq!(config.embedder_family(), "local");
        assert!(config.embedder_url.is_none());
        assert_eq!(config.vector_dimension, DEFAULT_VECTOR_DIMENSION);
    }

    #[test]
    fn embed_url_switches_provider_to_openai_compatible() {
        let mut config = KatokConfig::default();
        config.apply_env_lookup(lookup(&[("KATOK_EMBED_URL", "http://127.0.0.1:8080/v1")]));
        assert_eq!(config.embedder_provider, "openai-compatible");
        assert_eq!(config.embedder_family(), "openai-compatible");
        assert_eq!(
            config.embedder_url.as_deref(),
            Some("http://127.0.0.1:8080/v1")
        );
    }

    #[test]
    fn embed_url_does_not_override_explicit_test_mode() {
        let mut config = KatokConfig::default();
        config.apply_env_lookup(lookup(&[
            ("KATOK_EMBEDDER", "local-test"),
            ("KATOK_EMBED_URL", "http://127.0.0.1:8080"),
        ]));
        assert_eq!(config.embedder_provider, "local-test");
        assert_eq!(config.embedder_family(), "mock");
    }

    #[test]
    fn lazykatok_prefixed_variables_are_honored() {
        let mut config = KatokConfig::default();
        config.apply_env_lookup(lookup(&[
            ("LAZYKATOK_EMBED_URL", "http://127.0.0.1:9000"),
            ("LAZYKATOK_EMBED_MODEL", "Qwen3-Embedding-0.6B"),
            ("LAZYKATOK_EMBED_DIM", "1024"),
            ("LAZYKATOK_EMBED_API_KEY", "secret-token"),
        ]));
        assert_eq!(
            config.embedder_url.as_deref(),
            Some("http://127.0.0.1:9000")
        );
        assert_eq!(config.embedder_model, "Qwen3-Embedding-0.6B");
        assert_eq!(config.vector_dimension, 1024);
        assert_eq!(config.embedder_api_key.as_deref(), Some("secret-token"));
    }

    #[test]
    fn api_key_can_be_indirected_through_named_variable() {
        let mut config = KatokConfig {
            embedder_api_key_env: Some("MY_EMBED_KEY".to_string()),
            ..KatokConfig::default()
        };
        config.apply_env_lookup(lookup(&[("MY_EMBED_KEY", "indirect-token")]));
        assert_eq!(config.embedder_api_key.as_deref(), Some("indirect-token"));
    }

    #[test]
    fn invalid_dimension_is_ignored() {
        let mut config = KatokConfig::default();
        config.apply_env_lookup(lookup(&[("KATOK_EMBED_DIM", "not-a-number")]));
        assert_eq!(config.vector_dimension, DEFAULT_VECTOR_DIMENSION);
    }

    #[test]
    fn http_provider_aliases_map_to_openai_compatible_family() {
        for alias in ["openai", "openai-compatible", "http", "loopback-http"] {
            let mut config = KatokConfig::default();
            config.apply_env_lookup(lookup(&[("KATOK_EMBEDDER", alias)]));
            assert_eq!(config.embedder_provider, alias);
            assert_eq!(
                config.embedder_family(),
                "openai-compatible",
                "alias {alias}"
            );
        }
    }

    #[test]
    fn embedder_endpoint_alias_is_accepted_in_toml() {
        let config: KatokConfig = toml::from_str("embedder_endpoint = \"http://host/v1\"\n")
            .expect("parse aliased endpoint");
        assert_eq!(config.embedder_url.as_deref(), Some("http://host/v1"));
        assert_eq!(config.embedder_family(), "openai-compatible");
    }
}
