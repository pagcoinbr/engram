use engram_config::Config;
use engram_graph::GraphClient;
use engram_models::OpenAiCompatibleClient;
use engram_retrieval::{bm25, rrf};
use engram_store::{Memory, load};
use engram_vector::QdrantClient;
use serde::Serialize;
use std::{collections::HashMap, path::Path, process::Command};

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
    recall_with_config(config, config_path, slug, query, k).await
}

async fn recall_with_config(
    config: Config,
    config_path: &Path,
    slug: &str,
    query: &str,
    k: usize,
) -> Result<Output, String> {
    let store = config_path
        .parent()
        .ok_or("configuration path has no parent")?
        .join("projects")
        .join(slug)
        .join("memory");
    let memories = load(store).map_err(|error| error.to_string())?;
    let mut legs = HashMap::new();
    let graphiti_compat = graph_backend(&config) == "graphiti_compat";
    let keyword = if graphiti_compat {
        legs.insert("keyword".into(), "disabled by graphiti_compat".into());
        Vec::new()
    } else {
        legs.insert("keyword".into(), "ok".into());
        bm25(&memories, query, k * 2)
            .into_iter()
            .map(|hit| hit.file)
            .collect::<Vec<_>>()
    };
    let vector = if graphiti_compat {
        legs.insert("vector".into(), "disabled by graphiti_compat".into());
        Vec::new()
    } else {
        vector_leg(&config, query, slug, k * 2, &mut legs).await
    };
    let graph_limit = if graphiti_compat { k } else { k * 2 };
    let (graph, facts) = graph_leg(&config, query, graph_limit, &mut legs).await;
    if graphiti_compat
        && legs
            .get("graph")
            .is_some_and(|value| value != "graphiti_compat")
    {
        return Err("Graphiti compatibility recall is unavailable".into());
    }
    let by_file: HashMap<String, &Memory> = memories
        .iter()
        .map(|memory| (memory.file.clone(), memory))
        .collect();
    let ranked = if graphiti_compat {
        graph.clone()
    } else {
        rrf(&[keyword.clone(), vector.clone(), graph.clone()], k, 60.0)
            .into_iter()
            .map(|hit| hit.file)
            .collect()
    };
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
            if graph.contains(&file) {
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

fn graph_backend(config: &Config) -> String {
    std::env::var("ENGRAM_GRAPH_BACKEND").unwrap_or_else(|_| config.graph.backend.clone())
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
    config: &Config,
    query: &str,
    k: usize,
    legs: &mut HashMap<String, String>,
) -> (Vec<String>, Vec<String>) {
    let backend = graph_backend(config);
    if backend == "graphiti_compat" {
        let graph_dir =
            std::env::var("ENGRAM_GRAPH").unwrap_or_else(|_| "/root/.claude/graph".into());
        let python = format!("{graph_dir}/venv/bin/python");
        let interpreter = if Path::new(&python).is_file() {
            python
        } else {
            "python3".into()
        };
        let output = Command::new(interpreter)
            .arg(format!("{graph_dir}/memory_graph_recall.py"))
            .arg(query)
            .arg("--k")
            .arg(k.to_string())
            .arg("--json")
            .output();
        if let Ok(output) = output
            && output.status.success()
        {
            let Ok(records) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)
            else {
                legs.insert("graph".into(), "graphiti_compat invalid response".into());
                return (Vec::new(), Vec::new());
            };
            let files = records
                .iter()
                .filter_map(|row| row.get("file").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect();
            let facts = records
                .iter()
                .flat_map(|row| {
                    row.get("facts")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect();
            legs.insert("graph".into(), "graphiti_compat".into());
            return (files, facts);
        }
        legs.insert("graph".into(), "graphiti_compat unavailable".into());
        return (Vec::new(), Vec::new());
    }
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
    let keyword = client
        .native_keyword_files(query, k)
        .await
        .unwrap_or_default();
    let semantic = match OpenAiCompatibleClient::new(config.embed.url.clone()) {
        Ok(embeddings) => match embeddings.embedding(&config.embed.model, query).await {
            Ok(vector) => client
                .native_semantic_files(&vector, k)
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    legs.insert("graph".into(), "ok".into());
    (
        rrf(
            &[
                keyword.iter().map(|hit| hit.file.clone()).collect(),
                semantic.iter().map(|hit| hit.file.clone()).collect(),
            ],
            k,
            60.0,
        )
        .into_iter()
        .map(|hit| hit.file)
        .collect(),
        facts,
    )
}
