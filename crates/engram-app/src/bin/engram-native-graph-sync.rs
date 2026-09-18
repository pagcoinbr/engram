use clap::Parser;
use engram_config::Config;
use engram_graph::{GraphClient, NativeTriple, RELATION_TAXONOMY, is_valid_relation};
use engram_models::OpenAiCompatibleClient;
use engram_store::load;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::ExitCode, time::Duration};

#[derive(Parser)]
struct Args {
    #[arg(
        long,
        env = "ENGRAM_CONFIG",
        default_value = "/root/.claude/engram.yaml"
    )]
    config: PathBuf,
    #[arg(long, default_value = "-root")]
    slug: String,
    #[arg(long, default_value_t = 25)]
    limit: usize,
    #[arg(long, env = "NEO4J_URI", default_value = "bolt://127.0.0.1:7687")]
    uri: String,
    #[arg(long, env = "NEO4J_DATABASE", default_value = "neo4j")]
    database: String,
    #[arg(long, env = "NEO4J_PASSWORD")]
    password: String,
}

#[derive(Deserialize)]
struct Extraction {
    facts: Vec<String>,
    #[serde(default)]
    triples: Vec<Triple>,
}
#[derive(Deserialize)]
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
    let result = async {
        let directory = args
            .config
            .parent()
            .ok_or("configuration path has no parent")?
            .join("projects")
            .join(&args.slug)
            .join("memory");
        let client = GraphClient::new(&args.uri, &args.database, "neo4j", args.password)
            .map_err(|error| error.to_string())?;
        let memories = load(directory).map_err(|error| error.to_string())?;
        let config = Config::load(&args.config).map_err(|error| error.to_string())?;
        let reasoning = OpenAiCompatibleClient::new(config.llama_cpp.url.clone()).map_err(|error| error.to_string())?;
        let embeddings = OpenAiCompatibleClient::new(config.embed.url.clone()).map_err(|error| error.to_string())?;
        let mut count = 0;
        for memory in &memories {
            if count >= args.limit {
                break;
            }
            let sha = format!(
                "{:x}",
                Sha256::digest(
                    format!("{}{}{}", memory.name, memory.description, memory.body).as_bytes()
                )
            );
            if client
                .native_memory_is_current(&memory.file, &sha)
                .await
                .map_err(|error| error.to_string())?
            {
                continue;
            }
            client
                .upsert_native_memory(
                    &memory.file,
                    &memory.name,
                    &memory.description,
                    &memory.body,
                    &sha,
                )
                .await
                .map_err(|error| error.to_string())?;
            let extraction = tokio::time::timeout(Duration::from_secs(90), reasoning.chat(&config.llama_cpp.model, &format!("Extract durable facts and typed triples. Return JSON only: {{\"facts\":[\"claim\"],\"triples\":[{{\"subject\":\"x\",\"relation\":\"uses\",\"object\":\"y\",\"confidence\":0.9,\"temporal\":\"current\"}}]}}. Relations must be one of: {}. Infer temporal as current when present tense applies, formerly for past or replaced states, and use supersedes when wording says replacement, migration, or succession. Confidence is 0 to 1; preserve uncertain triples.\n{}\n{}", RELATION_TAXONOMY.join(", "), memory.description, memory.body))).await.ok().and_then(Result::ok).and_then(|raw| serde_json::from_str::<Extraction>(&raw).ok()).unwrap_or(Extraction { facts: facts(&memory.description, &memory.body), triples: Vec::new() });
            let extracted = if extraction.facts.is_empty() { facts(&memory.description, &memory.body) } else { extraction.facts };
            let source = format!("{}\n{}", memory.description, memory.body);
            let triples = extraction
                .triples
                .into_iter()
                .filter_map(|triple| normalize_triple(triple, &source))
                .collect::<Vec<_>>();
            client
                .replace_native_facts(&memory.file, &extracted)
                .await
                .map_err(|error| error.to_string())?;
            let mut fact_vectors = Vec::new();
            for fact in &extracted {
                if let Ok(vector) = embeddings.embedding(&config.embed.model, fact).await {
                    fact_vectors.push((fact.as_str(), vector));
                }
            }
            client.set_native_fact_embeddings(&memory.file, &fact_vectors).await.map_err(|error| error.to_string())?;
            client
                .replace_native_triples(&memory.file, &triples)
                .await
                .map_err(|error| error.to_string())?;
            client
                .mark_native_triples_current(&memory.file)
                .await
                .map_err(|error| error.to_string())?;
            count += 1;
        }
        Ok::<_, String>(count)
    }
    .await;
    match result {
        Ok(count) => {
            println!("synced {count} native graph memories");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("engram-native-graph-sync: {error}");
            ExitCode::FAILURE
        }
    }
}

fn normalize_triple(triple: Triple, source: &str) -> Option<NativeTriple> {
    let relation = triple
        .relation
        .trim()
        .to_lowercase()
        .replace([' ', '-'], "_");
    let source = source.to_lowercase();
    let temporal = match triple.temporal.trim().to_lowercase().as_str() {
        "formerly" | "former" | "past" => "formerly",
        "superseded" | "supersedes" => "supersedes",
        _ if ["superseded", "replaced by", "migrated to", "succeeded by"]
            .iter()
            .any(|phrase| source.contains(phrase)) =>
        {
            "supersedes"
        }
        _ if ["formerly", "previously", "used to", "was "]
            .iter()
            .any(|phrase| source.contains(phrase)) =>
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

    #[test]
    fn normalizes_temporal_language_and_quarantines_invalid_relations() {
        let triple = Triple {
            subject: "Engram".into(),
            relation: "uses".into(),
            object: "Neo4j".into(),
            confidence: 0.4,
            temporal: String::new(),
        };
        let value = normalize_triple(triple, "Engram previously used Neo4j.").unwrap();
        assert_eq!(value.temporal, "formerly");
        assert_eq!(value.confidence, 0.4);
        let invalid = Triple {
            subject: "Engram".into(),
            relation: "guesses".into(),
            object: "Neo4j".into(),
            confidence: 1.0,
            temporal: String::new(),
        };
        assert!(normalize_triple(invalid, "").is_none());
    }
}
