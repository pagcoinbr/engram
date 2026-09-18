use clap::Parser;
use engram_hybrid::recall;
use serde::Deserialize;
use std::{collections::BTreeSet, io::Read, path::PathBuf};

#[derive(Parser)]
struct Args {
    #[arg(
        long,
        env = "ENGRAM_CONFIG",
        default_value = "/root/.claude/engram.yaml"
    )]
    config: PathBuf,
}

#[derive(Deserialize)]
struct Prompt {
    prompt: String,
    session_id: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(payload) = serde_json::from_str::<Prompt>(&input) else {
        return;
    };
    if payload.prompt.trim().len() < 25
        || matches!(payload.prompt.trim().chars().next(), Some('/' | '!'))
    {
        return;
    }
    let session = payload
        .session_id
        .filter(|value| {
            value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        })
        .unwrap_or_else(|| "default".into());
    let state_path = args
        .config
        .parent()
        .unwrap()
        .join("logs")
        .join("rust-recall-inject")
        .join(format!("{session}.json"));
    let mut seen = load_seen(&state_path);
    let Ok(result) = recall(&args.config, "-root", &payload.prompt, 4).await else {
        return;
    };
    let fresh = result
        .results
        .into_iter()
        .filter(|item| seen.insert(item.file.clone()))
        .collect::<Vec<_>>();
    if fresh.is_empty() {
        return;
    }
    if let Some(parent) = state_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temp = state_path.with_extension("json.tmp");
    if std::fs::write(&temp, serde_json::to_vec(&seen).unwrap()).is_ok() {
        let _ = std::fs::rename(temp, state_path);
    }
    println!("<relevant-memory>");
    for item in fresh {
        println!("- {}: {}", item.name, item.description);
    }
    println!("</relevant-memory>");
}

fn load_seen(path: &std::path::Path) -> BTreeSet<String> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}
