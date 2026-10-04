//! Neo4j access and every Cypher statement engram issues.
//!
//! The statements live in named constants rather than inline string literals so
//! they can be asserted on in unit tests. That is not cosmetic: the obsolete-triple
//! predicate shipped as `NOT [t IN $triples | t.key] CONTAINS obsolete.key`, which
//! applies Cypher's *string* `CONTAINS` to a list, evaluates to `null`, and
//! therefore retired nothing — ever. No test could see it, because no test touched
//! a query string.

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

/// Triple lifecycle states. Only `Active` is eligible for recall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TripleStatus {
    /// Believed true now.
    Active,
    /// Below the confidence floor — stored, never recalled.
    Quarantined,
    /// Explicitly a past state. Recall must not present it as current.
    Historical,
}

impl TripleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Quarantined => "quarantined",
            Self::Historical => "historical",
        }
    }

    /// Whether this state carries a `valid_until`, i.e. is closed off in time.
    pub fn is_closed(self) -> bool {
        !matches!(self, Self::Active)
    }
}

/// Confidence below this is kept but never recalled.
pub const CONFIDENCE_FLOOR: f64 = 0.7;

/// Decide a triple's lifecycle state.
///
/// `temporal: "formerly"` used to be stored as `active` with no `valid_until`,
/// and no recall predicate ever read `temporal` — so a memory saying a service
/// *used to* run somewhere was returned as though it still did. A past state is
/// now closed off at write time, which is the only place that information exists.
pub fn triple_status(triple: &NativeTriple) -> TripleStatus {
    if triple.confidence < CONFIDENCE_FLOOR {
        return TripleStatus::Quarantined;
    }
    match triple.temporal.trim().to_lowercase().as_str() {
        "formerly" | "historical" | "past" => TripleStatus::Historical,
        _ => TripleStatus::Active,
    }
}

/// A stable identity for a triple within one memory.
pub fn triple_key(triple: &NativeTriple) -> String {
    format!(
        "{}|{}|{}",
        triple.subject.to_lowercase(),
        triple.relation,
        triple.object.to_lowercase()
    )
}

/// Which earlier claims a batch of triples retires.
///
/// For `(A, supersedes, B)`, the claim being replaced is the one about **B** — the
/// superseded object. The previous query instead looked for another triple with the
/// same `subject` and the relation `supersedes`, which is a different thing
/// entirely: it never matched the `uses`/`runs_on` claim actually being replaced,
/// so the old claim stayed active alongside the new one.
pub fn supersession_targets(triples: &[NativeTriple]) -> Vec<String> {
    let mut targets: Vec<String> = triples
        .iter()
        .filter(|triple| {
            triple.relation == "supersedes" && triple_status(triple) == TripleStatus::Active
        })
        .map(|triple| triple.object.trim().to_lowercase())
        .filter(|object| !object.is_empty())
        .collect();
    targets.sort();
    targets.dedup();
    targets
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

// ---------------------------------------------------------------------------
// Cypher. Every native statement is scoped by $slug.
//
// Native nodes were keyed on the bare filename, but filenames are only unique
// INSIDE projects/<slug>/memory. Two stores holding a `deploy-notes.md` merged
// into one node, and recall returned the other project's claims. Qdrant already
// namespaced by slug (`{slug}::{file}`); the graph did not.
// ---------------------------------------------------------------------------

const LEGACY_FACTS_FOR_TOKENS: &str = "\
UNWIND $names AS nm \
MATCH (n:Entity)-[r:RELATES_TO]-(m:Entity) \
WHERE toLower(n.name) = toLower(nm) \
RETURN r.fact AS fact LIMIT $lim";

const NATIVE_FACTS_FOR_TOKENS: &str = "\
UNWIND $names AS name \
MATCH (m:EngramMemory {slug: $slug})-[:HAS_TRIPLE]->(t:EngramTriple {status: 'active'}) \
WHERE t.valid_until IS NULL \
  AND (toLower(t.subject) CONTAINS toLower(name) OR toLower(t.object) CONTAINS toLower(name)) \
RETURN DISTINCT t.subject + ' ' + t.relation + ' ' + t.object AS fact LIMIT $lim";

const LEGACY_SEMANTIC_FILES: &str = "\
MATCH (n:Entity)-[e:RELATES_TO]->(m:Entity) \
WITH e, vector.similarity.cosine(e.fact_embedding, $vector) AS score WHERE score > 0 \
UNWIND coalesce(e.episodes, []) AS episode \
MATCH (ep:Episodic {uuid: episode}) WHERE ep.file IS NOT NULL \
RETURN ep.file AS file, collect(DISTINCT e.fact) AS facts, max(score) AS score \
ORDER BY score DESC LIMIT $limit";

const LEGACY_KEYWORD_FILES: &str = "\
CALL db.index.fulltext.queryRelationships('edge_name_and_fact', $query, {limit: $limit}) \
YIELD relationship AS rel, score \
UNWIND coalesce(rel.episodes, []) AS episode \
MATCH (ep:Episodic {uuid: episode}) WHERE ep.file IS NOT NULL \
RETURN ep.file AS file, collect(DISTINCT rel.fact) AS facts, max(score) AS score \
ORDER BY score DESC LIMIT $limit";

const NATIVE_KEYWORD_LEGACY_INDEX: &str = "\
CALL db.index.fulltext.queryNodes('engram_native_legacy_edges', $query, {limit: $limit}) \
YIELD node AS edge, score \
RETURN edge.file AS file, collect(DISTINCT edge.fact) AS facts, max(score) AS score \
ORDER BY score DESC LIMIT $limit";

const NATIVE_KEYWORD_FILES: &str = "\
MATCH (m:EngramMemory {slug: $slug}) \
WITH m, [token IN $tokens WHERE toLower(coalesce(m.name, '') + ' ' + coalesce(m.description, '') + ' ' + coalesce(m.body, '')) CONTAINS token] AS memory_matches \
WHERE size(memory_matches) > 0 \
OPTIONAL MATCH (m)-[:HAS_FACT]->(f:EngramFact) \
  WHERE f.valid_until IS NULL AND any(token IN $tokens WHERE toLower(f.text) CONTAINS token) \
WITH m, memory_matches, collect(DISTINCT f.text) AS fact_text, count(f) AS fact_score \
OPTIONAL MATCH (m)-[:HAS_TRIPLE]->(t:EngramTriple {status: 'active'}) \
  WHERE t.valid_until IS NULL AND any(token IN $tokens WHERE toLower(t.subject + ' ' + t.relation + ' ' + t.object) CONTAINS token) \
WITH m, memory_matches, fact_text, fact_score, collect(DISTINCT t.subject + ' ' + t.relation + ' ' + t.object) AS triple_text, count(t) AS triple_score \
WITH m, fact_text + triple_text AS texts, size(memory_matches) + fact_score + triple_score AS score \
RETURN m.file AS file, texts AS facts, score ORDER BY score DESC LIMIT $limit";

const NATIVE_SEMANTIC_FILES: &str = "\
MATCH (m:EngramMemory {slug: $slug})-[:HAS_FACT]->(f:EngramFact) \
WHERE f.valid_until IS NULL AND f.embedding IS NOT NULL \
WITH m, f, vector.similarity.cosine(f.embedding, $vector) AS score WHERE score > 0 \
RETURN m.file AS file, collect(DISTINCT f.text) AS facts, max(score) AS score \
ORDER BY score DESC LIMIT $limit";

const UPSERT_NATIVE_MEMORY: &str = "\
MERGE (m:EngramMemory {slug: $slug, file: $file}) \
SET m.name = $name, m.description = $description, m.body = $body, m.updated_at = datetime() \
RETURN m.file";

/// The commit marker, written only after facts, embeddings and triples are all in.
///
/// The SHA used to be stamped by the upsert at the START of a sync, so a failure
/// part-way through left the new SHA next to stale facts and the next run skipped
/// the memory as current — a transient embedding outage became a permanently
/// incomplete index.
const MARK_NATIVE_MEMORY_CURRENT: &str = "\
MATCH (m:EngramMemory {slug: $slug, file: $file}) \
SET m.sha = $sha, m.embedding_space = $space, m.native_triple_version = 2, \
    m.native_triples_synced_at = datetime() \
RETURN m.file";

const NATIVE_MEMORY_IS_CURRENT: &str = "\
MATCH (m:EngramMemory {slug: $slug, file: $file, sha: $sha, embedding_space: $space, native_triple_version: 2}) \
RETURN count(m) > 0";

const REPLACE_NATIVE_FACTS: &str = "\
MATCH (m:EngramMemory {slug: $slug, file: $file}) \
OPTIONAL MATCH (m)-[old:HAS_FACT]->(obsolete:EngramFact) WHERE NOT obsolete.text IN $facts \
SET obsolete.valid_until = datetime() DELETE old \
WITH m UNWIND $facts AS fact \
MERGE (f:EngramFact {slug: $slug, memory_file: $file, text: fact}) \
ON CREATE SET f.valid_from = datetime(), f.created_at = datetime() \
SET f.valid_until = null, f.updated_at = datetime() \
MERGE (m)-[:HAS_FACT]->(f) \
WITH f, [word IN split(toLower(f.text), ' ') WHERE size(word) >= 6] AS names \
UNWIND names AS name \
MERGE (e:EngramEntity {name: name}) MERGE (f)-[:MENTIONS]->(e)";

/// Retire triples this memory no longer asserts.
///
/// `NOT obsolete.key IN [...]` — list membership. The shipped version used the
/// string operator `CONTAINS`, which yields `null` against a list, so the `WHERE`
/// never passed and obsolete triples stayed active indefinitely. The sibling fact
/// query above always had it right.
const RETIRE_OBSOLETE_TRIPLES: &str = "\
MATCH (m:EngramMemory {slug: $slug, file: $file}) \
OPTIONAL MATCH (m)-[old:HAS_TRIPLE]->(obsolete:EngramTriple) \
WHERE NOT obsolete.key IN [triple IN $triples | triple.key] \
SET obsolete.valid_until = datetime(), obsolete.status = 'superseded' DELETE old";

const WRITE_NATIVE_TRIPLES: &str = "\
MATCH (m:EngramMemory {slug: $slug, file: $file}) \
UNWIND $triples AS triple \
MERGE (t:EngramTriple {slug: $slug, memory_file: $file, key: triple.key}) \
ON CREATE SET t.valid_from = datetime(), t.created_at = datetime() \
SET t.subject = triple.subject, t.relation = triple.relation, t.object = triple.object, \
    t.confidence = triple.confidence, t.temporal = triple.temporal, t.status = triple.status, \
    t.valid_until = CASE WHEN triple.closed THEN coalesce(t.valid_until, datetime()) ELSE null END, \
    t.updated_at = datetime() \
MERGE (m)-[:HAS_TRIPLE]->(t) \
MERGE (subject:EngramEntity {name: toLower(triple.subject)}) \
MERGE (object:EngramEntity {name: toLower(triple.object)}) \
MERGE (t)-[:SUBJECT]->(subject) MERGE (t)-[:OBJECT]->(object)";

/// Close the claims named by a `supersedes` triple's object. Targets are computed
/// in Rust ([`supersession_targets`]) instead of inferred from relation names
/// inside the query.
const APPLY_SUPERSESSION: &str = "\
MATCH (m:EngramMemory {slug: $slug, file: $file})-[:HAS_TRIPLE]->(prior:EngramTriple) \
WHERE prior.valid_until IS NULL AND prior.relation <> 'supersedes' \
  AND toLower(prior.object) IN $targets \
SET prior.valid_until = datetime(), prior.status = 'superseded'";

const SET_NATIVE_FACT_EMBEDDINGS: &str = "\
UNWIND $points AS point \
MATCH (f:EngramFact {slug: $slug, memory_file: $file, text: point.text}) \
SET f.embedding = point.embedding, f.embedding_updated_at = datetime() \
RETURN count(f)";

/// Drop graph data for memories no longer in the store.
///
/// Sync upserted current files and never retired anything, so a deleted or renamed
/// memory left its claims behind, still eligible for recall.
const PRUNE_MISSING_MEMORIES: &str = "\
MATCH (m:EngramMemory {slug: $slug}) WHERE NOT m.file IN $files \
OPTIONAL MATCH (m)-[:HAS_FACT]->(f:EngramFact) \
OPTIONAL MATCH (m)-[:HAS_TRIPLE]->(t:EngramTriple) \
DETACH DELETE f, t, m";

const NATIVE_MEMORY_FILES: &str = "\
MATCH (m:EngramMemory {slug: $slug}) RETURN collect(m.file)";

impl GraphClient {
    pub fn new(
        uri: &str,
        database: &str,
        user: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, GraphError> {
        Self::with_endpoint(uri, database, user, password, None)
    }

    /// Build a client, optionally with an explicit HTTP(S) transaction endpoint.
    ///
    /// Required for a non-loopback Neo4j: basic-auth credentials must not cross a
    /// network in plaintext, and the derived endpoint is always `http://`. The URI
    /// check used to reject remote hosts with a message promising an HTTPS endpoint
    /// that no code path accepted.
    pub fn with_endpoint(
        uri: &str,
        database: &str,
        user: impl Into<String>,
        password: impl Into<String>,
        http_url: Option<&str>,
    ) -> Result<Self, GraphError> {
        let endpoint = match http_url.map(str::trim).filter(|url| !url.is_empty()) {
            Some(url) => explicit_endpoint(url, database)?,
            None => http_endpoint(uri, database)?,
        };
        Ok(Self {
            endpoint,
            user: user.into(),
            password: password.into(),
            client: Client::new(),
        })
    }

    /// Build from the resolved config credentials.
    pub fn from_credentials(creds: &engram_config::GraphCredentials) -> Result<Self, GraphError> {
        Self::with_endpoint(
            &creds.uri,
            &creds.database,
            creds.user.clone(),
            creds.password.expose().to_string(),
            creds.http_url.as_deref(),
        )
    }

    pub async fn facts_for_tokens(
        &self,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<String>, GraphError> {
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        self.strings(
            LEGACY_FACTS_FOR_TOKENS,
            serde_json::json!({"names": tokens, "lim": limit}),
        )
        .await
    }

    pub async fn native_facts_for_tokens(
        &self,
        slug: &str,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<String>, GraphError> {
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        self.strings(
            NATIVE_FACTS_FOR_TOKENS,
            serde_json::json!({"slug": slug, "names": tokens, "lim": limit}),
        )
        .await
    }

    pub async fn semantic_files(
        &self,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<RecallHit>, GraphError> {
        self.file_hits(
            LEGACY_SEMANTIC_FILES,
            serde_json::json!({"vector": vector, "limit": limit}),
        )
        .await
    }

    pub async fn keyword_files(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RecallHit>, GraphError> {
        self.file_hits(
            LEGACY_KEYWORD_FILES,
            serde_json::json!({"query": query, "limit": limit}),
        )
        .await
    }

    pub async fn native_keyword_files(
        &self,
        slug: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RecallHit>, GraphError> {
        // The legacy-edge fulltext index is an optimisation; a MISSING index is
        // fine and we fall through, but a real failure (auth, network) must not be
        // mistaken for "no hits" — it is propagated.
        match self
            .file_hits(
                NATIVE_KEYWORD_LEGACY_INDEX,
                serde_json::json!({"query": query, "limit": limit}),
            )
            .await
        {
            Ok(hits) if !hits.is_empty() => return Ok(hits),
            Ok(_) => {}
            Err(GraphError::Database(_)) => {}
            Err(error) => return Err(error),
        }
        let tokens = query
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| word.len() >= 4)
            .map(|word| word.to_lowercase())
            .collect::<Vec<_>>();
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        self.file_hits(
            NATIVE_KEYWORD_FILES,
            serde_json::json!({"slug": slug, "tokens": tokens, "limit": limit}),
        )
        .await
    }

    pub async fn native_semantic_files(
        &self,
        slug: &str,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<RecallHit>, GraphError> {
        self.file_hits(
            NATIVE_SEMANTIC_FILES,
            serde_json::json!({"slug": slug, "vector": vector, "limit": limit}),
        )
        .await
    }

    pub async fn upsert_native_memory(
        &self,
        slug: &str,
        file: &str,
        name: &str,
        description: &str,
        body: &str,
    ) -> Result<(), GraphError> {
        self.query(
            UPSERT_NATIVE_MEMORY,
            serde_json::json!({"slug": slug, "file": file, "name": name, "description": description, "body": body}),
        )
        .await?;
        Ok(())
    }

    pub async fn native_memory_is_current(
        &self,
        slug: &str,
        file: &str,
        sha: &str,
        space: &str,
    ) -> Result<bool, GraphError> {
        let response = self
            .query(
                NATIVE_MEMORY_IS_CURRENT,
                serde_json::json!({"slug": slug, "file": file, "sha": sha, "space": space}),
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
        slug: &str,
        file: &str,
        facts: &[String],
    ) -> Result<(), GraphError> {
        self.query(
            REPLACE_NATIVE_FACTS,
            serde_json::json!({"slug": slug, "file": file, "facts": facts}),
        )
        .await?;
        Ok(())
    }

    pub async fn replace_native_triples(
        &self,
        slug: &str,
        file: &str,
        triples: &[NativeTriple],
    ) -> Result<(), GraphError> {
        let payload = triple_payload(triples);
        self.query(
            RETIRE_OBSOLETE_TRIPLES,
            serde_json::json!({"slug": slug, "file": file, "triples": payload}),
        )
        .await?;
        if payload.is_empty() {
            return Ok(());
        }
        self.query(
            WRITE_NATIVE_TRIPLES,
            serde_json::json!({"slug": slug, "file": file, "triples": payload}),
        )
        .await?;
        let targets = supersession_targets(triples);
        if !targets.is_empty() {
            self.query(
                APPLY_SUPERSESSION,
                serde_json::json!({"slug": slug, "file": file, "targets": targets}),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn set_native_fact_embeddings(
        &self,
        slug: &str,
        file: &str,
        points: &[(&str, Vec<f32>)],
    ) -> Result<(), GraphError> {
        if points.is_empty() {
            return Ok(());
        }
        self.query(
            SET_NATIVE_FACT_EMBEDDINGS,
            serde_json::json!({"slug": slug, "file": file, "points": points.iter().map(|(text, embedding)| serde_json::json!({"text": text, "embedding": embedding})).collect::<Vec<_>>() }),
        )
        .await?;
        Ok(())
    }

    /// Record that `file` is fully synced. Must be the LAST write of a sync.
    pub async fn mark_native_memory_current(
        &self,
        slug: &str,
        file: &str,
        sha: &str,
        space: &str,
    ) -> Result<(), GraphError> {
        self.query(
            MARK_NATIVE_MEMORY_CURRENT,
            serde_json::json!({"slug": slug, "file": file, "sha": sha, "space": space}),
        )
        .await?;
        Ok(())
    }

    /// Every memory file the graph holds for a slug.
    pub async fn native_memory_files(&self, slug: &str) -> Result<Vec<String>, GraphError> {
        let response = self
            .query(NATIVE_MEMORY_FILES, serde_json::json!({"slug": slug}))
            .await?;
        Ok(response
            .results
            .into_iter()
            .flat_map(|set| set.data)
            .next()
            .and_then(|row| row.row.into_iter().next())
            .and_then(|value| value.as_array().cloned())
            .map(|files| {
                files
                    .into_iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Remove graph data for memories that are no longer in the store.
    pub async fn prune_missing_memories(
        &self,
        slug: &str,
        files: &[String],
    ) -> Result<(), GraphError> {
        self.query(
            PRUNE_MISSING_MEMORIES,
            serde_json::json!({"slug": slug, "files": files}),
        )
        .await?;
        Ok(())
    }

    pub async fn import_legacy_fact_embeddings(&self, slug: &str) -> Result<(), GraphError> {
        self.query("CREATE FULLTEXT INDEX engram_native_legacy_edges IF NOT EXISTS FOR (e:EngramLegacyEdge) ON EACH [e.name, e.fact]", serde_json::json!({})).await?;
        self.query("MATCH (:Entity)-[r:RELATES_TO]->(:Entity) UNWIND coalesce(r.episodes, []) AS episode MATCH (ep:Episodic {uuid: episode}) WHERE ep.file IS NOT NULL MERGE (edge:EngramLegacyEdge {key: elementId(r) + '|' + episode}) SET edge.file = ep.file, edge.name = r.name, edge.fact = r.fact, edge.embedding = r.fact_embedding, edge.updated_at = datetime() RETURN count(edge)", serde_json::json!({})).await?;
        self.query("CREATE FULLTEXT INDEX engram_native_fact_text IF NOT EXISTS FOR (f:EngramFact) ON EACH [f.text, f.legacy_name]", serde_json::json!({})).await?;
        // Scoped by slug like every other native write: an unscoped import
        // attaches another project's legacy facts to this store's memories.
        self.query("MATCH (:Entity)-[r:RELATES_TO]->(:Entity) UNWIND coalesce(r.episodes, []) AS episode MATCH (ep:Episodic {uuid: episode}) WHERE ep.file IS NOT NULL MATCH (m:EngramMemory {slug: $slug, file: ep.file}) WHERE r.fact IS NOT NULL AND r.fact_embedding IS NOT NULL MERGE (f:EngramFact {slug: $slug, memory_file: m.file, text: r.fact}) ON CREATE SET f.created_at = datetime(), f.valid_from = datetime() SET f.legacy_name = r.name, f.embedding = r.fact_embedding, f.embedding_updated_at = datetime(), f.valid_until = null, f.updated_at = datetime() MERGE (m)-[:HAS_FACT]->(f) RETURN count(DISTINCT f)", serde_json::json!({"slug": slug})).await?;
        Ok(())
    }

    async fn strings(
        &self,
        statement: &str,
        parameters: serde_json::Value,
    ) -> Result<Vec<String>, GraphError> {
        let response = self.query(statement, parameters).await?;
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

/// Serialize triples for Cypher, dropping relations outside the taxonomy and
/// attaching the status decided by [`triple_status`].
fn triple_payload(triples: &[NativeTriple]) -> Vec<serde_json::Value> {
    triples
        .iter()
        .filter(|triple| is_valid_relation(&triple.relation))
        .map(|triple| {
            let status = triple_status(triple);
            serde_json::json!({
                "key": triple_key(triple),
                "subject": triple.subject,
                "relation": triple.relation,
                "object": triple.object,
                "confidence": triple.confidence,
                "temporal": triple.temporal,
                "status": status.as_str(),
                "closed": status.is_closed(),
            })
        })
        .collect()
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
        return Err(GraphError::Uri(format!(
            "remote Neo4j ({host}) requires an explicit HTTPS endpoint — set graph.neo4j_http_url"
        )));
    }
    Ok(format!("http://{host}:7474/db/{database}/tx/commit"))
}

/// Validate an operator-supplied transaction endpoint.
///
/// Plain `http` is allowed only for loopback, matching the policy already enforced
/// on the Python side (`memory_recall._neo4j_base_url`): basic-auth credentials
/// must not travel unencrypted.
pub fn explicit_endpoint(url: &str, database: &str) -> Result<String, GraphError> {
    let trimmed = url.trim().trim_end_matches('/');
    let (scheme, rest) = trimmed
        .split_once("://")
        .ok_or_else(|| GraphError::Uri("neo4j_http_url must be an absolute HTTP(S) URL".into()))?;
    let host = rest
        .split('/')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    let loopback = matches!(host, "127.0.0.1" | "localhost" | "::1");
    match scheme {
        "https" => {}
        "http" if loopback => {}
        "http" => {
            return Err(GraphError::Uri(format!(
                "refusing to send Neo4j credentials to {host} over plaintext HTTP; use https://"
            )));
        }
        other => return Err(GraphError::Uri(format!("unsupported scheme {other}"))),
    }
    // Accept either a bare base URL or a full transaction endpoint.
    if trimmed.contains("/tx/commit") {
        return Ok(trimmed.to_string());
    }
    Ok(format!("{trimmed}/db/{database}/tx/commit"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triple(
        subject: &str,
        relation: &str,
        object: &str,
        confidence: f64,
        temporal: &str,
    ) -> NativeTriple {
        NativeTriple {
            subject: subject.into(),
            relation: relation.into(),
            object: object.into(),
            confidence,
            temporal: temporal.into(),
        }
    }

    #[test]
    fn loopback_only_without_an_explicit_endpoint() {
        assert_eq!(
            http_endpoint("bolt://127.0.0.1:7687", "neo4j").unwrap(),
            "http://127.0.0.1:7474/db/neo4j/tx/commit"
        );
        let error = http_endpoint("bolt://10.0.0.9:7687", "neo4j").unwrap_err();
        assert!(
            error.to_string().contains("graph.neo4j_http_url"),
            "the error must name the setting that fixes it: {error}"
        );
    }

    /// The remote path the old error message promised but never implemented.
    #[test]
    fn an_explicit_https_endpoint_enables_remote_neo4j() {
        assert_eq!(
            explicit_endpoint("https://neo.example:7473", "memories").unwrap(),
            "https://neo.example:7473/db/memories/tx/commit"
        );
        // a full endpoint is passed through
        assert_eq!(
            explicit_endpoint("https://neo.example:7473/db/x/tx/commit", "ignored").unwrap(),
            "https://neo.example:7473/db/x/tx/commit"
        );
        // loopback may stay plaintext; a remote host may not
        assert!(explicit_endpoint("http://127.0.0.1:7474", "neo4j").is_ok());
        assert!(explicit_endpoint("http://neo.example:7474", "neo4j").is_err());
        assert!(explicit_endpoint("neo.example", "neo4j").is_err());
    }

    /// The release blocker: `CONTAINS` against a list is `null`, so nothing was
    /// ever retired. Assert the membership form directly.
    #[test]
    fn obsolete_triples_use_list_membership_not_string_contains() {
        assert!(
            RETIRE_OBSOLETE_TRIPLES
                .contains("NOT obsolete.key IN [triple IN $triples | triple.key]"),
            "{RETIRE_OBSOLETE_TRIPLES}"
        );
        assert!(
            !RETIRE_OBSOLETE_TRIPLES.contains("CONTAINS obsolete.key"),
            "the string operator is back: {RETIRE_OBSOLETE_TRIPLES}"
        );
    }

    /// Filenames are unique only within a store, so every native statement that
    /// reads or writes memory-scoped data must be slug-scoped.
    #[test]
    fn every_native_statement_is_scoped_by_slug() {
        for (name, statement) in [
            ("NATIVE_FACTS_FOR_TOKENS", NATIVE_FACTS_FOR_TOKENS),
            ("NATIVE_KEYWORD_FILES", NATIVE_KEYWORD_FILES),
            ("NATIVE_SEMANTIC_FILES", NATIVE_SEMANTIC_FILES),
            ("UPSERT_NATIVE_MEMORY", UPSERT_NATIVE_MEMORY),
            ("MARK_NATIVE_MEMORY_CURRENT", MARK_NATIVE_MEMORY_CURRENT),
            ("NATIVE_MEMORY_IS_CURRENT", NATIVE_MEMORY_IS_CURRENT),
            ("REPLACE_NATIVE_FACTS", REPLACE_NATIVE_FACTS),
            ("RETIRE_OBSOLETE_TRIPLES", RETIRE_OBSOLETE_TRIPLES),
            ("WRITE_NATIVE_TRIPLES", WRITE_NATIVE_TRIPLES),
            ("APPLY_SUPERSESSION", APPLY_SUPERSESSION),
            ("SET_NATIVE_FACT_EMBEDDINGS", SET_NATIVE_FACT_EMBEDDINGS),
            ("PRUNE_MISSING_MEMORIES", PRUNE_MISSING_MEMORIES),
            ("NATIVE_MEMORY_FILES", NATIVE_MEMORY_FILES),
        ] {
            assert!(statement.contains("$slug"), "{name} is not slug-scoped");
        }
    }

    /// Recall must only surface claims that are both active and still open.
    #[test]
    fn recall_predicates_exclude_closed_and_inactive_triples() {
        for statement in [NATIVE_FACTS_FOR_TOKENS, NATIVE_KEYWORD_FILES] {
            assert!(statement.contains("status: 'active'"), "{statement}");
            assert!(statement.contains("valid_until IS NULL"), "{statement}");
        }
        assert!(NATIVE_SEMANTIC_FILES.contains("f.valid_until IS NULL"));
    }

    #[test]
    fn a_past_state_is_never_stored_as_active() {
        // the exact case from the extractor: "X was previously on Y"
        let formerly = triple("api", "runs_on", "host-a", 0.9, "formerly");
        assert_eq!(triple_status(&formerly), TripleStatus::Historical);
        assert!(triple_status(&formerly).is_closed());

        let current = triple("api", "runs_on", "host-b", 0.9, "current");
        assert_eq!(triple_status(&current), TripleStatus::Active);
        assert!(!triple_status(&current).is_closed());

        // low confidence is quarantined whatever its tense, and is also closed
        let unsure = triple("api", "runs_on", "host-c", 0.4, "current");
        assert_eq!(triple_status(&unsure), TripleStatus::Quarantined);
        assert!(triple_status(&unsure).is_closed());
    }

    #[test]
    fn the_payload_carries_the_computed_status_and_drops_unknown_relations() {
        let triples = vec![
            triple("api", "runs_on", "host-b", 0.9, "current"),
            triple("api", "runs_on", "host-a", 0.9, "formerly"),
            triple("api", "guesses", "host-z", 0.9, "current"),
        ];
        let payload = triple_payload(&triples);
        assert_eq!(payload.len(), 2, "an off-taxonomy relation was kept");
        assert_eq!(payload[0]["status"], "active");
        assert_eq!(payload[0]["closed"], false);
        assert_eq!(payload[1]["status"], "historical");
        assert_eq!(payload[1]["closed"], true);
        assert_eq!(payload[0]["key"], "api|runs_on|host-b");
    }

    /// `(A, supersedes, B)` retires the claim about **B**. Keying off
    /// `subject + relation = supersedes` (the old behaviour) never matched the
    /// `uses`/`runs_on` claim actually being replaced.
    #[test]
    fn supersession_targets_the_superseded_object() {
        let triples = vec![
            triple("postgres-16", "supersedes", "Postgres-14", 0.9, "current"),
            triple("api", "uses", "postgres-16", 0.9, "current"),
        ];
        assert_eq!(supersession_targets(&triples), vec!["postgres-14"]);

        // a quarantined supersedes claim must not retire anything
        let unsure = vec![triple("a", "supersedes", "b", 0.3, "current")];
        assert!(supersession_targets(&unsure).is_empty());

        // no supersedes triple, no targets
        assert!(supersession_targets(&[triple("a", "uses", "b", 0.9, "current")]).is_empty());

        // and the query retires by object, never by relation name
        assert!(APPLY_SUPERSESSION.contains("toLower(prior.object) IN $targets"));
        assert!(
            !APPLY_SUPERSESSION.contains("relation = 'supersedes'"),
            "supersession is still inferred inside the query"
        );
    }

    #[test]
    fn supersession_does_not_retire_the_superseding_claim_itself() {
        assert!(APPLY_SUPERSESSION.contains("prior.relation <> 'supersedes'"));
    }
}
