use clap::Parser;
use engram_config::Config;
use engram_graph::GraphClient;
use engram_models::OpenAiCompatibleClient;
use engram_store::load;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::ExitCode};

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
        for memory in &memories {
            let sha = format!(
                "{:x}",
                Sha256::digest(
                    format!("{}{}{}", memory.name, memory.description, memory.body).as_bytes()
                )
            );
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
            let extracted = reasoning.chat(&config.llama_cpp.model, &format!("Extract durable facts and typed triples. Return JSON only: {{\"facts\":[\"claim\"],\"triples\":[{{\"subject\":\"x\",\"relation\":\"uses\",\"object\":\"y\",\"confidence\":0.9,\"temporal\":\"current\"}}]}}.\n{}\n{}", memory.description, memory.body)).await.ok().and_then(|raw| serde_json::from_str::<Extraction>(&raw).ok()).map(|value| { let mut facts = value.facts; facts.extend(value.triples.into_iter().filter(|triple| triple.confidence >= 0.7).map(|triple| format!("{} {} {} ({})", triple.subject, triple.relation, triple.object, triple.temporal))); facts }).filter(|facts| !facts.is_empty()).unwrap_or_else(|| facts(&memory.description, &memory.body));
            client
                .replace_native_facts(&memory.file, &extracted)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok::<_, String>(memories.len())
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

fn facts(description: &str, body: &str) -> Vec<String> {
    format!("{description} {body}")
        .split(['.', '!', '?', '\n'])
        .map(str::trim)
        .filter(|fact| fact.len() >= 20)
        .take(12)
        .map(str::to_string)
        .collect()
}
