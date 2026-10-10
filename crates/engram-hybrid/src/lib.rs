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
use engram_tenant::{GraphScope, Tenant};
use engram_vector::{Corpus, QdrantClient, Scope};
use serde::Serialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Serialize)]
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

#[derive(Debug, Serialize)]
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
    tenant: &Tenant,
    slug: &str,
    query: &str,
    k: usize,
) -> Result<Output, String> {
    let config = Config::load(config_path).map_err(|error| error.to_string())?;
    recall_with_config(config, config_path, tenant, slug, query, k).await
}

pub async fn recall_native(
    config_path: &Path,
    tenant: &Tenant,
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
        tenant,
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
    tenant: &Tenant,
    slug: &str,
    query: &str,
    k: usize,
) -> Result<Output, String> {
    let config = Config::load(config_path).map_err(|error| error.to_string())?;
    recall_with_backend(
        config,
        config_path,
        tenant,
        slug,
        query,
        k,
        None,
        Mode::Fast,
    )
    .await
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
    tenant: &Tenant,
    slug: &str,
    query: &str,
    k: usize,
) -> Result<Output, String> {
    recall_with_backend(
        config,
        config_path,
        tenant,
        slug,
        query,
        k,
        None,
        Mode::Full,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn recall_with_backend(
    config: Config,
    config_path: &Path,
    tenant: &Tenant,
    slug: &str,
    query: &str,
    k: usize,
    force_backend: Option<&str>,
    mode: Mode,
) -> Result<Output, String> {
    // Authorise before reading anything. `graph_scope` refuses a slug this
    // tenant does not own, which is the one place operator input (`--slug`)
    // could ask one agent's process to read another agent's store. Doing it here
    // rather than inside each leg means a refusal cannot be bypassed by a leg
    // that forgot to check — and the markdown store below is read straight off
    // disk, where no database filter would have helped.
    let scope = tenant
        .graph_scope(slug)
        .map_err(|error| error.to_string())?;
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
    //
    // ...but only when there is a Graphiti to be compatible WITH. Compat is the
    // default (an upgraded install must not be moved off its populated index), and
    // compat failure is fatal by design, because silently reordering is the one
    // thing this mode exists to prevent. On an install that never had the graph —
    // `install.sh --no-graph`, which writes no `graph:` block — those two correct
    // decisions combined into a broken one: recall failed closed on
    // `ModuleNotFoundError: graphiti_core` for every query, out of the box.
    //
    // An absent Graphiti is not a failure to preserve ordering; there is no
    // ordering to preserve. So it degrades to the ordinary hybrid path instead,
    // and says so in `legs`. An install that HAS Graphiti and then fails still
    // fails closed — there the ordering is the whole point.
    let graphiti_installed = graphiti_usable(&engram_paths::graph_dir());
    let graphiti_compat = backend == "graphiti_compat" && mode == Mode::Full && graphiti_installed;
    if backend == "graphiti_compat" && mode == Mode::Full && !graphiti_installed {
        legs.insert(
            "graph".into(),
            "graphiti_compat requested but Graphiti is not installed; \
             using local keyword + vector recall"
                .into(),
        );
    }
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
        // In fast mode the vector leg gets its OWN, tighter deadline.
        //
        // The hook wraps this entire call in `recall.inject.timeout_ms`, so a
        // slow embedding endpoint used to consume the whole budget and the
        // prompt got NOTHING — including the local BM25 result that was already
        // computed in ~66ms from markdown on disk. The slowest leg starving the
        // fastest is the wrong failure: incomplete beats empty.
        //
        // This is not hypothetical. A tenanted host indexes one store per
        // tenant, so the embedding endpoint sees far more load than it did when
        // the daemon owned a single store — and a queued request behind three
        // indexing passes blew the 2.5s budget on every prompt.
        let leg_budget = (mode == Mode::Fast)
            .then(|| Duration::from_millis(config.recall.inject.timeout_ms.max(250) * 2 / 5));
        vector_leg(&config, tenant, query, slug, k * 2, &mut legs, leg_budget).await
    };
    let graph_limit = if graphiti_compat { k } else { k * 2 };
    // Dispatch on the decision actually made, not on the configured name: with
    // compat requested but unavailable we must not call the compat leg again just
    // to re-discover that.
    let effective_backend = if backend == "graphiti_compat" && !graphiti_compat {
        "native"
    } else {
        backend.as_str()
    };
    let graph = match mode {
        Mode::Full => {
            graph_leg(
                &config,
                effective_backend,
                &scope,
                query,
                graph_limit,
                &mut legs,
            )
            .await
        }
        Mode::Fast => fast_graph_leg(&config, &scope, query, &mut legs).await,
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
    tenant: &Tenant,
    query: &str,
    slug: &str,
    k: usize,
    legs: &mut HashMap<String, String>,
    budget: Option<Duration>,
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
        // Scope to the active embedding space always, and to the slug when
        // configured: a reindex after a model change leaves both spaces in the
        // collection at once, and vectors from two models cannot be compared.
        //
        // The TENANT boundary is the collection, not a filter clause — see
        // QdrantClient::for_tenant. `scope_to_slug: false` therefore widens the
        // search across this tenant's own stores and no further.
        let space = config.embedding_space_id();
        let scope = match config.recall.scope_to_slug {
            true => Scope::slug(&space, slug),
            false => Scope::space(&space),
        };
        let hits = QdrantClient::for_tenant(config, tenant, Corpus::Memory)
            .search(vector, k, scope)
            .await?;
        Ok::<_, Box<dyn std::error::Error>>(hits)
    };
    // Unbounded when no budget is given (an explicit CLI/MCP call may wait);
    // bounded in fast mode, where a prompt may not.
    let result = match budget {
        Some(budget) => tokio::time::timeout(budget, result).await,
        None => Ok(result.await),
    };
    match result {
        Ok(Ok(hits)) => {
            legs.insert("vector".into(), "ok".into());
            hits.into_iter().map(|hit| hit.file).collect()
        }
        Ok(Err(error)) => {
            legs.insert("vector".into(), error.to_string());
            Vec::new()
        }
        Err(_) => {
            legs.insert(
                "vector".into(),
                format!(
                    "timed out after {}ms; the local legs still ran",
                    budget.unwrap_or_default().as_millis()
                ),
            );
            Vec::new()
        }
    }
}

async fn graph_leg(
    config: &Config,
    backend: &str,
    scope: &GraphScope,
    query: &str,
    k: usize,
    legs: &mut HashMap<String, String>,
) -> GraphLeg {
    if backend == "graphiti_compat" {
        return graphiti_compat_leg(config, scope, &engram_paths::graph_dir(), query, k, legs)
            .await;
    }
    native_leg(config, scope, query, k, legs).await
}

/// The installed Graphiti recall script.
fn graphiti_script(graph_dir: &Path) -> PathBuf {
    graph_dir.join("memory_graph_recall.py")
}

/// Whether Graphiti is actually usable here, which is what distinguishes "this
/// install has an index whose ordering must be preserved" from "this install never
/// had the graph".
///
/// The signal is the **venv**, not the script. `install.sh` copies `graph/*.py`
/// unconditionally and only builds `graph/venv` under `--graph`, so the script's
/// presence proves nothing — and with no venv the compat leg falls back to bare
/// `python3`, where `import graphiti_core` is exactly the failure we are trying to
/// classify.
fn graphiti_usable(graph_dir: &Path) -> bool {
    graph_dir.join("venv/bin/python").is_file() && graphiti_script(graph_dir).is_file()
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
    scope: &GraphScope,
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
    let script = graphiti_script(graph_dir);
    // tokio::process + a deadline. This was a blocking Command::output() with NO
    // timeout, called from an async handler AND from the UserPromptSubmit hook: a
    // stalled Neo4j blocked a tokio worker and delayed the user's prompt
    // indefinitely, failing open only once the call eventually returned.
    let mut command = tokio::process::Command::new(&interpreter);
    command
        .arg(&script)
        .arg(query)
        .arg("--k")
        .arg(k.to_string());
    // `--group` is sent ONLY for a named tenant, and that asymmetry is load-bearing.
    //
    // An older installed script does not merely ignore the flag. Its parser
    // rebuilds the query from every argument that does not start with "--", so
    // `--group canonical` turns the search for "qdrant embedding space" into a
    // search for "qdrant embedding space canonical" — the flag's VALUE lands in
    // the query text. Sending it unconditionally therefore degraded recall
    // quality on every pre-tenancy install until its scripts were refreshed,
    // silently, with results still coming back. (Found by running the new binary
    // against the live install; no unit test would have shown it, because the
    // fixtures all implement the new parser.)
    //
    // The legacy group is what a new script defaults to when the flag is absent,
    // so omitting it is not a behaviour change — just a quieter command line. A
    // named tenant does send it, and must: there the echo check below turns an
    // old script into a hard refusal rather than a silent cross-tenant read.
    if !scope.tenant().is_empty() && scope.tenant() != engram_tenant::LEGACY_GRAPH_GROUP {
        command.arg("--group").arg(scope.tenant());
    }
    let spawned = command
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
    // Make the script PROVE it honoured the scope.
    //
    // `--group` is silently ignored by a script that predates it, so its
    // presence on the command line guarantees nothing. A new script echoes the
    // group it actually filtered on; if that echo is missing or wrong while a
    // named tenant is active, the result may contain another identity's records
    // and the leg fails. In compatibility mode a failed graph leg is fatal
    // (below), which is the correct severity here: returning one agent's
    // memories to another is strictly worse than returning none.
    //
    // The legacy tenant is exempt, and must be: a pre-tenancy install has no
    // boundary to cross, and requiring the echo there would break every install
    // that has not yet updated its graph scripts.
    if !scope.tenant().is_empty() && scope.tenant() != engram_tenant::LEGACY_GRAPH_GROUP {
        let echoed = reply.get("group").and_then(serde_json::Value::as_str);
        if echoed != Some(scope.tenant()) {
            legs.insert(
                "graph".into(),
                format!(
                    "graphiti_compat did not confirm tenant scoping (asked for {:?}, \
                     reply said {:?}); the installed {} is too old to partition by \
                     group — re-run install.sh",
                    scope.tenant(),
                    echoed.unwrap_or("nothing"),
                    script.display(),
                ),
            );
            return GraphLeg::empty();
        }
    }
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
    scope: &GraphScope,
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

    match client.native_facts_for_tokens(scope, &tokens, 6).await {
        Ok(facts) => leg.loose_facts = facts,
        Err(error) => problems.push(format!("facts: {error}")),
    }

    let keyword = match client.native_keyword_files(scope, query, k).await {
        Ok(hits) => hits,
        Err(error) => {
            problems.push(format!("keyword: {error}"));
            Vec::new()
        }
    };

    let semantic = match embed_query(config, query).await {
        Ok(vector) => match client.native_semantic_files(scope, &vector, k).await {
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
    scope: &GraphScope,
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
            .native_facts_for_tokens(scope, &tokens, config.recall.inject.max_facts.max(1))
            .await
    } else {
        client
            .facts_for_tokens(scope, &tokens, config.recall.inject.max_facts.max(1))
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

    /// The pre-tenancy identity: Graphiti's historical single group.
    ///
    /// Most tests below assert behaviour that predates tenancy, so they use
    /// this — which also keeps them exercising the upgrade path rather than only
    /// the new one.
    fn legacy_tenant() -> Tenant {
        Tenant::resolve(&config("backend: ollama\n"), None).unwrap()
    }

    fn legacy_scope() -> GraphScope {
        legacy_tenant().graph_scope("-test").unwrap()
    }

    fn named_tenant(name: &str) -> Tenant {
        let config = config(&format!(
            "tenants:\n  {name}:\n    slugs: ['-test']\n    vault: /vaults/{name}\n"
        ));
        Tenant::resolve(&config, Some(name)).unwrap()
    }

    /// `ENGRAM_GRAPH_BACKEND` is process-global and cargo runs tests
    /// concurrently, so the one test that manipulates it takes a lock.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Compat mode must not fail closed when Graphiti was never installed.
    ///
    /// Two individually correct decisions combined into a broken one: compat is
    /// the default so an upgraded install is not moved off its populated index,
    /// and compat failure is fatal so ordering is never silently changed. On an
    /// `install.sh --no-graph` install — which writes no `graph:` block — that
    /// made recall fail closed on `ModuleNotFoundError: graphiti_core` for every
    /// query, out of the box. Found by running install.sh as a non-root user.
    #[test]
    fn a_missing_graphiti_degrades_instead_of_failing_closed() {
        let absent = std::env::temp_dir().join("engram-no-graphiti-at-all");
        assert!(!graphiti_usable(&absent), "an empty dir is not a Graphiti");

        // The SCRIPT is not the signal: install.sh copies graph/*.py
        // unconditionally and only builds the venv under --graph, so a
        // --no-graph install has the script and no way to run it. Keying on the
        // script made this degradation a no-op, which is how the first attempt
        // at this fix still failed closed on a real non-root install.
        let fake = FakeGraphiti::new("script-without-venv", "import sys; sys.exit(1)");
        assert!(graphiti_script(fake.dir()).is_file());
        assert!(
            !graphiti_usable(fake.dir()),
            "a script with no venv must not count as an installed Graphiti"
        );

        // With a venv present it counts, and a failure there stays fatal.
        std::fs::create_dir_all(fake.dir().join("venv/bin")).unwrap();
        std::fs::write(fake.dir().join("venv/bin/python"), "#!/bin/sh\nexit 1\n").unwrap();
        assert!(graphiti_usable(fake.dir()));
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
        let output = recall_fast(
            &config_path,
            &legacy_tenant(),
            "-test",
            "qdrant embedding collection",
            4,
        )
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

    /// A slow embedding endpoint must not cost the prompt its local results.
    ///
    /// The hook wraps the whole recall in `recall.inject.timeout_ms`, so a
    /// queued embedding request used to consume the entire budget and inject
    /// NOTHING — discarding a BM25 result already computed in ~66ms from
    /// markdown on disk. Found on a live host: a tenanted install indexes one
    /// store per tenant, so the embedding endpoint sees far more load than when
    /// the daemon owned a single store, and every prompt timed out at exactly
    /// 2.515s with zero results while a direct embedding call took 0.6s.
    #[tokio::test]
    async fn a_stalled_vector_leg_does_not_starve_the_local_legs() {
        let dir = std::env::temp_dir().join(format!(
            "engram-hybrid-legbudget-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let store = dir.join("projects/-test/memory");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(
            store.join("reference_chateau.md"),
            "---\nname: chateau-host-memory\ndescription: the host ledger\nmetadata:\n  type: reference\n---\n\
             chateau free output is unreliable; the real 251 GiB ledger lives here.\n",
        )
        .unwrap();
        // A vector endpoint that accepts the connection and never answers — the
        // shape of a saturated embedder, not of a refused one.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = dead.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Accept and hold, so the client waits rather than erroring.
            let mut held = Vec::new();
            while let Ok((socket, _)) = dead.accept().await {
                held.push(socket);
            }
        });
        let config_path = dir.join("engram.yaml");
        std::fs::write(
            &config_path,
            format!(
                "graph: {{backend: graphiti_compat}}\n\
                 vector_store: {{enabled: true, url: 'http://127.0.0.1:{port}'}}\n\
                 embed: {{provider: llama_cpp, url: 'http://127.0.0.1:{port}/v1', model: m, dim: 1024}}\n\
                 recall: {{inject: {{timeout_ms: 1000}}}}\n"
            ),
        )
        .unwrap();

        let tenant = legacy_tenant();
        let started = std::time::Instant::now();
        let output = recall_fast(&config_path, &tenant, "-test", "chateau host ledger", 4)
            .await
            .expect("fast recall must survive a stalled vector leg");
        let elapsed = started.elapsed();

        // The local result survives...
        assert_eq!(
            output.results.first().map(|hit| hit.file.as_str()),
            Some("reference_chateau.md"),
            "the BM25 result was discarded because the vector leg was slow: {:?}",
            output.legs
        );
        // ...the vector leg reports why, rather than looking healthy...
        assert!(
            output.legs["vector"].contains("timed out"),
            "expected an explicit vector timeout, got {:?}",
            output.legs["vector"]
        );
        // ...and the leg's own deadline is a FRACTION of the prompt budget, so
        // there is time left to do the local work and return.
        assert!(
            elapsed < Duration::from_millis(1000),
            "the vector leg consumed the whole prompt budget: {elapsed:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Every guard on the one path by which an agent's output reaches disk.
    ///
    /// Containment, subtree confinement, extension, and redaction BEFORE the
    /// bytes land — the last one matters most, because a vault is synced: a
    /// credential written there has left the box whatever the index holds.
    #[test]
    fn agent_writes_are_guarded_four_ways() {
        let dir = std::env::temp_dir().join(format!(
            "engram-hybrid-write-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        let vault = dir.join("vault");
        std::fs::create_dir_all(vault.join("Runbooks")).unwrap();
        std::fs::create_dir_all(vault.join("_agent")).unwrap();
        std::fs::write(vault.join("Runbooks/DNS.md"), "# curated\n").unwrap();
        let config: Config = serde_yaml::from_str(&format!(
            "tenants:\n  t:\n    slugs: ['-t']\n    vault: {}\n",
            vault.display()
        ))
        .unwrap();
        config.validate().unwrap();
        let tenant = Tenant::resolve(&config, Some("t")).unwrap();

        // 1. the happy path, including a directory that does not exist yet
        let report = wiki_write(
            &tenant,
            "_agent/findings/dns.md",
            "# Finding\n\nResolver drops queries under load.\n",
            WriteMode::Create,
        )
        .unwrap();
        assert!(report.contains("_agent/findings/dns.md"), "{report}");
        let written = std::fs::read_to_string(vault.join("_agent/findings/dns.md")).unwrap();
        assert!(written.contains("Resolver drops queries"));

        // 2. create refuses to clobber; append adds; replace overwrites
        assert!(
            wiki_write(&tenant, "_agent/findings/dns.md", "x", WriteMode::Create).is_err(),
            "create must not silently discard an earlier note"
        );
        wiki_write(
            &tenant,
            "_agent/findings/dns.md",
            "More detail.\n",
            WriteMode::Append,
        )
        .unwrap();
        let appended = std::fs::read_to_string(vault.join("_agent/findings/dns.md")).unwrap();
        assert!(appended.contains("Resolver drops") && appended.contains("More detail"));
        wiki_write(
            &tenant,
            "_agent/findings/dns.md",
            "# Fresh\n",
            WriteMode::Replace,
        )
        .unwrap();
        assert!(
            !std::fs::read_to_string(vault.join("_agent/findings/dns.md"))
                .unwrap()
                .contains("Resolver drops")
        );

        // 3. the curated vault, traversal, and non-markdown are all refused
        for (path, why) in [
            ("Runbooks/DNS.md", "curated page"),
            ("_agent/../Runbooks/DNS.md", "traversal out of the subtree"),
            ("../outside.md", "traversal out of the vault"),
            ("/etc/cron.d/x.md", "absolute path"),
            ("_agent/payload.sh", "non-markdown"),
        ] {
            assert!(
                wiki_write(&tenant, path, "x", WriteMode::Replace).is_err(),
                "{why} was allowed: {path}"
            );
        }
        // the curated page is untouched by any of that
        assert_eq!(
            std::fs::read_to_string(vault.join("Runbooks/DNS.md")).unwrap(),
            "# curated\n"
        );

        // 4. a credential is redacted ON DISK, not merely in the index
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let report = wiki_write(
            &tenant,
            "_agent/creds.md",
            &format!("token={secret}\n"),
            WriteMode::Create,
        )
        .unwrap();
        let on_disk = std::fs::read_to_string(vault.join("_agent/creds.md")).unwrap();
        assert!(
            !on_disk.contains(secret),
            "the credential reached the vault: {on_disk}"
        );
        assert!(on_disk.contains(engram_secrets::REDACTION), "{on_disk}");
        assert!(
            report.contains("redacted"),
            "the caller must be told: {report}"
        );

        // and a runaway write is capped
        assert!(
            wiki_write(
                &tenant,
                "_agent/big.md",
                &"x".repeat(MAX_WRITE_BYTES + 1),
                WriteMode::Replace
            )
            .is_err()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `wiki_fetch` takes a path from the MODEL, so containment is enforced
    /// there and not only at the MCP boundary.
    ///
    /// A path argument is the one input an agent fully controls, and the vault
    /// sits next to other tenants' vaults and the config itself. Traversal is
    /// refused on shape before anything is opened, and a symlink out of the
    /// vault is refused after resolution.
    #[test]
    fn wiki_fetch_refuses_to_leave_the_vault() {
        let dir = std::env::temp_dir().join(format!(
            "engram-hybrid-fetch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        let vault = dir.join("vault");
        std::fs::create_dir_all(vault.join("Runbooks")).unwrap();
        std::fs::write(vault.join("Runbooks/DNS.md"), "# DNS\n\nResolver notes.\n").unwrap();
        std::fs::write(
            dir.join("secret.md"),
            "# other tenant\n\nNOT-FOR-THIS-AGENT\n",
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("secret.md"), vault.join("leak.md")).unwrap();

        let config: Config = serde_yaml::from_str(&format!(
            "tenants:\n  t:\n    slugs: ['-t']\n    vault: {}\n",
            vault.display()
        ))
        .unwrap();
        config.validate().unwrap();
        let tenant = Tenant::resolve(&config, Some("t")).unwrap();

        // its own page reads
        let page = wiki_fetch(&tenant, "Runbooks/DNS.md", None, 8000).unwrap();
        assert!(page.contains("Resolver notes"));

        for bad in [
            "../secret.md",
            "/etc/passwd",
            "Runbooks/../../secret.md",
            "leak.md",
        ] {
            let error =
                wiki_fetch(&tenant, bad, None, 8000).expect_err(&format!("{bad} was not refused"));
            assert!(
                !error.contains("NOT-FOR-THIS-AGENT"),
                "the refusal leaked the content: {error}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The whole point of the feature, end to end: one identity's recall must not
    /// surface another's memory, even when the two stores hold a file of the same
    /// name saying different things.
    ///
    /// Colliding filenames on purpose. Node and point identity used to be the
    /// bare filename, so `Runbook.md` in two stores merged into one record — and
    /// the markdown store is read straight off DISK, where no database filter
    /// would have helped. This asserts the refusal happens before any read, and
    /// that the other tenant's text appears nowhere in the response.
    #[tokio::test]
    async fn one_tenants_recall_cannot_reach_another_tenants_store() {
        let dir = std::env::temp_dir().join(format!(
            "engram-hybrid-isolation-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        for (slug, secret) in [("-work", "WORK-ONLY-PAYROLL"), ("-home", "HOME-ONLY-NAS")] {
            let store = dir.join(format!("projects/{slug}/memory"));
            std::fs::create_dir_all(&store).unwrap();
            // the same filename in both stores, saying different things
            std::fs::write(
                store.join("Runbook.md"),
                format!(
                    "---\nname: runbook\ndescription: the runbook\nmetadata:\n  type: reference\n---\n\
                     Restore procedure for {secret} systems.\n"
                ),
            )
            .unwrap();
        }
        let config_path = dir.join("engram.yaml");
        std::fs::write(
            &config_path,
            "vector_store: {enabled: false}\ngraph: {backend: native}\n\
             tenants:\n  work:\n    slugs: ['-work']\n  homelab:\n    slugs: ['-home']\n",
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();
        let work = Tenant::resolve(&config, Some("work")).unwrap();

        // Its own store: found, with its own content.
        let mine = recall(&config_path, &work, "-work", "restore procedure", 4)
            .await
            .expect("a tenant must be able to read its own store");
        assert_eq!(
            mine.results.first().map(|hit| hit.file.as_str()),
            Some("Runbook.md")
        );

        // The other tenant's store: refused outright, not filtered afterwards.
        let error = recall(&config_path, &work, "-home", "restore procedure", 4)
            .await
            .expect_err("reading another identity's store must fail");
        assert!(
            error.contains("does not own"),
            "the refusal must name the cause: {error}"
        );

        // And the other tenant's text is nowhere in the successful response —
        // the check that would catch a leak through any leg, not just the store.
        let rendered = serde_json::to_string(&mine).unwrap();
        assert!(
            !rendered.contains("HOME-ONLY-NAS"),
            "the other identity's content reached the response: {rendered}"
        );
        assert!(
            rendered.contains("Runbook.md"),
            "guard the guard — the response must be non-empty: {rendered}"
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
        let leg = graphiti_compat_leg(
            &config,
            &legacy_scope(),
            graphiti.dir(),
            "anything",
            4,
            &mut legs,
        )
        .await;
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
        let leg = graphiti_compat_leg(
            &config,
            &legacy_scope(),
            graphiti.dir(),
            "anything",
            4,
            &mut legs,
        )
        .await;

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
        let leg = graphiti_compat_leg(
            &config,
            &legacy_scope(),
            graphiti.dir(),
            "anything",
            4,
            &mut legs,
        )
        .await;

        assert_eq!(legs["graph"], "graphiti_compat");
        // Graphiti's ordering is preserved exactly
        assert_eq!(leg.files, vec!["a.md", "b.md"]);
        assert_eq!(leg.facts_by_file["a.md"], vec!["a owns x"]);
        assert_eq!(leg.facts_by_file["b.md"], vec!["b uses y", "b runs_on z"]);
    }

    /// An old Graphiti script must not be able to silently defeat tenant
    /// scoping.
    ///
    /// `memory_graph_recall.py` parses `sys.argv` by hand and ignores unknown
    /// flags, so passing `--group work` to a script that predates the flag is a
    /// no-op: it would return every tenant's records while this side believed it
    /// had asked for one. Presence on the command line proves nothing, so the
    /// reply has to echo the group it filtered on.
    #[tokio::test]
    async fn a_graphiti_script_that_cannot_scope_by_tenant_is_refused() {
        let config = config("recall: {timeout_ms: 5000}\n");
        let work = named_tenant("work");
        let scope = work.graph_scope("-test").unwrap();

        // An old script: records, no echo. This is the dangerous case.
        let old = FakeGraphiti::new(
            "no-group-echo",
            "import json\nprint(json.dumps({'records': [{'file': 'secret.md', 'facts': ['x']}]}))\n",
        );
        let mut legs = HashMap::new();
        let leg = graphiti_compat_leg(&config, &scope, old.dir(), "anything", 4, &mut legs).await;
        assert!(
            leg.files.is_empty(),
            "unverified records must not be returned: {:?}",
            leg.files
        );
        assert!(
            legs["graph"].contains("did not confirm tenant scoping"),
            "the refusal must say why: {:?}",
            legs["graph"]
        );

        // A script echoing the WRONG group is refused the same way — this is the
        // shape a copy-paste of another tenant's invocation would produce.
        let wrong = FakeGraphiti::new(
            "wrong-group-echo",
            "import json\nprint(json.dumps({'records': [{'file': 'a.md', 'facts': []}],\
             'group': 'homelab'}))\n",
        );
        let mut legs = HashMap::new();
        let leg = graphiti_compat_leg(&config, &scope, wrong.dir(), "anything", 4, &mut legs).await;
        assert!(leg.files.is_empty());
        assert!(
            legs["graph"].contains("did not confirm"),
            "{:?}",
            legs["graph"]
        );

        // The correct echo passes.
        let good = FakeGraphiti::new(
            "right-group-echo",
            "import json\nprint(json.dumps({'records': [{'file': 'a.md', 'facts': ['f']}],\
             'group': 'work'}))\n",
        );
        let mut legs = HashMap::new();
        let leg = graphiti_compat_leg(&config, &scope, good.dir(), "anything", 4, &mut legs).await;
        assert_eq!(legs["graph"], "graphiti_compat");
        assert_eq!(leg.files, vec!["a.md"]);
    }

    /// A pre-tenancy install must receive no `--group` at all.
    ///
    /// This is not tidiness. An older installed script rebuilds its query from
    /// every argument that does not begin with "--", so the flag's VALUE joins
    /// the query text: asking for "qdrant embedding space" became asking for
    /// "qdrant embedding space canonical". Recall still returned results, for a
    /// polluted query, on every install that had not refreshed its scripts.
    ///
    /// Found by running the binary against the live install, not by a unit test —
    /// every fixture here implements the NEW parser, so none of them could see
    /// it. Hence this test asserts the argv the child is given rather than the
    /// records it returns: the bug lives in what a *different* parser would make
    /// of that argv.
    #[tokio::test]
    async fn the_legacy_path_sends_no_group_flag_that_an_old_parser_would_eat() {
        // The fixture echoes its own argv instead of records, so the assertion is
        // about the command line itself.
        let argv = FakeGraphiti::new(
            "argv-echo",
            "import json, sys\nprint(json.dumps({'records': [], 'neighbours': [],\
             'group': 'canonical', 'argv': sys.argv[1:]}))\n",
        );
        let config = config("recall: {timeout_ms: 5000}\n");

        let mut legs = HashMap::new();
        graphiti_compat_leg(
            &config,
            &legacy_scope(),
            argv.dir(),
            "dns failover",
            4,
            &mut legs,
        )
        .await;
        assert_eq!(legs["graph"], "graphiti_compat");

        // Re-run the child directly to read the argv it saw.
        let seen = |scope: &GraphScope| {
            let mut command = std::process::Command::new("python3");
            command
                .arg(graphiti_script(argv.dir()))
                .arg("dns failover")
                .arg("--k")
                .arg("4");
            if !scope.tenant().is_empty() && scope.tenant() != engram_tenant::LEGACY_GRAPH_GROUP {
                command.arg("--group").arg(scope.tenant());
            }
            let out = command.arg("--json").arg("--json-full").output().unwrap();
            let reply: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            reply["argv"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect::<Vec<_>>()
        };

        let legacy_argv = seen(&legacy_scope());
        assert!(
            !legacy_argv.iter().any(|arg| arg == "--group"),
            "the legacy path must send no --group: {legacy_argv:?}"
        );
        assert!(
            !legacy_argv.iter().any(|arg| arg == "canonical"),
            "the group VALUE is what an old parser folds into the query: {legacy_argv:?}"
        );

        // A named tenant does send it — there the echo check makes an old script
        // fail closed instead of quietly returning another identity's records.
        let named = named_tenant("work").graph_scope("-test").unwrap();
        let named_argv = seen(&named);
        assert!(
            named_argv
                .windows(2)
                .any(|pair| pair == ["--group", "work"]),
            "a named tenant must scope the child: {named_argv:?}"
        );
    }

    /// ...but a pre-tenancy install must NOT be made to prove anything, or every
    /// install that has not refreshed its graph scripts breaks on upgrade.
    #[tokio::test]
    async fn a_legacy_install_does_not_have_to_prove_scoping() {
        let config = config("recall: {timeout_ms: 5000}\n");
        let old = FakeGraphiti::new(
            "legacy-no-echo",
            "import json\nprint(json.dumps([{'file': 'a.md', 'facts': ['f']}]))\n",
        );
        let mut legs = HashMap::new();
        let leg = graphiti_compat_leg(
            &config,
            &legacy_scope(),
            old.dir(),
            "anything",
            4,
            &mut legs,
        )
        .await;
        assert_eq!(legs["graph"], "graphiti_compat");
        assert_eq!(
            leg.files,
            vec!["a.md"],
            "the bare-array shape from an older script must still work"
        );
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
        let leg = graphiti_compat_leg(
            &config,
            &legacy_scope(),
            full.dir(),
            "anything",
            4,
            &mut legs,
        )
        .await;
        assert_eq!(legs["graph"], "graphiti_compat");
        assert_eq!(leg.files, vec!["a.md"]);
        assert_eq!(leg.neighbours, vec!["related-one", "related-two"]);

        // a script predating --json-full prints a bare array
        let legacy = FakeGraphiti::new(
            "legacy-shape",
            "import json\nprint(json.dumps([{'file': 'a.md', 'facts': ['f1']}]))\n",
        );
        let mut legs = HashMap::new();
        let leg = graphiti_compat_leg(
            &config,
            &legacy_scope(),
            legacy.dir(),
            "anything",
            4,
            &mut legs,
        )
        .await;
        assert_eq!(
            legs["graph"], "graphiti_compat",
            "the old shape must still work"
        );
        assert_eq!(leg.files, vec!["a.md"]);
        assert!(leg.neighbours.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Wiki recall.
//
// A SEPARATE entry point from `recall`, deliberately. Memories are atomic facts
// and wiki chunks are fragments of long documents; fusing them into one ranking
// would let a 40-chunk page outvote every memory in the store, and the prompt
// hook injects from `recall` on every single prompt. Wiki is retrieved when
// asked for — which is also why `wiki_fetch` exists: search finds the section,
// fetch returns the surrounding document within a budget.
// ---------------------------------------------------------------------------

/// One wiki search result.
#[derive(Debug, Serialize)]
pub struct WikiHit {
    pub path: String,
    pub title: String,
    /// `Page > Section > Subsection`.
    pub breadcrumb: String,
    pub chunk_index: usize,
    pub text: String,
    pub sources: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct WikiOutput {
    pub query: String,
    pub results: Vec<WikiHit>,
    pub legs: HashMap<String, String>,
}

/// A chunk read off disk, for the local keyword leg.
struct LocalChunk {
    path: String,
    title: String,
    breadcrumb: String,
    index: usize,
    text: String,
}

impl engram_retrieval::Indexable for LocalChunk {
    fn id(&self) -> String {
        format!("{}#{}", self.path, self.index)
    }
    fn text(&self) -> String {
        // The breadcrumb is searchable too: "DNS > Failover" should match a
        // query for "dns failover" even when neither word is in the prose.
        format!("{} {} {}", self.title, self.breadcrumb, self.text)
    }
}

/// Search one tenant's vault: keyword over the files, vectors over the chunks,
/// fused with RRF.
///
/// The keyword leg reads and re-chunks the vault on each call. That is a real
/// cost and an accepted one: it needs no index, so it works the moment a file is
/// saved and keeps working when Qdrant or the embedder is down — the same
/// local-first posture the memory keyword leg has. Vault size is the limit here;
/// at the point that hurts, the chunks are already in Qdrant and the vector leg
/// is the one to lean on.
pub async fn wiki_search(
    config_path: &Path,
    tenant: &Tenant,
    query: &str,
    k: usize,
) -> Result<WikiOutput, String> {
    let config = Config::load(config_path).map_err(|error| error.to_string())?;
    // Say so plainly rather than returning an empty result with a puzzling leg
    // status: an unconfigured vault is a setup step, not a search that found
    // nothing, and the two look identical to a caller.
    if tenant.vault().is_none() {
        return Err(format!(
            "tenant '{}' has no vault configured; add `vault: /path/to/vault` under it \
             in engram.yaml",
            tenant.label()
        ));
    }
    let mut legs = HashMap::new();

    let local = local_chunks(tenant, &mut legs);
    let keyword: Vec<String> = engram_retrieval::rank(&local, query, k * 3)
        .into_iter()
        .map(|hit| hit.file)
        .collect();
    if !local.is_empty() {
        legs.insert("keyword".into(), "ok".into());
    }

    let mut by_id: HashMap<String, WikiHit> = local
        .iter()
        .map(|chunk| {
            (
                format!("{}#{}", chunk.path, chunk.index),
                WikiHit {
                    path: chunk.path.clone(),
                    title: chunk.title.clone(),
                    breadcrumb: chunk.breadcrumb.clone(),
                    chunk_index: chunk.index,
                    text: chunk.text.clone(),
                    sources: Vec::new(),
                },
            )
        })
        .collect();

    let vector = wiki_vector_leg(&config, tenant, query, k * 3, &mut by_id, &mut legs).await;

    let ranked = rrf(
        &[keyword.clone(), vector.clone()],
        k,
        config.recall.hybrid.k_rrf,
    );
    let results = ranked
        .into_iter()
        .filter_map(|hit| {
            let mut found = by_id.remove(&hit.file)?;
            if keyword.contains(&hit.file) {
                found.sources.push("keyword".into());
            }
            if vector.contains(&hit.file) {
                found.sources.push("vector".into());
            }
            Some(found)
        })
        .collect();
    Ok(WikiOutput {
        query: query.into(),
        results,
        legs,
    })
}

/// Walk, parse and chunk the vault for the keyword leg.
fn local_chunks(tenant: &Tenant, legs: &mut HashMap<String, String>) -> Vec<LocalChunk> {
    let root = match tenant.vault_root() {
        Ok(root) => root,
        Err(error) => {
            legs.insert("keyword".into(), error.to_string());
            return Vec::new();
        }
    };
    let paths = match engram_wiki::walk(&root) {
        Ok(paths) => paths,
        Err(error) => {
            legs.insert("keyword".into(), error.to_string());
            return Vec::new();
        }
    };
    let params = engram_wiki::ChunkParams::default();
    let mut chunks = Vec::new();
    for path in &paths {
        let Ok(raw) = std::fs::read_to_string(engram_wiki::absolute(&root, path)) else {
            continue;
        };
        let document = engram_wiki::parse(path, &raw, 0);
        for chunk in engram_wiki::chunk(&document, params) {
            chunks.push(LocalChunk {
                path: path.clone(),
                title: document.title.clone(),
                breadcrumb: chunk.breadcrumb(),
                index: chunk.index,
                text: chunk.text.clone(),
            });
        }
    }
    chunks
}

async fn wiki_vector_leg(
    config: &Config,
    tenant: &Tenant,
    query: &str,
    k: usize,
    by_id: &mut HashMap<String, WikiHit>,
    legs: &mut HashMap<String, String>,
) -> Vec<String> {
    if !config.vector_store.enabled {
        legs.insert("vector".into(), "disabled".into());
        return Vec::new();
    }
    let space = config.embedding_space_id();
    let result = async {
        let embeddings = OpenAiCompatibleClient::new(config.embed_endpoint())?
            .with_api_key(config.embed.api_key.present())
            .with_timeout(config.embed_timeout_seconds());
        let vector = embeddings
            .embedding(&config.embed.model, &config.query_text(query))
            .await?;
        let hits = QdrantClient::for_tenant(config, tenant, Corpus::Wiki)
            .search_chunks(vector, k, Scope::space(&space))
            .await?;
        Ok::<_, Box<dyn std::error::Error>>(hits)
    }
    .await;
    match result {
        Ok(hits) => {
            legs.insert("vector".into(), "ok".into());
            hits.into_iter()
                .map(|hit| {
                    let id = format!("{}#{}", hit.path, hit.chunk_index);
                    // A chunk may be in Qdrant and not on disk — the vault is
                    // unmounted, or indexing ran before a delete. The stored
                    // text means the hit is still answerable.
                    by_id.entry(id.clone()).or_insert_with(|| WikiHit {
                        path: hit.path,
                        title: hit.title,
                        breadcrumb: hit.breadcrumb,
                        chunk_index: hit.chunk_index,
                        text: hit.text,
                        sources: Vec::new(),
                    });
                    id
                })
                .collect()
        }
        Err(error) => {
            legs.insert("vector".into(), error.to_string());
            Vec::new()
        }
    }
}

/// A whole document, or one of its sections, within a character budget.
///
/// This is the half that makes the feature useful: search locates the section,
/// this returns the context around it. Returning a truncated fragment is what
/// the memory path already does badly for long documents, so the budget cuts at
/// a section boundary where it can and says what it dropped.
pub fn wiki_fetch(
    tenant: &Tenant,
    path: &str,
    heading: Option<&str>,
    max_chars: usize,
) -> Result<String, String> {
    // Through the tenant, so `..` and a symlink out of the vault are refused
    // before anything is opened.
    let resolved = tenant
        .resolve_in_vault(Path::new(path))
        .map_err(|error| error.to_string())?;
    let raw = std::fs::read_to_string(&resolved).map_err(|error| format!("{path}: {error}"))?;
    let document = engram_wiki::parse(path, &raw, 0);
    let params = engram_wiki::ChunkParams::default();
    let chunks = engram_wiki::chunk(&document, params);

    let wanted: Vec<&engram_wiki::Chunk> = match heading {
        None => chunks.iter().collect(),
        Some(heading) => {
            let matching: Vec<&engram_wiki::Chunk> = chunks
                .iter()
                .filter(|chunk| {
                    chunk
                        .heading_path
                        .iter()
                        .any(|part| part.eq_ignore_ascii_case(heading.trim()))
                })
                .collect();
            if matching.is_empty() {
                return Err(format!(
                    "no section matching {heading:?} in {path}; sections are: {}",
                    chunks
                        .iter()
                        .map(|chunk| chunk.breadcrumb())
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
            }
            matching
        }
    };

    // Redact on the READ path too. The vault is hand-authored/imported and never
    // passed a save-time guard, and this text is handed straight to the model over
    // MCP — the same boundary the write and index paths redact at. (`path` is a
    // vault-relative path, not content, so it is left as-is.)
    let (safe_title, _) = engram_secrets::redact(&document.title);
    let mut out = format!("# {safe_title}\n_{}_\n\n", path);
    let mut dropped = 0;
    for chunk in wanted {
        let (safe_crumb, _) = engram_secrets::redact(&chunk.breadcrumb());
        let (safe_text, _) = engram_secrets::redact(&chunk.text);
        let piece = format!("## {safe_crumb}\n\n{safe_text}\n\n");
        if out.len() + piece.len() > max_chars && !out.is_empty() {
            dropped += 1;
            continue;
        }
        out.push_str(&piece);
    }
    if dropped > 0 {
        out.push_str(&format!(
            "_[{dropped} further section(s) omitted to stay within {max_chars} characters — \
             fetch a specific heading to read them]_\n"
        ));
    }
    Ok(out)
}

/// How an agent write behaves when the file already exists.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WriteMode {
    /// Refuse if the path exists. The default, so an agent cannot silently
    /// discard a note it or another session wrote earlier.
    Create,
    /// Append, creating the file if absent. The natural mode for a running log.
    Append,
    /// Overwrite. Explicit, because it is the only mode that destroys content.
    Replace,
}

impl WriteMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "create" => Ok(Self::Create),
            "append" => Ok(Self::Append),
            "replace" => Ok(Self::Replace),
            other => Err(format!(
                "unknown mode {other:?}; use create, append or replace"
            )),
        }
    }
}

/// Largest single write. A guard against a looping agent filling the disk, not
/// a judgement about document length — a page longer than this should be
/// several pages anyway.
const MAX_WRITE_BYTES: usize = 1 << 20;

/// Write a note into the agent-writable subtree of a tenant's vault.
///
/// Four independent guards, because this is the only path by which an agent's
/// output reaches the operator's disk:
///
/// 1. **Containment** — the path must resolve inside the vault, symlinks and
///    `..` included, so one identity cannot write into another's vault.
/// 2. **Subtree** — and inside `agent_subtree` specifically, so a curated page
///    cannot be touched. Separate from (1) on purpose: a future change to one
///    must not silently widen the other.
/// 3. **Extension** — `.md` only. Obsidian renders nothing else, and without
///    this an agent could drop a script or a binary into a directory a human
///    browses and a sync client propagates.
/// 4. **Redaction** — through the shared detector, BEFORE the bytes hit disk.
///    Not merely before indexing: a credential written to a synced vault has
///    left the box regardless of what the index holds.
///
/// The write itself is temp file + fsync + rename, the discipline the config
/// saver already uses, so a crash leaves either the old file or the new one and
/// never a half-written page for Obsidian to render.
pub fn wiki_write(
    tenant: &Tenant,
    path: &str,
    content: &str,
    mode: WriteMode,
) -> Result<String, String> {
    use std::io::Write as _;

    if content.len() > MAX_WRITE_BYTES {
        return Err(format!(
            "content is {} bytes; the limit is {MAX_WRITE_BYTES}",
            content.len()
        ));
    }
    let relative = path.trim();
    if !relative.to_ascii_lowercase().ends_with(".md") {
        return Err(format!(
            "{relative:?} must end in .md — the vault holds markdown, and a sync \
             client propagates whatever is in it"
        ));
    }
    let target = tenant
        .resolve_for_agent_write(Path::new(relative))
        .map_err(|error| error.to_string())?;

    let (safe, redacted) = engram_secrets::redact(content);
    if target.exists() && mode == WriteMode::Create {
        return Err(format!(
            "{relative} already exists; use mode=append to add to it or \
             mode=replace to overwrite it"
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| format!("{relative} has no parent directory"))?;
    std::fs::create_dir_all(parent).map_err(|error| format!("{relative}: {error}"))?;

    if mode == WriteMode::Append && target.exists() {
        // Append must NOT read-modify-rename the whole file: two concurrent appends
        // would each read the old content and the last rename would silently discard
        // the other's addition. Open with O_APPEND, under which each write() syscall
        // seeks to EOF atomically, so no append is ever lost. Separator and body are
        // concatenated into ONE write_all so they are not split by a concurrent
        // append; a note large enough for the kernel to split one write_all into
        // several syscalls could still interleave with another concurrent large
        // append (no data loss, only ordering) — fine for hand-sized notes. The
        // separator is decided from a cheap last-byte read — a stale read only adds or
        // omits one blank line, it can never lose data the way the whole-file replace did.
        let ends_with_newline = {
            use std::io::{Read, Seek, SeekFrom};
            std::fs::File::open(&target)
                .and_then(|mut f| {
                    if f.seek(SeekFrom::End(0))? == 0 {
                        return Ok(true); // empty file: no separator wanted
                    }
                    f.seek(SeekFrom::End(-1))?;
                    let mut last = [0u8; 1];
                    f.read_exact(&mut last)?;
                    Ok(last[0] == b'\n')
                })
                .unwrap_or(true)
        };
        let separator: &str = if ends_with_newline { "" } else { "\n" };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&target)
            .map_err(|error| format!("{relative}: {error}"))?;
        let payload = format!("{separator}{safe}");
        file.write_all(payload.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("{relative}: {error}"))?;
        if let Ok(dir) = std::fs::File::open(parent) {
            dir.sync_all().ok();
        }
    } else {
        // Create (refused above if the path exists), Replace, or appending to a file
        // that does not exist yet: write the whole body to a uniquely named temp file
        // in the destination directory and atomically rename it into place. The temp
        // name carries a per-process monotonic counter as well as the pid, so two
        // concurrent writes to the same target in one process cannot collide on it.
        static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temporary = parent.join(format!(
            ".engram-write-{}-{}-{}.tmp",
            std::process::id(),
            WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            target
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        ));
        let mut file =
            std::fs::File::create(&temporary).map_err(|error| format!("{relative}: {error}"))?;
        let result = file
            .write_all(safe.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("{relative}: {error}"));
        if let Err(error) = result {
            std::fs::remove_file(&temporary).ok();
            return Err(error);
        }
        drop(file);
        std::fs::rename(&temporary, &target).map_err(|error| {
            std::fs::remove_file(&temporary).ok();
            format!("{relative}: {error}")
        })?;
        // fsync the DIRECTORY too, or the rename itself can be lost on power loss
        // while the file contents survive — the same reason the config saver does it.
        if let Ok(dir) = std::fs::File::open(parent) {
            dir.sync_all().ok();
        }
    }

    let note = match redacted {
        0 => String::new(),
        1 => " (1 credential redacted)".into(),
        many => format!(" ({many} credentials redacted)"),
    };
    Ok(format!(
        "wrote {} bytes to {relative}{note}; it will be indexed on the next wiki pass",
        safe.len()
    ))
}
