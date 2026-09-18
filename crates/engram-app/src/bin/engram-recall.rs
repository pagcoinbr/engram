use clap::Parser;
use engram_config::Config;
use engram_graph::GraphClient;
use engram_models::OpenAiCompatibleClient;
use engram_retrieval::{bm25, rrf};
use engram_store::{Memory, load};
use engram_vector::QdrantClient;
use serde::Serialize;
use std::{collections::HashMap, path::PathBuf};

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
    #[arg(long, default_value_t = 6)]
    k: usize,
    query: String,
}

#[derive(Serialize)]
struct ResultItem {
    file: String,
    name: String,
    description: String,
    sources: Vec<String>,
}
#[derive(Serialize)]
struct Output {
    query: String,
    results: Vec<ResultItem>,
    facts: Vec<String>,
    legs: HashMap<String, String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let config = match Config::load(&args.config) {
        Ok(config) => config,
        Err(error) => fail(error),
    };
    let store_dir = args
        .config
        .parent()
        .unwrap()
        .join("projects")
        .join(&args.slug)
        .join("memory");
    let memories = match load(store_dir) {
        Ok(memories) => memories,
        Err(error) => fail(error),
    };
    let mut legs = HashMap::new();
    let keyword = bm25(&memories, &args.query, args.k * 2)
        .into_iter()
        .map(|hit| hit.file)
        .collect::<Vec<_>>();
    legs.insert("keyword".into(), "ok".into());
    let vector = vector_leg(&config, &args.query, &args.slug, args.k * 2, &mut legs).await;
    let facts = graph_leg(&args.query, &mut legs).await;
    let rankings = vec![keyword.clone(), vector.clone()];
    let by_file: HashMap<String, &Memory> = memories
        .iter()
        .map(|memory| (memory.file.clone(), memory))
        .collect();
    let results = rrf(&rankings, args.k, 60.0)
        .into_iter()
        .filter_map(|hit| {
            let memory = by_file.get(&hit.file)?;
            let mut sources = Vec::new();
            if keyword.contains(&hit.file) {
                sources.push("keyword".into());
            }
            if vector.contains(&hit.file) {
                sources.push("vector".into());
            }
            Some(ResultItem {
                file: memory.file.clone(),
                name: memory.name.clone(),
                description: memory.description.clone(),
                sources,
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            query: args.query,
            results,
            facts,
            legs
        })
        .unwrap()
    );
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
    if !matches!(config.embed.provider.as_str(), "llama_cpp" | "openai") {
        legs.insert("vector".into(), "unsupported provider".into());
        return Vec::new();
    }
    let result = async {
        let embeddings = OpenAiCompatibleClient::new(config.embed.url.clone())?;
        let vector = embeddings.embedding(&config.embed.model, query).await?;
        let hits = QdrantClient::new(
            config.vector_store.url.clone(),
            config.vector_store.collection.clone(),
        )
        .search(vector, k, Some(slug))
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

async fn graph_leg(query: &str, legs: &mut HashMap<String, String>) -> Vec<String> {
    let password = std::env::var("NEO4J_PASSWORD").unwrap_or_default();
    if password.is_empty() {
        legs.insert("graph".into(), "password unavailable".into());
        return Vec::new();
    }
    let tokens = query
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .filter(|word| word.len() >= 4)
        .take(6)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let uri = std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://127.0.0.1:7687".into());
    let database = std::env::var("NEO4J_DATABASE").unwrap_or_else(|_| "neo4j".into());
    match GraphClient::new(&uri, &database, "neo4j", password) {
        Ok(client) => match client.facts_for_tokens(&tokens, 6).await {
            Ok(facts) => {
                legs.insert("graph".into(), "ok".into());
                facts
            }
            Err(error) => {
                legs.insert("graph".into(), error.to_string());
                Vec::new()
            }
        },
        Err(error) => {
            legs.insert("graph".into(), error.to_string());
            Vec::new()
        }
    }
}

fn fail(error: impl std::fmt::Display) -> ! {
    eprintln!("{error}");
    std::process::exit(1)
}
