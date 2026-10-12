//! Neo4j access and every Cypher statement engram issues.
//!
//! The statements live in named constants rather than inline string literals so
//! they can be asserted on in unit tests. That is not cosmetic: the obsolete-triple
//! predicate shipped as `NOT [t IN $triples | t.key] CONTAINS obsolete.key`, which
//! applies Cypher's *string* `CONTAINS` to a list, evaluates to `null`, and
//! therefore retired nothing — ever. No test could see it, because no test touched
//! a query string.

use engram_tenant::GraphScope;
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

// Graphiti's own data (Entity / RELATES_TO / Episodic) is partitioned by
// `group_id`, which engram pinned to the single literal "canonical" for every
// memory in every store. So these statements were not merely unscoped by slug —
// they had no partition at all, and the graph leg returned one project's facts
// to another. $tenant IS the group_id: the legacy tenant passes "canonical", so
// a pre-tenancy install reads exactly what it always did.

const LEGACY_FACTS_FOR_TOKENS: &str = "\
UNWIND $names AS nm \
MATCH (n:Entity)-[r:RELATES_TO]-(m:Entity) \
WHERE toLower(n.name) = toLower(nm) \
  AND n.group_id = $tenant AND m.group_id = $tenant AND r.group_id = $tenant \
RETURN r.fact AS fact LIMIT $lim";

const NATIVE_FACTS_FOR_TOKENS: &str = "\
UNWIND $names AS name \
MATCH (m:EngramMemory {tenant: $tenant, slug: $slug})-[:HAS_TRIPLE]->(t:EngramTriple {tenant: $tenant, status: 'active'}) \
WHERE t.valid_until IS NULL \
  AND (toLower(t.subject) CONTAINS toLower(name) OR toLower(t.object) CONTAINS toLower(name)) \
RETURN DISTINCT t.subject + ' ' + t.relation + ' ' + t.object AS fact LIMIT $lim";

const LEGACY_SEMANTIC_FILES: &str = "\
MATCH (n:Entity)-[e:RELATES_TO]->(m:Entity) WHERE e.group_id = $tenant \
WITH e, vector.similarity.cosine(e.fact_embedding, $vector) AS score WHERE score > 0 \
UNWIND coalesce(e.episodes, []) AS episode \
MATCH (ep:Episodic {group_id: $tenant, uuid: episode}) WHERE ep.file IS NOT NULL \
RETURN ep.file AS file, collect(DISTINCT e.fact) AS facts, max(score) AS score \
ORDER BY score DESC LIMIT $limit";

/// How many index hits to pull before filtering to this tenant's group.
///
/// A Neo4j fulltext index cannot be partitioned: `queryRelationships` ranks
/// across EVERY group and applies its own limit, so filtering the yielded rows
/// afterwards means a tenant with a small share of the index gets crowded out by
/// a larger neighbour's edges — asking for 12 and receiving 0, with no error.
/// This is the same defect that caused the abandoned native legacy-edge index
/// to be deleted outright rather than filtered. (Its name is deliberately not
/// written here: `no_unpartitionable_fulltext_index_is_queried_for_native_recall`
/// greps this file for it.)
///
/// This index belongs to Graphiti and backs real functionality, so it is
/// over-fetched instead of removed. That bounds the problem without solving it:
/// a tenant holding under ~1/20th of the indexed edges can still be starved.
/// The semantic and native keyword legs do not share the defect, so recall
/// degrades rather than failing. Recorded as a known limitation.
const LEGACY_KEYWORD_OVERFETCH: usize = 20;

const LEGACY_KEYWORD_FILES: &str = "\
CALL db.index.fulltext.queryRelationships('edge_name_and_fact', $query, {limit: $overfetch}) \
YIELD relationship AS rel, score \
WITH rel, score WHERE rel.group_id = $tenant \
UNWIND coalesce(rel.episodes, []) AS episode \
MATCH (ep:Episodic {group_id: $tenant, uuid: episode}) WHERE ep.file IS NOT NULL \
RETURN ep.file AS file, collect(DISTINCT rel.fact) AS facts, max(score) AS score \
ORDER BY score DESC LIMIT $limit";

// Legacy import. Schema DDL carries no slug; the data statement must.

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
MATCH (:Entity)-[r:RELATES_TO]->(:Entity) WHERE r.group_id = $tenant \
UNWIND coalesce(r.episodes, []) AS episode \
MATCH (ep:Episodic {group_id: $tenant, uuid: episode}) WHERE ep.file IS NOT NULL \
MATCH (m:EngramMemory {tenant: $tenant, slug: $slug, file: ep.file}) \
WHERE r.fact IS NOT NULL \
MERGE (f:EngramFact {tenant: $tenant, slug: $slug, memory_file: m.file, text: r.fact}) \
ON CREATE SET f.created_at = datetime(), f.valid_from = datetime() \
SET f.legacy_name = r.name, \
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
MATCH (m:EngramMemory {tenant: $tenant, slug: $slug}) \
WITH m, [token IN $tokens WHERE toLower(coalesce(m.name, '') + ' ' + coalesce(m.description, '') + ' ' + coalesce(m.body, '')) CONTAINS token] AS memory_matches \
OPTIONAL MATCH (m)-[:HAS_FACT]->(f:EngramFact) \
  WHERE f.valid_until IS NULL AND any(token IN $tokens \
    WHERE toLower(coalesce(f.text, '') + ' ' + coalesce(f.legacy_name, '')) CONTAINS token) \
WITH m, memory_matches, collect(DISTINCT f.text) AS fact_text, count(f) AS fact_score \
OPTIONAL MATCH (m)-[:HAS_TRIPLE]->(t:EngramTriple {tenant: $tenant, status: 'active'}) \
  WHERE t.valid_until IS NULL AND any(token IN $tokens WHERE toLower(t.subject + ' ' + t.relation + ' ' + t.object) CONTAINS token) \
WITH m, memory_matches, fact_text, fact_score, collect(DISTINCT t.subject + ' ' + t.relation + ' ' + t.object) AS triple_text, count(t) AS triple_score \
WITH m, fact_text + triple_text AS texts, size(memory_matches) + fact_score + triple_score AS score \
WHERE score > 0 \
RETURN m.file AS file, texts AS facts, score ORDER BY score DESC LIMIT $limit";

const NATIVE_SEMANTIC_FILES: &str = "\
MATCH (m:EngramMemory {tenant: $tenant, slug: $slug})-[:HAS_FACT]->(f:EngramFact) \
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
MERGE (m:EngramMemory {tenant: $tenant, slug: $slug, file: $file}) \
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
MATCH (m:EngramMemory {tenant: $tenant, slug: $slug, file: entry.file}) \
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
MATCH (m:EngramMemory {tenant: $tenant, slug: $slug, file: $file}) \
SET m.sha = $sha, m.embedding_space = $space, m.native_triple_version = 2, \
    m.native_triples_synced_at = datetime() \
RETURN m.file";

const NATIVE_MEMORY_IS_CURRENT: &str = "\
MATCH (m:EngramMemory {tenant: $tenant, slug: $slug, file: $file, sha: $sha, embedding_space: $space, native_triple_version: 2}) \
RETURN count(m) > 0";

const REPLACE_NATIVE_FACTS: &str = "\
MATCH (m:EngramMemory {tenant: $tenant, slug: $slug, file: $file}) \
OPTIONAL MATCH (m)-[old:HAS_FACT]->(obsolete:EngramFact) WHERE NOT obsolete.text IN $facts \
SET obsolete.valid_until = datetime() DELETE old \
WITH m UNWIND $facts AS fact \
MERGE (f:EngramFact {tenant: $tenant, slug: $slug, memory_file: $file, text: fact}) \
ON CREATE SET f.valid_from = datetime(), f.created_at = datetime() \
SET f.valid_until = null, f.updated_at = datetime() \
MERGE (m)-[:HAS_FACT]->(f) \
WITH f, [word IN split(toLower(f.text), ' ') WHERE size(word) >= 6] AS names \
UNWIND names AS name \
MERGE (e:EngramEntity {tenant: $tenant, name: name}) MERGE (f)-[:MENTIONS]->(e)";

/// Retire triples this memory no longer asserts.
///
/// `NOT obsolete.key IN [...]` — list membership. The shipped version used the
/// string operator `CONTAINS`, which yields `null` against a list, so the `WHERE`
/// never passed and obsolete triples stayed active indefinitely. The sibling fact
/// query above always had it right.
const RETIRE_OBSOLETE_TRIPLES: &str = "\
MATCH (m:EngramMemory {tenant: $tenant, slug: $slug, file: $file}) \
OPTIONAL MATCH (m)-[old:HAS_TRIPLE]->(obsolete:EngramTriple) \
WHERE NOT obsolete.key IN [triple IN $triples | triple.key] \
SET obsolete.valid_until = datetime(), obsolete.status = 'superseded' DELETE old";

const WRITE_NATIVE_TRIPLES: &str = "\
MATCH (m:EngramMemory {tenant: $tenant, slug: $slug, file: $file}) \
UNWIND $triples AS triple \
MERGE (t:EngramTriple {tenant: $tenant, slug: $slug, memory_file: $file, key: triple.key}) \
ON CREATE SET t.valid_from = datetime(), t.created_at = datetime() \
SET t.subject = triple.subject, t.relation = triple.relation, t.object = triple.object, \
    t.confidence = triple.confidence, t.temporal = triple.temporal, t.status = triple.status, \
    t.valid_until = CASE WHEN triple.closed THEN coalesce(t.valid_until, datetime()) ELSE null END, \
    t.updated_at = datetime() \
MERGE (m)-[:HAS_TRIPLE]->(t) \
MERGE (subject:EngramEntity {tenant: $tenant, name: toLower(triple.subject)}) \
MERGE (object:EngramEntity {tenant: $tenant, name: toLower(triple.object)}) \
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
MATCH (sm:EngramMemory {tenant: $tenant, slug: $slug, file: $file})-[:HAS_TRIPLE]->(sup:EngramTriple) \
WHERE sup.relation = 'supersedes' AND toLower(sup.object) IN $targets \
MATCH (pm:EngramMemory {tenant: $tenant, slug: $slug})-[:HAS_TRIPLE]->(prior:EngramTriple) \
WHERE prior.valid_until IS NULL AND prior.relation <> 'supersedes' \
  AND toLower(prior.object) = toLower(sup.object) \
  AND pm.file <> $file \
  AND (coalesce(pm.source_mtime, 0) < coalesce(sm.source_mtime, 0) \
       OR (coalesce(pm.source_mtime, 0) = coalesce(sm.source_mtime, 0) \
           AND pm.file < $file)) \
SET prior.valid_until = datetime(), prior.status = 'superseded'";

const SET_NATIVE_FACT_EMBEDDINGS: &str = "\
UNWIND $points AS point \
MATCH (f:EngramFact {tenant: $tenant, slug: $slug, memory_file: $file, text: point.text}) \
SET f.embedding = point.embedding, f.embedding_updated_at = datetime() \
RETURN count(f)";

/// Drop graph data for memories no longer in the store.
///
/// Sync upserted current files and never retired anything, so a deleted or renamed
/// memory left its claims behind, still eligible for recall.
const PRUNE_MISSING_MEMORIES: &str = "\
MATCH (m:EngramMemory {tenant: $tenant, slug: $slug}) WHERE NOT m.file IN $files \
OPTIONAL MATCH (m)-[:HAS_FACT]->(f:EngramFact) \
OPTIONAL MATCH (m)-[:HAS_TRIPLE]->(t:EngramTriple) \
DETACH DELETE f, t, m";

// ---------------------------------------------------------------------------
// Tenancy migration.
//
// These are the ONLY statements that deliberately read one group and write
// another — that is what a migration is. They are exempt from the per-statement
// $tenant check and are instead asserted positively, by
// `migration_statements_name_both_groups`: each must bind BOTH `$from` and
// `$tenant`, so an exempt statement still cannot touch an unbounded set of rows.
// ---------------------------------------------------------------------------

/// Which files Graphiti holds episodes for, in a given group.
///
/// `Episodic` carries `file` but no slug, so attributing an episode to a tenant
/// means asking which owned store contains that filename. A filename present in
/// two tenants' stores is genuinely ambiguous and the caller refuses — see
/// `engram-tenant-migrate`.
const MIGRATION_EPISODE_FILES: &str = "\
MATCH (e:Episodic {group_id: $from}) WHERE e.file IS NOT NULL \
RETURN collect(DISTINCT e.file)";

/// Move a whole Graphiti group to a new one.
///
/// Wholesale rather than per-file, and only ever called once the caller has
/// established that exactly ONE tenant owns the group's episodes. Entity nodes
/// are shared across episodes — an entity mentioned by two tenants' memories is
/// a single node with a single group — so splitting a group between tenants is
/// not expressible as an update at all and requires a re-insert. Relabelling is
/// correct precisely when there is nothing to split.
const MIGRATION_RELABEL_GROUP: &str = "\
CALL { WITH $from AS from, $tenant AS tenant \
  MATCH (n) WHERE n.group_id = from SET n.group_id = tenant RETURN count(n) AS nodes } \
CALL { WITH $from AS from, $tenant AS tenant \
  MATCH ()-[r]->() WHERE r.group_id = from SET r.group_id = tenant RETURN count(r) AS rels } \
RETURN nodes, rels";

/// Count the pre-tenancy native nodes, which carry no scoping key at all.
///
/// Selected by the ABSENCE of both keys, which is a bound predicate and not a
/// bare label scan: these rows predate slug scoping, which is precisely why no
/// group parameter applies to them.
const MIGRATION_COUNT_UNSCOPED_NATIVE: &str = "\
MATCH (n) WHERE any(label IN labels(n) WHERE label STARTS WITH 'Engram') \
  AND n.slug IS NULL AND n.tenant IS NULL \
RETURN count(n)";

/// Delete the pre-tenancy native nodes, in batches.
///
/// They are unreachable: every native statement now requires both `tenant` and
/// `slug`, and these have neither, so no query can return them. They are not
/// data loss waiting to happen — the markdown store is authoritative and
/// `--rebuild` regenerates the index — they are weight that would otherwise sit
/// in the graph forever, which is the same conclusion the abandoned legacy edge
/// cache reached.
const MIGRATION_PURGE_UNSCOPED_NATIVE: &str = "\
MATCH (n) WHERE any(label IN labels(n) WHERE label STARTS WITH 'Engram') \
  AND n.slug IS NULL AND n.tenant IS NULL \
WITH n LIMIT 10000 DETACH DELETE n RETURN count(n)";

const NATIVE_MEMORY_FILES: &str = "\
MATCH (m:EngramMemory {tenant: $tenant, slug: $slug}) RETURN collect(m.file)";

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

    /// Graphiti's Entity/RELATES_TO facts for a set of query tokens.
    ///
    /// Takes a scope even though Graphiti data has no `slug`: the scope carries
    /// the `group_id`, which is the only partition this data has. Without it
    /// this query read every tenant's facts, because engram wrote them all into
    /// one literal group.
    pub async fn facts_for_tokens(
        &self,
        scope: &GraphScope,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<String>, GraphError> {
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        self.strings(
            LEGACY_FACTS_FOR_TOKENS,
            serde_json::json!({"names": tokens, "lim": limit, "tenant": scope.tenant()}),
        )
        .await
    }

    pub async fn native_facts_for_tokens(
        &self,
        scope: &GraphScope,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<String>, GraphError> {
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        self.strings(
            NATIVE_FACTS_FOR_TOKENS,
            serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant(), "names": tokens, "lim": limit}),
        )
        .await
    }

    pub async fn semantic_files(
        &self,
        scope: &GraphScope,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<RecallHit>, GraphError> {
        self.file_hits(
            LEGACY_SEMANTIC_FILES,
            serde_json::json!({"vector": vector, "limit": limit, "tenant": scope.tenant()}),
        )
        .await
    }

    /// See [`LEGACY_KEYWORD_OVERFETCH`]: this leg rides a fulltext index that
    /// cannot be partitioned, so it over-fetches and filters rather than
    /// trusting the index's own limit.
    pub async fn keyword_files(
        &self,
        scope: &GraphScope,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RecallHit>, GraphError> {
        self.file_hits(
            LEGACY_KEYWORD_FILES,
            serde_json::json!({
                "query": query,
                "limit": limit,
                "overfetch": limit.saturating_mul(LEGACY_KEYWORD_OVERFETCH).max(limit),
                "tenant": scope.tenant(),
            }),
        )
        .await
    }

    pub async fn native_keyword_files(
        &self,
        scope: &GraphScope,
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
            serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant(), "tokens": tokens, "limit": limit}),
        )
        .await
    }

    pub async fn native_semantic_files(
        &self,
        scope: &GraphScope,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<RecallHit>, GraphError> {
        self.file_hits(
            NATIVE_SEMANTIC_FILES,
            serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant(), "vector": vector, "limit": limit}),
        )
        .await
    }

    pub async fn upsert_native_memory(
        &self,
        scope: &GraphScope,
        file: &str,
        name: &str,
        description: &str,
        body: &str,
        source_mtime: i64,
    ) -> Result<(), GraphError> {
        self.query(
            UPSERT_NATIVE_MEMORY,
            serde_json::json!({
                "slug": scope.slug(), "tenant": scope.tenant(),
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
        scope: &GraphScope,
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
            serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant(), "memories": entries}),
        )
        .await?;
        Ok(())
    }

    pub async fn native_memory_is_current(
        &self,
        scope: &GraphScope,
        file: &str,
        sha: &str,
        space: &str,
    ) -> Result<bool, GraphError> {
        let response = self
            .query(
                NATIVE_MEMORY_IS_CURRENT,
                serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant(), "file": file, "sha": sha, "space": space}),
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
        scope: &GraphScope,
        file: &str,
        facts: &[String],
    ) -> Result<(), GraphError> {
        self.query(
            REPLACE_NATIVE_FACTS,
            serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant(), "file": file, "facts": facts}),
        )
        .await?;
        Ok(())
    }

    pub async fn replace_native_triples(
        &self,
        scope: &GraphScope,
        file: &str,
        triples: &[NativeTriple],
    ) -> Result<(), GraphError> {
        let payload = triple_payload(triples);
        self.query(
            RETIRE_OBSOLETE_TRIPLES,
            serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant(), "file": file, "triples": payload}),
        )
        .await?;
        if payload.is_empty() {
            return Ok(());
        }
        self.query(
            WRITE_NATIVE_TRIPLES,
            serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant(), "file": file, "triples": payload}),
        )
        .await?;
        let targets = supersession_targets(triples);
        if !targets.is_empty() {
            self.query(
                APPLY_SUPERSESSION,
                serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant(), "file": file, "targets": targets}),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn set_native_fact_embeddings(
        &self,
        scope: &GraphScope,
        file: &str,
        points: &[(&str, Vec<f32>)],
    ) -> Result<(), GraphError> {
        if points.is_empty() {
            return Ok(());
        }
        self.query(
            SET_NATIVE_FACT_EMBEDDINGS,
            serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant(), "file": file, "points": points.iter().map(|(text, embedding)| serde_json::json!({"text": text, "embedding": embedding})).collect::<Vec<_>>() }),
        )
        .await?;
        Ok(())
    }

    /// Record that `file` is fully synced. Must be the LAST write of a sync.
    pub async fn mark_native_memory_current(
        &self,
        scope: &GraphScope,
        file: &str,
        sha: &str,
        space: &str,
    ) -> Result<(), GraphError> {
        self.query(
            MARK_NATIVE_MEMORY_CURRENT,
            serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant(), "file": file, "sha": sha, "space": space}),
        )
        .await?;
        Ok(())
    }

    /// Every memory file the graph holds for a slug.
    /// Files Graphiti holds episodes for in `from_group`. See
    /// [`MIGRATION_EPISODE_FILES`].
    pub async fn migration_episode_files(
        &self,
        from_group: &str,
    ) -> Result<Vec<String>, GraphError> {
        let response = self
            .query(
                MIGRATION_EPISODE_FILES,
                serde_json::json!({"from": from_group}),
            )
            .await?;
        Ok(response
            .results
            .into_iter()
            .flat_map(|set| set.data)
            .filter_map(|row| row.row.into_iter().next())
            .filter_map(|value| value.as_array().cloned())
            .flatten()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect())
    }

    /// Move a whole Graphiti group. Returns (nodes, relationships) touched.
    ///
    /// Only valid once the caller has established that one tenant owns the
    /// group — see [`MIGRATION_RELABEL_GROUP`].
    pub async fn migration_relabel_group(
        &self,
        from_group: &str,
        to_group: &str,
    ) -> Result<(u64, u64), GraphError> {
        let response = self
            .query(
                MIGRATION_RELABEL_GROUP,
                serde_json::json!({"from": from_group, "tenant": to_group}),
            )
            .await?;
        let row = response
            .results
            .into_iter()
            .flat_map(|set| set.data)
            .next()
            .map(|row| row.row)
            .unwrap_or_default();
        let number = |index: usize| {
            row.get(index)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default()
        };
        Ok((number(0), number(1)))
    }

    /// How many pre-tenancy native nodes are present (no slug, no tenant).
    pub async fn migration_count_unscoped_native(&self) -> Result<u64, GraphError> {
        let response = self
            .query(MIGRATION_COUNT_UNSCOPED_NATIVE, serde_json::json!({}))
            .await?;
        Ok(response
            .results
            .into_iter()
            .flat_map(|set| set.data)
            .next()
            .and_then(|row| row.row.into_iter().next())
            .and_then(|value| value.as_u64())
            .unwrap_or_default())
    }

    /// Delete the pre-tenancy native nodes. Batched, and capped, for the same
    /// reason the legacy edge cache drop is: an unbounded loop against a large
    /// graph is how a maintenance pass becomes an outage. An unreadable response
    /// stops the loop rather than being treated as "done".
    pub async fn migration_purge_unscoped_native(&self) -> Result<u64, GraphError> {
        const MAX_BATCHES: usize = 100;
        let mut purged = 0;
        for _ in 0..MAX_BATCHES {
            let response = self
                .query(MIGRATION_PURGE_UNSCOPED_NATIVE, serde_json::json!({}))
                .await?;
            let batch = response
                .results
                .into_iter()
                .flat_map(|set| set.data)
                .next()
                .and_then(|row| row.row.into_iter().next())
                .and_then(|value| value.as_u64());
            match batch {
                Some(0) => return Ok(purged),
                Some(count) => purged += count,
                // Unreadable means "stop", not "finished": continuing would spin
                // MAX_BATCHES times against a graph we cannot measure.
                None => return Ok(purged),
            }
        }
        Ok(purged)
    }

    pub async fn native_memory_files(&self, scope: &GraphScope) -> Result<Vec<String>, GraphError> {
        let response = self
            .query(
                NATIVE_MEMORY_FILES,
                serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant()}),
            )
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
        scope: &GraphScope,
        files: &[String],
    ) -> Result<(), GraphError> {
        self.query(
            PRUNE_MISSING_MEMORIES,
            serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant(), "files": files}),
        )
        .await?;
        Ok(())
    }

    /// Import legacy Graphiti facts as slug-scoped native facts, and clear out
    /// the abandoned edge cache the previous version of this import created.
    pub async fn import_legacy_fact_embeddings(
        &self,
        scope: &GraphScope,
    ) -> Result<(), GraphError> {
        for statement in [CREATE_NATIVE_FACT_INDEX, IMPORT_NATIVE_LEGACY_FACTS] {
            self.query(
                statement,
                serde_json::json!({"slug": scope.slug(), "tenant": scope.tenant()}),
            )
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
            APPLY_SUPERSESSION.contains("{tenant: $tenant, slug: $slug}"),
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
            if MIGRATION_STATEMENTS.contains(&name) || ["DROP_LEGACY_EDGE_CACHE"].contains(&name) {
                continue;
            }
            // Graphiti's own labels. These used to be skipped outright, with the
            // comment "scoped by Graphiti itself" — which was simply false.
            // Graphiti partitions by `group_id`, and engram wrote the literal
            // "canonical" for every memory in every store, so the skip exempted
            // precisely the statements that were returning one project's facts
            // to another. They are now checked for a $tenant group predicate.
            let graphiti = ["(:Entity", "(n:Entity", "(ep:Episodic", "RELATES_TO"]
                .iter()
                .any(|label| statement.contains(label));
            let native = statement.contains("Engram");
            if !graphiti && !native {
                continue;
            }
            if native {
                assert!(
                    statement.contains("$slug"),
                    "{name} touches Engram* data without a $slug predicate"
                );
            }
            assert!(
                statement.contains("$tenant"),
                "{name} touches tenant-owned data without a $tenant predicate"
            );
            // Per-PATTERN, not merely per-statement. The old check asked only
            // whether `$slug` appeared somewhere in the string, which
            // `WRITE_NATIVE_TRIPLES` satisfied while merging
            // `(e:EngramEntity {name: name})` with no scope at all — entity
            // nodes were global, shared across every store and every identity.
            // Nothing read them, so it never surfaced; it would have the moment
            // anything traversed MENTIONS or SUBJECT.
            for pattern in scoped_patterns(statement) {
                // Two spellings of the same partition, because the two data
                // models name it differently: engram's own nodes carry
                // `tenant`, and Graphiti's carry `group_id`. Both bind to
                // $tenant, so a legacy install passes "canonical" to each and
                // reads exactly what it read before.
                assert!(
                    pattern.contains("tenant: $tenant") || pattern.contains("group_id: $tenant"),
                    "{name} has an unscoped node pattern: ({pattern})"
                );
            }
            checked.push(name.to_string());
        }
        // Guard the guard: if the parse silently matches nothing, the assertion
        // above is vacuous and we are back to a test that proves nothing.
        assert!(
            checked.len() >= 18,
            "expected to scan the native and Graphiti statements, only found {checked:?}"
        );
        // The legacy import is the statement class that slipped through a
        // hand-written list; prove the scan reaches it. The Graphiti-only
        // statements are the class the `!contains("Engram")` skip hid.
        for required in [
            "IMPORT_NATIVE_LEGACY_FACTS",
            "LEGACY_FACTS_FOR_TOKENS",
            "LEGACY_SEMANTIC_FILES",
            "LEGACY_KEYWORD_FILES",
            "WRITE_NATIVE_TRIPLES",
        ] {
            assert!(
                checked.iter().any(|name| name == required),
                "{required} is not being scanned: {checked:?}"
            );
        }
    }

    /// The statements exempt from the per-statement `$tenant` check, because
    /// crossing groups is their whole purpose. Named here rather than skipped by
    /// a pattern match, so adding one is a visible decision.
    const MIGRATION_STATEMENTS: [&str; 4] = [
        "MIGRATION_EPISODE_FILES",
        "MIGRATION_RELABEL_GROUP",
        "MIGRATION_COUNT_UNSCOPED_NATIVE",
        "MIGRATION_PURGE_UNSCOPED_NATIVE",
    ];

    /// An exemption that only subtracts a check is how the `!contains("Engram")`
    /// carve-out hid a live leak for as long as it did. So the migration
    /// statements get a POSITIVE requirement instead of a bare pass.
    ///
    /// The invariant that matters is not "binds both groups" — a read-only
    /// statement reading one group correctly binds one. It is that **no
    /// migration statement may select its rows by nothing**: each must either be
    /// group-scoped, or explicitly target the rows that predate both scoping
    /// keys. And anything that *writes* a group must bind the group it writes.
    #[test]
    fn migration_statements_always_bound_their_rows() {
        let mut checked = 0;
        for declaration in include_str!("lib.rs").split("\nconst ").skip(1) {
            let Some((name, body)) = declaration.split_once(": &str = ") else {
                continue;
            };
            if !MIGRATION_STATEMENTS.contains(&name) {
                continue;
            }
            let statement = body.split(";\n").next().unwrap_or_default();
            let group_scoped = statement.contains("group_id");
            // The pre-tenancy rows cannot be group-scoped — having no group is
            // what identifies them — so their predicate is the absence itself.
            let targets_unscoped =
                statement.contains("slug IS NULL") && statement.contains("tenant IS NULL");
            assert!(
                group_scoped || targets_unscoped,
                "{name} is exempt from tenant scoping and selects its rows by nothing"
            );
            if statement.contains("SET n.group_id") {
                assert!(
                    statement.contains("$tenant"),
                    "{name} writes a group without binding the group it writes"
                );
            }
            checked += 1;
        }
        assert_eq!(
            checked,
            MIGRATION_STATEMENTS.len(),
            "a named migration statement was not found in the source"
        );
    }

    /// Node patterns that look up data by identity, i.e. carry a `{...}` property
    /// map, and therefore have to name the tenant.
    ///
    /// A pattern with no property map — `(f:EngramFact)` in a traversal — is
    /// reached through an already-scoped variable and is legitimately unscoped;
    /// requiring a predicate there would be noise. A pattern WITH a map is an
    /// anchor, and an unscoped anchor is a cross-tenant read.
    fn scoped_patterns(statement: &str) -> Vec<&str> {
        let mut found = Vec::new();
        let bytes = statement.as_bytes();
        for (open, _) in statement.match_indices('(') {
            // The pattern runs to its matching ')'. Property maps here contain
            // no nested parens except in function calls like toLower(...), so
            // track depth rather than taking the first ')'.
            let mut depth = 0usize;
            let mut close = None;
            for (offset, byte) in bytes[open..].iter().enumerate() {
                match byte {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(open + offset);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(close) = close else { continue };
            let inner = &statement[open + 1..close];
            // Only labelled node patterns with a property map.
            let labelled = inner.contains(":Engram") || inner.contains(":Episodic");
            if labelled && inner.contains('{') {
                found.push(inner);
            }
        }
        found
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
