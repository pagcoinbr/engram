use clap::Parser;
use engram_graph::GraphClient;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "bolt://127.0.0.1:7687")]
    uri: String,
    #[arg(long, default_value = "neo4j")]
    database: String,
    #[arg(long, default_value = "neo4j")]
    user: String,
    #[arg(long, env = "NEO4J_PASSWORD")]
    password: String,
    #[arg(long)]
    query: String,
    /// Graphiti `group_id` to search. Required: engram wrote every memory into
    /// one literal group before tenancy, so an unpartitioned query here reads
    /// every identity's facts. Pass `canonical` for a pre-tenancy graph, or the
    /// tenant name otherwise.
    #[arg(long)]
    group: String,
}
#[tokio::main]
async fn main() {
    let args = Args::parse();
    let tokens = args
        .query
        .split_whitespace()
        .filter(|word| word.len() >= 4)
        .take(6)
        .map(str::to_string)
        .collect::<Vec<_>>();
    // `unchecked` because this binary has no engram.yaml to resolve a tenant
    // from — see GraphScope::unchecked. The operator owns the correctness of
    // --group here; the scope still partitions the query.
    let scope = engram_tenant::GraphScope::unchecked(&args.group, "");
    let result = async {
        GraphClient::new(&args.uri, &args.database, args.user, args.password)?
            .facts_for_tokens(&scope, &tokens, 6)
            .await
    }
    .await;
    match result {
        Ok(facts) => println!("{}", serde_json::to_string_pretty(&facts).unwrap()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
