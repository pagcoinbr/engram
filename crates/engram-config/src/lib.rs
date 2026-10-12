//! The typed view of `engram.yaml`.
//!
//! Only part of the file is modelled — the rest stays in the YAML and is preserved
//! verbatim on save — but everything the Rust binaries *act on* has to be here.
//! Fields that were missing caused real failures rather than mere omissions:
//! `local_enabled` is the documented master kill-switch and was ignored; the
//! `api_key` entries were in the shipped example and dropped, so authenticated
//! model and Qdrant Cloud calls went out unauthenticated; `recall.inject` was
//! parsed by the Python hook and hard-coded in the Rust one.

mod secret;
pub use secret::Secret;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path};
use thiserror::Error;
use url::Url;

pub const CONFIG_VERSION: u32 = 1;

/// Embedding providers implemented by the Rust crates. Anything else is a valid
/// engram configuration that the *Python* path has to serve — see
/// [`Config::rust_embedding_supported`].
pub const RUST_EMBED_PROVIDERS: [&str; 2] = ["llama_cpp", "openai"];

/// [`Config::embedding_space_id`] of the reference configuration
/// `{provider: llama_cpp, url: http://127.0.0.1:8081/v1, model: bge-m3, dim: 1024}`,
/// pinned so Rust and `bin/engram_llm.py` cannot drift apart. See the test
/// `embedding_space_id_matches_the_python_implementation`.
pub const PINNED_EMBEDDING_SPACE_ID: &str = "b941b4f74fc6de19";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub config_version: u32,
    /// The master switch. False means "do no automated memory work", and it
    /// outranks `vector_store.enabled` and everything else.
    #[serde(default = "default_true")]
    pub local_enabled: bool,
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default)]
    pub llama_cpp: LlamaCpp,
    #[serde(default)]
    pub embed: Embed,
    #[serde(default)]
    pub vector_store: VectorStore,
    #[serde(default)]
    pub graph: Graph,
    #[serde(default)]
    pub recall: Recall,
    /// Agent identities, each owning one Obsidian vault and a set of memory
    /// stores. See [`Config::tenancy_enabled`] for what an EMPTY map means —
    /// it is load-bearing, not a degenerate case.
    ///
    /// A `BTreeMap` rather than a `HashMap` so the daemon's per-tenant loop and
    /// every error message that lists tenants are deterministically ordered.
    #[serde(default)]
    pub tenants: BTreeMap<String, TenantConfig>,
}

/// One agent identity: which memory stores it owns, and which vault it reads.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TenantConfig {
    /// The memory stores (`projects/<slug>/memory`) this tenant owns. A slug
    /// may belong to exactly one tenant — see [`Config::validate`].
    #[serde(default)]
    pub slugs: Vec<String>,
    /// Absolute path to this tenant's Obsidian vault.
    ///
    /// Optional: a tenant may own memory stores and no vault at all. That is
    /// not a placeholder for the sake of it — it is the state every tenant is
    /// in during the migration, before any vault exists, and the tenant model
    /// has to be usable then.
    #[serde(default)]
    pub vault: String,
    /// The one subtree of the vault an agent may write to, relative to the
    /// vault root. Everything else is read-only to the agent.
    #[serde(default = "default_agent_subtree")]
    pub agent_subtree: String,
    /// Opt-in LLM fact extraction over wiki sections.
    ///
    /// Off by default because the cost is one generation call per section of
    /// every page, repeated whenever a page changes. Structural indexing
    /// (`[[links]]`, headings, tags) is pure parsing and always on.
    #[serde(default)]
    pub extract_facts: bool,
}

impl Default for TenantConfig {
    fn default() -> Self {
        Self {
            slugs: Vec::new(),
            vault: String::new(),
            agent_subtree: default_agent_subtree(),
            extract_facts: false,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct LlamaCpp {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub max_tokens: u32,
    #[serde(default)]
    pub api_key: Secret,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Embed {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_dimension")]
    pub dim: u32,
    /// Asymmetric models (bge, e5, nomic) need these, and applying them to only one
    /// side indexes documents and queries into different sub-spaces.
    #[serde(default)]
    pub query_prefix: String,
    #[serde(default)]
    pub document_prefix: String,
    #[serde(default)]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub api_key: Secret,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct VectorStore {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_qdrant_url")]
    pub url: String,
    #[serde(default = "default_collection")]
    pub collection: String,
    /// Base name for the wiki corpus, kept separate from `collection` because
    /// the two hold different shapes: one point per memory file versus many
    /// chunks per wiki document. Both are suffixed per tenant.
    #[serde(default = "default_wiki_collection")]
    pub wiki_collection: String,
    #[serde(default)]
    pub timeout_seconds: u64,
    /// Qdrant Cloud. Sent as the `api-key` header.
    #[serde(default)]
    pub api_key: Secret,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Graph {
    /// Defaults to `graphiti_compat`, NOT `native`.
    ///
    /// `install.sh` never adds a `graph:` block to an existing `engram.yaml`, so
    /// almost every upgraded install reaches this default. Defaulting to `native`
    /// silently moved those users off their populated Graphiti index and onto the
    /// unproven native path; the safe default is the one they were already running.
    #[serde(default = "default_graph_backend")]
    pub backend: String,
    #[serde(default = "default_neo4j_uri")]
    pub neo4j_uri: String,
    #[serde(default = "default_neo4j_database")]
    pub neo4j_database: String,
    #[serde(default = "default_neo4j_user")]
    pub neo4j_user: String,
    /// Normally left unset: the installer writes the password to `graph/.env`.
    /// [`Graph::credentials`] reads both.
    #[serde(default)]
    pub neo4j_password: Secret,
    /// An explicit HTTPS transaction endpoint, required for a non-loopback Neo4j
    /// because basic-auth credentials must not cross a network in plaintext.
    #[serde(default)]
    pub neo4j_http_url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Recall {
    #[serde(default = "default_true")]
    pub scope_to_slug: bool,
    /// Ceiling for one recall, used to bound the Graphiti child process.
    ///
    /// Distinct from `inject.timeout_ms`: that is the per-PROMPT budget the hook
    /// enforces on top of this, and it is deliberately much shorter (a prompt that
    /// waits is a worse outcome than a prompt without recall). An explicit CLI,
    /// MCP or API call is allowed to wait — real Graphiti recall measures ~4s on a
    /// populated graph, so sharing the hook's 2.5s would have made every one of
    /// those calls fail.
    #[serde(default = "default_recall_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub hybrid: Hybrid,
    #[serde(default)]
    pub inject: Inject,
}

impl Default for Recall {
    fn default() -> Self {
        Self {
            scope_to_slug: true,
            timeout_ms: default_recall_timeout_ms(),
            hybrid: Hybrid::default(),
            inject: Inject::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Hybrid {
    #[serde(default = "default_k_rrf")]
    pub k_rrf: f64,
    #[serde(default = "default_k")]
    pub default_k: usize,
    /// Per-leg RRF weights, e.g. `{graph: 1.0, vector: 1.0, keyword: 0.8}`.
    #[serde(default)]
    pub weights: BTreeMap<String, f64>,
}

/// `recall.inject` — the prompt hook's budget. The hook runs on EVERY prompt, so
/// these are the numbers that decide whether a stalled backend delays the user.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Inject {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_inject_k")]
    pub k: usize,
    #[serde(default = "default_max_facts")]
    pub max_facts: usize,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_true() -> bool {
    true
}
fn default_backend() -> String {
    "ollama".into()
}
fn default_dimension() -> u32 {
    768
}
fn default_graph_backend() -> String {
    "graphiti_compat".into()
}
fn default_neo4j_uri() -> String {
    "bolt://127.0.0.1:7687".into()
}
fn default_neo4j_database() -> String {
    "neo4j".into()
}
fn default_neo4j_user() -> String {
    "neo4j".into()
}
fn default_qdrant_url() -> String {
    "http://127.0.0.1:6333".into()
}
fn default_collection() -> String {
    "engram_memory".into()
}
fn default_wiki_collection() -> String {
    "engram_wiki".into()
}
fn default_k_rrf() -> f64 {
    60.0
}
fn default_k() -> usize {
    6
}
fn default_inject_k() -> usize {
    4
}
fn default_max_facts() -> usize {
    6
}
fn default_timeout_ms() -> u64 {
    2500
}
fn default_agent_subtree() -> String {
    "_agent".into()
}
fn default_recall_timeout_ms() -> u64 {
    15_000
}

impl Default for Embed {
    fn default() -> Self {
        Self {
            provider: String::new(),
            url: String::new(),
            model: String::new(),
            dim: default_dimension(),
            query_prefix: String::new(),
            document_prefix: String::new(),
            timeout_seconds: 0,
            api_key: Secret::default(),
        }
    }
}

impl Default for VectorStore {
    fn default() -> Self {
        Self {
            enabled: false,
            url: default_qdrant_url(),
            collection: default_collection(),
            wiki_collection: default_wiki_collection(),
            timeout_seconds: 0,
            api_key: Secret::default(),
        }
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self {
            backend: default_graph_backend(),
            neo4j_uri: default_neo4j_uri(),
            neo4j_database: default_neo4j_database(),
            neo4j_user: default_neo4j_user(),
            neo4j_password: Secret::default(),
            neo4j_http_url: String::new(),
        }
    }
}

impl Default for Hybrid {
    fn default() -> Self {
        Self {
            k_rrf: default_k_rrf(),
            default_k: default_k(),
            weights: BTreeMap::new(),
        }
    }
}

impl Default for Inject {
    fn default() -> Self {
        Self {
            enabled: true,
            k: default_inject_k(),
            max_facts: default_max_facts(),
            timeout_ms: default_timeout_ms(),
        }
    }
}

/// Resolved Neo4j connection details, including the password from `graph/.env`.
#[derive(Clone, Debug)]
pub struct GraphCredentials {
    pub uri: String,
    pub database: String,
    pub user: String,
    pub password: Secret,
    pub http_url: Option<String>,
}

impl Graph {
    /// Resolve Neo4j credentials the way the rest of the system does.
    ///
    /// Precedence per field: environment (`NEO4J_URI`, `NEO4J_DATABASE`,
    /// `NEO4J_USER`, `NEO4J_PASSWORD`, `NEO4J_HTTP_URL`), then `engram.yaml`, then
    /// the installer-managed `graph/.env` for the password. The user was previously
    /// hard-coded to `"neo4j"` and the password read from the environment only,
    /// which fails under the standard installer layout where it lives in
    /// `graph/.env`.
    pub fn credentials(&self) -> GraphCredentials {
        let env = |key: &str| {
            std::env::var(key)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let password = env("NEO4J_PASSWORD")
            .or_else(|| self.neo4j_password.present().map(str::to_string))
            .or_else(password_from_graph_env)
            .unwrap_or_default();
        let http_url = env("NEO4J_HTTP_URL").or_else(|| {
            let configured = self.neo4j_http_url.trim();
            (!configured.is_empty()).then(|| configured.to_string())
        });
        GraphCredentials {
            uri: env("NEO4J_URI").unwrap_or_else(|| self.neo4j_uri.clone()),
            database: env("NEO4J_DATABASE").unwrap_or_else(|| self.neo4j_database.clone()),
            user: env("NEO4J_USER").unwrap_or_else(|| self.neo4j_user.clone()),
            password: Secret::new(password),
            http_url,
        }
    }
}

/// Parse `NEO4J_PASSWORD=` out of `<graph dir>/.env`, which `install.sh` writes
/// with mode 600. Mirrors `bin/memory_recall.py::_neo4j_password`.
fn password_from_graph_env() -> Option<String> {
    let text = fs::read_to_string(engram_paths::graph_dir().join(".env")).ok()?;
    for line in text.lines() {
        let line = line
            .trim()
            .strip_prefix("export ")
            .unwrap_or(line.trim())
            .trim();
        if let Some(value) = line.strip_prefix("NEO4J_PASSWORD=") {
            let value = value.trim().trim_matches('"').trim_matches('\'').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ModelProfile {
    pub role: &'static str,
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub expected_dimension: Option<u32>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct RecommendedEmbedder {
    pub id: &'static str,
    pub label: &'static str,
    pub dimension: u32,
    pub use_case: &'static str,
}

pub fn recommended_embedders() -> Vec<RecommendedEmbedder> {
    vec![
        RecommendedEmbedder {
            id: "bge-m3",
            label: "BGE-M3",
            dimension: 1024,
            use_case: "Current multilingual baseline.",
        },
        RecommendedEmbedder {
            id: "qwen3-embedding-0.6b",
            label: "Qwen3-Embedding-0.6B",
            dimension: 1024,
            use_case: "Alternative to benchmark for semantic precision.",
        },
        RecommendedEmbedder {
            id: "embeddinggemma-300m",
            label: "EmbeddingGemma-300M",
            dimension: 768,
            use_case: "Compact multilingual deployment.",
        },
        RecommendedEmbedder {
            id: "nomic-embed-text-v1.5",
            label: "Nomic Embed Text v1.5",
            dimension: 768,
            use_case: "Established lightweight local option.",
        },
    ]
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read config: {0}")]
    Read(#[from] std::io::Error),
    #[error("could not parse YAML: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("{0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let mut config: Self = serde_yaml::from_str(&fs::read_to_string(path)?)?;
        if config.config_version == 0 {
            config.config_version = CONFIG_VERSION;
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.embed.dim == 0 {
            return Err(ConfigError::Invalid(
                "embed.dim must be greater than zero".into(),
            ));
        }
        if !self.embed.url.is_empty() {
            endpoint(&self.embed.url, "embed.url")?;
        }
        if !self.llama_cpp.url.is_empty() {
            endpoint(&self.llama_cpp.url, "llama_cpp.url")?;
        }
        if !self.vector_store.url.is_empty() {
            endpoint(&self.vector_store.url, "vector_store.url")?;
        }
        // `embed_endpoint()` documents a fallback to `llama_cpp.url`, so require
        // the *effective* endpoint rather than `embed.url` specifically —
        // rejecting an empty `embed.url` outright contradicted the fallback and
        // made the documented single-endpoint setup unloadable.
        // Through embed_provider() so the check is case-insensitive and sees
        // the same provider the embedding call will actually use.
        if self.embed_provider() == "llama_cpp" && self.embed_endpoint().is_empty() {
            return Err(ConfigError::Invalid(
                "embed.url (or llama_cpp.url) is required for llama_cpp/openai embeddings".into(),
            ));
        }
        if !matches!(self.graph.backend.as_str(), "native" | "graphiti_compat") {
            return Err(ConfigError::Invalid(
                "graph.backend must be native or graphiti_compat".into(),
            ));
        }
        // Deliberately NOT rejected here: `graph.backend: native` without the
        // endpoints the Rust sync needs.
        //
        // That check lived here briefly and was a mistake. `validate()` runs on
        // every load, so it did not merely warn about an unwritable index — it
        // made the config unloadable for every Rust consumer, including read-only
        // recall against an already-populated native index, the parity evaluator,
        // and `ENGRAM_GRAPH_BACKEND=graphiti_compat`, which could no longer rescue
        // the install because validation happens before the override is read. A
        // load-time error is the wrong severity for a condition that only affects
        // the nightly writer; the daemon reports it at task time, where the
        // operator can act on it and nothing else breaks.
        // See `task_graph` in daemon/engram-daemon.py.
        if !self.graph.neo4j_http_url.is_empty() {
            endpoint(&self.graph.neo4j_http_url, "graph.neo4j_http_url")?;
        }
        self.validate_tenants()?;
        Ok(())
    }

    /// Whether this install uses tenancy at all.
    ///
    /// An empty `tenants:` map is not a degenerate case, it is the upgrade
    /// path: every existing `engram.yaml` has no such block, and those installs
    /// must keep working exactly as before (slug-scoped, no tenant required).
    /// Once an operator declares even one tenant, `--tenant` becomes mandatory
    /// on every binary — including when only one tenant exists, because
    /// "obviously the only one" is precisely the kind of default that makes a
    /// cross-tenant read possible later.
    pub fn tenancy_enabled(&self) -> bool {
        !self.tenants.is_empty()
    }

    /// Which tenant owns `slug`, if tenancy is in use.
    pub fn tenant_of_slug(&self, slug: &str) -> Option<&str> {
        self.tenants
            .iter()
            .find(|(_, tenant)| tenant.slugs.iter().any(|owned| owned == slug))
            .map(|(name, _)| name.as_str())
    }

    /// Configured slugs that no tenant claims — what the migration has to get
    /// an answer for before it can run.
    pub fn unassigned_slugs<'a>(&self, present: &[&'a str]) -> Vec<&'a str> {
        present
            .iter()
            .copied()
            .filter(|slug| self.tenant_of_slug(slug).is_none())
            .collect()
    }

    fn validate_tenants(&self) -> Result<(), ConfigError> {
        let invalid = |message: String| ConfigError::Invalid(message);
        // slug -> the tenant that claimed it first, so a duplicate names both.
        let mut owners: BTreeMap<&str, &str> = BTreeMap::new();
        let mut vaults: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, tenant) in &self.tenants {
            if name.trim().is_empty() {
                return Err(invalid("a tenant name may not be empty".into()));
            }
            // Lowercase alphanumerics and dashes only. The name becomes a Qdrant
            // collection suffix and a Neo4j property value, both case-sensitive:
            // allowing `Work` and `work` would make two tenants that look like
            // one in every log line and config listing.
            if let Some(bad) = name
                .chars()
                .find(|ch| !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && *ch != '-')
            {
                return Err(invalid(format!(
                    "tenant '{name}' contains {bad:?}; names may use only lowercase letters, digits and '-'"
                )));
            }
            if tenant.slugs.is_empty() && tenant.vault.trim().is_empty() {
                return Err(invalid(format!(
                    "tenant '{name}' owns no slugs and has no vault; it would never serve anything"
                )));
            }
            for slug in &tenant.slugs {
                let slug = slug.trim();
                if slug.is_empty() {
                    return Err(invalid(format!("tenant '{name}' lists an empty slug")));
                }
                // THE isolation rule. Two tenants sharing a store is exactly the
                // cross-tenant read this whole model exists to prevent, so it is
                // a config error rather than a last-writer-wins resolution.
                if let Some(first) = owners.insert(slug, name) {
                    return Err(invalid(format!(
                        "slug '{slug}' is claimed by both '{first}' and '{name}'; \
                         a memory store belongs to exactly one tenant"
                    )));
                }
            }
            let vault = tenant.vault.trim();
            if !vault.is_empty() {
                if !Path::new(vault).is_absolute() {
                    return Err(invalid(format!(
                        "tenant '{name}' vault must be an absolute path, got '{vault}'"
                    )));
                }
                if let Some(first) = vaults.insert(vault, name) {
                    return Err(invalid(format!(
                        "vault '{vault}' is shared by '{first}' and '{name}'; \
                         a vault belongs to exactly one tenant"
                    )));
                }
            }
            let subtree = tenant.agent_subtree.trim();
            if subtree.is_empty() {
                return Err(invalid(format!(
                    "tenant '{name}' agent_subtree may not be empty; \
                     it is the only place an agent may write"
                )));
            }
            // A subtree that escapes the vault would make the write boundary
            // meaningless, so reject the shape outright rather than relying on
            // the runtime check in engram-tenant to catch it every time.
            if Path::new(subtree).is_absolute()
                || subtree
                    .split(['/', '\\'])
                    .any(|part| part == ".." || part == "~")
            {
                return Err(invalid(format!(
                    "tenant '{name}' agent_subtree '{subtree}' must be a relative path \
                     inside the vault, with no '..'"
                )));
            }
        }
        Ok(())
    }

    /// The effective embedding provider, matching `engram_llm._embed_provider`:
    /// an explicit `embed.provider` wins (with `openai` an alias for `llama_cpp`),
    /// otherwise Ollama when generating via Ollama, else the CPU fastembed path.
    ///
    /// Case-insensitive, because `engram_llm._embed_provider` lowercases before
    /// matching. Matching case-sensitively here made `provider: OpenAI` resolve to
    /// `llama_cpp` in Python and `fastembed` in Rust — two different embedding
    /// spaces, two different fingerprints, and an index each side believed the
    /// other had corrupted.
    pub fn embed_provider(&self) -> &'static str {
        match self.embed.provider.trim().to_ascii_lowercase().as_str() {
            "openai" | "llama_cpp" => "llama_cpp",
            "ollama" => "ollama",
            "fastembed" => "fastembed",
            _ if self.backend.trim().eq_ignore_ascii_case("ollama") => "ollama",
            _ => "fastembed",
        }
    }

    /// Whether the Rust indexer and native graph sync can serve this config.
    ///
    /// They implement exactly one embedding transport: an OpenAI-compatible
    /// `/v1/embeddings` endpoint. The daemon and save hook used to prefer the Rust
    /// binary whenever it merely existed, so an Ollama or FastEmbed install — both
    /// fully supported configurations — had its indexing routed to a binary that
    /// could not perform it. Callers gate on this and fall back to Python.
    pub fn rust_embedding_supported(&self) -> bool {
        RUST_EMBED_PROVIDERS.contains(&self.embed_provider()) && !self.embed_endpoint().is_empty()
    }

    /// Whether the Rust native graph sync can perform *fact extraction* here.
    ///
    /// Separate from [`Config::rust_embedding_supported`] because the two halves
    /// of a native sync use different services and a config can satisfy one and
    /// not the other: gating on embeddings alone let the sync run with no
    /// generation endpoint at all and fail on the URL parse.
    ///
    /// The requirement is exactly `llama_cpp.url`, which is what the sync's
    /// reasoning client is built from — NOT `backend`. Requiring
    /// `backend: llama_cpp`/`openai` as well looked stricter and was simply wrong:
    /// `backend` selects the *pipeline's* generation backend (the Python
    /// harvest/distill path), while the Rust sync has always used `llama_cpp.url`
    /// directly. A perfectly ordinary `backend: ollama` install with an
    /// OpenAI-compatible `llama_cpp.url` alongside it was working, and that gate
    /// declared it unserviceable.
    pub fn rust_reasoning_supported(&self) -> bool {
        !self.llama_cpp.url.trim().is_empty()
    }

    /// Whether the Rust native graph sync can serve this config at all: it needs
    /// both legs.
    pub fn rust_native_sync_supported(&self) -> bool {
        self.rust_embedding_supported() && self.rust_reasoning_supported()
    }

    /// Where embeddings are requested: `embed.url`, else the generation endpoint.
    pub fn embed_endpoint(&self) -> &str {
        if self.embed.url.is_empty() {
            &self.llama_cpp.url
        } else {
            &self.embed.url
        }
    }

    pub fn embed_timeout_seconds(&self) -> u64 {
        match (self.embed.timeout_seconds, self.llama_cpp.timeout_seconds) {
            (0, 0) => 90,
            (0, fallback) => fallback,
            (configured, _) => configured,
        }
    }

    /// An identity for the embedding space, so a model swap invalidates the index.
    ///
    /// Freshness used to hash only memory content. Switching to a different model
    /// of the SAME dimension therefore left every stored vector looking current:
    /// the UI warned about reindexing, but an ordinary run still skipped every
    /// record, leaving two models' vectors mixed in one collection. Anything that
    /// changes what a vector *means* has to be in here.
    pub fn embedding_space_id(&self) -> String {
        let mut hasher = Sha256::new();
        for part in [
            self.embed_provider(),
            self.embed_endpoint(),
            self.embed.model.as_str(),
            self.embed.query_prefix.as_str(),
            self.embed.document_prefix.as_str(),
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0u8]);
        }
        hasher.update(self.embed.dim.to_le_bytes());
        format!("{:x}", hasher.finalize())[..16].to_string()
    }

    /// Prefix a document before indexing it.
    pub fn document_text(&self, text: &str) -> String {
        format!("{}{}", self.embed.document_prefix, text)
    }

    /// Prefix a query before searching with it.
    pub fn query_text(&self, text: &str) -> String {
        format!("{}{}", self.embed.query_prefix, text)
    }

    pub fn profiles(&self) -> Vec<ModelProfile> {
        let generation = ModelProfile {
            role: "reasoning",
            provider: self.backend.clone(),
            endpoint: self.llama_cpp.url.clone(),
            model: self.llama_cpp.model.clone(),
            expected_dimension: None,
        };
        let embedding = ModelProfile {
            role: "embedding",
            provider: self.embed.provider.clone(),
            endpoint: self.embed_endpoint().to_string(),
            model: self.embed.model.clone(),
            expected_dimension: Some(self.embed.dim),
        };
        vec![generation, embedding]
    }
}

fn endpoint(value: &str, name: &str) -> Result<(), ConfigError> {
    let parsed = Url::parse(value)
        .map_err(|_| ConfigError::Invalid(format!("{name} must be an absolute HTTP(S) URL")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ConfigError::Invalid(format!("{name} must use HTTP(S)")));
    }
    // Credentials belong in `api_key`, which is wrapped in `Secret` and redacted
    // on every serialization path. A URL carrying userinfo
    // (`https://user:pass@host/v1`) smuggles a password past all of that: the
    // endpoint is published verbatim by /api/v1/status and the model editor.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ConfigError::Invalid(format!(
            "{name} must not embed credentials in the URL; use the matching api_key instead"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llama_cpp_embedding_profile_keeps_its_space() {
        let config: Config = serde_yaml::from_str("backend: llama_cpp\nllama_cpp: {url: http://ai/v1, model: qwen}\nembed: {provider: llama_cpp, url: http://embed/v1, model: bge-m3, dim: 1024}\n").unwrap();
        config.validate().unwrap();
        assert_eq!(config.profiles()[1].expected_dimension, Some(1024));
        assert_eq!(config.profiles()[1].endpoint, "http://embed/v1");
    }

    #[test]
    fn llama_cpp_embeddings_require_an_endpoint() {
        let config: Config =
            serde_yaml::from_str("embed: {provider: llama_cpp, model: bge-m3, dim: 1024}\n")
                .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn catalog_includes_the_bge_m3_baseline() {
        assert!(
            recommended_embedders()
                .iter()
                .any(|item| item.id == "bge-m3" && item.dimension == 1024)
        );
    }

    /// An upgraded install has no `graph:` block. It must keep reading the Graphiti
    /// index it already populated, not silently switch to the native path.
    #[test]
    fn a_missing_graph_block_stays_on_graphiti() {
        let config: Config = serde_yaml::from_str("backend: ollama\n").unwrap();
        assert_eq!(config.graph.backend, "graphiti_compat");
        assert!(config.local_enabled, "the master switch defaults on");
        assert!(config.recall.inject.enabled);
        assert_eq!(config.recall.inject.timeout_ms, 2500);
        assert_eq!(config.recall.inject.k, 4);
    }

    #[test]
    fn rust_indexing_is_only_claimed_for_providers_it_implements() {
        let openai: Config =
            serde_yaml::from_str("embed: {provider: llama_cpp, url: http://e/v1, dim: 1024}\n")
                .unwrap();
        assert!(openai.rust_embedding_supported());

        for yaml in [
            "embed: {provider: ollama, dim: 768}\n",
            "embed: {provider: fastembed, dim: 768}\n",
            // auto-selected: no provider named at all
            "backend: ollama\n",
            // llama_cpp with no endpoint cannot be served either
            "embed: {provider: llama_cpp, dim: 1024}\n",
        ] {
            let config: Config = serde_yaml::from_str(yaml).unwrap();
            assert!(
                !config.rust_embedding_supported(),
                "claimed support for {yaml:?} (provider {})",
                config.embed_provider()
            );
        }
    }

    #[test]
    fn embedding_space_changes_with_anything_that_changes_a_vector() {
        let base: Config = serde_yaml::from_str(
            "embed: {provider: llama_cpp, url: http://e/v1, model: bge-m3, dim: 1024}\n",
        )
        .unwrap();
        let id = base.embedding_space_id();

        // same dimension, different model — the case content hashing missed
        let other_model: Config = serde_yaml::from_str(
            "embed: {provider: llama_cpp, url: http://e/v1, model: qwen3-embedding-0.6b, dim: 1024}\n",
        )
        .unwrap();
        assert_ne!(id, other_model.embedding_space_id());

        for yaml in [
            "embed: {provider: llama_cpp, url: http://other/v1, model: bge-m3, dim: 1024}\n",
            "embed: {provider: llama_cpp, url: http://e/v1, model: bge-m3, dim: 768}\n",
            "embed: {provider: llama_cpp, url: http://e/v1, model: bge-m3, dim: 1024, query_prefix: 'query: '}\n",
            "embed: {provider: llama_cpp, url: http://e/v1, model: bge-m3, dim: 1024, document_prefix: 'passage: '}\n",
        ] {
            let changed: Config = serde_yaml::from_str(yaml).unwrap();
            assert_ne!(
                id,
                changed.embedding_space_id(),
                "space unchanged for {yaml:?}"
            );
        }

        // and it is stable for an identical configuration
        let same: Config = serde_yaml::from_str(
            "embed: {provider: openai, url: http://e/v1, model: bge-m3, dim: 1024}\n",
        )
        .unwrap();
        assert_eq!(id, same.embedding_space_id(), "openai aliases to llama_cpp");
    }

    /// The Rust and Python fingerprints must be byte-identical.
    ///
    /// Both implementations write freshness state for the SAME Qdrant collection,
    /// so if they disagreed each would see the other's records as belonging to a
    /// foreign space and re-embed everything on every run, forever. A literal
    /// digest pinned from both sides is the only form of this check that cannot
    /// drift: `tests/test_embed_space.py` asserts the same constant.
    /// Credentials belong in `api_key`, which is wrapped in `Secret`. A URL
    /// carrying userinfo smuggled a password past all of that — the endpoint is
    /// published verbatim by /api/v1/status and the model editor, so the one
    /// field guaranteed to be visible became a credential channel.
    #[test]
    fn urls_may_not_carry_credentials() {
        for yaml in [
            "embed: {provider: llama_cpp, url: 'https://user:pass@host/v1', dim: 1024}\n",
            "embed: {provider: llama_cpp, url: 'https://token@host/v1', dim: 1024}\n",
            "llama_cpp: {url: 'https://user:pass@host/v1'}\nembed: {dim: 1024}\n",
            "vector_store: {url: 'http://user:pass@host:6333'}\nembed: {dim: 1024}\n",
        ] {
            let config: Config = serde_yaml::from_str(yaml).unwrap();
            let error = config
                .validate()
                .expect_err(&format!("accepted credentials in {yaml:?}"));
            assert!(
                format!("{error}").contains("must not embed credentials"),
                "{error}"
            );
        }
        // the same URLs without userinfo are fine
        let clean: Config = serde_yaml::from_str(
            "embed: {provider: llama_cpp, url: 'https://host/v1', dim: 1024}\n",
        )
        .unwrap();
        clean.validate().unwrap();
    }

    /// Native sync needs an embedding endpoint AND a generation endpoint.
    ///
    /// Gating on embeddings alone let the sync run with no generation endpoint and
    /// fail on the URL parse. The requirement is `llama_cpp.url` specifically —
    /// what the sync's reasoning client is actually built from — and NOT
    /// `backend`, which selects the Python pipeline's generation backend. A first
    /// attempt at this gate required both and wrongly declared an ordinary
    /// `backend: ollama` install with an OpenAI-compatible `llama_cpp.url`
    /// unserviceable.
    #[test]
    fn native_sync_requires_an_endpoint_for_each_half() {
        let embed_ok = "embed: {provider: llama_cpp, url: 'http://e/v1', dim: 1024}";

        for backend in ["llama_cpp", "openai", "ollama", "claude"] {
            let config: Config = serde_yaml::from_str(&format!(
                "backend: {backend}\nllama_cpp: {{url: 'http://g/v1'}}\n{embed_ok}\n"
            ))
            .unwrap();
            assert!(
                config.rust_native_sync_supported(),
                "backend {backend:?} has both endpoints and must be serviceable:                  `backend` is the Python pipeline's generation choice, not the                  endpoint this sync extracts against"
            );
        }

        // No generation endpoint: the half that would fail on a URL parse.
        let no_generation: Config =
            serde_yaml::from_str(&format!("backend: llama_cpp\n{embed_ok}\n")).unwrap();
        assert!(!no_generation.rust_native_sync_supported());
        assert!(!no_generation.rust_reasoning_supported());
        assert!(
            no_generation.rust_embedding_supported(),
            "the embedding half alone is satisfied, which is why one check was not enough"
        );

        // No embedding endpoint Rust can use.
        let no_embedding: Config = serde_yaml::from_str(
            "backend: ollama\nllama_cpp: {url: 'http://g/v1'}\nembed: {provider: ollama, dim: 768}\n",
        )
        .unwrap();
        assert!(!no_embedding.rust_native_sync_supported());
        assert!(no_embedding.rust_reasoning_supported());
        assert!(!no_embedding.rust_embedding_supported());
    }

    /// An unwritable native index must not make the config unloadable.
    ///
    /// Rejecting it in `validate()` broke read-only recall against an existing
    /// native index, the parity evaluator, and the `ENGRAM_GRAPH_BACKEND` escape
    /// hatch — validation runs before the override is consulted. The daemon
    /// reports it at task time instead.
    #[test]
    fn a_native_backend_without_a_rust_writer_still_loads() {
        let config: Config = serde_yaml::from_str(
            "graph: {backend: native}\nbackend: ollama\nembed: {provider: ollama, dim: 768}\n",
        )
        .unwrap();
        config
            .validate()
            .expect("an unwritable native index is a daemon-time concern, not a load error");
        assert!(!config.rust_native_sync_supported());
    }

    #[test]
    fn provider_resolution_is_case_insensitive() {
        for spelling in ["openai", "OpenAI", "OPENAI", " llama_cpp ", "LLAMA_CPP"] {
            let config: Config = serde_yaml::from_str(&format!(
                "embed: {{provider: '{spelling}', url: 'http://e/v1', model: m, dim: 1024}}\n"
            ))
            .unwrap();
            assert_eq!(
                config.embed_provider(),
                "llama_cpp",
                "{spelling:?} did not resolve to llama_cpp"
            );
            assert!(config.rust_embedding_supported(), "{spelling:?}");
        }
        for spelling in ["Ollama", "OLLAMA"] {
            let config: Config =
                serde_yaml::from_str(&format!("embed: {{provider: '{spelling}', dim: 768}}\n"))
                    .unwrap();
            assert_eq!(config.embed_provider(), "ollama", "{spelling:?}");
        }
        // the generation backend too, for the reasoning gate
        let config: Config = serde_yaml::from_str(
            "backend: LLAMA_CPP\nllama_cpp: {url: 'http://g/v1'}\nembed: {dim: 1024}\n",
        )
        .unwrap();
        assert!(config.rust_reasoning_supported());
    }

    #[test]
    fn embedding_space_id_matches_the_python_implementation() {
        let config: Config = serde_yaml::from_str(
            "embed: {provider: llama_cpp, url: 'http://127.0.0.1:8081/v1', model: bge-m3, dim: 1024}\n",
        )
        .unwrap();
        assert_eq!(
            config.embedding_space_id(),
            PINNED_EMBEDDING_SPACE_ID,
            "fingerprint drifted from bin/engram_llm.py::embedding_space_id"
        );
    }

    #[test]
    fn prefixes_are_applied_per_side() {
        let config: Config = serde_yaml::from_str(
            "embed: {provider: llama_cpp, url: http://e/v1, dim: 1024, query_prefix: 'query: ', document_prefix: 'passage: '}\n",
        )
        .unwrap();
        assert_eq!(config.query_text("hello"), "query: hello");
        assert_eq!(config.document_text("hello"), "passage: hello");
    }

    /// The API serializes the config straight to the browser under a "secrets
    /// redacted" heading. This is that promise, enforced.
    #[test]
    fn credentials_never_appear_in_a_serialized_config() {
        let config: Config = serde_yaml::from_str(
            "llama_cpp: {url: 'http://ai/v1', api_key: 'sk-generation'}\n\
             embed: {provider: llama_cpp, url: 'http://e/v1', dim: 1024, api_key: 'sk-embed'}\n\
             vector_store: {enabled: true, url: 'https://q.cloud:6333', api_key: 'qdrant-cloud-key'}\n\
             graph: {backend: native, neo4j_password: 'neo-secret'}\n",
        )
        .unwrap();
        // they parsed...
        assert_eq!(config.embed.api_key.present(), Some("sk-embed"));
        assert_eq!(
            config.vector_store.api_key.present(),
            Some("qdrant-cloud-key")
        );

        // ...and they do not come back out
        for rendered in [
            serde_json::to_string(&config).unwrap(),
            serde_yaml::to_string(&config).unwrap(),
            format!("{config:?}"),
        ] {
            for secret in [
                "sk-generation",
                "sk-embed",
                "qdrant-cloud-key",
                "neo-secret",
            ] {
                assert!(!rendered.contains(secret), "leaked {secret} in {rendered}");
            }
        }
    }

    #[test]
    fn graph_credentials_prefer_the_environment_then_yaml() {
        let config: Config = serde_yaml::from_str(
            "graph: {backend: native, neo4j_uri: 'bolt://db:7687', neo4j_user: 'svc', neo4j_password: 'from-yaml'}\n",
        )
        .unwrap();
        let creds = config.graph.credentials();
        assert_eq!(creds.uri, "bolt://db:7687");
        assert_eq!(
            creds.user, "svc",
            "the user is no longer hard-coded to neo4j"
        );
        assert_eq!(creds.password.expose(), "from-yaml");
        assert_eq!(creds.http_url, None);
    }

    /// The upgrade path. Every `engram.yaml` in existence has no `tenants:`
    /// block, and those installs must keep working untouched — so an empty map
    /// means "tenancy off", never "no tenant matched".
    #[test]
    fn an_absent_tenants_block_leaves_tenancy_off() {
        let config: Config = serde_yaml::from_str("backend: ollama\n").unwrap();
        config.validate().unwrap();
        assert!(!config.tenancy_enabled());
        assert_eq!(config.tenant_of_slug("-root"), None);
    }

    /// The isolation rule, as a config error.
    ///
    /// Two tenants sharing a memory store IS the cross-tenant read the tenant
    /// model exists to prevent. Resolving it by last-writer-wins would produce
    /// exactly the leak, quietly, so it has to fail the load.
    #[test]
    fn a_memory_store_belongs_to_exactly_one_tenant() {
        let shared: Config = serde_yaml::from_str(
            "tenants:\n  work:\n    slugs: ['-root-a', '-root-shared']\n  \
             homelab:\n    slugs: ['-root-b', '-root-shared']\n",
        )
        .unwrap();
        let error = shared
            .validate()
            .expect_err("a shared slug must not load at all");
        let error = format!("{error}");
        assert!(error.contains("-root-shared"), "{error}");
        // and it must name BOTH claimants, or the operator cannot fix it
        assert!(
            error.contains("work") && error.contains("homelab"),
            "{error}"
        );

        // the same two tenants with disjoint stores are fine
        let clean: Config = serde_yaml::from_str(
            "tenants:\n  work:\n    slugs: ['-root-a']\n  homelab:\n    slugs: ['-root-b']\n",
        )
        .unwrap();
        clean.validate().unwrap();
        assert!(clean.tenancy_enabled());
        assert_eq!(clean.tenant_of_slug("-root-a"), Some("work"));
        assert_eq!(clean.tenant_of_slug("-root-b"), Some("homelab"));
        assert_eq!(clean.tenant_of_slug("-root-never-assigned"), None);
    }

    /// A shared vault is the same leak by the other door.
    #[test]
    fn a_vault_belongs_to_exactly_one_tenant() {
        let config: Config = serde_yaml::from_str(
            "tenants:\n  work:\n    vault: /vaults/shared\n  homelab:\n    vault: /vaults/shared\n",
        )
        .unwrap();
        let error = format!("{}", config.validate().unwrap_err());
        assert!(error.contains("shared by"), "{error}");
    }

    #[test]
    fn tenant_names_and_paths_are_constrained() {
        for (yaml, expect) in [
            // uppercase would make `Work` and `work` two tenants that read as one
            ("tenants:\n  Work:\n    slugs: ['-a']\n", "lowercase"),
            ("tenants:\n  wo rk:\n    slugs: ['-a']\n", "lowercase"),
            // a relative vault resolves against whatever cwd the binary had
            ("tenants:\n  work:\n    vault: vaults/work\n", "absolute"),
            // an empty tenant is a typo, not a configuration
            ("tenants:\n  work: {}\n", "never serve anything"),
            ("tenants:\n  work:\n    slugs: ['']\n", "empty slug"),
            // a subtree that escapes the vault voids the write boundary
            (
                "tenants:\n  work:\n    vault: /v\n    agent_subtree: '../outside'\n",
                "relative path",
            ),
            (
                "tenants:\n  work:\n    vault: /v\n    agent_subtree: '/etc'\n",
                "relative path",
            ),
            (
                "tenants:\n  work:\n    vault: /v\n    agent_subtree: ''\n",
                "may not be empty",
            ),
        ] {
            let config: Config = serde_yaml::from_str(yaml).unwrap();
            let error = format!(
                "{}",
                config.validate().expect_err(&format!("accepted {yaml:?}"))
            );
            assert!(
                error.contains(expect),
                "for {yaml:?} wanted {expect:?}, got {error}"
            );
        }
    }

    #[test]
    fn the_agent_subtree_defaults_without_being_written_out() {
        let config: Config =
            serde_yaml::from_str("tenants:\n  work:\n    vault: /vaults/work\n").unwrap();
        config.validate().unwrap();
        assert_eq!(config.tenants["work"].agent_subtree, "_agent");
        assert!(
            !config.tenants["work"].extract_facts,
            "LLM extraction over every section is opt-in"
        );
    }

    /// The migration must not guess which tenant an existing store belongs to.
    #[test]
    fn unassigned_slugs_are_reported_rather_than_adopted() {
        let config: Config =
            serde_yaml::from_str("tenants:\n  work:\n    slugs: ['-root-MJSV']\n").unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.unassigned_slugs(&["-root-MJSV", "-root", "-root-bbhost"]),
            vec!["-root", "-root-bbhost"]
        );
        assert!(config.unassigned_slugs(&["-root-MJSV"]).is_empty());
    }

    #[test]
    fn embed_timeout_falls_back_to_the_generation_timeout() {
        let explicit: Config = serde_yaml::from_str(
            "embed: {dim: 1024, timeout_seconds: 30}\nllama_cpp: {timeout_seconds: 600}\n",
        )
        .unwrap();
        assert_eq!(explicit.embed_timeout_seconds(), 30);
        let inherited: Config =
            serde_yaml::from_str("llama_cpp: {timeout_seconds: 600}\n").unwrap();
        assert_eq!(inherited.embed_timeout_seconds(), 600);
        let neither: Config = serde_yaml::from_str("backend: ollama\n").unwrap();
        assert_eq!(neither.embed_timeout_seconds(), 90);
    }
}
