use clap::Parser;
use engram_models::OpenAiCompatibleClient;
use engram_vector::QdrantClient;

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
    #[arg(long)]
    slug: Option<String>,
}
#[tokio::main]
async fn main() {
    let args = Args::parse();
    let result = async {
        let embeddings = OpenAiCompatibleClient::new(args.embed_endpoint)?;
        let vector = embeddings.embedding(&args.model, &args.query).await?;
        let hits = QdrantClient::new(args.qdrant_url, args.collection)
            .search(vector, 6, args.slug.as_deref())
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
