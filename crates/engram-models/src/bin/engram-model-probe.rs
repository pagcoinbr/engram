use clap::Parser;
use engram_models::OpenAiCompatibleClient;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    model: String,
    #[arg(long)]
    expected_dimension: Option<u32>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let result = async {
        let client = OpenAiCompatibleClient::new(args.endpoint)?;
        client.probe(&args.model, args.expected_dimension).await
    }
    .await;
    match result {
        Ok(report) => println!("{}", serde_json::to_string_pretty(&report).unwrap()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
