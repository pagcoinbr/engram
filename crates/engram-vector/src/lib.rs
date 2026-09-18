use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const POINT_NAMESPACE: Uuid = Uuid::from_u128(0x6f9b7c2e2a4d5e1f9c3a0a1b2c3d4e5f);

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
    #[error(
        "collection '{collection}' has dimension {actual:?}; expected {expected}. Rebuild the index before changing embedding spaces"
    )]
    Dimension {
        collection: String,
        expected: u32,
        actual: Option<u64>,
    },
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Hit {
    pub file: String,
    pub name: String,
    pub description: String,
    pub score: f64,
}

#[derive(Debug, Serialize)]
pub struct IndexPoint<'a> {
    pub file: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub memory_type: &'a str,
    pub slug: &'a str,
    pub sha: &'a str,
    pub vector: Vec<f32>,
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

    pub async fn ensure_collection(
        &self,
        dimension: u32,
        recreate: bool,
    ) -> Result<(), VectorError> {
        let url = format!("{}/collections/{}", self.base_url, self.collection);
        let response = self.client.get(&url).send().await?;
        if response.status().is_success() && !recreate {
            let body: serde_json::Value = response.json().await?;
            let actual = body
                .pointer("/result/config/params/vectors/size")
                .and_then(serde_json::Value::as_u64);
            if actual == Some(dimension.into()) {
                return Ok(());
            }
            return Err(VectorError::Dimension {
                collection: self.collection.clone(),
                expected: dimension,
                actual,
            });
        }
        if response.status().is_success() && recreate {
            self.client.delete(&url).send().await?.error_for_status()?;
        }
        self.client
            .put(&url)
            .json(&serde_json::json!({"vectors": {"size": dimension, "distance": "Cosine"}}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn upsert(&self, point: IndexPoint<'_>) -> Result<(), VectorError> {
        let id = Uuid::new_v5(
            &POINT_NAMESPACE,
            format!("{}::{}", point.slug, point.file).as_bytes(),
        );
        self.client
            .put(format!(
                "{}/collections/{}/points",
                self.base_url, self.collection
            ))
            .json(&serde_json::json!({"points": [{
                "id": id.to_string(),
                "vector": point.vector,
                "payload": {
                    "file": point.file,
                    "name": point.name,
                    "description": point.description,
                    "type": point.memory_type,
                    "slug": point.slug,
                    "sha": point.sha,
                }
            }]}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}
