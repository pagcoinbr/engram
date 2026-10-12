use clap::Parser;
use engram_models::OpenAiCompatibleClient;
use engram_vector::{QdrantClient, Scope};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    embed_endpoint: String,
    #[arg(long)]
    model: String,
    #[arg(long, default_value = "http://127.0.0.1:6333")]
    qdrant_url: String,
    #[arg(long, default_value = "engram_memory")]
    collection: String,
    #[arg(long)]
    query: String,
    #[arg(long, allow_hyphen_values = true)]
    slug: Option<String>,
    /// Which embedding space to search. Required: this binary is a low-level
    /// probe with no config, so it cannot derive the active space itself, and
    /// searching without pinning one scores the query against points written by
    /// a different model — which returns confident nonsense rather than an
    /// error. It was optional, which made the dangerous call the shorter one.
    ///
    /// Get it from `engram-app`'s /api/v1/status, or compute it with
    /// `bin/engram_llm.py::embedding_space_id`.
    #[arg(long)]
    space: String,
}
#[tokio::main]
async fn main() {
    let args = Args::parse();
    let result = async {
        let embeddings = OpenAiCompatibleClient::new(args.embed_endpoint)?;
        let vector = embeddings.embedding(&args.model, &args.query).await?;
        let scope = match args.slug.as_deref() {
            Some(slug) => Scope::slug(&args.space, slug),
            None => Scope::space(&args.space),
        };
        let hits = QdrantClient::new(args.qdrant_url, args.collection)
            .search(vector, 6, scope)
            .await?;
        Ok::<_, Box<dyn std::error::Error>>(hits)
    }
    .await;
    match result {
        Ok(hits) => println!("{}", serde_json::to_string_pretty(&hits).unwrap()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
