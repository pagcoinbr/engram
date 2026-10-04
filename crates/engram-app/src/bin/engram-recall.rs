use clap::Parser;
use engram_hybrid::recall;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    /// Resolved by engram-paths when omitted, so this is not pinned to one host.
    #[arg(long, env = "ENGRAM_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long)]
    slug: Option<String>,
    #[arg(long, default_value_t = 6)]
    k: usize,
    query: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let config = engram_paths::config_path(args.config);
    // A CLI invocation runs inside whatever project the operator is sitting in, so
    // the cwd-derived store is a sensible step in the chain — the same order the
    // Python CLI uses.
    let slug = engram_paths::resolve_slug_in_cwd(args.slug.as_deref());
    match recall(&config, &slug, &args.query, args.k).await {
        Ok(output) => println!("{}", serde_json::to_string_pretty(&output).unwrap()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
