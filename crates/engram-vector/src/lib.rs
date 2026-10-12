use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

/// Which corpus a client addresses. The two have different payload shapes —
/// one point per memory file, many chunks per wiki document — and must never
/// share a collection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Corpus {
    Memory,
    Wiki,
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
    /// The owning tenant, written but never filtered on.
    ///
    /// The collection name IS the tenant boundary (see
    /// [`QdrantClient::for_tenant`]); this field does not enforce it and must
    /// not be mistaken for doing so. It is here so a point can be audited back
    /// to its owner and a misfiled one is detectable — which matters most
    /// during the migration, when points are being moved between collections.
    pub tenant: &'a str,
    pub sha: &'a str,
    /// Which embedding space produced `vector` — see
    /// `Config::embedding_space_id`. Stored so an operator can see at a glance
    /// whether a collection holds vectors from more than one model; freshness is
    /// enforced through `sha`, which the space id is folded into.
    pub space: &'a str,
    pub vector: Vec<f32>,
}

/// One wiki chunk to store. See [`QdrantClient::upsert_chunks`].
#[derive(Debug)]
pub struct WikiPoint<'a> {
    pub tenant: &'a str,
    pub path: &'a str,
    pub title: &'a str,
    /// `Page > Section > Subsection`, flattened for display and filtering.
    pub heading_path: &'a str,
    pub index: usize,
    pub count: usize,
    pub tags: &'a [String],
    /// The chunk's own text, stored so a hit can be shown without reading the
    /// file — recall must work even if the vault is unmounted at that moment.
    pub text: &'a str,
    pub sha: &'a str,
    pub space: &'a str,
    pub source_mtime: i64,
    pub vector: Vec<f32>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct ChunkHit {
    pub path: String,
    pub title: String,
    pub breadcrumb: String,
    pub chunk_index: usize,
    pub text: String,
    pub score: f64,
}

/// Deterministic id for a chunk: tenant, path, chunk index.
///
/// Includes the tenant even though collections are per-tenant, so an id is
/// globally unique and a point copied between collections during a migration
/// cannot collide with a different tenant's chunk.
pub fn wiki_point_id(tenant: &str, path: &str, index: usize) -> Uuid {
    Uuid::new_v5(
        &POINT_NAMESPACE,
        format!("{tenant}::{path}::{index}").as_bytes(),
    )
}

#[derive(Deserialize)]
struct ScrollResponse {
    result: ScrollResult,
}
#[derive(Deserialize)]
struct ScrollResult {
    points: Vec<ScrollPoint>,
    next_page_offset: Option<serde_json::Value>,
}
#[derive(Deserialize)]
struct ScrollPoint {
    payload: Option<Payload>,
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

/// What a search is allowed to see.
///
/// A struct rather than a pair of `Option` arguments. The embedding space is
/// never optional — comparing vectors from two models produces confident
/// nonsense — and the tenant boundary is enforced one level up, by the
/// collection name coming from a `Tenant`. What remains genuinely optional is
/// the slug: `recall.scope_to_slug` lets an operator search every store a
/// tenant owns at once.
#[derive(Clone, Copy, Debug)]
pub struct Scope<'a> {
    pub space: &'a str,
    pub slug: Option<&'a str>,
}

impl<'a> Scope<'a> {
    /// Every store in the current collection, pinned to one embedding space.
    pub fn space(space: &'a str) -> Self {
        Self { space, slug: None }
    }

    /// One store, pinned to one embedding space.
    pub fn slug(space: &'a str, slug: &'a str) -> Self {
        Self {
            space,
            slug: Some(slug),
        }
    }
}

/// The payload filter for a search. Extracted so its shape can be asserted
/// without a live Qdrant.
fn search_filter(scope: Scope<'_>) -> serde_json::Value {
    // The space clause is unconditional: it used to be an Option that the one
    // caller happened to fill in, which is a guarantee resting on a call site
    // rather than on the type.
    let mut must = vec![serde_json::json!({"key": "space", "match": {"value": scope.space}})];
    if let Some(slug) = scope.slug {
        must.push(serde_json::json!({"key": "slug", "match": {"value": slug}}));
    }
    serde_json::json!({"must": must})
}

impl QdrantClient {
    pub fn new(base_url: impl Into<String>, collection: impl Into<String>) -> Self {
        Self::with_credentials(base_url, collection, None, 0)
    }

    /// Build a client addressed at one tenant's collection for one corpus.
    ///
    /// This replaces a `from_config` that read `vector_store.collection`
    /// directly. The collection name now comes from a resolved
    /// [`engram_tenant::Tenant`], which is the whole isolation mechanism:
    /// reaching another identity's vectors requires *naming* its collection,
    /// rather than merely forgetting to add a filter clause. There is
    /// deliberately no constructor that picks a collection on its own.
    ///
    /// `vector_store.api_key` is documented and shipped in `engram.yaml.example`
    /// for Qdrant Cloud; it was not modelled in Rust, so a Cloud deployment got
    /// unauthenticated requests and a 401 with no explanation.
    pub fn for_tenant(
        config: &engram_config::Config,
        tenant: &engram_tenant::Tenant,
        corpus: Corpus,
    ) -> Self {
        let collection = match corpus {
            Corpus::Memory => tenant.memory_collection(),
            Corpus::Wiki => tenant.wiki_collection(),
        };
        Self::with_credentials(
            config.vector_store.url.clone(),
            collection.to_string(),
            config.vector_store.api_key.present(),
            config.vector_store.timeout_seconds,
        )
    }

    pub fn with_credentials(
        base_url: impl Into<String>,
        collection: impl Into<String>,
        api_key: Option<&str>,
        timeout_seconds: u64,
    ) -> Self {
        // The key is attached as a default header rather than per request: every
        // method below builds its own request, and one missed call site is an
        // unauthenticated query.
        let mut builder = Client::builder();
        if let Some(key) = api_key.map(str::trim).filter(|key| !key.is_empty()) {
            let mut headers = reqwest::header::HeaderMap::new();
            if let Ok(value) = reqwest::header::HeaderValue::from_str(key) {
                let mut value = value;
                value.set_sensitive(true);
                headers.insert("api-key", value);
            }
            builder = builder.default_headers(headers);
        }
        if timeout_seconds > 0 {
            builder = builder.timeout(std::time::Duration::from_secs(timeout_seconds));
        }
        Self {
            base_url: base_url.into().trim_end_matches('/').into(),
            collection: collection.into(),
            client: builder.build().unwrap_or_else(|_| Client::new()),
        }
    }
    /// `scope.space` is the active [`engram_config::Config::embedding_space_id`].
    ///
    /// Filtering on it is not optional hygiene. Indexing is incremental, so during
    /// a reindex after a same-dimension model change the collection holds points
    /// from BOTH spaces at once — and vectors from two models are not comparable,
    /// so scoring a new query against old points produces confident nonsense with
    /// nothing to indicate it. Writing the space onto the payload without reading
    /// it back here left exactly that window open. A query returns fewer results
    /// until the reindex finishes, which is the right failure: incomplete beats
    /// wrongly ranked.
    ///
    /// The tenant boundary is NOT enforced here. It is enforced by
    /// [`QdrantClient::for_tenant`], which takes the collection name from a
    /// resolved `Tenant` — so reaching another identity's points requires naming
    /// its collection rather than forgetting a filter clause.
    pub async fn search(
        &self,
        vector: Vec<f32>,
        limit: usize,
        scope: Scope<'_>,
    ) -> Result<Vec<Hit>, VectorError> {
        let mut body = serde_json::json!({"query": vector, "limit": limit, "with_payload": true});
        body["filter"] = search_filter(scope);
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
                    "tenant": point.tenant,
                    "sha": point.sha,
                    "space": point.space,
                }
            }]}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// One wiki chunk.
    ///
    /// Separate from [`IndexPoint`] because the two corpora have genuinely
    /// different identities: a memory is one point per file, a wiki document is
    /// N points per file and needs the chunk index in its id and its payload.
    pub async fn upsert_chunks(&self, chunks: &[WikiPoint<'_>]) -> Result<(), VectorError> {
        if chunks.is_empty() {
            return Ok(());
        }
        let points: Vec<serde_json::Value> = chunks
            .iter()
            .map(|chunk| {
                serde_json::json!({
                    "id": wiki_point_id(chunk.tenant, chunk.path, chunk.index).to_string(),
                    "vector": chunk.vector,
                    "payload": {
                        "tenant": chunk.tenant,
                        "path": chunk.path,
                        "title": chunk.title,
                        "heading_path": chunk.heading_path,
                        "chunk_index": chunk.index,
                        "chunk_count": chunk.count,
                        "tags": chunk.tags,
                        "text": chunk.text,
                        "sha": chunk.sha,
                        "space": chunk.space,
                        "mtime": chunk.source_mtime,
                    }
                })
            })
            .collect();
        self.client
            .put(format!(
                "{}/collections/{}/points?wait=true",
                self.base_url, self.collection
            ))
            .json(&serde_json::json!({"points": points}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Drop a document's chunks from `keep_below` upwards.
    ///
    /// Called AFTER the new chunks are written, never before. An edit that
    /// shortens a page leaves orphaned tail chunks — text that is no longer in
    /// the document but still answers queries — and deleting first would leave a
    /// window in which the page is partly or wholly missing from recall. Writing
    /// first and trimming after is monotonic: the index is always a superset of
    /// the truth, never a subset.
    pub async fn prune_chunks(
        &self,
        tenant: &str,
        path: &str,
        keep_below: usize,
    ) -> Result<(), VectorError> {
        self.client
            .post(format!(
                "{}/collections/{}/points/delete?wait=true",
                self.base_url, self.collection
            ))
            .json(&serde_json::json!({"filter": {"must": [
                {"key": "tenant", "match": {"value": tenant}},
                {"key": "path", "match": {"value": path}},
                {"key": "chunk_index", "range": {"gte": keep_below}},
            ]}}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Every wiki document path present for a tenant, with the sha recorded on
    /// its chunks — the freshness and prune index.
    pub async fn wiki_documents(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, String>, VectorError> {
        let mut documents = BTreeMap::new();
        let mut offset = None;
        loop {
            let mut body = serde_json::json!({
                "limit": 512,
                "with_payload": ["path", "sha"],
                "with_vector": false,
                "filter": {"must": [{"key": "tenant", "match": {"value": tenant}}]},
            });
            if let Some(value) = offset {
                body["offset"] = value;
            }
            let response = self
                .client
                .post(format!(
                    "{}/collections/{}/points/scroll",
                    self.base_url, self.collection
                ))
                .json(&body)
                .send()
                .await?;
            // A collection that does not exist yet holds no documents. Treating
            // 404 as an error made `--dry-run` fail on a first-ever index, since
            // only the writing path creates the collection first.
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(documents);
            }
            let page: serde_json::Value = response.error_for_status()?.json().await?;
            for point in page
                .pointer("/result/points")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                let path = point.pointer("/payload/path").and_then(|v| v.as_str());
                let sha = point.pointer("/payload/sha").and_then(|v| v.as_str());
                if let (Some(path), Some(sha)) = (path, sha) {
                    documents.insert(path.to_string(), sha.to_string());
                }
            }
            match page.pointer("/result/next_page_offset").cloned() {
                Some(serde_json::Value::Null) | None => return Ok(documents),
                Some(value) => offset = Some(value),
            }
        }
    }

    /// Remove every chunk of a document.
    pub async fn delete_document(&self, tenant: &str, path: &str) -> Result<(), VectorError> {
        self.prune_chunks(tenant, path, 0).await
    }

    /// Search wiki chunks, returning the payload a caller needs to show a hit.
    pub async fn search_chunks(
        &self,
        vector: Vec<f32>,
        limit: usize,
        scope: Scope<'_>,
    ) -> Result<Vec<ChunkHit>, VectorError> {
        let body = serde_json::json!({
            "query": vector,
            "limit": limit,
            "with_payload": true,
            "filter": search_filter(scope),
        });
        let raw = self
            .client
            .post(format!(
                "{}/collections/{}/points/query",
                self.base_url, self.collection
            ))
            .json(&body)
            .send()
            .await?;
        // A collection that was never created holds no chunks. Surfacing the raw
        // 404 made "this vault has not been indexed yet" read like a transport
        // failure in the leg status.
        if raw.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let response: serde_json::Value = raw.error_for_status()?.json().await?;
        Ok(response
            .pointer("/result/points")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|point| {
                let payload = point.get("payload")?;
                Some(ChunkHit {
                    path: payload.get("path")?.as_str()?.to_string(),
                    title: payload
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    breadcrumb: payload
                        .get("heading_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    chunk_index: payload
                        .get("chunk_index")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default() as usize,
                    text: payload
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    score: point
                        .get("score")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or_default(),
                })
            })
            .collect())
    }

    pub async fn delete(&self, slug: &str, file: &str) -> Result<(), VectorError> {
        let id = Uuid::new_v5(&POINT_NAMESPACE, format!("{slug}::{file}").as_bytes());
        self.client
            .post(format!(
                "{}/collections/{}/points/delete",
                self.base_url, self.collection
            ))
            .json(&serde_json::json!({"points": [id.to_string()]}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn is_current(&self, slug: &str, file: &str, sha: &str) -> Result<bool, VectorError> {
        let id = Uuid::new_v5(&POINT_NAMESPACE, format!("{slug}::{file}").as_bytes());
        let response = self
            .client
            .get(format!(
                "{}/collections/{}/points/{}",
                self.base_url, self.collection, id
            ))
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        let body: serde_json::Value = response.error_for_status()?.json().await?;
        Ok(body
            .pointer("/result/payload/sha")
            .and_then(serde_json::Value::as_str)
            == Some(sha))
    }

    pub async fn files(&self, slug: &str) -> Result<Vec<String>, VectorError> {
        let mut files = Vec::new();
        let mut offset = None;
        loop {
            let mut body = serde_json::json!({"limit": 256, "with_payload": true, "with_vector": false, "filter": {"must": [{"key": "slug", "match": {"value": slug}}]}});
            if let Some(value) = offset {
                body["offset"] = value;
            }
            let response = self
                .client
                .post(format!(
                    "{}/collections/{}/points/scroll",
                    self.base_url, self.collection
                ))
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json::<ScrollResponse>()
                .await?;
            files.extend(
                response
                    .result
                    .points
                    .into_iter()
                    .filter_map(|point| point.payload.and_then(|payload| payload.file)),
            );
            let next = response.result.next_page_offset;
            if next.is_none() {
                return Ok(files);
            }
            offset = next;
        }
    }

    /// Copy one slug's points into `destination`, unchanged, stamping `tenant`.
    ///
    /// For the tenancy migration. The vectors are carried across verbatim rather
    /// than recomputed: re-embedding 71 points is cheap, but re-embedding is also
    /// how an index silently changes meaning — the source vectors came from
    /// whatever model was configured when they were written, and the point of
    /// the migration is to move them, not to reinterpret them. Their `space`
    /// payload still records which model they belong to, so a later reindex can
    /// tell.
    ///
    /// Idempotent: point ids are a uuid5 of `slug::file`, so re-running upserts
    /// over the same ids instead of duplicating.
    pub async fn migrate_points(
        &self,
        destination: &str,
        slug: &str,
        tenant: &str,
    ) -> Result<u64, VectorError> {
        let mut moved = 0;
        let mut offset = None;
        loop {
            let mut body = serde_json::json!({
                "limit": 256,
                "with_payload": true,
                "with_vector": true,
                "filter": {"must": [{"key": "slug", "match": {"value": slug}}]},
            });
            if let Some(value) = offset {
                body["offset"] = value;
            }
            let page: serde_json::Value = self
                .client
                .post(format!(
                    "{}/collections/{}/points/scroll",
                    self.base_url, self.collection
                ))
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let points = page
                .pointer("/result/points")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            if points.is_empty() {
                return Ok(moved);
            }
            let rewritten: Vec<serde_json::Value> = points
                .iter()
                .map(|point| {
                    let mut payload = point
                        .get("payload")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    payload["tenant"] = serde_json::json!(tenant);
                    serde_json::json!({
                        "id": point.get("id"),
                        "vector": point.get("vector"),
                        "payload": payload,
                    })
                })
                .collect();
            moved += rewritten.len() as u64;
            self.client
                .put(format!(
                    "{}/collections/{}/points?wait=true",
                    self.base_url, destination
                ))
                .json(&serde_json::json!({"points": rewritten}))
                .send()
                .await?
                .error_for_status()?;
            let next = page.pointer("/result/next_page_offset").cloned();
            match next {
                Some(serde_json::Value::Null) | None => return Ok(moved),
                Some(value) => offset = Some(value),
            }
        }
    }

    pub async fn count(&self, slug: Option<&str>) -> Result<u64, VectorError> {
        let mut body = serde_json::json!({"exact": true});
        if let Some(slug) = slug {
            body["filter"] =
                serde_json::json!({"must": [{"key": "slug", "match": {"value": slug}}]});
        }
        let response: serde_json::Value = self
            .client
            .post(format!(
                "{}/collections/{}/points/count",
                self.base_url, self.collection
            ))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(response
            .pointer("/result/count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The search filter must pin the embedding space, not just the slug.
    ///
    /// Indexing is incremental, so a same-dimension model change leaves points
    /// from two spaces in one collection, and vectors from different models are
    /// not comparable — scoring a new query against old points produces
    /// confident nonsense. The `space` payload was being written and never read
    /// back, which left exactly that window open.
    #[test]
    fn the_search_filter_pins_the_embedding_space() {
        let body = search_filter(Scope::slug("b941b4f74fc6de19", "-home-alice"));
        let must = body["must"].as_array().expect("a must clause");
        let keys = must
            .iter()
            .filter_map(|clause| clause["key"].as_str())
            .collect::<Vec<_>>();
        assert!(keys.contains(&"space"), "space is not filtered: {body}");
        assert!(keys.contains(&"slug"), "slug is not filtered: {body}");

        // The space is no longer expressible as absent — it was an Option that
        // one call site happened to fill in, which is a guarantee resting on a
        // call site rather than on the type. Dropping the slug is still allowed.
        let every_store = search_filter(Scope::space("b941b4f74fc6de19"));
        let must = every_store["must"].as_array().expect("a must clause");
        assert_eq!(must.len(), 1);
        assert_eq!(must[0]["key"], "space");
    }

    /// Two tenants must never address the same collection, and neither may
    /// address the pre-tenancy one.
    ///
    /// This is the isolation boundary for vectors: there is no constructor that
    /// chooses a collection by itself, so a cross-tenant read requires naming
    /// the other tenant rather than omitting a filter.
    #[test]
    fn each_tenant_and_corpus_addresses_its_own_collection() {
        let config: engram_config::Config = serde_yaml::from_str(
            "vector_store: {url: 'http://127.0.0.1:6333'}\n\
             tenants:\n  work:\n    slugs: ['-a']\n  homelab:\n    slugs: ['-b']\n",
        )
        .unwrap();
        config.validate().unwrap();
        let name = |tenant: &str, corpus| {
            let tenant = engram_tenant::Tenant::resolve(&config, Some(tenant)).unwrap();
            QdrantClient::for_tenant(&config, &tenant, corpus).collection
        };

        let seen = [
            name("work", Corpus::Memory),
            name("work", Corpus::Wiki),
            name("homelab", Corpus::Memory),
            name("homelab", Corpus::Wiki),
        ];
        assert_eq!(seen[0], "engram_memory__work");
        assert_eq!(seen[1], "engram_wiki__work");

        let unique: std::collections::BTreeSet<_> = seen.iter().collect();
        assert_eq!(unique.len(), seen.len(), "collections collide: {seen:?}");
        for collection in &seen {
            assert_ne!(
                collection, "engram_memory",
                "a named tenant must not address the shared pre-tenancy collection"
            );
        }
    }
}
