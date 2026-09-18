use engram_config::Config;
use engram_graph::GraphClient;
use engram_models::OpenAiCompatibleClient;
use engram_retrieval::{bm25, rrf};
use engram_store::{Memory, load};
use engram_vector::QdrantClient;
use serde::Serialize;
use std::{collections::HashMap, path::Path};

#[derive(Serialize)]
pub struct ResultItem {
    pub file: String,
    pub name: String,
    pub description: String,
    pub sources: Vec<String>,
}
#[derive(Serialize)]
pub struct Output {
    pub query: String,
    pub results: Vec<ResultItem>,
    pub facts: Vec<String>,
    pub legs: HashMap<String, String>,
}

pub async fn recall(
    config_path: &Path,
    slug: &str,
    query: &str,
    k: usize,
) -> Result<Output, String> {
    let config = Config::load(config_path).map_err(|error| error.to_string())?;
    let store = config_path
        .parent()
        .ok_or("configuration path has no parent")?
        .join("projects")
        .join(slug)
        .join("memory");
    let memories = load(store).map_err(|error| error.to_string())?;
    let mut legs = HashMap::new();
    let keyword = bm25(&memories, query, k * 2)
        .into_iter()
        .map(|hit| hit.file)
        .collect::<Vec<_>>();
    legs.insert("keyword".into(), "ok".into());
    let vector = vector_leg(&config, query, slug, k * 2, &mut legs).await;
    let (graph, facts) = graph_leg(&config, query, k * 2, &mut legs).await;
    let by_file: HashMap<String, &Memory> = memories
        .iter()
        .map(|memory| (memory.file.clone(), memory))
        .collect();
    let results = rrf(&[keyword.clone(), vector.clone(), graph.clone()], k, 60.0)
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
            if graph.contains(&hit.file) {
                sources.push("graph".into());
            }
            Some(ResultItem {
                file: memory.file.clone(),
                name: memory.name.clone(),
                description: memory.description.clone(),
                sources,
            })
        })
        .collect();
    Ok(Output {
        query: query.into(),
        results,
        facts,
        legs,
    })
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

async fn graph_leg(
    _config: &Config,
    query: &str,
    k: usize,
    legs: &mut HashMap<String, String>,
) -> (Vec<String>, Vec<String>) {
    let password = std::env::var("NEO4J_PASSWORD").unwrap_or_default();
    if password.is_empty() {
        legs.insert("graph".into(), "password unavailable".into());
        return (Vec::new(), Vec::new());
    }
    let tokens = query
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .filter(|word| word.len() >= 4)
        .take(6)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let uri = std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://127.0.0.1:7687".into());
    let database = std::env::var("NEO4J_DATABASE").unwrap_or_else(|_| "neo4j".into());
    let Ok(client) = GraphClient::new(&uri, &database, "neo4j", password) else {
        legs.insert("graph".into(), "invalid client".into());
        return (Vec::new(), Vec::new());
    };
    let mut facts = client
        .native_facts_for_tokens(&tokens, 6)
        .await
        .unwrap_or_default();
    facts.sort();
    facts.dedup();
    let native_keyword = client
        .native_keyword_files(query, k)
        .await
        .unwrap_or_default();
    legs.insert("graph".into(), "ok".into());
    (
        rrf(
            &[native_keyword.iter().map(|hit| hit.file.clone()).collect()],
            k,
            60.0,
        )
        .into_iter()
        .map(|hit| hit.file)
        .collect(),
        facts,
    )
}
