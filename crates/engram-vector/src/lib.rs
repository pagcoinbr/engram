use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone)]
pub struct QdrantClient {
    base_url: String,
    collection: String,
    client: Client,
}

#[derive(Debug, Error)]
pub enum VectorError {
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Hit {
    pub file: String,
    pub name: String,
    pub description: String,
    pub score: f64,
}
#[derive(Deserialize)]
struct Response {
    result: Points,
}
#[derive(Deserialize)]
struct Points {
    points: Vec<Point>,
}
#[derive(Deserialize)]
struct Point {
    payload: Option<Payload>,
    score: Option<f64>,
}
#[derive(Deserialize)]
struct Payload {
    file: Option<String>,
    name: Option<String>,
    description: Option<String>,
}

impl QdrantClient {
    pub fn new(base_url: impl Into<String>, collection: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').into(),
            collection: collection.into(),
            client: Client::new(),
        }
    }
    pub async fn search(
        &self,
        vector: Vec<f32>,
        limit: usize,
        slug: Option<&str>,
    ) -> Result<Vec<Hit>, VectorError> {
        let mut body = serde_json::json!({"query": vector, "limit": limit, "with_payload": true});
        if let Some(slug) = slug {
            body["filter"] =
                serde_json::json!({"must": [{"key": "slug", "match": {"value": slug}}]});
        }
        let response = self
            .client
            .post(format!(
                "{}/collections/{}/points/query",
                self.base_url, self.collection
            ))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<Response>()
            .await?;
        Ok(response
            .result
            .points
            .into_iter()
            .filter_map(|point| {
                let payload = point.payload?;
                Some(Hit {
                    file: payload.file?,
                    name: payload.name.unwrap_or_default(),
                    description: payload.description.unwrap_or_default(),
                    score: point.score.unwrap_or_default(),
                })
            })
            .collect())
    }
}
