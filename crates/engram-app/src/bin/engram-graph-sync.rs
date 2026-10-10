use clap::Parser;
use std::{
    path::PathBuf,
    process::{Command, ExitCode},
};

#[derive(Parser)]
struct Args {
    #[arg(long, env = "ENGRAM_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long)]
    graph_dir: Option<PathBuf>,
    #[arg(long, env = "ENGRAM_GRAPH_PYTHON")]
    python: Option<PathBuf>,
    #[arg(long, default_value_t = 25)]
    limit: usize,
    // allow_hyphen_values because EVERY engram slug starts with '-'.
    /// Which store to operate on. Forwarded to graph_sync.py.
    #[arg(long, allow_hyphen_values = true)]
    slug: Option<String>,
    /// Which identity to write as. Forwarded to graph_sync.py, which refuses to
    /// insert without it once tenants are configured — writing into the shared
    /// Graphiti group is what made the graph leg cross projects.
    #[arg(long, env = "ENGRAM_TENANT")]
    tenant: Option<String>,
    #[arg(long, value_parser = ["insert", "export", "reconcile"], default_value = "insert")]
    mode: String,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let config = engram_paths::config_path(args.config);
    let graph_dir = args.graph_dir.unwrap_or_else(engram_paths::graph_dir);
    let python = args
        .python
        .unwrap_or_else(|| graph_dir.join("venv/bin/python"));
    let interpreter = if python.is_file() {
        python
    } else {
        PathBuf::from("python3")
    };
    let mut command = Command::new(interpreter);
    command.arg(graph_dir.join("graph_sync.py"));
    // Forward the scope before the mode flags: graph_sync.py reads ONE store and
    // writes ONE group, so a wrapper that dropped these silently operated on
    // whichever store the environment named.
    if let Some(slug) = args.slug.as_deref() {
        command.arg("--slug").arg(slug);
    }
    if let Some(tenant) = args.tenant.as_deref() {
        command.arg("--tenant").arg(tenant);
    }
    // Hand the child the SAME resolved paths rather than letting it re-derive them.
    // A parent and child disagreeing about which engram.yaml is active is how one
    // installation's daemon ended up acting on another's store.
    command.env("ENGRAM_CONFIG", &config);
    command.env("ENGRAM_GRAPH", &graph_dir);
    match args.mode.as_str() {
        "insert" => {
            command
                .arg("--insert")
                .arg("--limit")
                .arg(args.limit.to_string());
        }
        "export" => {
            command.arg("--export").arg("--verify");
        }
        "reconcile" => {
            command.arg("--reconcile");
        }
        _ => unreachable!(),
    }
    let status = command.status();
    match status {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(1, 255) as u8),
        Err(error) => {
            eprintln!("engram-graph-sync: {error}");
            ExitCode::FAILURE
        }
    }
}
