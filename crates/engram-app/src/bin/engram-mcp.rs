use clap::Parser;
use engram_hybrid::recall;
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

#[derive(Parser)]
struct Args {
    #[arg(
        long,
        env = "ENGRAM_CONFIG",
        default_value = "/root/.claude/engram.yaml"
    )]
    config: PathBuf,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = request.get("id") else {
            continue;
        };
        let response = match request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "initialize" => ok(
                id,
                json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"engram-rust","version":"0.1.0"}}),
            ),
            "tools/list" => ok(
                id,
                json!({"tools":[{"name":"memory_recall_hybrid","description":"Hybrid Markdown, vector, and graph memory recall.","inputSchema":{"type":"object","properties":{"q":{"type":"string"},"slug":{"type":"string"},"k":{"type":"integer","minimum":1,"maximum":20}},"required":["q"]}}]}),
            ),
            "tools/call" => {
                call(
                    id,
                    request.get("params").unwrap_or(&Value::Null),
                    &args.config,
                )
                .await
            }
            _ => err(id, -32601, "method not found"),
        };
        let _ = writeln!(stdout, "{response}");
        let _ = stdout.flush();
    }
}

async fn call(id: &Value, params: &Value, config: &Path) -> Value {
    if params.get("name").and_then(Value::as_str) != Some("memory_recall_hybrid") {
        return err(id, -32602, "unknown tool");
    }
    let arguments = params.get("arguments").unwrap_or(&Value::Null);
    let Some(query) = arguments
        .get("q")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        return err(id, -32602, "q is required");
    };
    let slug = arguments
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or("-root");
    let k = arguments
        .get("k")
        .and_then(Value::as_u64)
        .unwrap_or(6)
        .clamp(1, 20) as usize;
    match recall(config, slug, query, k).await {
        Ok(result) => ok(
            id,
            json!({"content":[{"type":"text","text":serde_json::to_string(&result).unwrap()}]}),
        ),
        Err(error) => ok(
            id,
            json!({"content":[{"type":"text","text":error}],"isError":true}),
        ),
    }
}
fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}
fn err(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
