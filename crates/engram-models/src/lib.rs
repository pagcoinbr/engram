use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

#[derive(Clone)]
pub struct OpenAiCompatibleClient {
    base_url: String,
    client: Client,
    /// Sent as `Authorization: Bearer`. Requests previously carried no headers at
    /// all, so `llama_cpp.api_key` — documented and present in shipped configs —
    /// was silently dropped and every authenticated endpoint rejected the call.
    api_key: Option<String>,
    max_tokens: u32,
}

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("invalid endpoint: {0}")]
    Endpoint(String),
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("server returned no embedding")]
    EmptyEmbedding,
    /// Distinct from `EmptyEmbedding`: a chat call that comes back with no choices
    /// used to be reported as "server returned no embedding", which sent anyone
    /// debugging it looking at the wrong endpoint.
    #[error("server returned no completion")]
    EmptyCompletion,
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
            client: build_client(90)?,
            api_key: None,
            max_tokens: 512,
        })
    }

    /// Attach a bearer token. `None` leaves the client unauthenticated.
    pub fn with_api_key(mut self, key: Option<&str>) -> Self {
        self.api_key = key
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_string);
        self
    }

    /// Replace the fixed 90s timeout with the configured one. An embedding server
    /// loading a large model can legitimately need longer; a recall path needs far
    /// less.
    pub fn with_timeout(mut self, seconds: u64) -> Self {
        if seconds > 0
            && let Ok(client) = build_client(seconds)
        {
            self.client = client;
        }
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        if max_tokens > 0 {
            self.max_tokens = max_tokens;
        }
        self
    }

    /// Every outbound request goes through here, so authentication cannot be
    /// forgotten on one call site.
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self
            .client
            .request(method, format!("{}/{path}", self.base_url));
        match &self.api_key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        }
    }

    pub async fn models(&self) -> Result<Vec<String>, ProbeError> {
        Ok(self
            .request(reqwest::Method::GET, "models")
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
            .request(reqwest::Method::POST, "embeddings")
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
        let response = self
            .request(reqwest::Method::POST, "chat/completions")
            .json(&serde_json::json!({
                "model": model,
                "temperature": 0,
                "max_tokens": self.max_tokens,
                // engram callers want structured output, never chain-of-thought:
                // reasoning models otherwise spend the whole budget on <think> and
                // return truncated or empty JSON.
                "chat_template_kwargs": {"enable_thinking": false},
                "messages": [{"role": "user", "content": prompt}],
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<ChatResponse>()
            .await?;
        response
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message.content)
            .ok_or(ProbeError::EmptyCompletion)
    }
}

fn build_client(timeout_seconds: u64) -> Result<Client, ProbeError> {
    Client::builder()
        .timeout(Duration::from_secs(timeout_seconds))
        .build()
        .map_err(ProbeError::Request)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_a_v1_endpoint() {
        assert!(OpenAiCompatibleClient::new("http://ai:8091/v1").is_ok());
        assert!(OpenAiCompatibleClient::new("http://ai:8091").is_err());
    }

    #[test]
    fn api_key_is_only_set_when_one_was_configured() {
        let client = OpenAiCompatibleClient::new("http://ai:8091/v1").unwrap();
        assert_eq!(client.clone().with_api_key(None).api_key, None);
        // an empty or whitespace value in YAML is not a credential
        assert_eq!(client.clone().with_api_key(Some("   ")).api_key, None);
        assert_eq!(
            client.with_api_key(Some(" sk-abc ")).api_key.as_deref(),
            Some("sk-abc")
        );
    }

    #[test]
    fn an_empty_completion_is_not_reported_as_a_missing_embedding() {
        assert_eq!(
            ProbeError::EmptyCompletion.to_string(),
            "server returned no completion"
        );
    }
}
