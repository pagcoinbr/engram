//! Compare native graph recall against the Graphiti compatibility path.
//!
//! The previous version did not measure what its name claimed. It compared
//! `GraphClient::keyword_files` — a raw Neo4j full-text query — rather than the
//! Graphiti recall the compatibility mode actually serves, and it converted both
//! sides to `HashSet`s, discarding order. Since the entire point of compatibility
//! mode is to preserve Graphiti's *ranking*, a set-overlap score could pass while
//! the ordering was wrong.
//!
//! This version runs both real paths (`recall` under `graphiti_compat`, and
//! `recall_native`) and reports ordered agreement, per-record facts, and the empty
//! -result case. It is a diagnostic gate, not a frozen-fixture parity suite.

use clap::Parser;
use engram_hybrid::{recall, recall_native};
use serde::Deserialize;
use std::{fs, path::PathBuf, process::ExitCode};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "tests/graph_recall_eval.json")]
    cases: PathBuf,
    #[arg(long, default_value_t = 8)]
    limit: usize,
    #[arg(long, env = "ENGRAM_CONFIG")]
    config: Option<PathBuf>,
    // allow_hyphen_values because EVERY engram slug starts with '-' (it is a
    // path with separators replaced), so clap read `--slug -home-alice` as a
    // missing value followed by an unknown flag. The documented invocation was
    // unusable as typed.
    #[arg(long, allow_hyphen_values = true)]
    slug: Option<String>,
    /// Require exact ordered agreement rather than reporting the score.
    #[arg(long)]
    strict: bool,
}

#[derive(Deserialize)]
struct Case {
    query: String,
}

/// How far two ranked file lists agree, as a fraction of the reference length.
///
/// Prefix agreement, not set overlap: the score drops as soon as the orders
/// diverge, which is the property compatibility mode is supposed to hold.
fn ordered_agreement(reference: &[String], candidate: &[String]) -> f64 {
    if reference.is_empty() {
        // Both empty is agreement; a reference with nothing and a candidate with
        // something is not.
        return if candidate.is_empty() { 1.0 } else { 0.0 };
    }
    let matching = reference
        .iter()
        .zip(candidate.iter())
        .take_while(|(left, right)| left == right)
        .count();
    matching as f64 / reference.len() as f64
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("engram-graph-recall-eval: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let config = engram_paths::config_path(args.config);
    let slug = engram_paths::resolve_slug(args.slug.as_deref());
    let cases: Vec<Case> = serde_json::from_str(
        &fs::read_to_string(&args.cases)
            .map_err(|error| format!("could not read {}: {error}", args.cases.display()))?,
    )
    .map_err(|error| error.to_string())?;

    let mut scores = Vec::new();
    for case in &cases {
        // The real compatibility path, with its real ordering — not a raw
        // full-text query standing in for it.
        let reference = recall(&config, &slug, &case.query, args.limit)
            .await
            .map_err(|error| format!("{}: graphiti_compat recall failed: {error}", case.query))?;
        let native = recall_native(&config, &slug, &case.query, args.limit)
            .await
            .map_err(|error| format!("{}: native recall failed: {error}", case.query))?;

        let reference_files: Vec<String> = reference
            .results
            .iter()
            .map(|hit| hit.file.clone())
            .collect();
        let native_files: Vec<String> = native.results.iter().map(|hit| hit.file.clone()).collect();
        let score = ordered_agreement(&reference_files, &native_files);
        scores.push(score);

        // Per-record facts are part of the response contract, so a native result
        // that finds the right memory with none of its facts is reported.
        let factless = native
            .results
            .iter()
            .filter(|hit| {
                hit.facts.is_empty()
                    && reference
                        .results
                        .iter()
                        .any(|other| other.file == hit.file && !other.facts.is_empty())
            })
            .map(|hit| hit.file.clone())
            .collect::<Vec<_>>();

        println!("{}", case.query);
        println!("  graphiti: {}", render(&reference_files));
        println!("  native:   {}", render(&native_files));
        println!("  ordered agreement: {score:.3}");
        if !factless.is_empty() {
            println!("  lost per-record facts for: {}", factless.join(", "));
        }
    }

    if scores.is_empty() {
        return Err(format!("no cases in {}", args.cases.display()));
    }
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    println!(
        "\nmean ordered agreement: {mean:.3} over {} case(s)",
        scores.len()
    );
    if args.strict && mean < 1.0 {
        return Err("native ordering does not match Graphiti on this evaluation set".into());
    }
    Ok(())
}

fn render(files: &[String]) -> String {
    if files.is_empty() {
        "(none)".into()
    } else {
        files.join(" > ")
    }
}

#[cfg(test)]
mod tests {
    use super::ordered_agreement;

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// The bug this replaced: a HashSet comparison scored these as identical.
    #[test]
    fn reordering_lowers_the_score() {
        let reference = files(&["a.md", "b.md", "c.md"]);
        assert_eq!(ordered_agreement(&reference, &reference), 1.0);

        let swapped = files(&["b.md", "a.md", "c.md"]);
        assert_eq!(
            ordered_agreement(&reference, &swapped),
            0.0,
            "a set comparison would have called this perfect parity"
        );

        // the same set, first element right
        let tail_swapped = files(&["a.md", "c.md", "b.md"]);
        assert!((ordered_agreement(&reference, &tail_swapped) - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn empty_results_are_compared_honestly() {
        // both empty is agreement
        assert_eq!(ordered_agreement(&[], &[]), 1.0);
        // a free 1.0 for "the reference found nothing but we found things" was how
        // the old evaluator could pass without comparing anything
        assert_eq!(ordered_agreement(&[], &files(&["a.md"])), 0.0);
        // and finding nothing where Graphiti found something is a total miss
        assert_eq!(ordered_agreement(&files(&["a.md"]), &[]), 0.0);
    }

    #[test]
    fn a_truncated_candidate_scores_partially() {
        let reference = files(&["a.md", "b.md", "c.md", "d.md"]);
        assert_eq!(
            ordered_agreement(&reference, &files(&["a.md", "b.md"])),
            0.5
        );
    }
}
