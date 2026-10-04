//! Recall orchestration: the keyword, vector and graph legs, and their fusion.
//!
//! Note on compatibility mode: with `graph.backend: graphiti_compat` the outer
//! keyword and vector legs are deliberately DISABLED and Graphiti's own ordering is
//! returned untouched. That is a product decision — the point of the compatibility
//! window is to not change Graphiti's ranking — not hybrid fusion. The leg status
//! map says so explicitly so nothing downstream has to guess.

use engram_config::Config;
use engram_graph::GraphClient;
use engram_models::OpenAiCompatibleClient;
use engram_retrieval::{bm25, rrf};
use engram_store::{Memory, load};
use engram_vector::QdrantClient;
use serde::Serialize;
use std::{collections::HashMap, path::Path, time::Duration};

#[derive(Serialize)]
pub struct ResultItem {
    pub file: String,
    pub name: String,
    pub description: String,
    pub sources: Vec<String>,
    /// The facts that led to THIS memory.
    ///
    /// Graphiti returns facts grouped per record and the Python contract
    /// (`memory_recall.fuse`) keeps them that way. Flattening everything into one
    /// global list, as this crate used to, throws away the attribution: a caller
    /// could see twelve facts and not know which memory any of them came from.
    pub facts: Vec<String>,
}

#[derive(Serialize)]
pub struct Output {
    pub query: String,
    pub results: Vec<ResultItem>,
    /// Every fact from the graph leg, deduped — retained for callers that just
    /// want context. Per-record attribution lives on each [`ResultItem`].
    pub facts: Vec<String>,
    /// Graphiti's 1-hop related memories. Part of its response contract and
    /// previously dropped on the floor by the `--json` output shape.
    pub neighbours: Vec<String>,
    pub legs: HashMap<String, String>,
}

/// What the graph leg produced.
struct GraphLeg {
    files: Vec<String>,
    /// file -> facts, so attribution survives fusion.
    facts_by_file: HashMap<String, Vec<String>>,
    /// Facts not attributable to a single file (the native token leg).
    loose_facts: Vec<String>,
    /// 1-hop related memory names, when the backend reports them.
    neighbours: Vec<String>,
}

impl GraphLeg {
    fn empty() -> Self {
        Self {
            files: Vec::new(),
            facts_by_file: HashMap::new(),
            loose_facts: Vec::new(),
            neighbours: Vec::new(),
        }
    }

    /// All facts, deduped, for `Output::facts`.
    fn all_facts(&self) -> Vec<String> {
        let mut facts: Vec<String> = self
            .facts_by_file
            .values()
            .flatten()
            .chain(self.loose_facts.iter())
            .cloned()
            .collect();
        facts.sort();
        facts.dedup();
        facts
    }
}

pub async fn recall(
    config_path: &Path,
    slug: &str,
    query: &str,
    k: usize,
) -> Result<Output, String> {
    let config = Config::load(config_path).map_err(|error| error.to_string())?;
    recall_with_config(config, config_path, slug, query, k).await
}

pub async fn recall_native(
    config_path: &Path,
    slug: &str,
    query: &str,
    k: usize,
) -> Result<Output, String> {
    let mut config = Config::load(config_path).map_err(|error| error.to_string())?;
    config.graph.backend = "native".into();
    // Force the native path even if ENGRAM_GRAPH_BACKEND says otherwise: a stray
    // export in the environment used to silently defeat this function, including
    // inside the parity evaluator, which then compared native against native.
    recall_with_backend(
        config,
        config_path,
        slug,
        query,
        k,
        Some("native"),
        Mode::Full,
    )
    .await
}

/// Recall for the prompt hook: the cheap legs only, no Graphiti child.
///
/// Mirrors `memory_recall.recall(fast=True)` on the Python side, and exists for the
/// same reason. Real Graphiti recall measures ~3.5s against a populated graph,
/// while the per-prompt budget is 2.5s — so a hook that used the full path would
/// time out on every single prompt and inject nothing. Fast mode runs local BM25
/// plus the vector leg, and swaps the Graphiti child for ONE Neo4j fact query, so
/// it stays inside the budget whatever `graph.backend` says.
pub async fn recall_fast(
    config_path: &Path,
    slug: &str,
    query: &str,
    k: usize,
) -> Result<Output, String> {
    let config = Config::load(config_path).map_err(|error| error.to_string())?;
    recall_with_backend(config, config_path, slug, query, k, None, Mode::Fast).await
}

/// Which legs to run.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Honour `graph.backend`, including spawning Graphiti in compatibility mode.
    Full,
    /// Local + vector legs plus a single graph fact query. No child process.
    Fast,
}

async fn recall_with_config(
    config: Config,
    config_path: &Path,
    slug: &str,
    query: &str,
    k: usize,
) -> Result<Output, String> {
    recall_with_backend(config, config_path, slug, query, k, None, Mode::Full).await
}

async fn recall_with_backend(
    config: Config,
    config_path: &Path,
    slug: &str,
    query: &str,
    k: usize,
    force_backend: Option<&str>,
    mode: Mode,
) -> Result<Output, String> {
    let store = engram_paths::store_dir(config_path, slug);
    let memories = load(&store).map_err(|error| {
        format!(
            "could not read the memory store at {}: {error}",
            store.display()
        )
    })?;
    let mut legs = HashMap::new();
    let backend = force_backend
        .map(str::to_string)
        .unwrap_or_else(|| graph_backend(&config));
    // In compatibility mode the local legs are suppressed so Graphiti's ordering
    // is returned untouched. Fast mode has no Graphiti ordering to preserve, so
    // they are exactly what it has to work with.
    let graphiti_compat = backend == "graphiti_compat" && mode == Mode::Full;
    let keyword = if graphiti_compat {
        legs.insert(
            "keyword".into(),
            "disabled: graphiti_compat preserves Graphiti ordering".into(),
        );
        Vec::new()
    } else {
        legs.insert("keyword".into(), "ok".into());
        bm25(&memories, query, k * 2)
            .into_iter()
            .map(|hit| hit.file)
            .collect::<Vec<_>>()
    };
    let vector = if graphiti_compat {
        legs.insert(
            "vector".into(),
            "disabled: graphiti_compat preserves Graphiti ordering".into(),
        );
        Vec::new()
    } else {
        vector_leg(&config, query, slug, k * 2, &mut legs).await
    };
    let graph_limit = if graphiti_compat { k } else { k * 2 };
    let graph = match mode {
        Mode::Full => graph_leg(&config, &backend, slug, query, graph_limit, &mut legs).await,
        Mode::Fast => fast_graph_leg(&config, slug, query, &mut legs).await,
    };
    if graphiti_compat
        && legs
            .get("graph")
            .is_some_and(|value| value != "graphiti_compat")
    {
        return Err(format!(
            "Graphiti compatibility recall is unavailable: {}",
            legs.get("graph").map(String::as_str).unwrap_or("unknown")
        ));
    }
    let by_file: HashMap<String, &Memory> = memories
        .iter()
        .map(|memory| (memory.file.clone(), memory))
        .collect();
    let ranked: Vec<String> = if graphiti_compat {
        graph.files.clone()
    } else {
        // Weights and the RRF constant come from config; they were hardcoded to
        // equal weighting and 60.0 while engram.yaml documented both.
        rrf(
            &[keyword.clone(), vector.clone(), graph.files.clone()],
            k,
            config.recall.hybrid.k_rrf,
        )
        .into_iter()
        .map(|hit| hit.file)
        .collect()
    };
    // A graph hit for a file that is not on disk in this slug's store is dropped.
    // Say so, rather than silently returning fewer results than the leg found:
    // with the wrong slug this produced zero results and a healthy-looking report.
    let dropped = ranked
        .iter()
        .filter(|file| !by_file.contains_key(*file))
        .count();
    if dropped > 0 {
        legs.insert(
            "store".into(),
            format!(
                "{dropped} hit(s) had no file in {} — wrong slug?",
                store.display()
            ),
        );
    }
    let results = ranked
        .into_iter()
        .filter_map(|file| {
            let memory = by_file.get(&file)?;
            let mut sources = Vec::new();
            if keyword.contains(&file) {
                sources.push("keyword".into());
            }
            if vector.contains(&file) {
                sources.push("vector".into());
            }
            if graph.files.contains(&file) {
                sources.push("graph".into());
            }
            Some(ResultItem {
                file: memory.file.clone(),
                name: memory.name.clone(),
                description: memory.description.clone(),
                sources,
                facts: graph.facts_by_file.get(&file).cloned().unwrap_or_default(),
            })
        })
        .collect();
    Ok(Output {
        query: query.into(),
        results,
        facts: graph.all_facts(),
        neighbours: graph.neighbours.clone(),
        legs,
    })
}

fn graph_backend(config: &Config) -> String {
    std::env::var("ENGRAM_GRAPH_BACKEND")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| config.graph.backend.clone())
}

async fn vector_leg(
    config: &Config,
    query: &str,
    slug: &str,
    k: usize,
    legs: &mut HashMap<String, String>,
) -> Vec<String> {
    if !config.vector_store.enabled {
        legs.insert("vector".into(), "disabled".into());
        return Vec::new();
    }
    let result = async {
        let embeddings = OpenAiCompatibleClient::new(config.embed_endpoint())?
            .with_api_key(config.embed.api_key.present())
            .with_timeout(config.embed_timeout_seconds());
        // Query prefix, not the document prefix: an asymmetric model indexed with
        // one and queried with neither searches a different part of the space.
        let vector = embeddings
            .embedding(&config.embed.model, &config.query_text(query))
            .await?;
        let scope = config.recall.scope_to_slug.then_some(slug);
        let hits = QdrantClient::from_config(config)
            .search(vector, k, scope)
            .await?;
        Ok::<_, Box<dyn std::error::Error>>(hits)
    }
    .await;
    match result {
        Ok(hits) => {
            legs.insert("vector".into(), "ok".into());
            hits.into_iter().map(|hit| hit.file).collect()
        }
        Err(error) => {
            legs.insert("vector".into(), error.to_string());
            Vec::new()
        }
    }
}

async fn graph_leg(
    config: &Config,
    backend: &str,
    slug: &str,
    query: &str,
    k: usize,
    legs: &mut HashMap<String, String>,
) -> GraphLeg {
    if backend == "graphiti_compat" {
        return graphiti_compat_leg(config, &engram_paths::graph_dir(), query, k, legs).await;
    }
    native_leg(config, slug, query, k, legs).await
}

/// How long the Graphiti child gets before it is killed.
///
/// This bounds the CHILD so a stalled Neo4j cannot hang the process forever — it
/// is not the per-prompt budget. The recall hook layers its much shorter
/// `recall.inject.timeout_ms` on top, which is the right split: an explicit CLI or
/// MCP call may wait for a ~4s Graphiti query, while a prompt may not.
fn graphiti_budget(config: &Config) -> Duration {
    // The floor keeps a misconfigured 0 from meaning "never run".
    Duration::from_millis(config.recall.timeout_ms.max(250))
}

/// `graph_dir` is passed in rather than read from the environment here, so the
/// caller owns resolution and tests need no process-global state.
async fn graphiti_compat_leg(
    config: &Config,
    graph_dir: &Path,
    query: &str,
    k: usize,
    legs: &mut HashMap<String, String>,
) -> GraphLeg {
    let python = graph_dir.join("venv/bin/python");
    let interpreter = if python.is_file() {
        python
    } else {
        "python3".into()
    };
    let script = graph_dir.join("memory_graph_recall.py");
    // tokio::process + a deadline. This was a blocking Command::output() with NO
    // timeout, called from an async handler AND from the UserPromptSubmit hook: a
    // stalled Neo4j blocked a tokio worker and delayed the user's prompt
    // indefinitely, failing open only once the call eventually returned.
    let spawned = tokio::process::Command::new(&interpreter)
        .arg(&script)
        .arg(query)
        .arg("--k")
        .arg(k.to_string())
        // BOTH flags, deliberately. --json-full returns Graphiti's `neighbours`
        // (the 1-hop related memories) which the bare-array --json shape drops, but
        // an older INSTALLED script does not recognise --json-full and would fall
        // back to printing markdown. With both, an old script sees --json and
        // prints the array; a new one sees --json-full and prints the full object.
        // The parser below accepts either shape, so the Rust binary and the Python
        // script can be upgraded independently.
        .arg("--json")
        .arg("--json-full")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(graphiti_budget(config), spawned).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            legs.insert(
                "graph".into(),
                format!(
                    "graphiti_compat could not run {}: {error}",
                    script.display()
                ),
            );
            return GraphLeg::empty();
        }
        Err(_) => {
            // kill_on_drop reaps the child as the future is dropped here.
            legs.insert(
                "graph".into(),
                format!(
                    "graphiti_compat timed out after {}ms",
                    graphiti_budget(config).as_millis()
                ),
            );
            return GraphLeg::empty();
        }
    };
    if !output.status.success() {
        // stderr used to be discarded, so every failure looked the same.
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim().lines().last().unwrap_or("no output");
        legs.insert(
            "graph".into(),
            format!("graphiti_compat exited {}: {detail}", output.status),
        );
        return GraphLeg::empty();
    }
    let Ok(reply) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        legs.insert("graph".into(), "graphiti_compat invalid response".into());
        return GraphLeg::empty();
    };
    // Accept both shapes: {"records": [...], "neighbours": [...]} from --json-full,
    // and a bare array from a script predating that flag.
    let (records, neighbours) = match &reply {
        serde_json::Value::Array(records) => (records.clone(), Vec::new()),
        serde_json::Value::Object(map) => (
            map.get("records")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default(),
            map.get("neighbours")
                .and_then(serde_json::Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        ),
        _ => {
            legs.insert("graph".into(), "graphiti_compat invalid response".into());
            return GraphLeg::empty();
        }
    };
    let mut leg = GraphLeg::empty();
    leg.neighbours = neighbours;
    for row in &records {
        let Some(file) = row.get("file").and_then(serde_json::Value::as_str) else {
            continue;
        };
        leg.files.push(file.to_string());
        let facts = row
            .get("facts")
            .and_then(serde_json::Value::as_array)
            .map(|facts| {
                facts
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        leg.facts_by_file.insert(file.to_string(), facts);
    }
    legs.insert("graph".into(), "graphiti_compat".into());
    leg
}

async fn native_leg(
    config: &Config,
    slug: &str,
    query: &str,
    k: usize,
    legs: &mut HashMap<String, String>,
) -> GraphLeg {
    let creds = config.graph.credentials();
    if creds.password.is_empty() {
        legs.insert(
            "graph".into(),
            "native: no Neo4j password (set NEO4J_PASSWORD or graph/.env)".into(),
        );
        return GraphLeg::empty();
    }
    let client = match GraphClient::from_credentials(&creds) {
        Ok(client) => client,
        Err(error) => {
            legs.insert("graph".into(), format!("native: {error}"));
            return GraphLeg::empty();
        }
    };
    let tokens = query
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .filter(|word| word.len() >= 4)
        .take(6)
        .map(str::to_string)
        .collect::<Vec<_>>();

    // Each sub-leg's failure is recorded. These were all `unwrap_or_default()`
    // followed by an unconditional legs["graph"] = "ok", so an auth failure, a
    // schema error or a dead database all reported as a healthy empty result.
    let mut problems = Vec::new();
    let mut leg = GraphLeg::empty();

    match client.native_facts_for_tokens(slug, &tokens, 6).await {
        Ok(facts) => leg.loose_facts = facts,
        Err(error) => problems.push(format!("facts: {error}")),
    }

    let keyword = match client.native_keyword_files(slug, query, k).await {
        Ok(hits) => hits,
        Err(error) => {
            problems.push(format!("keyword: {error}"));
            Vec::new()
        }
    };

    let semantic = match embed_query(config, query).await {
        Ok(vector) => match client.native_semantic_files(slug, &vector, k).await {
            Ok(hits) => hits,
            Err(error) => {
                problems.push(format!("semantic: {error}"));
                Vec::new()
            }
        },
        Err(error) => {
            problems.push(format!("embedding: {error}"));
            Vec::new()
        }
    };

    // Keep each hit's own facts rather than melting them into one list.
    for hit in keyword.iter().chain(semantic.iter()) {
        let entry = leg.facts_by_file.entry(hit.file.clone()).or_default();
        for fact in &hit.facts {
            if !entry.contains(fact) {
                entry.push(fact.clone());
            }
        }
    }
    leg.files = rrf(
        &[
            keyword.iter().map(|hit| hit.file.clone()).collect(),
            semantic.iter().map(|hit| hit.file.clone()).collect(),
        ],
        k,
        config.recall.hybrid.k_rrf,
    )
    .into_iter()
    .map(|hit| hit.file)
    .collect();

    legs.insert(
        "graph".into(),
        if problems.is_empty() {
            "ok".into()
        } else {
            format!("native degraded — {}", problems.join("; "))
        },
    );
    leg
}

/// One Neo4j query for facts about the query's tokens — no ranking, no child
/// process. The legacy Entity/RELATES_TO leg when Graphiti owns the graph, the
/// native triple leg otherwise; the same choice `memory_recall.graph_facts` makes.
async fn fast_graph_leg(
    config: &Config,
    slug: &str,
    query: &str,
    legs: &mut HashMap<String, String>,
) -> GraphLeg {
    let mut leg = GraphLeg::empty();
    let tokens = query
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .filter(|word| word.len() >= 4)
        .take(6)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        legs.insert("graph".into(), "fast: no usable query tokens".into());
        return leg;
    }
    let creds = config.graph.credentials();
    if creds.password.is_empty() {
        legs.insert("graph".into(), "fast: no Neo4j password".into());
        return leg;
    }
    let client = match GraphClient::from_credentials(&creds) {
        Ok(client) => client,
        Err(error) => {
            legs.insert("graph".into(), format!("fast: {error}"));
            return leg;
        }
    };
    let native = graph_backend(config) == "native";
    let facts = if native {
        client
            .native_facts_for_tokens(slug, &tokens, config.recall.inject.max_facts.max(1))
            .await
    } else {
        client
            .facts_for_tokens(&tokens, config.recall.inject.max_facts.max(1))
            .await
    };
    match facts {
        Ok(mut facts) => {
            facts.sort();
            facts.dedup();
            leg.loose_facts = facts;
            legs.insert("graph".into(), "fast facts".into());
        }
        Err(error) => {
            legs.insert("graph".into(), format!("fast: {error}"));
        }
    }
    leg
}

async fn embed_query(config: &Config, query: &str) -> Result<Vec<f32>, String> {
    OpenAiCompatibleClient::new(config.embed_endpoint())
        .map_err(|error| error.to_string())?
        .with_api_key(config.embed.api_key.present())
        .with_timeout(config.embed_timeout_seconds())
        .embedding(&config.embed.model, &config.query_text(query))
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn config(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).unwrap()
    }

    /// `ENGRAM_GRAPH_BACKEND` is process-global and cargo runs tests
    /// concurrently, so the one test that manipulates it takes a lock.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A fixture `<graph dir>/memory_graph_recall.py` standing in for Graphiti.
    /// With no venv present, `graphiti_compat_leg` falls back to `python3`.
    struct FakeGraphiti(std::path::PathBuf);

    impl FakeGraphiti {
        fn new(name: &str, script: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "engram-hybrid-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("memory_graph_recall.py"), script).unwrap();
            Self(dir)
        }

        fn dir(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FakeGraphiti {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn the_graphiti_budget_comes_from_config_and_has_a_floor() {
        let configured = config("recall: {timeout_ms: 1200}\n");
        assert_eq!(graphiti_budget(&configured), Duration::from_millis(1200));
        // the documented default
        let default = config("backend: ollama\n");
        assert_eq!(graphiti_budget(&default), Duration::from_millis(15_000));
        // 0 must not mean "never run"
        let zero = config("recall: {timeout_ms: 0}\n");
        assert_eq!(graphiti_budget(&zero), Duration::from_millis(250));
    }

    /// The child bound and the per-prompt bound are different numbers. Sharing the
    /// hook's 2.5s made every CLI/MCP call fail against a real ~4s Graphiti query.
    #[test]
    fn the_prompt_budget_is_separate_from_and_shorter_than_the_child_budget() {
        let c = config("backend: ollama\n");
        assert!(
            Duration::from_millis(c.recall.inject.timeout_ms) < graphiti_budget(&c),
            "the per-prompt budget must be the tighter of the two"
        );
        assert_eq!(c.recall.inject.timeout_ms, 2500);
    }

    #[test]
    fn the_env_override_only_applies_when_it_is_set_to_something() {
        let _guard = env_lock();
        let base = config("graph: {backend: graphiti_compat}\n");
        unsafe { std::env::remove_var("ENGRAM_GRAPH_BACKEND") };
        assert_eq!(graph_backend(&base), "graphiti_compat");
        // an empty export is not a choice; it used to win over the config
        unsafe { std::env::set_var("ENGRAM_GRAPH_BACKEND", "  ") };
        assert_eq!(graph_backend(&base), "graphiti_compat");
        unsafe { std::env::set_var("ENGRAM_GRAPH_BACKEND", "native") };
        assert_eq!(graph_backend(&base), "native");
        unsafe { std::env::remove_var("ENGRAM_GRAPH_BACKEND") };
    }

    #[test]
    fn facts_stay_attributed_to_their_memory() {
        let mut leg = GraphLeg::empty();
        leg.files = vec!["a.md".into(), "b.md".into()];
        leg.facts_by_file
            .insert("a.md".into(), vec!["a1".into(), "shared".into()]);
        leg.facts_by_file
            .insert("b.md".into(), vec!["shared".into()]);
        leg.loose_facts = vec!["token-fact".into()];

        assert_eq!(leg.facts_by_file["a.md"], vec!["a1", "shared"]);
        // the global list is deduped across sources
        assert_eq!(leg.all_facts(), vec!["a1", "shared", "token-fact"]);
    }

    /// Fast mode is what the prompt hook uses. It must produce results WITHOUT
    /// spawning Graphiti — the full path takes ~3.5s against a real graph, well
    /// past the 2.5s per-prompt budget, so a hook on the full path injects nothing.
    #[tokio::test]
    async fn fast_recall_uses_the_local_legs_and_no_child_process() {
        let dir = std::env::temp_dir().join(format!(
            "engram-hybrid-fast-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let store = dir.join("projects/-test/memory");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(
            store.join("reference_qdrant.md"),
            "---\nname: qdrant-notes\ndescription: the vector store\nmetadata:\n  type: reference\n---\nQdrant holds the embedding collection for recall.\n",
        )
        .unwrap();
        // graphiti_compat is selected, and the script it would run hangs forever.
        // Fast mode must not touch it.
        let config_path = dir.join("engram.yaml");
        std::fs::write(
            &config_path,
            "graph: {backend: graphiti_compat}\nvector_store: {enabled: false}\n",
        )
        .unwrap();

        let started = std::time::Instant::now();
        let output = recall_fast(&config_path, "-test", "qdrant embedding collection", 4)
            .await
            .expect("fast recall must not fail when only the graph is unavailable");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(2500),
            "fast recall took {elapsed:?}, over the per-prompt budget"
        );
        assert_eq!(
            output.results.first().map(|hit| hit.file.as_str()),
            Some("reference_qdrant.md"),
            "the local BM25 leg must still run in compatibility mode's fast path"
        );
        // The local legs are explicitly ON here, unlike the full compat path.
        assert_eq!(output.legs["keyword"], "ok");
        assert!(
            output.legs["graph"].starts_with("fast"),
            "expected the single-query fact leg, got {:?}",
            output.legs["graph"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A stalled Graphiti must not hold a prompt open. The child here sleeps far
    /// longer than the budget; the leg has to come back promptly, empty, and say
    /// why.
    #[tokio::test]
    async fn a_hung_graphiti_child_is_killed_at_the_deadline() {
        // A sleeping script stands in for a stalled Neo4j.
        let graphiti = FakeGraphiti::new("hang", "import time\ntime.sleep(30)\n");
        let config = config("recall: {timeout_ms: 300}\n");
        let mut legs = HashMap::new();
        let started = std::time::Instant::now();
        let leg = graphiti_compat_leg(&config, graphiti.dir(), "anything", 4, &mut legs).await;
        let elapsed = started.elapsed();

        assert!(leg.files.is_empty(), "a timed-out leg must yield nothing");
        assert!(
            elapsed < Duration::from_secs(5),
            "the deadline was not enforced: took {elapsed:?}"
        );
        assert!(
            legs["graph"].contains("timed out"),
            "the failure must be explicit, got {:?}",
            legs["graph"]
        );
    }

    /// A child that fails must report its stderr, not a single opaque string.
    #[tokio::test]
    async fn a_failing_graphiti_child_reports_why() {
        let graphiti = FakeGraphiti::new(
            "fail",
            "import sys\nsys.stderr.write('neo4j unreachable\\n')\nsys.exit(1)\n",
        );
        let config = config("recall: {timeout_ms: 5000}\n");
        let mut legs = HashMap::new();
        let leg = graphiti_compat_leg(&config, graphiti.dir(), "anything", 4, &mut legs).await;

        assert!(leg.files.is_empty());
        assert!(
            legs["graph"].contains("neo4j unreachable"),
            "stderr was discarded: {:?}",
            legs["graph"]
        );
    }

    /// Per-record facts must survive the compatibility path, which is the one
    /// whose response shape we are meant to preserve.
    #[tokio::test]
    async fn graphiti_records_keep_their_own_facts() {
        let graphiti = FakeGraphiti::new(
            "ok",
            "import json\nprint(json.dumps([\
             {'file': 'a.md', 'facts': ['a owns x']},\
             {'file': 'b.md', 'facts': ['b uses y', 'b runs_on z']}]))\n",
        );
        let config = config("recall: {timeout_ms: 5000}\n");
        let mut legs = HashMap::new();
        let leg = graphiti_compat_leg(&config, graphiti.dir(), "anything", 4, &mut legs).await;

        assert_eq!(legs["graph"], "graphiti_compat");
        // Graphiti's ordering is preserved exactly
        assert_eq!(leg.files, vec!["a.md", "b.md"]);
        assert_eq!(leg.facts_by_file["a.md"], vec!["a owns x"]);
        assert_eq!(leg.facts_by_file["b.md"], vec!["b uses y", "b runs_on z"]);
    }

    /// Graphiti's 1-hop neighbours are part of its reply and were dropped by the
    /// bare-array `--json` shape. Both shapes must parse.
    #[tokio::test]
    async fn graphiti_neighbours_survive_and_the_legacy_shape_still_parses() {
        let config = config("recall: {timeout_ms: 5000}\n");

        let full = FakeGraphiti::new(
            "neighbours",
            "import json\nprint(json.dumps({'records': [{'file': 'a.md', 'facts': ['f1']}],\
             'neighbours': ['related-one', 'related-two']}))\n",
        );
        let mut legs = HashMap::new();
        let leg = graphiti_compat_leg(&config, full.dir(), "anything", 4, &mut legs).await;
        assert_eq!(legs["graph"], "graphiti_compat");
        assert_eq!(leg.files, vec!["a.md"]);
        assert_eq!(leg.neighbours, vec!["related-one", "related-two"]);

        // a script predating --json-full prints a bare array
        let legacy = FakeGraphiti::new(
            "legacy-shape",
            "import json\nprint(json.dumps([{'file': 'a.md', 'facts': ['f1']}]))\n",
        );
        let mut legs = HashMap::new();
        let leg = graphiti_compat_leg(&config, legacy.dir(), "anything", 4, &mut legs).await;
        assert_eq!(
            legs["graph"], "graphiti_compat",
            "the old shape must still work"
        );
        assert_eq!(leg.files, vec!["a.md"]);
        assert!(leg.neighbours.is_empty());
    }
}
