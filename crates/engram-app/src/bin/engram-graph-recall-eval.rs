use clap::Parser;
use engram_graph::GraphClient;
use engram_hybrid::recall_native;
use serde::Deserialize;
use std::{fs, path::PathBuf, process::ExitCode};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "tests/graph_recall_eval.json")]
    cases: PathBuf,
    #[arg(long, env = "NEO4J_URI", default_value = "bolt://127.0.0.1:7687")]
    uri: String,
    #[arg(long, env = "NEO4J_DATABASE", default_value = "neo4j")]
    database: String,
    #[arg(long, env = "NEO4J_PASSWORD")]
    password: String,
    #[arg(long, default_value_t = 8)]
    limit: usize,
    #[arg(long, default_value = "/root/.claude/engram.yaml")]
    config: PathBuf,
    #[arg(long, default_value = "-root")]
    slug: String,
}

#[derive(Deserialize)]
struct Case {
    query: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let result = async {
        let cases: Vec<Case> = serde_json::from_str(
            &fs::read_to_string(args.cases).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let client = GraphClient::new(&args.uri, &args.database, "neo4j", args.password)
            .map_err(|error| error.to_string())?;
        let mut legacy_total = 0usize;
        let mut native_total = 0usize;
        for case in cases {
            let legacy = client
                .keyword_files(&case.query, args.limit)
                .await
                .map_err(|error| error.to_string())?;
            let native = recall_native(&args.config, &args.slug, &case.query, args.limit)
                .await
                .map_err(|error| error.to_string())?;
            let legacy_files = legacy
                .into_iter()
                .map(|hit| hit.file)
                .collect::<std::collections::HashSet<_>>();
            let native_files = native
                .results
                .into_iter()
                .map(|hit| hit.file)
                .collect::<std::collections::HashSet<_>>();
            let matched = legacy_files.intersection(&native_files).count();
            let missing = legacy_files
                .difference(&native_files)
                .cloned()
                .collect::<Vec<_>>();
            legacy_total += legacy_files.len();
            native_total += matched;
            println!(
                "{}: native matched {matched}/{} legacy files; missing: {}",
                case.query,
                legacy_files.len(),
                missing.join(", ")
            );
        }
        let recall = if legacy_total == 0 {
            1.0
        } else {
            native_total as f64 / legacy_total as f64
        };
        println!("native parity recall: {recall:.3} ({native_total}/{legacy_total})");
        (recall >= 1.0)
            .then_some(())
            .ok_or_else(|| "native recall is below Graphiti on this evaluation set".to_string())
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("engram-graph-recall-eval: {error}");
            ExitCode::FAILURE
        }
    }
}
