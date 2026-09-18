use serde::{Deserialize, Serialize};
use std::{fs, path::Path};
use thiserror::Error;
use url::Url;

pub const CONFIG_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub config_version: u32,
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default)]
    pub llama_cpp: LlamaCpp,
    #[serde(default)]
    pub embed: Embed,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct LlamaCpp {
    #[serde(default)] pub url: String,
    #[serde(default)] pub model: String,
    #[serde(default)] pub timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Embed {
    #[serde(default)] pub provider: String,
    #[serde(default)] pub url: String,
    #[serde(default)] pub model: String,
    #[serde(default = "default_dimension")] pub dim: u32,
    #[serde(default)] pub query_prefix: String,
    #[serde(default)] pub document_prefix: String,
}

fn default_backend() -> String { "ollama".into() }
fn default_dimension() -> u32 { 768 }
impl Default for Embed {
    fn default() -> Self {
        Self { provider: String::new(), url: String::new(), model: String::new(), dim: default_dimension(), query_prefix: String::new(), document_prefix: String::new() }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ModelProfile {
    pub role: &'static str,
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub expected_dimension: Option<u32>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read config: {0}")]
    Read(#[from] std::io::Error),
    #[error("could not parse YAML: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("{0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let mut config: Self = serde_yaml::from_str(&fs::read_to_string(path)?)?;
        if config.config_version == 0 { config.config_version = CONFIG_VERSION; }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.embed.dim == 0 { return Err(ConfigError::Invalid("embed.dim must be greater than zero".into())); }
        if !self.embed.url.is_empty() { endpoint(&self.embed.url, "embed.url")?; }
        if !self.llama_cpp.url.is_empty() { endpoint(&self.llama_cpp.url, "llama_cpp.url")?; }
        if matches!(self.embed.provider.as_str(), "llama_cpp" | "openai") && self.embed.url.is_empty() {
            return Err(ConfigError::Invalid("embed.url is required for llama_cpp/openai embeddings".into()));
        }
        Ok(())
    }

    pub fn profiles(&self) -> Vec<ModelProfile> {
        let generation_endpoint = self.llama_cpp.url.clone();
        let generation = ModelProfile { role: "reasoning", provider: self.backend.clone(), endpoint: generation_endpoint, model: self.llama_cpp.model.clone(), expected_dimension: None };
        let embed_endpoint = if self.embed.url.is_empty() { self.llama_cpp.url.clone() } else { self.embed.url.clone() };
        let embedding = ModelProfile { role: "embedding", provider: self.embed.provider.clone(), endpoint: embed_endpoint, model: self.embed.model.clone(), expected_dimension: Some(self.embed.dim) };
        vec![generation, embedding]
    }
}

fn endpoint(value: &str, name: &str) -> Result<(), ConfigError> {
    let parsed = Url::parse(value).map_err(|_| ConfigError::Invalid(format!("{name} must be an absolute HTTP(S) URL")))?;
    if !matches!(parsed.scheme(), "http" | "https") { return Err(ConfigError::Invalid(format!("{name} must use HTTP(S)"))); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn llama_cpp_embedding_profile_keeps_its_space() {
        let config: Config = serde_yaml::from_str("backend: llama_cpp\nllama_cpp: {url: http://ai/v1, model: qwen}\nembed: {provider: llama_cpp, url: http://embed/v1, model: bge-m3, dim: 1024}\n").unwrap();
        config.validate().unwrap();
        assert_eq!(config.profiles()[1].expected_dimension, Some(1024));
        assert_eq!(config.profiles()[1].endpoint, "http://embed/v1");
    }
    #[test]
    fn llama_cpp_embeddings_require_an_endpoint() {
        let config: Config = serde_yaml::from_str("embed: {provider: llama_cpp, model: bge-m3, dim: 1024}\n").unwrap();
        assert!(config.validate().is_err());
    }
}
