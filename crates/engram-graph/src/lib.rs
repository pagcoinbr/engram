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

/// A fact read out of the legacy graph for the bootstrap.
pub struct LegacyFact {
    pub file: String,
    pub text: String,
    pub subject: String,
    pub object: String,
    pub legacy_name: String,
    pub embedding: Option<Vec<f32>>,
    /// Graphiti's `invalid_at` (or `expired_at`): when a newer fact superseded
    /// this one. Graphiti never deletes a contradicted fact, it stamps it.
    pub valid_until: Option<String>,
}

/// A fact ready to be written by the bootstrap: redacted, with a vector of the
/// text actually stored.
#[derive(serde::Serialize)]
pub struct BootstrapFact {
    pub text: String,
    pub subject: String,
    pub object: String,
    pub legacy_name: String,
    pub embedding: Option<Vec<f32>>,
    pub redacted: bool,
    /// Set for a superseded legacy fact, which is imported as history: every
    /// native recall query requires `valid_until IS NULL`.
    pub valid_until: Option<String>,
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
  AND r.invalid_at IS NULL AND r.expired_at IS NULL \
RETURN r.fact AS fact LIMIT $lim";

/// Triples whose subject/object mention a token, plus facts whose ENTITY
/// endpoints equal one. The second branch serves facts imported from the legacy
/// graph: they carry no triples (753 free-form Graphiti relation names do not map
/// onto the taxonomy without a model), only the entity names of the edge they came
/// from, so it matches them exactly the way the legacy query matched `Entity.name`
/// (equality, so filler words never hit). Without it a bootstrapped store recalls
/// zero facts on the hook's fast path.
const NATIVE_FACTS_FOR_TOKENS: &str = "\
UNWIND $names AS name \
CALL { \
  WITH name \
  MATCH (m:EngramMemory {slug: $slug})-[:HAS_TRIPLE]->(t:EngramTriple {status: 'active'}) \
  WHERE t.valid_until IS NULL \
    AND (toLower(t.subject) CONTAINS toLower(name) OR toLower(t.object) CONTAINS toLower(name)) \
  RETURN t.subject + ' ' + t.relation + ' ' + t.object AS fact \
  UNION \
  WITH name \
  MATCH (m:EngramMemory {slug: $slug})-[:HAS_FACT]->(f:EngramFact) \
  WHERE f.valid_until IS NULL \
    AND (toLower(f.subject) = toLower(name) OR toLower(f.object) = toLower(name)) \
  RETURN f.text AS fact \
} \
RETURN DISTINCT fact LIMIT $lim";

const LEGACY_SEMANTIC_FILES: &str = "\
MATCH (n:Entity)-[e:RELATES_TO]->(m:Entity) \
WHERE e.invalid_at IS NULL AND e.expired_at IS NULL \
WITH e, vector.similarity.cosine(e.fact_embedding, $vector) AS score WHERE score > 0 \
UNWIND coalesce(e.episodes, []) AS episode \
MATCH (ep:Episodic {uuid: episode}) WHERE ep.file IS NOT NULL \
RETURN ep.file AS file, collect(DISTINCT e.fact) AS facts, max(score) AS score \
ORDER BY score DESC LIMIT $limit";

const LEGACY_KEYWORD_FILES: &str = "\
CALL db.index.fulltext.queryRelationships('edge_name_and_fact', $query, {limit: $limit}) \
YIELD relationship AS rel, score \
WITH rel, score WHERE rel.invalid_at IS NULL AND rel.expired_at IS NULL \
UNWIND coalesce(rel.episodes, []) AS episode \
MATCH (ep:Episodic {uuid: episode}) WHERE ep.file IS NOT NULL \
RETURN ep.file AS file, collect(DISTINCT rel.fact) AS facts, max(score) AS score \
ORDER BY score DESC LIMIT $limit";

// Legacy import. Schema DDL carries no slug; the data statement must.

/// Every legacy episode for these files with the verbatim markdown Graphiti
/// ingested (`Episodic.source_md`). A file can have several episodes (older
/// ingested versions); the caller picks the one whose bytes match the file.
const LEGACY_SOURCES: &str = "\
MATCH (e:Episodic) WHERE e.file IN $files AND e.source_md IS NOT NULL \
RETURN e.file, e.uuid, e.source_md";

/// Files the bootstrap must not touch: already current, or carrying facts from
/// somewhere else (a failed extracting sync leaves facts without a marker). A node
/// an interrupted bootstrap left behind has neither, so a retry resumes it.
const NATIVE_FILES: &str = "\
MATCH (m:EngramMemory {slug: $slug}) \
WHERE m.sha IS NOT NULL OR (m)-[:HAS_FACT]->() \
RETURN m.file";

/// The facts of exactly these legacy episodes, read OUT of the legacy graph so
/// they can be redacted and re-embedded before anything native is written. An
/// older ingested version of a file is a different episode and never matches.
const LEGACY_VERIFIED_FACTS: &str = "\
MATCH (source:Entity)-[r:RELATES_TO]->(target:Entity) \
WHERE r.fact IS NOT NULL \
UNWIND coalesce(r.episodes, []) AS episode \
WITH source, r, target, episode WHERE episode IN $episodes \
MATCH (ep:Episodic {uuid: episode}) \
RETURN DISTINCT ep.file, r.fact, coalesce(source.name, ''), coalesce(target.name, ''), \
       coalesce(r.name, ''), r.fact_embedding, \
       toString(coalesce(r.invalid_at, r.expired_at))";

/// One memory's bootstrap, as ONE statement and therefore one transaction: its
/// already-redacted facts and its commit marker land together or not at all. An
/// interruption leaves a node with no facts and no marker, which NATIVE_FILES
/// lets the next run resume.
const COMMIT_BOOTSTRAPPED_MEMORY: &str = "\
MATCH (m:EngramMemory {slug: $slug, file: $file}) \
CALL { \
  WITH m \
  UNWIND $facts AS row \
  MERGE (f:EngramFact {slug: $slug, memory_file: m.file, text: row.text}) \
  ON CREATE SET f.created_at = datetime(), f.valid_from = datetime() \
  SET f.legacy_name = row.legacy_name, f.subject = row.subject, f.object = row.object, \
      f.embedding = row.embedding, f.embedding_updated_at = datetime(), \
      f.redacted_at = CASE WHEN row.redacted THEN datetime() ELSE null END, \
      f.valid_until = CASE WHEN row.valid_until IS NULL THEN null \
                           ELSE datetime(row.valid_until) END, \
      f.updated_at = datetime() \
  MERGE (m)-[:HAS_FACT]->(f) \
  RETURN count(f) AS written \
} \
SET m.sha = $sha, m.embedding_space = $space, m.native_triple_version = 2, \
    m.native_triples_synced_at = datetime() \
RETURN written";

const CREATE_NATIVE_FACT_INDEX: &str = "\
CREATE FULLTEXT INDEX engram_native_fact_text IF NOT EXISTS \
FOR (f:EngramFact) ON EACH [f.text, f.legacy_name]";

/// Remove the abandoned `EngramLegacyEdge` cache.
///
/// Installs that ran the old import carry unscoped edge nodes. They are filtered
/// out of every current query, but a fulltext index ranks across all of them, so
/// leaving them in place means permanent index bloat and crowd-out. Dropped in
/// batches so a large legacy graph does not build one enormous transaction.
const DROP_LEGACY_EDGE_CACHE: &str = "\
MATCH (edge:EngramLegacyEdge) WITH edge LIMIT 10000 DETACH DELETE edge RETURN count(edge)";

/// Scoped by slug like every other native write: an unscoped import attaches
/// another project's legacy facts to this store's memories.
///
/// Embedding-optional. It used to require `r.fact_embedding IS NOT NULL`, which
/// silently dropped every legacy fact Graphiti had not embedded — facts the old
/// keyword cache *did* carry, so removing that cache without relaxing this would
/// have lost them for good. An unembedded fact simply sits out the semantic leg
/// (which requires `f.embedding IS NOT NULL`) and is still found by keyword.
const IMPORT_NATIVE_LEGACY_FACTS: &str = "\
MATCH (source:Entity)-[r:RELATES_TO]->(target:Entity) \
UNWIND coalesce(r.episodes, []) AS episode \
MATCH (ep:Episodic {uuid: episode}) WHERE ep.file IS NOT NULL \
MATCH (m:EngramMemory {slug: $slug, file: ep.file}) \
WHERE r.fact IS NOT NULL \
MERGE (f:EngramFact {slug: $slug, memory_file: m.file, text: r.fact}) \
ON CREATE SET f.created_at = datetime(), f.valid_from = datetime() \
SET f.legacy_name = r.name, f.subject = source.name, f.object = target.name, \
    f.embedding = coalesce(r.fact_embedding, f.embedding), \
    f.embedding_updated_at = CASE WHEN r.fact_embedding IS NULL THEN f.embedding_updated_at \
                                  ELSE datetime() END, \
    f.valid_until = null, f.updated_at = datetime() \
MERGE (m)-[:HAS_FACT]->(f) \
RETURN count(DISTINCT f)";

/// Keyword recall over a memory's own text, its facts, and its active triples.
///
/// A match in ANY of the three qualifies the memory. The memory-text match used
/// to be a hard pre-filter, so a memory whose only match was in an extracted fact
/// or triple — exactly what the graph exists to add over plain text search — was
/// discarded before its facts were ever examined. `legacy_name` is searched too,
/// since imported Graphiti facts carry their relation name there.
const NATIVE_KEYWORD_FILES: &str = "\
MATCH (m:EngramMemory {slug: $slug}) \
WITH m, [token IN $tokens WHERE toLower(coalesce(m.name, '') + ' ' + coalesce(m.description, '') + ' ' + coalesce(m.body, '')) CONTAINS token] AS memory_matches \
OPTIONAL MATCH (m)-[:HAS_FACT]->(f:EngramFact) \
  WHERE f.valid_until IS NULL AND any(token IN $tokens \
    WHERE toLower(coalesce(f.text, '') + ' ' + coalesce(f.legacy_name, '')) CONTAINS token) \
WITH m, memory_matches, collect(DISTINCT f.text) AS fact_text, count(f) AS fact_score \
OPTIONAL MATCH (m)-[:HAS_TRIPLE]->(t:EngramTriple {status: 'active'}) \
  WHERE t.valid_until IS NULL AND any(token IN $tokens WHERE toLower(t.subject + ' ' + t.relation + ' ' + t.object) CONTAINS token) \
WITH m, memory_matches, fact_text, fact_score, collect(DISTINCT t.subject + ' ' + t.relation + ' ' + t.object) AS triple_text, count(t) AS triple_score \
WITH m, fact_text + triple_text AS texts, size(memory_matches) + fact_score + triple_score AS score \
WHERE score > 0 \
RETURN m.file AS file, texts AS facts, score ORDER BY score DESC LIMIT $limit";

const NATIVE_SEMANTIC_FILES: &str = "\
MATCH (m:EngramMemory {slug: $slug})-[:HAS_FACT]->(f:EngramFact) \
WHERE f.valid_until IS NULL AND f.embedding IS NOT NULL \
WITH m, f, vector.similarity.cosine(f.embedding, $vector) AS score WHERE score > 0 \
RETURN m.file AS file, collect(DISTINCT f.text) AS facts, max(score) AS score \
ORDER BY score DESC LIMIT $limit";

/// Open a sync generation: write the new content and *clear the commit marker*.
///
/// Clearing is the point. Moving the stamp to the end of the sync was not enough
/// on its own — an interrupted run left the new body and new facts sitting beside
/// the PREVIOUS generation's `sha`, so if the file was later reverted to that
/// prior content the freshness check matched and the half-written generation was
/// skipped forever. With the marker nulled first, any interruption leaves the
/// memory unstamped and therefore retryable, whatever the file does afterwards.
const UPSERT_NATIVE_MEMORY: &str = "\
MERGE (m:EngramMemory {slug: $slug, file: $file}) \
SET m.name = $name, m.description = $description, m.body = $body, \
    m.source_mtime = $source_mtime, m.updated_at = datetime(), \
    m.sha = null, m.embedding_space = null, \
    m.native_triple_version = null, m.native_triples_synced_at = null \
RETURN m.file";

/// Backfill the supersession ordering key for the whole store, in one statement.
///
/// `source_mtime` is intentionally absent from the freshness hash — a `touch` must
/// not cost a full re-extraction — which means the upsert that writes it only runs
/// when content changed. Every node predating the field therefore stayed unset,
/// and supersession compared against `coalesce(..., 0)`.
///
/// It has to cover the WHOLE store before any memory is synced, not each memory as
/// the loop reaches it. Memories are processed in filename order, so a per-memory
/// backfill still left later-sorting nodes unset while an earlier superseding
/// memory was applying supersession against them — reading their mtime as zero and
/// retiring claims that were in fact newer. One batched write beforehand removes
/// the ordering dependence entirely. The guard makes it a no-op once values match.
const SET_NATIVE_MEMORY_MTIMES: &str = "\
UNWIND $memories AS entry \
MATCH (m:EngramMemory {slug: $slug, file: entry.file}) \
WHERE coalesce(m.source_mtime, 0) <> entry.source_mtime \
SET m.source_mtime = entry.source_mtime \
RETURN count(m)";

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
///
/// Reaches across the store, but only to other memories and only to claims the
/// operator wrote *earlier*.
///
/// Three attempts, because both obvious bounds are wrong:
///
/// - Restricting to `$file` missed the common case entirely. A memory recording
///   "Postgres supersedes SQLite" almost never lives in the same file as the claim
///   it replaces.
/// - Removing the bound let an extraction retire its own siblings — triples are
///   written immediately before this runs — hence `pm.file <> $file`.
/// - Ordering by the node timestamps (`valid_from`, `created_at`, `updated_at`)
///   looks like a fix and is not: those are *ingestion* times. Whichever memory
///   the sync happened to process first wins, so on a first full sync a
///   supersession would silently fail whenever its memory sorted ahead of the
///   claim it supersedes — turning a wrong result into an unpredictable one.
///
/// So the comparison is on `source_mtime`, the source file's own mtime: the
/// operator's ordering of events, independent of our sync order and stable across
/// re-indexing.
///
/// mtime is whole seconds, so ties are common — two memories saved in one editor
/// pass share a timestamp. A plain `<=` made ties *symmetric*: two memories with
/// opposing supersessions could each retire the other's claims, so a single second
/// of coincidence could retire both sides of a pair. The filename breaks the tie,
/// which is arbitrary but gives a total order, so at most one direction ever
/// applies and the outcome does not depend on sync order.
const APPLY_SUPERSESSION: &str = "\
MATCH (sm:EngramMemory {slug: $slug, file: $file})-[:HAS_TRIPLE]->(sup:EngramTriple) \
WHERE sup.relation = 'supersedes' AND toLower(sup.object) IN $targets \
MATCH (pm:EngramMemory {slug: $slug})-[:HAS_TRIPLE]->(prior:EngramTriple) \
WHERE prior.valid_until IS NULL AND prior.relation <> 'supersedes' \
  AND toLower(prior.object) = toLower(sup.object) \
  AND pm.file <> $file \
  AND (coalesce(pm.source_mtime, 0) < coalesce(sm.source_mtime, 0) \
       OR (coalesce(pm.source_mtime, 0) = coalesce(sm.source_mtime, 0) \
           AND pm.file < $file)) \
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
        // There used to be a legacy-edge fulltext "fast path" ahead of this, and
        // it was removed rather than repaired. A Neo4j fulltext index cannot be
        // partitioned, so `queryNodes` ranked across every store and applied its
        // limit BEFORE any slug filter: other projects' edges occupied the top
        // positions, this store's hits were crowded out, and because a single
        // surviving hit short-circuited the scoped query below, recall silently
        // returned an under-filled result instead of the correct one. Over-fetching
        // to compensate made the leg several times more expensive without making it
        // correct.
        //
        // Legacy facts are not lost with it — they are imported as slug-scoped
        // `EngramFact` nodes (see `import_legacy_fact_embeddings`, which no longer
        // requires an embedding) and the query below searches facts, triples and
        // `legacy_name`. Two differences remain, both deliberate: this is
        // substring matching rather than Lucene ranking, and it ignores tokens
        // shorter than four characters. An install that has not re-run the import
        // will see fewer legacy facts until it does.
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
        source_mtime: i64,
    ) -> Result<(), GraphError> {
        self.query(
            UPSERT_NATIVE_MEMORY,
            serde_json::json!({
                "slug": slug,
                "file": file,
                "name": name,
                "description": description,
                "body": body,
                "source_mtime": source_mtime,
            }),
        )
        .await?;
        Ok(())
    }

    /// See [`SET_NATIVE_MEMORY_MTIMES`]: brings the supersession ordering key up
    /// to date for the whole store. Call once, before syncing any memory.
    pub async fn set_native_memory_mtimes(
        &self,
        slug: &str,
        memories: &[(String, i64)],
    ) -> Result<(), GraphError> {
        if memories.is_empty() {
            return Ok(());
        }
        let entries = memories
            .iter()
            .map(|(file, source_mtime)| serde_json::json!({"file": file, "source_mtime": source_mtime}))
            .collect::<Vec<_>>();
        self.query(
            SET_NATIVE_MEMORY_MTIMES,
            serde_json::json!({"slug": slug, "memories": entries}),
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

    /// `(file, episode uuid, source_md)` for every legacy episode of these files.
    pub async fn legacy_sources(
        &self,
        files: &[String],
    ) -> Result<Vec<(String, String, String)>, GraphError> {
        let response = self
            .query(LEGACY_SOURCES, serde_json::json!({"files": files}))
            .await?;
        Ok(response
            .results
            .into_iter()
            .flat_map(|set| set.data)
            .filter_map(|row| {
                let text = |i: usize| Some(row.row.get(i)?.as_str()?.to_string());
                Some((text(0)?, text(1)?, text(2)?))
            })
            .collect())
    }

    /// Files that already have a native node in this store.
    pub async fn native_files(&self, slug: &str) -> Result<Vec<String>, GraphError> {
        self.strings(NATIVE_FILES, serde_json::json!({"slug": slug}))
            .await
    }

    /// The facts of exactly these legacy episodes (see LEGACY_VERIFIED_FACTS).
    pub async fn legacy_verified_facts(
        &self,
        episodes: &[String],
    ) -> Result<Vec<LegacyFact>, GraphError> {
        let response = self
            .query(
                LEGACY_VERIFIED_FACTS,
                serde_json::json!({"episodes": episodes}),
            )
            .await?;
        Ok(response
            .results
            .into_iter()
            .flat_map(|set| set.data)
            .filter_map(|row| {
                let text = |i: usize| Some(row.row.get(i)?.as_str()?.to_string());
                let embedding = row.row.get(5).and_then(|value| {
                    value
                        .as_array()?
                        .iter()
                        .map(|x| x.as_f64().map(|x| x as f32))
                        .collect::<Option<Vec<f32>>>()
                });
                Some(LegacyFact {
                    file: text(0)?,
                    text: text(1)?,
                    subject: text(2)?,
                    object: text(3)?,
                    legacy_name: text(4)?,
                    embedding,
                    valid_until: text(6),
                })
            })
            .collect())
    }

    /// Write one memory's (already redacted) facts and its commit marker in a
    /// single transaction. See COMMIT_BOOTSTRAPPED_MEMORY.
    pub async fn commit_bootstrapped_memory(
        &self,
        slug: &str,
        file: &str,
        facts: &[BootstrapFact],
        sha: &str,
        space: &str,
    ) -> Result<(), GraphError> {
        self.query(CREATE_NATIVE_FACT_INDEX, serde_json::json!({}))
            .await?;
        self.query(
            COMMIT_BOOTSTRAPPED_MEMORY,
            serde_json::json!({"slug": slug, "file": file, "facts": facts,
                "sha": sha, "space": space}),
        )
        .await?;
        Ok(())
    }

    /// Import legacy Graphiti facts as slug-scoped native facts, and clear out
    /// the abandoned edge cache the previous version of this import created.
    pub async fn import_legacy_fact_embeddings(&self, slug: &str) -> Result<(), GraphError> {
        for statement in [CREATE_NATIVE_FACT_INDEX, IMPORT_NATIVE_LEGACY_FACTS] {
            self.query(statement, serde_json::json!({"slug": slug}))
                .await?;
        }
        // Batched, so a large legacy graph is cleaned without one transaction big
        // enough to fail — and capped, because "loop until a batch deletes
        // nothing" is only bounded if nothing is writing. An old importer still
        // running elsewhere recreates these nodes, and an unbounded loop would
        // then chase it forever inside a sync. Whatever is left is collected by
        // the next run; this is cleanup, not a correctness requirement.
        const MAX_BATCHES: usize = 100;
        for _ in 0..MAX_BATCHES {
            let response = self
                .query(DROP_LEGACY_EDGE_CACHE, serde_json::json!({}))
                .await?;
            // A response we cannot read is treated as "stop", not "done": it may
            // mean rows remain, so it must not be mistaken for a clean finish.
            let Some(deleted) = response
                .results
                .iter()
                .flat_map(|set| set.data.iter())
                .filter_map(|row| row.row.first().and_then(serde_json::Value::as_u64))
                .next()
            else {
                break;
            };
            if deleted == 0 {
                break;
            }
        }
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

    /// Opening a generation must CLEAR the commit marker.
    ///
    /// Moving the stamp to the end of the sync was not sufficient on its own: an
    /// interrupted run left new content and new facts beside the previous
    /// generation's `sha`, so reverting the file to that prior content made the
    /// freshness check match and the half-written generation was skipped forever.
    #[test]
    fn opening_a_generation_invalidates_the_previous_marker() {
        for field in [
            "m.sha = null",
            "m.embedding_space = null",
            "m.native_triple_version = null",
            "m.native_triples_synced_at = null",
        ] {
            assert!(
                UPSERT_NATIVE_MEMORY.contains(field),
                "upsert does not clear {field}: {UPSERT_NATIVE_MEMORY}"
            );
        }
        // ...and the marker is written by exactly one statement, at the end.
        assert!(MARK_NATIVE_MEMORY_CURRENT.contains("m.sha = $sha"));
    }

    /// Supersession reaches across the store, but only to other memories and only
    /// to claims the operator wrote earlier.
    ///
    /// Restricting it to `$file` meant a memory recording "X supersedes Y" could
    /// only retire a claim written in that same file — almost never where it
    /// lives, so the common case did nothing. Widening it to the whole slug
    /// unqualified then let it retire its own siblings (triples are written just
    /// before this runs) and, on a re-index, claims newer than the event. Bounding
    /// it by the node timestamps was wrong a third way: those are ingestion times,
    /// so the result depended on sync order.
    #[test]
    fn supersession_is_bounded_in_both_directions() {
        assert!(
            APPLY_SUPERSESSION.contains("{slug: $slug}"),
            "not slug-scoped: {APPLY_SUPERSESSION}"
        );
        for guard in [
            // other memories, not the one asserting the supersession
            "pm.file <> $file",
            // and only claims the operator wrote earlier, by SOURCE mtime
            "coalesce(pm.source_mtime, 0) < coalesce(sm.source_mtime, 0)",
            // ...with the filename breaking whole-second ties into a total order,
            // so two memories cannot each retire the other's claims
            "AND pm.file < $file",
            // anchored on the real superseding triple
            "sup.relation = 'supersedes'",
        ] {
            assert!(
                APPLY_SUPERSESSION.contains(guard),
                "missing guard `{guard}`: {APPLY_SUPERSESSION}"
            );
        }
        // Ingestion timestamps must never be the ordering key: whichever memory
        // the sync happened to reach first would win, so a supersession would
        // silently fail depending on filename order.
        for ingestion in ["valid_from <", "created_at <", "updated_at <"] {
            assert!(
                !APPLY_SUPERSESSION.contains(ingestion),
                "ordering on an ingestion timestamp (`{ingestion}`) makes this \
                 sync-order dependent: {APPLY_SUPERSESSION}"
            );
        }
        // ...and source_mtime has to actually be persisted for that to work —
        // from the upsert when content changed, and from the backfill when it did
        // not, since mtime is deliberately outside the freshness hash. Without
        // the second write the key only ever landed on memories that happened to
        // change, leaving every older node comparing against zero.
        assert!(
            UPSERT_NATIVE_MEMORY.contains("m.source_mtime = $source_mtime"),
            "the ordering key is never written: {UPSERT_NATIVE_MEMORY}"
        );
        assert!(
            SET_NATIVE_MEMORY_MTIMES.contains("m.source_mtime = entry.source_mtime"),
            "unchanged memories never get the ordering key: {SET_NATIVE_MEMORY_MTIMES}"
        );
    }

    /// The legacy-edge fulltext path is gone and must not come back.
    ///
    /// A Neo4j fulltext index cannot be partitioned, so it ranked across every
    /// store and applied its limit before any slug filter — crowding out this
    /// store's hits — and a single surviving hit short-circuited the scoped query,
    /// returning an under-filled result. Over-fetching made it costlier, not
    /// correct.
    #[test]
    fn no_unpartitionable_fulltext_index_is_queried_for_native_recall() {
        // Assembled from parts: spelling the index name as one literal here would
        // plant it in the very source this test scans.
        let index = ["engram", "native", "legacy", "edges"].join("_");
        assert!(
            !include_str!("lib.rs").contains(&index),
            "the unpartitionable legacy-edge index is being used again"
        );
        // ...and the abandoned nodes from the old import are cleaned up.
        assert!(DROP_LEGACY_EDGE_CACHE.contains("EngramLegacyEdge"));
    }

    /// Filenames are unique only within a store, so every native statement that
    /// reads or writes memory-scoped data must be slug-scoped.
    ///
    /// This scans the source instead of listing the statements. The previous
    /// version enumerated its own subjects by hand and omitted the legacy-edge
    /// query, so it passed while that statement returned other stores' memories —
    /// a test that chooses what to check cannot catch what it forgot to add.
    /// Anything touching an `Engram*` label is now in scope automatically, so a
    /// new unscoped statement fails here on arrival.
    #[test]
    fn every_native_statement_is_scoped_by_slug() {
        let mut checked = Vec::new();
        for declaration in include_str!("lib.rs").split("\nconst ").skip(1) {
            let Some((name, body)) = declaration.split_once(": &str = ") else {
                continue;
            };
            let statement = body.split(";\n").next().unwrap_or_default();
            // Schema DDL names labels but has no rows to scope.
            if statement.contains("CREATE FULLTEXT INDEX") {
                continue;
            }
            // One documented exception, kept as a named list so adding to it is a
            // visible decision rather than a quiet carve-out: the abandoned edge
            // cache is deleted wholesale precisely BECAUSE those nodes predate
            // slug scoping and carry no slug to filter on.
            if ["DROP_LEGACY_EDGE_CACHE"].contains(&name) {
                continue;
            }
            if !statement.contains("Engram") {
                continue; // a Graphiti-only statement, scoped by Graphiti itself
            }
            assert!(
                statement.contains("$slug"),
                "{name} touches Engram* data without a $slug predicate"
            );
            checked.push(name.to_string());
        }
        // Guard the guard: if the parse silently matches nothing, the assertion
        // above is vacuous and we are back to a test that proves nothing.
        assert!(
            checked.len() >= 14,
            "expected to scan the native statements, only found {checked:?}"
        );
        // The legacy import is the statement class that slipped through a
        // hand-written list; prove the scan reaches it.
        assert!(
            checked
                .iter()
                .any(|name| name == "IMPORT_NATIVE_LEGACY_FACTS"),
            "the legacy import is not being scanned: {checked:?}"
        );
    }

    /// Recall must only surface claims that are both active and still open.
    #[test]
    fn recall_predicates_exclude_closed_and_inactive_triples() {
        for statement in [NATIVE_FACTS_FOR_TOKENS, NATIVE_KEYWORD_FILES] {
            assert!(statement.contains("status: 'active'"), "{statement}");
            assert!(statement.contains("valid_until IS NULL"), "{statement}");
        }
        // the imported-fact branch: still only open facts, still this slug
        assert!(NATIVE_FACTS_FOR_TOKENS.contains("f.valid_until IS NULL"));
        assert_eq!(NATIVE_FACTS_FOR_TOKENS.matches("{slug: $slug}").count(), 2);
        assert!(NATIVE_SEMANTIC_FILES.contains("f.valid_until IS NULL"));
    }

    /// Imported legacy facts are matched like the legacy query matched entities:
    /// by equality, so a filler word ("that", "when") inside a fact's text or an
    /// entity name never pulls it in.
    /// The bootstrap import is bounded by the verified episode list and the
    /// slug: no other episode of the same file, and no other store's node.
    #[test]
    fn bootstrap_import_takes_only_verified_episodes() {
        assert!(LEGACY_VERIFIED_FACTS.contains("WHERE episode IN $episodes"));
        assert!(LEGACY_SOURCES.contains("e.uuid"));
        // facts and marker in ONE statement = one transaction
        assert!(COMMIT_BOOTSTRAPPED_MEMORY.contains("MERGE (m)-[:HAS_FACT]->(f)"));
        assert!(COMMIT_BOOTSTRAPPED_MEMORY.contains("SET m.sha = $sha"));
        assert!(COMMIT_BOOTSTRAPPED_MEMORY.contains("EngramFact {slug: $slug"));
        // a superseded legacy fact is imported as history, never as active
        assert!(LEGACY_VERIFIED_FACTS.contains("coalesce(r.invalid_at, r.expired_at)"));
        assert!(COMMIT_BOOTSTRAPPED_MEMORY.contains("datetime(row.valid_until)"));
        // and the legacy read paths stop serving superseded facts
        assert!(LEGACY_FACTS_FOR_TOKENS.contains("r.invalid_at IS NULL AND r.expired_at IS NULL"));
        assert!(LEGACY_SEMANTIC_FILES.contains("e.invalid_at IS NULL AND e.expired_at IS NULL"));
        assert!(LEGACY_KEYWORD_FILES.contains("rel.invalid_at IS NULL AND rel.expired_at IS NULL"));
        // a retry resumes fact-less, unstamped nodes and skips everything else
        assert!(NATIVE_FILES.contains("m.sha IS NOT NULL OR (m)-[:HAS_FACT]->()"));
    }

    #[test]
    fn imported_facts_match_entity_names_exactly() {
        assert!(NATIVE_FACTS_FOR_TOKENS.contains("toLower(f.subject) = toLower(name)"));
        assert!(NATIVE_FACTS_FOR_TOKENS.contains("toLower(f.object) = toLower(name)"));
        assert!(!NATIVE_FACTS_FOR_TOKENS.contains("f.text) CONTAINS"));
        assert!(IMPORT_NATIVE_LEGACY_FACTS.contains("f.subject = source.name"));
        assert!(IMPORT_NATIVE_LEGACY_FACTS.contains("f.object = target.name"));
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

        // The query retires by object identity, and the target list — computed
        // above, in Rust — is what selects the superseding triple.
        assert!(APPLY_SUPERSESSION.contains("toLower(sup.object) IN $targets"));
        assert!(APPLY_SUPERSESSION.contains("toLower(prior.object) = toLower(sup.object)"));
        // `sup.relation` identifies the asserting triple; what must never come
        // back is inferring the *prior* claim from a relation name.
        assert!(
            !APPLY_SUPERSESSION.contains("prior.relation = 'supersedes'"),
            "supersession is still inferred inside the query"
        );
    }

    #[test]
    fn supersession_does_not_retire_the_superseding_claim_itself() {
        assert!(APPLY_SUPERSESSION.contains("prior.relation <> 'supersedes'"));
    }
}
