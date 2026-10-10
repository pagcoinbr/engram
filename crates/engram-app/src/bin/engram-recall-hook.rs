//! The `UserPromptSubmit` hook: inject the memories matching each prompt.
//!
//! This runs before EVERY prompt the user sends, which sets the rules it has to
//! play by. It must fail open, it must respect its own enable switch, and it must
//! never outlast its budget — the recall call it makes was previously unbounded, so
//! a stalled Neo4j delayed the prompt indefinitely rather than being abandoned.

use clap::Parser;
use engram_config::Config;
use engram_hybrid::recall_fast;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

/// Session dedup state older than this is swept. Matches
/// `bin/hooks/memory-recall-inject.py`, which has always done this; the Rust hook
/// accumulated one file per session forever.
const STATE_TTL: Duration = Duration::from_secs(7 * 86_400);

/// Prompts shorter than this are not worth a recall round trip.
const MIN_PROMPT: usize = 25;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "ENGRAM_CONFIG")]
    config: Option<PathBuf>,
    // allow_hyphen_values because EVERY engram slug starts with '-' (it is a
    // path with separators replaced), so clap read `--slug -home-alice` as a
    // missing value followed by an unknown flag. The documented invocation was
    // unusable as typed.
    #[arg(long, allow_hyphen_values = true)]
    slug: Option<String>,
    /// The agent identity to inject memories for. Required on an install that
    /// defines tenants; without it this hook injects nothing and says so on
    /// stderr rather than guessing which identity the session belongs to.
    #[arg(long, env = "ENGRAM_TENANT")]
    tenant: Option<String>,
}

#[derive(Deserialize)]
struct Prompt {
    prompt: String,
    session_id: Option<String>,
    /// The session's project directory, as Claude Code supplies it.
    cwd: Option<String>,
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
    let prompt = payload.prompt.trim();
    if prompt.len() < MIN_PROMPT || matches!(prompt.chars().next(), Some('/' | '!')) {
        return;
    }

    let config_path = engram_paths::config_path(args.config);
    // The hook runs inside a Claude Code session, so the session's project
    // directory IS the project whose memories are wanted — and that is the `cwd`
    // in the payload, not the hook process's own working directory, which is
    // whatever the harness happened to launch it in. The Python hook has always
    // used the payload value; reading current_dir() here resolved a different
    // store, and a store that does not exist injects nothing, silently. The slug
    // was hard-wired to "-root" before that.
    let derived = engram_paths::resolve_slug_in(
        args.slug.as_deref(),
        payload
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|dir| !dir.is_empty())
            .map(Path::new),
    );

    // recall.inject was read by the Python hook and ignored here: `enabled` had no
    // effect and `k` was the literal 4. A config that cannot be read leaves the
    // documented defaults in place rather than disabling recall.
    let config = Config::load(&config_path);
    let inject = config
        .as_ref()
        .map(|config| config.recall.inject.clone())
        .unwrap_or_default();
    if !inject.enabled {
        return;
    }

    // Resolve the identity before recalling anything.
    //
    // This hook is fail-open everywhere else — a missing config or a slow
    // backend simply injects nothing, because delaying the user's prompt is
    // worse than omitting context. A tenant failure is fail-open in the same
    // direction, and that is also the SAFE direction: injecting nothing costs
    // the user some context, while injecting another identity's memories is the
    // single outcome the tenant model exists to prevent.
    //
    // Unlike the other failures this one is reported on stderr. Everything here
    // is silent by design, and silence would hide the one mistake an operator is
    // likely to make while setting tenants up: registering this hook without
    // `--tenant`, which would otherwise look exactly like "no relevant
    // memories" forever. Hook stderr surfaces in Claude Code's debug output.
    let (tenant, slug) = match config
        .map_err(|error| error.to_string())
        .and_then(|config| {
            // The identity follows the session's store, derived from the
            // PAYLOAD cwd. This hook is registered once in settings.json and has
            // to serve every identity on the host, so a fixed --tenant could
            // serve one. A session in a directory no tenant claims injects
            // nothing and says so, rather than borrowing another identity.
            let tenant = engram_tenant::resolve_tenant(
                &config,
                args.tenant.as_deref(),
                &derived,
                args.slug.as_deref(),
            )
            .map_err(|error| error.to_string())?;
            let slug = tenant
                .choose_slug(args.slug.as_deref(), &derived)
                .map_err(|error| error.to_string())?;
            Ok((tenant, slug))
        }) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("engram-recall-hook: not injecting: {error}");
            return;
        }
    };

    let session = payload
        .session_id
        .filter(|value| {
            !value.is_empty()
                && value
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        })
        .unwrap_or_else(|| "default".into());
    // `.parent().unwrap()` panicked on a parentless --config and silently wrote to
    // ./logs for a relative one.
    let state_dir = engram_paths::engram_home()
        .join("logs")
        .join("rust-recall-inject");
    let state_path = state_dir.join(format!("{session}.json"));
    sweep_stale_state(&state_dir);
    let mut seen = load_seen(&state_path);

    // Fail open ON THE DEADLINE. Waiting for the call to return eventually is the
    // same as having no timeout from the user's point of view.
    //
    // recall_fast, not recall: the full path spawns Graphiti, which measures ~3.5s
    // against a populated graph and would therefore blow this budget on every
    // prompt, injecting nothing at all.
    let budget = Duration::from_millis(inject.timeout_ms.max(250));
    let Ok(Ok(result)) = tokio::time::timeout(
        budget,
        recall_fast(&config_path, &tenant, &slug, prompt, inject.k.max(1)),
    )
    .await
    else {
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
    let _ = std::fs::create_dir_all(&state_dir);
    let temp = state_path.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&temp, serde_json::to_vec(&seen).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&temp, &state_path);
    } else {
        let _ = std::fs::remove_file(&temp);
    }

    println!("<relevant-memory>");
    for item in &fresh {
        println!("- {}: {}", item.name, item.description);
    }
    // Facts stay attributed to the memory that carried them, and the total is
    // capped by recall.inject.max_facts.
    let mut shown = 0;
    for item in &fresh {
        for fact in &item.facts {
            if shown >= inject.max_facts {
                break;
            }
            println!("  · {fact}");
            shown += 1;
        }
    }
    println!("</relevant-memory>");
}

fn load_seen(path: &std::path::Path) -> BTreeSet<String> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Drop session state files older than [`STATE_TTL`]. Cheap, best-effort, and
/// never a reason to skip injection.
fn sweep_stale_state(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > STATE_TTL);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
