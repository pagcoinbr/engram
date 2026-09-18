use clap::Parser;
use engram_hybrid::recall;
use std::path::PathBuf;

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
    #[arg(long, default_value_t = 6)]
    k: usize,
    query: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    match recall(&args.config, &args.slug, &args.query, args.k).await {
        Ok(output) => println!("{}", serde_json::to_string_pretty(&output).unwrap()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
