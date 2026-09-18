use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

#[derive(Clone)]
pub struct OpenAiCompatibleClient {
    base_url: String,
    client: Client,
}

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("invalid endpoint: {0}")]
    Endpoint(String),
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("server returned no embedding")]
    EmptyEmbedding,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelItem>,
}
#[derive(Debug, Deserialize)]
struct ModelItem {
    id: String,
}
#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}
#[derive(Debug, Deserialize)]
struct EmbeddingItem {
    embedding: Vec<f32>,
}
#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}
#[derive(Deserialize)]
struct Choice {
    message: Message,
}
#[derive(Deserialize)]
struct Message {
    content: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ProbeReport {
    pub endpoint: String,
    pub configured_model: String,
    pub observed_model: Option<String>,
    pub observed_dimension: Option<usize>,
    pub expected_dimension: Option<u32>,
    pub index_compatible: Option<bool>,
}

impl OpenAiCompatibleClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self, ProbeError> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        if !(base_url.starts_with("http://") || base_url.starts_with("https://"))
            || !base_url.ends_with("/v1")
        {
            return Err(ProbeError::Endpoint(
                "expected an absolute HTTP(S) /v1 endpoint".into(),
            ));
        }
        Ok(Self {
            base_url,
            client: Client::builder()
                .timeout(Duration::from_secs(90))
                .build()
                .map_err(ProbeError::Request)?,
        })
    }

    pub async fn models(&self) -> Result<Vec<String>, ProbeError> {
        Ok(self
            .client
            .get(format!("{}/models", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json::<ModelsResponse>()
            .await?
            .data
            .into_iter()
            .map(|item| item.id)
            .collect())
    }

    pub async fn embedding_dimension(&self, model: &str) -> Result<usize, ProbeError> {
        Ok(self.embedding(model, "engram health probe").await?.len())
    }

    pub async fn embedding(&self, model: &str, input: &str) -> Result<Vec<f32>, ProbeError> {
        let response = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .json(&serde_json::json!({"model": model, "input": input}))
            .send()
            .await?
            .error_for_status()?
            .json::<EmbeddingResponse>()
            .await?;
        response
            .data
            .into_iter()
            .next()
            .map(|item| item.embedding)
            .filter(|embedding| !embedding.is_empty())
            .ok_or(ProbeError::EmptyEmbedding)
    }

    pub async fn probe(
        &self,
        model: &str,
        expected_dimension: Option<u32>,
    ) -> Result<ProbeReport, ProbeError> {
        let observed_model = self.models().await?.into_iter().next();
        let observed_dimension = self.embedding_dimension(model).await?;
        Ok(ProbeReport {
            endpoint: self.base_url.clone(),
            configured_model: model.into(),
            observed_model,
            observed_dimension: Some(observed_dimension),
            expected_dimension,
            index_compatible: expected_dimension
                .map(|expected| expected as usize == observed_dimension),
        })
    }
    pub async fn chat(&self, model: &str, prompt: &str) -> Result<String, ProbeError> {
        let response = self.client.post(format!("{}/chat/completions", self.base_url)).json(&serde_json::json!({"model": model, "temperature": 0, "messages": [{"role": "user", "content": prompt}]})).send().await?.error_for_status()?.json::<ChatResponse>().await?;
        response
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message.content)
            .ok_or(ProbeError::EmptyEmbedding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_a_v1_endpoint() {
        assert!(OpenAiCompatibleClient::new("http://ai:8091/v1").is_ok());
        assert!(OpenAiCompatibleClient::new("http://ai:8091").is_err());
    }
}
