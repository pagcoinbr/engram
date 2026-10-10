//! Index one tenant's Obsidian vault into its wiki collection.
//!
//! Modelled on `engram-index`, and sharing its gates deliberately: the same
//! master switch, the same provider refusal with the same exit code, the same
//! redaction before anything crosses the network, the same scroll-and-prune for
//! documents that have left the vault. Where it differs is the shape of the
//! record — N chunks per document rather than one point per file — and that
//! difference drives every other difference here:
//!
//! - **Freshness is per DOCUMENT, not per chunk.** One sha, computed from the
//!   whole file, is written onto every chunk. An unchanged page then costs one
//!   comparison instead of one per chunk.
//! - **The chunker's version is in that sha.** Changing chunk boundaries has to
//!   invalidate stored chunks, or a re-chunk leaves documents looking current
//!   while their chunks no longer match any boundary the chunker would produce.
//!   The exact reason `embedding_space_id` is in there too.
//! - **Shrink is handled by writing before trimming.** An edit that shortens a
//!   page leaves orphan tail chunks; deleting first would blank the page from
//!   recall for the duration of the re-embed.

use clap::Parser;
use engram_models::OpenAiCompatibleClient;
use engram_tenant::Tenant;
use engram_vector::{Corpus, QdrantClient, WikiPoint};
use engram_wiki::{CHUNKER_VERSION, ChunkParams};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::ExitCode};

/// Same contract as `engram-index`: "this configuration is not mine to serve".
const EXIT_UNSUPPORTED_PROVIDER: u8 = 3;

#[derive(Parser)]
#[command(about = "Index a tenant's Obsidian vault for search")]
struct Args {
    #[arg(long, env = "ENGRAM_CONFIG")]
    config: Option<PathBuf>,
    /// Which identity's vault to index. Required here even though store-scoped
    /// callers can derive it: a vault is named by the TENANT, not by a slug, so
    /// there is no store from which to follow the identity.
    #[arg(long, env = "ENGRAM_TENANT")]
    tenant: Option<String>,
    /// Re-embed every document, ignoring the freshness sha.
    #[arg(long)]
    rebuild: bool,
    /// Only these vault-relative paths.
    #[arg(long, value_name = "PATH")]
    only: Vec<String>,
    /// Report what would change without writing.
    #[arg(long)]
    dry_run: bool,
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
        Ok(report) => {
            println!(
                "{} document(s), {} chunk(s) indexed; {} unchanged, {} pruned",
                report.documents, report.chunks, report.unchanged, report.pruned
            );
            ExitCode::SUCCESS
        }
        Err(IndexError::UnsupportedProvider(message)) => {
            eprintln!("engram-wiki-index: {message}");
            ExitCode::from(EXIT_UNSUPPORTED_PROVIDER)
        }
        Err(IndexError::Other(error)) => {
            eprintln!("engram-wiki-index: {error}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Default)]
struct Report {
    documents: usize,
    chunks: usize,
    unchanged: usize,
    pruned: usize,
}

async fn run(args: Args) -> Result<Report, IndexError> {
    let config_path = engram_paths::config_path(args.config);
    let config = engram_config::Config::load(&config_path).map_err(|e| e.to_string())?;
    let tenant = Tenant::resolve(&config, args.tenant.as_deref()).map_err(|e| e.to_string())?;

    if !config.local_enabled {
        return Err("local_enabled is false".into());
    }
    if !config.vector_store.enabled {
        return Err("vector_store.enabled is false".into());
    }
    if !config.rust_embedding_supported() {
        return Err(IndexError::UnsupportedProvider(format!(
            "embedding provider '{}' is not implemented in Rust (only {:?} with an endpoint are)",
            config.embed_provider(),
            engram_config::RUST_EMBED_PROVIDERS,
        )));
    }
    let Some(vault) = tenant.vault() else {
        return Err(format!(
            "tenant '{}' has no vault configured; add `vault: /path/to/vault` under it \
             in engram.yaml",
            tenant.label()
        )
        .into());
    };
    // Through the tenant, so a symlinked vault root is canonicalized once and
    // every containment check below compares against the same real path.
    let root = tenant.vault_root().map_err(|e| e.to_string())?;
    let paths = engram_wiki::walk(&root).map_err(|e| e.to_string())?;
    println!(
        "tenant {} vault {} — {} document(s) on disk",
        tenant.label(),
        vault.display(),
        paths.len()
    );

    let vectors = QdrantClient::for_tenant(&config, &tenant, Corpus::Wiki);
    let space = config.embedding_space_id();
    let mut report = Report::default();

    if !args.dry_run {
        vectors
            .ensure_collection(config.embed.dim, args.rebuild)
            .await
            .map_err(|e| e.to_string())?;
    }
    // Existing path -> sha, for freshness and for pruning documents that have
    // left the vault. One scroll rather than a round trip per document.
    let existing = match args.dry_run && args.rebuild {
        true => Default::default(),
        false => vectors
            .wiki_documents(tenant.graph_group())
            .await
            .map_err(|e| e.to_string())?,
    };

    let embeddings = OpenAiCompatibleClient::new(config.embed_endpoint())
        .map_err(|e| e.to_string())?
        .with_api_key(config.embed.api_key.present())
        .with_timeout(config.embed_timeout_seconds());
    let params = ChunkParams::default();

    let selected: Vec<&String> = match args.only.is_empty() {
        true => paths.iter().collect(),
        false => {
            let wanted: Vec<String> = args.only.iter().map(|p| p.replace('\\', "/")).collect();
            let selected: Vec<&String> =
                paths.iter().filter(|path| wanted.contains(path)).collect();
            if selected.len() != wanted.len() {
                return Err("one or more --only paths are not present in the vault".into());
            }
            selected
        }
    };

    for path in selected {
        let absolute = engram_wiki::absolute(&root, path);
        let metadata = std::fs::metadata(&absolute).map_err(|e| format!("{path}: {e}"))?;
        let source_mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|age| age.as_secs() as i64)
            .unwrap_or_default();
        let raw = std::fs::read_to_string(&absolute).map_err(|e| format!("{path}: {e}"))?;

        // The freshness key: embedding space, chunker version, content. Leaving
        // the chunker version out would let a boundary change go unnoticed.
        let sha = digest(&format!("{space}\u{0}{CHUNKER_VERSION}\u{0}{}", raw));
        if !args.rebuild && existing.get(path.as_str()) == Some(&sha) {
            report.unchanged += 1;
            continue;
        }

        let document = engram_wiki::parse(path, &raw, source_mtime);
        let chunks = engram_wiki::chunk(&document, params);
        if chunks.is_empty() {
            // An emptied page still has to lose its old chunks.
            if !args.dry_run {
                vectors
                    .delete_document(tenant.graph_group(), path)
                    .await
                    .map_err(|e| format!("{path}: {e}"))?;
            }
            report.pruned += 1;
            continue;
        }
        if args.dry_run {
            println!("  would index {path} — {} chunk(s)", chunks.len());
            report.documents += 1;
            report.chunks += chunks.len();
            continue;
        }

        let count = chunks.len();
        let mut points = Vec::with_capacity(count);
        let mut breadcrumbs = Vec::with_capacity(count);
        let mut texts = Vec::with_capacity(count);
        for chunk in &chunks {
            // Redact BEFORE the embedding request. A vault is hand-authored and
            // imported, so it never passed a save-time guard, and the endpoint
            // may be off-box — the same boundary engram-index redacts at.
            let (text, _) = engram_secrets::redact(&chunk.text);
            let (breadcrumb, _) = engram_secrets::redact(&chunk.breadcrumb());
            breadcrumbs.push(breadcrumb);
            texts.push(text);
        }
        for (index, chunk) in chunks.iter().enumerate() {
            let embedded = match breadcrumbs[index].is_empty() {
                true => texts[index].clone(),
                false => format!("{}\n\n{}", breadcrumbs[index], texts[index]),
            };
            let vector = embeddings
                .embedding(&config.embed.model, &config.document_text(&embedded))
                .await
                .map_err(|e| format!("{path} chunk {}: {e}", chunk.index))?;
            if vector.len() != config.embed.dim as usize {
                return Err(format!(
                    "{path} chunk {}: embedding dimension {} does not match configured {}",
                    chunk.index,
                    vector.len(),
                    config.embed.dim
                )
                .into());
            }
            points.push((index, vector));
        }
        let (safe_title, _) = engram_secrets::redact(&document.title);
        // Tags are authored/imported frontmatter and ride into the Qdrant payload
        // unembedded — redact each at the same boundary as title/text, so a token in
        // `tags: [ghp_…]` cannot be stored in the vector index.
        let safe_tags: Vec<String> = document
            .tags
            .iter()
            .map(|tag| engram_secrets::redact(tag).0)
            .collect();
        let wiki_points: Vec<WikiPoint<'_>> = points
            .into_iter()
            .map(|(index, vector)| WikiPoint {
                tenant: tenant.graph_group(),
                path,
                title: &safe_title,
                heading_path: &breadcrumbs[index],
                index,
                count,
                tags: &safe_tags,
                text: &texts[index],
                sha: &sha,
                space: &space,
                source_mtime,
                vector,
            })
            .collect();
        vectors
            .upsert_chunks(&wiki_points)
            .await
            .map_err(|e| format!("{path}: {e}"))?;
        // Trim AFTER writing: an edit that shortened the page leaves orphan tail
        // chunks, and deleting first would blank the page from recall for the
        // duration of the re-embed.
        vectors
            .prune_chunks(tenant.graph_group(), path, count)
            .await
            .map_err(|e| format!("{path}: {e}"))?;
        report.documents += 1;
        report.chunks += count;
    }

    // Documents that have left the vault. Skipped under --only, which has no
    // view of the whole vault and would prune everything it was not given.
    if args.only.is_empty() && !args.dry_run {
        for path in existing.keys() {
            if !paths.contains(path) {
                vectors
                    .delete_document(tenant.graph_group(), path)
                    .await
                    .map_err(|e| format!("pruning {path}: {e}"))?;
                report.pruned += 1;
            }
        }
    }
    Ok(report)
}

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
