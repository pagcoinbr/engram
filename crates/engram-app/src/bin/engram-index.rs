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
    #[arg(long, conflicts_with_all = ["only", "delete"])]
    rebuild: bool,
    #[arg(long, value_name = "FILE", conflicts_with = "delete")]
    only: Vec<String>,
    #[arg(long, value_name = "FILE")]
    delete: Option<String>,
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
    let mut memories = load(store).map_err(|error| error.to_string())?;
    let vectors = QdrantClient::new(config.vector_store.url, config.vector_store.collection);
    vectors
        .ensure_collection(config.embed.dim, args.rebuild)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(file) = args.delete {
        vectors
            .delete(&args.slug, &file)
            .await
            .map_err(|error| error.to_string())?;
        return Ok(0);
    }
    if !args.only.is_empty() {
        memories.retain(|memory| args.only.iter().any(|file| file == &memory.file));
        if memories.len() != args.only.len() {
            return Err("one or more --only files are not present in the memory store".into());
        }
    }
    let embeddings =
        OpenAiCompatibleClient::new(config.embed.url).map_err(|error| error.to_string())?;
    let mut indexed = 0;
    for memory in &memories {
        let body = redact(&memory.body);
        let input = format!(
            "{} {} {}",
            memory.name,
            memory.description,
            truncate(&body, 1500)
        );
        let sha = digest(&input);
        if !args.rebuild
            && vectors
                .is_current(&args.slug, &memory.file, &sha)
                .await
                .map_err(|error| format!("{}: {error}", memory.file))?
        {
            continue;
        }
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
                sha: &sha,
                vector,
            })
            .await
            .map_err(|error| format!("{}: {error}", memory.file))?;
        indexed += 1;
    }
    Ok(indexed)
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
