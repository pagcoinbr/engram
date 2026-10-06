use clap::Parser;
use engram_config::Config;
use engram_models::OpenAiCompatibleClient;
use engram_store::load;
use engram_vector::{IndexPoint, QdrantClient};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::ExitCode};

/// Exit code meaning "this configuration is not mine to serve".
///
/// Callers (the daemon, `memory_lib.sh`) gate on the config before reaching for
/// this binary, but they can be out of date or mis-parse the YAML; this makes the
/// refusal unambiguous and machine-checkable rather than a URL-parse failure deep
/// inside the HTTP client.
const EXIT_UNSUPPORTED_PROVIDER: u8 = 3;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "ENGRAM_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long)]
    slug: Option<String>,
    #[arg(long, conflicts_with_all = ["only", "delete"])]
    rebuild: bool,
    #[arg(long, value_name = "FILE", conflicts_with = "delete")]
    only: Vec<String>,
    #[arg(long, value_name = "FILE")]
    delete: Option<String>,
}

enum IndexError {
    UnsupportedProvider(String),
    Other(String),
}

impl<T: Into<String>> From<T> for IndexError {
    fn from(value: T) -> Self {
        Self::Other(value.into())
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(indexed) => {
            println!("indexed {indexed} memory(ies)");
            ExitCode::SUCCESS
        }
        Err(IndexError::UnsupportedProvider(message)) => {
            eprintln!("engram-index: {message}");
            ExitCode::from(EXIT_UNSUPPORTED_PROVIDER)
        }
        Err(IndexError::Other(error)) => {
            eprintln!("engram-index: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<usize, IndexError> {
    let config_path = engram_paths::config_path(args.config);
    let slug = engram_paths::resolve_slug(args.slug.as_deref());
    let config = Config::load(&config_path).map_err(|error| error.to_string())?;
    // The documented master switch outranks vector_store.enabled. Checking only
    // the latter let the save hook index while automation was meant to be off.
    if !config.local_enabled {
        return Err("local_enabled is false".into());
    }
    if !config.vector_store.enabled {
        return Err("vector_store.enabled is false".into());
    }
    // This binary speaks exactly one embedding transport: OpenAI-compatible
    // /v1/embeddings. Ollama and FastEmbed are supported engram configurations
    // served by the Python indexer, so refuse them clearly instead of failing on a
    // URL parse and leaving the memory unindexed.
    if !config.rust_embedding_supported() {
        return Err(IndexError::UnsupportedProvider(format!(
            "embedding provider '{}' is not implemented in Rust (only {:?} with an endpoint are); \
             use the Python indexer for this configuration",
            config.embed_provider(),
            engram_config::RUST_EMBED_PROVIDERS,
        )));
    }
    let store = engram_paths::store_dir(&config_path, &slug);
    let mut memories = load(store).map_err(|error| error.to_string())?;
    let vectors = QdrantClient::from_config(&config);
    vectors
        .ensure_collection(config.embed.dim, args.rebuild)
        .await
        .map_err(|error| error.to_string())?;
    if let Some(file) = args.delete {
        vectors
            .delete(&slug, &file)
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
    let embeddings = OpenAiCompatibleClient::new(config.embed_endpoint())
        .map_err(|error| error.to_string())?
        .with_api_key(config.embed.api_key.present())
        .with_timeout(config.embed_timeout_seconds());
    let space = config.embedding_space_id();
    let mut indexed = 0;
    for memory in &memories {
        let Redacted {
            name,
            description,
            input,
        } = redacted_input(&memory.name, &memory.description, &memory.body);
        // The space id is part of the freshness key, so switching to another model
        // of the SAME dimension invalidates every record instead of leaving two
        // models' vectors mixed in one collection.
        let sha = digest(&format!("{space}\u{0}{input}"));
        if !args.rebuild
            && vectors
                .is_current(&slug, &memory.file, &sha)
                .await
                .map_err(|error| format!("{}: {error}", memory.file))?
        {
            continue;
        }
        // Asymmetric models need the document prefix here and the query prefix at
        // recall time; applying neither indexed and queried in different spaces.
        let vector = embeddings
            .embedding(&config.embed.model, &config.document_text(&input))
            .await
            .map_err(|error| format!("{}: {error}", memory.file))?;
        if vector.len() != config.embed.dim as usize {
            return Err(format!(
                "{}: embedding dimension {} does not match configured {}",
                memory.file,
                vector.len(),
                config.embed.dim
            )
            .into());
        }
        vectors
            .upsert(IndexPoint {
                file: &memory.file,
                name: &name,
                description: &description,
                memory_type: &memory.memory_type,
                slug: &slug,
                sha: &sha,
                space: &space,
                vector,
            })
            .await
            .map_err(|error| format!("{}: {error}", memory.file))?;
        indexed += 1;
    }
    if args.only.is_empty() && !args.rebuild {
        let present: std::collections::HashSet<_> =
            memories.iter().map(|memory| memory.file.as_str()).collect();
        for file in vectors
            .files(&slug)
            .await
            .map_err(|error| error.to_string())?
        {
            if !present.contains(file.as_str()) {
                vectors
                    .delete(&slug, &file)
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(indexed)
}

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
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

/// One memory's text, scrubbed for both the embedding request and the payload.
struct Redacted {
    name: String,
    description: String,
    input: String,
}

/// Redact BEFORE the embedding request: the endpoint may be off-box, and
/// imported or hand-edited memories never passed the save-time guard.
///
/// All three fields, not just the body. Redacting the body alone while
/// interpolating `name` and `description` raw sent frontmatter secrets straight
/// to the endpoint and then stored the raw description in the Qdrant payload —
/// precisely the imported/hand-edited case this guard exists for.
fn redacted_input(name: &str, description: &str, body: &str) -> Redacted {
    let (name, _) = engram_secrets::redact(name);
    let (description, _) = engram_secrets::redact(description);
    let (body, _) = engram_secrets::redact(body);
    let input = format!("{} {} {}", name, description, truncate(&body, 1500));
    Redacted {
        name,
        description,
        input,
    }
}

#[cfg(test)]
mod tests {
    use super::{redacted_input, truncate};

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        assert_eq!(truncate("éabc", 1), "");
    }

    /// Redaction now comes from the shared detector (`engram-secrets`), which the
    /// local one-line regex could not match for most credential classes. Pinned
    /// here because this is the call site that sends text to a remote endpoint.
    #[test]
    fn redaction_covers_what_the_old_local_regex_missed() {
        for sample in [
            "token=abc123456",
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123",
            "-----BEGIN RSA PRIVATE KEY-----",
            "mnemonic: abandon abandon abandon abandon abandon ability",
            "AKIAIOSFODNN7EXAMPLE",
        ] {
            let (masked, count) = engram_secrets::redact(sample);
            assert!(count > 0, "not redacted: {sample}");
            assert!(masked.contains(engram_secrets::REDACTION), "{masked}");
        }
    }

    /// Frontmatter prose is as exposed as the body.
    ///
    /// Only `body` used to be scrubbed, while `name` and `description` were
    /// interpolated raw into the embedding request AND stored verbatim in the
    /// Qdrant payload — so a secret in frontmatter crossed the boundary twice.
    ///
    /// Scope note: the filename and type still go to Qdrant unredacted, and
    /// deliberately so — the filename is the record's identity and redacting it
    /// would break lookup and pruning. Secrets in *filenames* are not covered by
    /// this guard.
    #[test]
    fn redaction_covers_every_prose_field_sent_off_box() {
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let out = redacted_input(
            &format!("key {secret}"),
            &format!("desc {secret}"),
            &format!("body {secret}"),
        );
        for (field, value) in [
            ("input", &out.input),
            ("name", &out.name),
            ("description", &out.description),
        ] {
            assert!(
                !value.contains(secret),
                "{field} still carries the credential: {value}"
            );
            assert!(
                value.contains(engram_secrets::REDACTION),
                "{field}: {value}"
            );
        }
    }
}
