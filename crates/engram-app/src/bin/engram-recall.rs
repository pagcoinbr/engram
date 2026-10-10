use clap::Parser;
use engram_hybrid::recall;
use engram_tenant::{Derive, resolve_for_cli};
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    /// Resolved by engram-paths when omitted, so this is not pinned to one host.
    #[arg(long, env = "ENGRAM_CONFIG")]
    config: Option<PathBuf>,
    // allow_hyphen_values because EVERY engram slug starts with '-' (it is a
    // path with separators replaced), so clap read `--slug -home-alice` as a
    // missing value followed by an unknown flag. The documented invocation was
    // unusable as typed.
    #[arg(long, allow_hyphen_values = true)]
    slug: Option<String>,
    /// Which agent identity to recall as. Required on an install that defines
    /// tenants — including when it defines only one. Unlike --slug this needs no
    /// allow_hyphen_values: tenant names may not start with '-'.
    #[arg(long, env = "ENGRAM_TENANT")]
    tenant: Option<String>,
    #[arg(long, default_value_t = 6)]
    k: usize,
    query: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let config_path = engram_paths::config_path(args.config);
    // A CLI runs inside whatever project the operator is sitting in, so the
    // working directory is a reasonable hint for which store to use.
    let resolved = match resolve_for_cli(
        &config_path,
        args.tenant.as_deref(),
        args.slug.as_deref(),
        Derive::Cwd,
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("engram-recall: {error}");
            std::process::exit(2);
        }
    };
    match recall(
        &config_path,
        &resolved.tenant,
        &resolved.slug,
        &args.query,
        args.k,
    )
    .await
    {
        Ok(output) => println!("{}", serde_json::to_string_pretty(&output).unwrap()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
