use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub struct RecallHit {
    pub file: String,
    pub facts: Vec<String>,
    pub score: f64,
}

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
    pub async fn semantic_files(
        &self,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<RecallHit>, GraphError> {
        self.file_hits(
            "MATCH (n:Entity)-[e:RELATES_TO]->(m:Entity) WITH e, vector.similarity.cosine(e.fact_embedding, $vector) AS score WHERE score > 0 UNWIND coalesce(e.episodes, []) AS episode MATCH (ep:Episodic {uuid: episode}) WHERE ep.file IS NOT NULL RETURN ep.file AS file, collect(DISTINCT e.fact) AS facts, max(score) AS score ORDER BY score DESC LIMIT $limit",
            serde_json::json!({"vector": vector, "limit": limit}),
        ).await
    }
    pub async fn keyword_files(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RecallHit>, GraphError> {
        self.file_hits(
            "CALL db.index.fulltext.queryRelationships('edge_name_and_fact', $query, {limit: $limit}) YIELD relationship AS rel, score UNWIND coalesce(rel.episodes, []) AS episode MATCH (ep:Episodic {uuid: episode}) WHERE ep.file IS NOT NULL RETURN ep.file AS file, collect(DISTINCT rel.fact) AS facts, max(score) AS score ORDER BY score DESC LIMIT $limit",
            serde_json::json!({"query": query, "limit": limit}),
        ).await
    }
    pub async fn native_keyword_files(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RecallHit>, GraphError> {
        let tokens = query
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| word.len() >= 4)
            .map(|word| word.to_lowercase())
            .collect::<Vec<_>>();
        self.file_hits("MATCH (m:EngramMemory)-[:HAS_FACT]->(f:EngramFact) WHERE any(token IN $tokens WHERE toLower(f.text) CONTAINS token) RETURN m.file AS file, collect(DISTINCT f.text) AS facts, count(f) AS score ORDER BY score DESC LIMIT $limit", serde_json::json!({"tokens": tokens, "limit": limit})).await
    }
    pub async fn upsert_native_memory(
        &self,
        file: &str,
        name: &str,
        description: &str,
        body: &str,
        sha: &str,
    ) -> Result<(), GraphError> {
        self.query(
            "MERGE (m:EngramMemory {file: $file}) SET m.name = $name, m.description = $description, m.body = $body, m.sha = $sha, m.updated_at = datetime() RETURN m.file",
            serde_json::json!({"file": file, "name": name, "description": description, "body": body, "sha": sha}),
        ).await?;
        Ok(())
    }
    pub async fn replace_native_facts(
        &self,
        file: &str,
        facts: &[String],
    ) -> Result<(), GraphError> {
        self.query("MATCH (m:EngramMemory {file: $file}) OPTIONAL MATCH (m)-[old:HAS_FACT]->(:EngramFact) DELETE old WITH m UNWIND $facts AS fact MERGE (f:EngramFact {memory_file: $file, text: fact}) SET f.updated_at = datetime() MERGE (m)-[:HAS_FACT]->(f)", serde_json::json!({"file": file, "facts": facts})).await?;
        Ok(())
    }
    async fn file_hits(
        &self,
        statement: &str,
        parameters: serde_json::Value,
    ) -> Result<Vec<RecallHit>, GraphError> {
        let response = self.query(statement, parameters).await?;
        Ok(response
            .results
            .into_iter()
            .flat_map(|set| set.data)
            .filter_map(|row| {
                let file = row.row.first()?.as_str()?.to_string();
                let facts = row
                    .row
                    .get(1)?
                    .as_array()?
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect();
                let score = row.row.get(2)?.as_f64().unwrap_or_default();
                Some(RecallHit { file, facts, score })
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
