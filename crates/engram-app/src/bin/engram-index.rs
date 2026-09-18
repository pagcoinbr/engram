use clap::Parser;
use engram_config::Config;
use engram_models::OpenAiCompatibleClient;
use engram_store::load;
use engram_vector::{IndexPoint, QdrantClient};
use regex::Regex;
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
    #[arg(long)]
    rebuild: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(indexed) => {
            println!("indexed {indexed} memory(ies)");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("engram-index: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<usize, String> {
    let config = Config::load(&args.config).map_err(|error| error.to_string())?;
    if !config.vector_store.enabled {
        return Err("vector_store.enabled is false".into());
    }
    let store = args
        .config
        .parent()
        .ok_or("configuration path has no parent")?
        .join("projects")
        .join(&args.slug)
        .join("memory");
    let memories = load(store).map_err(|error| error.to_string())?;
    let vectors = QdrantClient::new(config.vector_store.url, config.vector_store.collection);
    vectors
        .ensure_collection(config.embed.dim, args.rebuild)
        .await
        .map_err(|error| error.to_string())?;
    let embeddings =
        OpenAiCompatibleClient::new(config.embed.url).map_err(|error| error.to_string())?;
    for memory in &memories {
        let body = redact(&memory.body);
        let input = format!(
            "{} {} {}",
            memory.name,
            memory.description,
            truncate(&body, 1500)
        );
        let vector = embeddings
            .embedding(&config.embed.model, &input)
            .await
            .map_err(|error| format!("{}: {error}", memory.file))?;
        if vector.len() != config.embed.dim as usize {
            return Err(format!(
                "{}: embedding dimension {} does not match configured {}",
                memory.file,
                vector.len(),
                config.embed.dim
            ));
        }
        vectors
            .upsert(IndexPoint {
                file: &memory.file,
                name: &memory.name,
                description: &memory.description,
                memory_type: &memory.memory_type,
                slug: &args.slug,
                sha: &digest(&input),
                vector,
            })
            .await
            .map_err(|error| format!("{}: {error}", memory.file))?;
    }
    Ok(memories.len())
}

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn redact(value: &str) -> String {
    Regex::new(r"(?i)(api[_-]?key|token|password|secret)\s*[:=]\s*\S+")
        .expect("valid redaction pattern")
        .replace_all(value, "$1: [REDACTED]")
        .into_owned()
}

fn truncate(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    value
        .char_indices()
        .take_while(|(index, _)| *index <= max_bytes)
        .last()
        .map(|(index, _)| &value[..index])
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{redact, truncate};

    #[test]
    fn redacts_common_key_assignments() {
        assert_eq!(redact("token=abc123"), "token: [REDACTED]");
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        assert_eq!(truncate("éabc", 1), "");
    }
}
