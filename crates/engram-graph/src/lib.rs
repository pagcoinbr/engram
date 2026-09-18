use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub struct RecallHit {
    pub file: String,
    pub facts: Vec<String>,
    pub score: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NativeTriple {
    pub subject: String,
    pub relation: String,
    pub object: String,
    pub confidence: f64,
    pub temporal: String,
}

pub const RELATION_TAXONOMY: &[&str] = &[
    "belongs_to",
    "conflicts_with",
    "connects_to",
    "depends_on",
    "hosts",
    "implements",
    "owns",
    "provides",
    "runs_on",
    "supersedes",
    "uses",
];

pub fn is_valid_relation(relation: &str) -> bool {
    RELATION_TAXONOMY.contains(&relation)
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
    pub async fn native_facts_for_tokens(
        &self,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<String>, GraphError> {
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let response = self.query("UNWIND $names AS name MATCH (t:EngramTriple {status: 'active'}) WHERE t.valid_until IS NULL AND (toLower(t.subject) CONTAINS toLower(name) OR toLower(t.object) CONTAINS toLower(name)) RETURN DISTINCT t.subject + ' ' + t.relation + ' ' + t.object AS fact LIMIT $lim", serde_json::json!({"names": tokens, "lim": limit})).await?;
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
        self.file_hits("MATCH (m:EngramMemory) WITH m, [token IN $tokens WHERE toLower(coalesce(m.name, '') + ' ' + coalesce(m.description, '') + ' ' + coalesce(m.body, '')) CONTAINS token] AS memory_matches WHERE size(memory_matches) > 0 OPTIONAL MATCH (m)-[:HAS_FACT]->(f:EngramFact) WHERE f.valid_until IS NULL AND any(token IN $tokens WHERE toLower(f.text) CONTAINS token) WITH m, memory_matches, collect(DISTINCT f.text) AS fact_text, count(f) AS fact_score OPTIONAL MATCH (m)-[:HAS_TRIPLE]->(t:EngramTriple {status: 'active'}) WHERE t.valid_until IS NULL AND any(token IN $tokens WHERE toLower(t.subject + ' ' + t.relation + ' ' + t.object) CONTAINS token) WITH m, memory_matches, fact_text, fact_score, collect(DISTINCT t.subject + ' ' + t.relation + ' ' + t.object) AS triple_text, count(t) AS triple_score WITH m, fact_text + triple_text AS texts, size(memory_matches) + fact_score + triple_score AS score RETURN m.file AS file, texts AS facts, score ORDER BY score DESC LIMIT $limit", serde_json::json!({"tokens": tokens, "limit": limit})).await
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
    pub async fn native_memory_is_current(
        &self,
        file: &str,
        sha: &str,
    ) -> Result<bool, GraphError> {
        let response = self
            .query(
                "MATCH (m:EngramMemory {file: $file, sha: $sha, native_triple_version: 1}) RETURN count(m) > 0",
                serde_json::json!({"file": file, "sha": sha}),
            )
            .await?;
        Ok(response
            .results
            .into_iter()
            .flat_map(|set| set.data)
            .next()
            .and_then(|row| row.row.into_iter().next())
            .and_then(|value| value.as_bool())
            .unwrap_or(false))
    }
    pub async fn replace_native_facts(
        &self,
        file: &str,
        facts: &[String],
    ) -> Result<(), GraphError> {
        self.query("MATCH (m:EngramMemory {file: $file}) OPTIONAL MATCH (m)-[old:HAS_FACT]->(obsolete:EngramFact) WHERE NOT obsolete.text IN $facts SET obsolete.valid_until = datetime() DELETE old WITH m UNWIND $facts AS fact MERGE (f:EngramFact {memory_file: $file, text: fact}) ON CREATE SET f.valid_from = datetime(), f.created_at = datetime() SET f.valid_until = null, f.updated_at = datetime() MERGE (m)-[:HAS_FACT]->(f) WITH f, [word IN split(toLower(f.text), ' ') WHERE size(word) >= 6] AS names UNWIND names AS name MERGE (e:EngramEntity {name: name}) MERGE (f)-[:MENTIONS]->(e)", serde_json::json!({"file": file, "facts": facts})).await?;
        Ok(())
    }
    pub async fn replace_native_triples(
        &self,
        file: &str,
        triples: &[NativeTriple],
    ) -> Result<(), GraphError> {
        self.query("MATCH (m:EngramMemory {file: $file}) OPTIONAL MATCH (m)-[old:HAS_TRIPLE]->(obsolete:EngramTriple) WHERE NOT [triple IN $triples | triple.key] CONTAINS obsolete.key SET obsolete.valid_until = datetime(), obsolete.status = 'superseded' DELETE old WITH m UNWIND $triples AS triple MERGE (t:EngramTriple {memory_file: $file, key: triple.key}) ON CREATE SET t.valid_from = datetime(), t.created_at = datetime() SET t.subject = triple.subject, t.relation = triple.relation, t.object = triple.object, t.confidence = triple.confidence, t.temporal = triple.temporal, t.status = triple.status, t.valid_until = CASE WHEN triple.status = 'active' THEN null ELSE t.valid_until END, t.updated_at = datetime() MERGE (m)-[:HAS_TRIPLE]->(t) MERGE (subject:EngramEntity {name: toLower(triple.subject)}) MERGE (object:EngramEntity {name: toLower(triple.object)}) MERGE (t)-[:SUBJECT]->(subject) MERGE (t)-[:OBJECT]->(object) WITH t, triple WHERE triple.relation = 'supersedes' AND triple.status = 'active' MATCH (prior:EngramTriple {memory_file: $file, subject: triple.subject, relation: triple.relation}) WHERE prior.key <> t.key AND prior.valid_until IS NULL SET prior.valid_until = datetime(), prior.status = 'superseded'", serde_json::json!({"file": file, "triples": triples.iter().filter(|triple| is_valid_relation(&triple.relation)).map(|triple| serde_json::json!({"key": format!("{}|{}|{}", triple.subject.to_lowercase(), triple.relation, triple.object.to_lowercase()), "subject": triple.subject, "relation": triple.relation, "object": triple.object, "confidence": triple.confidence, "temporal": triple.temporal, "status": if triple.confidence >= 0.7 { "active" } else { "quarantined" }})).collect::<Vec<_>>() })).await?;
        Ok(())
    }
    pub async fn mark_native_triples_current(&self, file: &str) -> Result<(), GraphError> {
        self.query("MATCH (m:EngramMemory {file: $file}) SET m.native_triple_version = 1, m.native_triples_synced_at = datetime() RETURN m.file", serde_json::json!({"file": file})).await?;
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
