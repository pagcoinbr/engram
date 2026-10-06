//! Extract facts and typed triples from memories into the native Neo4j schema.
//!
//! Ordering matters here. The previous version stamped the memory's SHA in its
//! FIRST write, before extraction, embeddings and triples had committed: a failure
//! part-way through left the new SHA beside stale data, and the next run skipped the
//! memory as current. Embedding errors were discarded outright, so a transient
//! outage produced a permanently incomplete semantic index. The commit marker is
//! now the last write, and a memory that fails any stage stays retryable.

use clap::Parser;
use engram_config::Config;
use engram_graph::{GraphClient, NativeTriple, RELATION_TAXONOMY, is_valid_relation};
use engram_models::OpenAiCompatibleClient;
use engram_store::load;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::ExitCode, time::Duration};

/// See `engram-index`: "this configuration is not mine to serve".
const EXIT_UNSUPPORTED_PROVIDER: u8 = 3;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "ENGRAM_CONFIG")]
    config: Option<PathBuf>,
    // allow_hyphen_values because EVERY engram slug starts with '-' (it is a
    // path with separators replaced), so clap read `--slug -home-alice` as a
    // missing value followed by an unknown flag. The documented invocation was
    // unusable as typed.
    #[arg(long, allow_hyphen_values = true)]
    slug: Option<String>,
    #[arg(long, default_value_t = 25)]
    limit: usize,
    #[arg(long)]
    import_legacy_embeddings: bool,
    /// Neo4j connection overrides. Omitted values come from engram.yaml and the
    /// installer-managed graph/.env, so a normal install needs none of these.
    #[arg(long)]
    uri: Option<String>,
    #[arg(long)]
    database: Option<String>,
    #[arg(long, env = "NEO4J_PASSWORD")]
    password: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Extraction {
    facts: Vec<String>,
    #[serde(default)]
    triples: Vec<Triple>,
}

#[derive(Debug, Deserialize)]
struct Triple {
    subject: String,
    relation: String,
    object: String,
    #[serde(default)]
    confidence: f64,
    #[serde(default)]
    temporal: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    match run(args).await {
        Ok(count) => {
            println!("synced {count} native graph memories");
            ExitCode::SUCCESS
        }
        Err(SyncError::UnsupportedProvider(message)) => {
            eprintln!("engram-native-graph-sync: {message}");
            ExitCode::from(EXIT_UNSUPPORTED_PROVIDER)
        }
        Err(SyncError::Other(error)) => {
            eprintln!("engram-native-graph-sync: {error}");
            ExitCode::FAILURE
        }
    }
}

enum SyncError {
    UnsupportedProvider(String),
    Other(String),
}

impl<T: Into<String>> From<T> for SyncError {
    fn from(value: T) -> Self {
        Self::Other(value.into())
    }
}

async fn run(args: Args) -> Result<usize, SyncError> {
    let config_path = engram_paths::config_path(args.config);
    let slug = engram_paths::resolve_slug(args.slug.as_deref());
    let config = Config::load(&config_path).map_err(|error| error.to_string())?;
    if !config.local_enabled {
        return Err("local_enabled is false".into());
    }
    // Credentials from config + graph/.env, with the CLI able to override.
    let mut creds = config.graph.credentials();
    if let Some(uri) = args.uri {
        creds.uri = uri;
    }
    if let Some(database) = args.database {
        creds.database = database;
    }
    if let Some(password) = args.password.filter(|value| !value.trim().is_empty()) {
        creds.password = engram_config::Secret::new(password);
    }
    if creds.password.is_empty() {
        return Err(
            "no Neo4j password: set NEO4J_PASSWORD, graph.neo4j_password, or graph/.env".into(),
        );
    }
    let client = GraphClient::from_credentials(&creds).map_err(|error| error.to_string())?;

    // The legacy import is pure Cypher — no model is called — so it must run
    // BEFORE the provider gates below. Gating it was a real obstruction: the
    // migration this release documents was unreachable on exactly the read-only
    // native install that needs it most.
    if args.import_legacy_embeddings {
        client
            .import_legacy_fact_embeddings(&slug)
            .await
            .map_err(|error| error.to_string())?;
        return Ok(0);
    }

    // Everything past here calls models. A native sync needs BOTH an
    // OpenAI-compatible embedding endpoint and a generation endpoint for fact
    // extraction; checking only the former sent the binary off to fail on a URL
    // parse. Note the generation requirement is `llama_cpp.url`, which is what
    // the reasoning client is built from — NOT `backend`, which selects the
    // Python pipeline's generation backend.
    if !config.rust_embedding_supported() {
        return Err(SyncError::UnsupportedProvider(format!(
            "embedding provider '{}' is not implemented in Rust; use the Python graph sync",
            config.embed_provider()
        )));
    }
    if !config.rust_reasoning_supported() {
        return Err(SyncError::UnsupportedProvider(
            "no generation endpoint for fact extraction: set llama_cpp.url to an \
             OpenAI-compatible server, or use the Python graph sync"
                .into(),
        ));
    }

    let memories =
        load(engram_paths::store_dir(&config_path, &slug)).map_err(|error| error.to_string())?;
    let reasoning = OpenAiCompatibleClient::new(config.llama_cpp.url.clone())
        .map_err(|error| error.to_string())?
        .with_api_key(config.llama_cpp.api_key.present())
        .with_timeout(config.llama_cpp.timeout_seconds)
        .with_max_tokens(config.llama_cpp.max_tokens);
    let embeddings = OpenAiCompatibleClient::new(config.embed_endpoint())
        .map_err(|error| error.to_string())?
        .with_api_key(config.embed.api_key.present())
        .with_timeout(config.embed_timeout_seconds());
    let space = config.embedding_space_id();

    // Retire graph data for memories that left the store. Only on a full pass:
    // a --limit run has not looked at everything, so it cannot conclude a file is
    // gone.
    let full_pass = args.limit >= memories.len();
    if full_pass {
        let present: Vec<String> = memories.iter().map(|memory| memory.file.clone()).collect();
        client
            .prune_missing_memories(&slug, &present)
            .await
            .map_err(|error| error.to_string())?;
    }

    // Bring the supersession ordering key up to date across the WHOLE store
    // before syncing anything.
    //
    // `source_mtime` is deliberately outside the freshness hash — a `touch` must
    // not cost a re-extraction — so the upsert only writes it when content
    // changed, leaving every node that predated the field unset. Doing the
    // backfill per memory inside the loop below was not enough either: memories
    // are processed in filename order, so a changed superseding memory could
    // apply supersession against a later-sorting node whose key was still unset,
    // read its mtime as zero, and retire claims that were actually newer. One
    // batched write up front, no model calls, and a no-op once values match.
    client
        .set_native_memory_mtimes(
            &slug,
            &memories
                .iter()
                .map(|memory| (memory.file.clone(), memory.source_mtime))
                .collect::<Vec<_>>(),
        )
        .await
        .map_err(|error| error.to_string())?;

    let mut count = 0;
    let mut failures = Vec::new();
    for memory in &memories {
        if count >= args.limit {
            break;
        }
        // The embedding space is part of the freshness key: a model swap at the
        // same dimension must invalidate stored fact vectors.
        let sha = format!(
            "{:x}",
            Sha256::digest(
                format!(
                    "{space}\u{0}{}\u{0}{}\u{0}{}",
                    memory.name, memory.description, memory.body
                )
                .as_bytes()
            )
        );
        if client
            .native_memory_is_current(&slug, &memory.file, &sha, &space)
            .await
            .map_err(|error| error.to_string())?
        {
            continue;
        }
        // One memory's failure must not abandon the batch, and must not mark it
        // current. It is simply retried on the next run.
        match sync_memory(
            &client,
            &config,
            &reasoning,
            &embeddings,
            &slug,
            &space,
            &sha,
            memory,
        )
        .await
        {
            Ok(()) => count += 1,
            Err(error) => failures.push(format!("{}: {error}", memory.file)),
        }
    }
    if !failures.is_empty() {
        eprintln!(
            "engram-native-graph-sync: {} memory(ies) left for retry — {}",
            failures.len(),
            failures.join("; ")
        );
    }
    Ok(count)
}

#[allow(clippy::too_many_arguments)]
async fn sync_memory(
    client: &GraphClient,
    config: &Config,
    reasoning: &OpenAiCompatibleClient,
    embeddings: &OpenAiCompatibleClient,
    slug: &str,
    space: &str,
    sha: &str,
    memory: &engram_store::Memory,
) -> Result<(), String> {
    // Redact before anything leaves the box. The reasoning endpoint may be remote
    // and Neo4j keeps whatever it is given; imported and hand-edited memories never
    // passed the save-time guard, so this is the only place that can catch them.
    //
    // `name` included: it is frontmatter like the description, it is written to
    // Neo4j by the upsert below, and a remote Neo4j is now a supported
    // configuration — so leaving it raw was a credential crossing the network.
    let (safe_name, _) = engram_secrets::redact(&memory.name);
    let (safe_description, _) = engram_secrets::redact(&memory.description);
    let (safe_body, _) = engram_secrets::redact(&memory.body);

    let prompt = format!(
        "Extract durable facts and typed triples. Return JSON only: \
         {{\"facts\":[\"claim\"],\"triples\":[{{\"subject\":\"x\",\"relation\":\"uses\",\"object\":\"y\",\"confidence\":0.9,\"temporal\":\"current\"}}]}}. \
         Relations must be one of: {}. Infer temporal as current when present tense applies, formerly for past or replaced states, \
         and use supersedes when wording says replacement, migration, or succession. Confidence is 0 to 1; preserve uncertain triples.\n{}\n{}",
        RELATION_TAXONOMY.join(", "),
        safe_description,
        safe_body
    );
    // Extraction failure must FAIL, not degrade.
    //
    // This chain used to be `.ok().and_then(Result::ok).and_then(..ok()).unwrap_or(..)`,
    // which collapsed a timeout, an HTTP error and malformed JSON alike into a
    // heuristic facts-only extraction with zero triples — and then the sync went
    // on to stamp the memory current. One blip of reasoning downtime silently
    // erased that memory's triples and marked the result final. An error here
    // leaves the memory unstamped, so the next run retries it.
    let raw = tokio::time::timeout(
        Duration::from_secs(90),
        reasoning.chat(&config.llama_cpp.model, &prompt),
    )
    .await
    .map_err(|_| "fact extraction timed out after 90s".to_string())?
    .map_err(|error| format!("fact extraction failed: {error}"))?;
    let extraction = parse_extraction(&raw)?;
    // An empty `facts` list is a legitimate answer about a thin memory, so the
    // sentence-level heuristic still backs it. That is a judgement about content,
    // not a mask over a failed call.
    let extracted = if extraction.facts.is_empty() {
        facts(&safe_description, &safe_body)
    } else {
        extraction.facts
    };
    let triples = extraction
        .triples
        .into_iter()
        .filter_map(normalize_triple)
        .collect::<Vec<_>>();

    client
        .upsert_native_memory(
            slug,
            &memory.file,
            &safe_name,
            &safe_description,
            &safe_body,
            memory.source_mtime,
        )
        .await
        .map_err(|error| error.to_string())?;
    client
        .replace_native_facts(slug, &memory.file, &extracted)
        .await
        .map_err(|error| error.to_string())?;

    // Every fact must embed. Discarding failures here is what produced a
    // permanently half-indexed memory that nothing would revisit.
    let mut fact_vectors = Vec::new();
    for fact in &extracted {
        let vector = embeddings
            .embedding(&config.embed.model, &config.document_text(fact))
            .await
            .map_err(|error| format!("embedding failed: {error}"))?;
        fact_vectors.push((fact.as_str(), vector));
    }
    client
        .set_native_fact_embeddings(slug, &memory.file, &fact_vectors)
        .await
        .map_err(|error| error.to_string())?;
    client
        .replace_native_triples(slug, &memory.file, &triples)
        .await
        .map_err(|error| error.to_string())?;
    // Last: everything above committed, so this memory really is current.
    client
        .mark_native_memory_current(slug, &memory.file, sha, space)
        .await
        .map_err(|error| error.to_string())
}

/// Parse the model's extraction, treating malformed output as a failure.
///
/// Unparseable JSON used to be swallowed alongside timeouts and HTTP errors,
/// degrading to a facts-only extraction with zero triples that was then stamped
/// current — so a model having a bad day permanently erased a memory's triples.
/// Returning `Err` leaves the memory unstamped and therefore retryable.
fn parse_extraction(raw: &str) -> Result<Extraction, String> {
    serde_json::from_str::<Extraction>(raw).map_err(|error| {
        format!(
            "fact extraction returned unparseable JSON ({error}): {}",
            raw.trim().chars().take(200).collect::<String>()
        )
    })
}

/// Clean up one extracted triple and settle its tense.
///
/// The tense heuristic reads the TRIPLE, not the whole memory. Scanning the entire
/// body for `"was "` or `"previously"` marked every triple in that memory as a past
/// state — one historical sentence in a long memory retired all of its current
/// claims.
fn normalize_triple(triple: Triple) -> Option<NativeTriple> {
    let relation = triple
        .relation
        .trim()
        .to_lowercase()
        .replace([' ', '-'], "_");
    let text = format!(
        "{} {} {}",
        triple.subject.trim(),
        relation,
        triple.object.trim()
    )
    .to_lowercase();
    let temporal = match triple.temporal.trim().to_lowercase().as_str() {
        "formerly" | "former" | "past" | "historical" => "formerly",
        "superseded" | "supersedes" => "supersedes",
        "current" | "present" => "current",
        // No usable tense from the model: fall back to the triple's own wording.
        _ if ["superseded", "replaced by", "migrated to", "succeeded by"]
            .iter()
            .any(|phrase| text.contains(phrase)) =>
        {
            "supersedes"
        }
        _ if ["formerly", "previously", "used to", "was "]
            .iter()
            .any(|phrase| text.contains(phrase)) =>
        {
            "formerly"
        }
        _ => "current",
    };
    let relation = if temporal == "supersedes" {
        "supersedes".to_string()
    } else {
        relation
    };
    (is_valid_relation(&relation)
        && !triple.subject.trim().is_empty()
        && !triple.object.trim().is_empty())
    .then_some(NativeTriple {
        subject: triple.subject.trim().to_string(),
        relation,
        object: triple.object.trim().to_string(),
        confidence: triple.confidence.clamp(0.0, 1.0),
        temporal: temporal.to_string(),
    })
}

fn facts(description: &str, body: &str) -> Vec<String> {
    format!("{description} {body}")
        .split(['.', '!', '?', '\n'])
        .map(str::trim)
        .filter(|fact| fact.len() >= 20)
        .take(12)
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed extraction must FAIL. It used to collapse into a facts-only
    /// result with no triples, which the sync then stamped as current — one blip
    /// of reasoning downtime silently wiped that memory's triples for good.
    #[test]
    fn unparseable_extraction_is_an_error_not_an_empty_result() {
        for raw in [
            "",
            "I'm sorry, I can't do that.",
            "{\"facts\": [\"a\"], \"triples\": ", // truncated mid-stream
            "<think>reasoning</think>",
        ] {
            let error =
                parse_extraction(raw).expect_err(&format!("malformed output accepted: {raw:?}"));
            assert!(error.contains("unparseable JSON"), "{error}");
        }
    }

    /// A model that legitimately finds nothing is not a failure.
    #[test]
    fn a_well_formed_empty_extraction_is_accepted() {
        let parsed = parse_extraction("{\"facts\":[],\"triples\":[]}").unwrap();
        assert!(parsed.facts.is_empty() && parsed.triples.is_empty());
    }

    fn triple(
        subject: &str,
        relation: &str,
        object: &str,
        temporal: &str,
        confidence: f64,
    ) -> Triple {
        Triple {
            subject: subject.into(),
            relation: relation.into(),
            object: object.into(),
            confidence,
            temporal: temporal.into(),
        }
    }

    #[test]
    fn keeps_the_models_tense_when_it_gives_one() {
        let value = normalize_triple(triple("Engram", "uses", "Neo4j", "formerly", 0.4)).unwrap();
        assert_eq!(value.temporal, "formerly");
        assert_eq!(value.confidence, 0.4);

        let current = normalize_triple(triple("Engram", "uses", "Neo4j", "current", 0.9)).unwrap();
        assert_eq!(current.temporal, "current");
    }

    #[test]
    fn rejects_relations_outside_the_taxonomy() {
        assert!(normalize_triple(triple("Engram", "guesses", "Neo4j", "", 1.0)).is_none());
        // and incomplete triples
        assert!(normalize_triple(triple("", "uses", "Neo4j", "", 1.0)).is_none());
        assert!(normalize_triple(triple("Engram", "uses", "  ", "", 1.0)).is_none());
    }

    #[test]
    fn normalizes_relation_spelling() {
        let value = normalize_triple(triple("a", "Runs-On", "b", "current", 0.9)).unwrap();
        assert_eq!(value.relation, "runs_on");
    }

    /// The scoping bug: the tense heuristic must read the triple, not the memory.
    /// A memory containing one "was ..." sentence used to have EVERY triple marked
    /// historical, retiring claims that were still true.
    #[test]
    fn the_tense_heuristic_reads_only_the_triple() {
        // The model gave no tense, and nothing in the triple says "past".
        let current = normalize_triple(triple("api", "runs_on", "host-b", "", 0.9)).unwrap();
        assert_eq!(
            current.temporal, "current",
            "an unrelated past-tense sentence elsewhere must not age this claim"
        );

        // Wording inside the triple itself still works.
        let past =
            normalize_triple(triple("api", "runs_on", "host-a (previously)", "", 0.9)).unwrap();
        assert_eq!(past.temporal, "formerly");
    }

    #[test]
    fn replacement_wording_in_the_triple_becomes_supersedes() {
        let value = normalize_triple(triple(
            "postgres-16",
            "uses",
            "replaced by nothing",
            "",
            0.9,
        ))
        .unwrap();
        assert_eq!(value.relation, "supersedes");
        assert_eq!(value.temporal, "supersedes");
    }
}
