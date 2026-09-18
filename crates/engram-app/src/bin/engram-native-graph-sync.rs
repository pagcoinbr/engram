use clap::Parser;
use engram_graph::GraphClient;
use engram_store::load;
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
            client
                .replace_native_facts(&memory.file, &facts(&memory.description, &memory.body))
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
