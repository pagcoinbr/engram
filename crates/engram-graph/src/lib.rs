use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;

#[derive(Clone)]
pub struct GraphClient {
    endpoint: String,
    user: String,
    password: String,
    client: Client,
}

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("invalid Neo4j URI: {0}")]
    Uri(String),
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("Neo4j error: {0}")]
    Database(String),
}

#[derive(Deserialize)]
struct Response {
    results: Vec<ResultSet>,
    errors: Vec<NeoError>,
}
#[derive(Deserialize)]
struct ResultSet {
    data: Vec<Row>,
}
#[derive(Deserialize)]
struct Row {
    row: Vec<serde_json::Value>,
}
#[derive(Deserialize)]
struct NeoError {
    message: String,
}

impl GraphClient {
    pub fn new(
        uri: &str,
        database: &str,
        user: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, GraphError> {
        let endpoint = http_endpoint(uri, database)?;
        Ok(Self {
            endpoint,
            user: user.into(),
            password: password.into(),
            client: Client::new(),
        })
    }
    pub async fn facts_for_tokens(
        &self,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<String>, GraphError> {
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let response = self.query("UNWIND $names AS nm MATCH (n:Entity)-[r:RELATES_TO]-(m:Entity) WHERE toLower(n.name)=toLower(nm) RETURN r.fact AS fact LIMIT $lim", serde_json::json!({"names": tokens, "lim": limit})).await?;
        Ok(response
            .results
            .into_iter()
            .flat_map(|set| set.data)
            .filter_map(|row| {
                row.row
                    .into_iter()
                    .next()
                    .and_then(|value| value.as_str().map(str::to_string))
            })
            .collect())
    }
    async fn query(
        &self,
        statement: &str,
        parameters: serde_json::Value,
    ) -> Result<Response, GraphError> {
        let response = self.client.post(&self.endpoint).basic_auth(&self.user, Some(&self.password)).json(&serde_json::json!({"statements": [{"statement": statement, "parameters": parameters}]})).send().await?.error_for_status()?.json::<Response>().await?;
        if let Some(error) = response.errors.first() {
            return Err(GraphError::Database(error.message.clone()));
        }
        Ok(response)
    }
}

pub fn http_endpoint(uri: &str, database: &str) -> Result<String, GraphError> {
    let trimmed = uri.trim();
    let authority = trimmed
        .split("://")
        .nth(1)
        .ok_or_else(|| GraphError::Uri("expected bolt://host:port".into()))?;
    let host = authority
        .split('/')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    if !matches!(host, "127.0.0.1" | "localhost" | "::1") {
        return Err(GraphError::Uri(
            "remote Neo4j requires an explicit HTTPS endpoint".into(),
        ));
    }
    Ok(format!("http://{host}:7474/db/{database}/tx/commit"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loopback_only() {
        assert_eq!(
            http_endpoint("bolt://127.0.0.1:7687", "neo4j").unwrap(),
            "http://127.0.0.1:7474/db/neo4j/tx/commit"
        );
        assert!(http_endpoint("bolt://10.0.0.9:7687", "neo4j").is_err());
    }
}
