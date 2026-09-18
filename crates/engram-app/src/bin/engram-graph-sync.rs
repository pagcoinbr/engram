use clap::Parser;
use std::{
    path::PathBuf,
    process::{Command, ExitCode},
};

#[derive(Parser)]
struct Args {
    #[arg(
        long,
        env = "ENGRAM_CONFIG",
        default_value = "/root/.claude/engram.yaml"
    )]
    config: PathBuf,
    #[arg(long, env = "ENGRAM_GRAPH")]
    graph_dir: Option<PathBuf>,
    #[arg(long, env = "ENGRAM_GRAPH_PYTHON")]
    python: Option<PathBuf>,
    #[arg(long, default_value_t = 25)]
    limit: usize,
    #[arg(long, value_parser = ["insert", "export", "reconcile"], default_value = "insert")]
    mode: String,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let graph_dir = args.graph_dir.unwrap_or_else(|| {
        args.config
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("graph")
    });
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
