use clap::Parser;
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    path::PathBuf,
    process::Command,
};

#[derive(Parser)]
struct Args {
    #[arg(long, env = "ENGRAM_GRAPH", default_value = "/root/.claude/graph")]
    graph_dir: PathBuf,
}

fn main() {
    let args = Args::parse();
    let python = args.graph_dir.join("venv/bin/python");
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let query = request
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let limit = request
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(8)
            .clamp(1, 50);
        let reply = Command::new(&python)
            .arg(args.graph_dir.join("memory_graph_recall.py"))
            .arg(query)
            .arg("--k")
            .arg(limit.to_string())
            .arg("--json")
            .output()
            .ok()
            .and_then(|result| result.status.success().then_some(result.stdout))
            .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
            .unwrap_or_else(|| json!([]));
        let _ = writeln!(
            out,
            "{}",
            json!({"backend":"graphiti_compat","records":reply})
        );
        let _ = out.flush();
    }
}
