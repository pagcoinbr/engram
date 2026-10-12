//! Move a pre-tenancy install onto tenants.
//!
//! Three things have to move, and they move differently:
//!
//! - **Qdrant** points are copied into the tenant's own collection, vectors
//!   carried across verbatim. No re-embedding: the vectors came from whatever
//!   model was configured when they were written, and the job here is to move
//!   them, not to reinterpret them.
//! - **Graphiti** data is *relabelled* to the tenant's group — but only when
//!   exactly one tenant owns the group's episodes. `Entity` nodes are shared
//!   across episodes, so an entity mentioned by two tenants' memories is a
//!   single node with a single group: splitting a group between tenants is not
//!   expressible as an update at all and needs a re-insert from
//!   `graph/extractions/`. Relabelling is correct precisely when there is
//!   nothing to split, and this refuses rather than guessing.
//! - **Pre-tenancy native nodes** are deleted, not migrated. They carry neither
//!   `slug` nor `tenant`, and every native statement now requires both, so no
//!   query can reach them. The markdown store is authoritative and `--rebuild`
//!   regenerates the index.
//!
//! Dry run by default. It also refuses to run while any store on disk is
//! unclaimed: a store no tenant owns has no collection to be written to, and
//! adopting it into whichever tenant happens to be running would put one
//! identity's memories inside another's boundary — the exact outcome tenancy
//! exists to prevent.

use clap::Parser;
use engram_config::Config;
use engram_graph::GraphClient;
use engram_tenant::{LEGACY_GRAPH_GROUP, Tenant};
use engram_vector::{Corpus, QdrantClient};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::ExitCode,
};

#[derive(Parser)]
#[command(about = "Move a pre-tenancy install onto tenants (dry run by default)")]
struct Args {
    #[arg(long, env = "ENGRAM_CONFIG")]
    config: Option<PathBuf>,
    /// Apply the plan. Without this nothing is written.
    #[arg(long)]
    execute: bool,
    /// Proceed even though some stores on disk belong to no tenant. Their data
    /// is left exactly where it is — this only silences the refusal.
    #[arg(long)]
    allow_unassigned: bool,
    /// Delete the pre-tenancy native graph nodes (no slug, no tenant). They are
    /// unreachable by every current query; separate flag because it is the one
    /// irreversible step, and `--rebuild` is how they come back.
    #[arg(long)]
    purge_native: bool,
    /// The Graphiti group to migrate FROM.
    #[arg(long, default_value = LEGACY_GRAPH_GROUP)]
    from_group: String,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match tokio::runtime::Runtime::new()
        .map_err(|error| error.to_string())
        .and_then(|runtime| runtime.block_on(run(args)))
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("engram-tenant-migrate: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let config_path = engram_paths::config_path(args.config);
    let config = Config::load(&config_path).map_err(|error| error.to_string())?;
    if !config.tenancy_enabled() {
        return Err(
            "no tenants are configured; add a 'tenants:' block to engram.yaml first \
             (see CONFIG.md), then re-run"
                .into(),
        );
    }
    let mode = if args.execute { "EXECUTE" } else { "DRY RUN" };
    println!(
        "engram-tenant-migrate [{mode}]  config: {}",
        config_path.display()
    );

    // ---- 1. Which stores exist, and who claims them ----------------------
    let present = stores_on_disk(&config_path)?;
    let mut by_tenant: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unassigned = Vec::new();
    for slug in &present {
        match config.tenant_of_slug(slug) {
            Some(tenant) => by_tenant
                .entry(tenant.into())
                .or_default()
                .push(slug.clone()),
            None => unassigned.push(slug.clone()),
        }
    }
    println!("\n-- stores ------------------------------------------------");
    for (tenant, slugs) in &by_tenant {
        for slug in slugs {
            println!("  {tenant:<16} {slug}");
        }
    }
    for slug in &unassigned {
        println!("  {:<16} {slug}", "UNASSIGNED");
    }
    // A slug declared in the config with no directory is worth saying out loud:
    // it is almost always a typo, and it would otherwise look like a tenant that
    // simply has no memories yet.
    let declared: BTreeSet<&str> = config
        .tenants
        .values()
        .flat_map(|tenant| tenant.slugs.iter().map(String::as_str))
        .collect();
    for slug in &declared {
        if !present.contains(*slug) {
            println!("  {:<16} {slug} (declared, no store on disk)", "MISSING");
        }
    }
    if !unassigned.is_empty() && !args.allow_unassigned {
        return Err(format!(
            "{} store(s) belong to no tenant: {}.\n  \
             Assign each one under a tenant's `slugs:` in engram.yaml, or re-run with \
             --allow-unassigned to leave their data where it is.\n  \
             They are not adopted automatically on purpose: putting one identity's \
             memories inside another's boundary is the outcome tenancy prevents.",
            unassigned.len(),
            unassigned.join(", ")
        ));
    }

    // ---- 2. Qdrant: copy points into per-tenant collections ---------------
    println!("\n-- qdrant ------------------------------------------------");
    if config.vector_store.enabled {
        let source = QdrantClient::with_credentials(
            config.vector_store.url.clone(),
            config.vector_store.collection.clone(),
            config.vector_store.api_key.present(),
            config.vector_store.timeout_seconds,
        );
        for (name, slugs) in &by_tenant {
            let tenant = Tenant::resolve(&config, Some(name)).map_err(|e| e.to_string())?;
            let mut total = 0;
            for slug in slugs {
                let points = source
                    .count(Some(slug))
                    .await
                    .map_err(|error| format!("counting {slug}: {error}"))?;
                total += points;
                println!(
                    "  {name:<16} {slug:<28} {points:>5} point(s) -> {}",
                    tenant.memory_collection()
                );
            }
            if total == 0 {
                continue;
            }
            if !args.execute {
                continue;
            }
            // Create the destination at the configured dimension before copying.
            // `ensure_collection` also guards a dimension mismatch, which would
            // otherwise surface as a per-point rejection half way through.
            let destination = QdrantClient::for_tenant(&config, &tenant, Corpus::Memory);
            destination
                .ensure_collection(config.embed.dim, false)
                .await
                .map_err(|error| format!("creating {}: {error}", tenant.memory_collection()))?;
            let mut moved = 0;
            for slug in slugs {
                moved += source
                    .migrate_points(tenant.memory_collection(), slug, tenant.graph_group())
                    .await
                    .map_err(|error| format!("copying {slug}: {error}"))?;
            }
            // Verify before claiming success. A copy that silently moved fewer
            // points than it counted is the failure worth catching here, while
            // the source is still intact.
            let arrived = destination
                .count(None)
                .await
                .map_err(|error| format!("verifying {}: {error}", tenant.memory_collection()))?;
            println!(
                "  {name:<16} copied {moved}, collection now holds {arrived} (expected >= {total})"
            );
            if arrived < total {
                return Err(format!(
                    "{}: expected at least {total} points after the copy, found {arrived}. \
                     The source collection is untouched; investigate before re-running.",
                    tenant.memory_collection()
                ));
            }
        }
        println!(
            "  source collection '{}' is left intact — drop it by hand once recall is verified",
            config.vector_store.collection
        );
    } else {
        println!("  vector_store.enabled is false — nothing to move");
    }

    // ---- 3. Graphiti: relabel, but only when unambiguous ------------------
    println!("\n-- graphiti ----------------------------------------------");
    let credentials = config.graph.credentials();
    if credentials.password.expose().is_empty() {
        println!("  no Neo4j password available — skipping the graph");
        return finish(args.execute);
    }
    let client = GraphClient::from_credentials(&credentials).map_err(|e| e.to_string())?;
    let files = client
        .migration_episode_files(&args.from_group)
        .await
        .map_err(|error| format!("reading group '{}': {error}", args.from_group))?;
    println!(
        "  group '{}' holds episodes for {} file(s)",
        args.from_group,
        files.len()
    );
    if files.is_empty() {
        println!("  nothing to relabel");
    } else {
        // Attribute each episode to a tenant by asking which owned store holds a
        // file of that name. Episodic carries `file` but no slug, so this is the
        // only evidence available.
        let owners = attribute(&config_path, &by_tenant, &files)?;
        let claimants: BTreeSet<&String> = owners.values().flatten().collect();
        let ambiguous: Vec<&String> = owners
            .iter()
            .filter(|(_, tenants)| tenants.len() > 1)
            .map(|(file, _)| file)
            .collect();
        let orphans = owners.values().filter(|tenants| tenants.is_empty()).count();
        for tenant in &claimants {
            let count = owners
                .values()
                .filter(|owners| owners.len() == 1 && owners.contains(tenant))
                .count();
            println!("  {tenant:<16} {count} episode file(s)");
        }
        if orphans > 0 {
            println!(
                "  {orphans} episode file(s) match no store on disk (deleted memories); \
                 they follow the group they are in"
            );
        }
        if !ambiguous.is_empty() {
            return Err(format!(
                "{} episode file(s) exist in more than one tenant's store, so the group \
                 cannot be split by relabelling: {}.\n  \
                 Graphiti Entity nodes are shared across episodes, so an entity named by \
                 two identities is ONE node with ONE group and no update can divide it.\n  \
                 Re-insert instead, per tenant, from the cached extractions:\n    \
                 python3 ~/.claude/graph/memory_graph_insert.py --tenant <name> --only <files>\n  \
                 The extractions in ~/.claude/graph/extractions/ mean this costs no LLM calls.",
                ambiguous.len(),
                ambiguous
                    .iter()
                    .take(5)
                    .map(|f| f.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if claimants.len() > 1 {
            return Err(format!(
                "group '{}' holds episodes for {} different tenants ({}).\n  \
                 Relabelling would move all of them to one group. Entity nodes are shared \
                 across episodes, so this group has to be re-inserted per tenant instead:\n    \
                 python3 ~/.claude/graph/memory_graph_insert.py --tenant <name> --only <files>",
                args.from_group,
                claimants.len(),
                claimants
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        match claimants.into_iter().next() {
            None => println!("  no episode maps to a tenant — leaving the group alone"),
            Some(name) => {
                let tenant = Tenant::resolve(&config, Some(name)).map_err(|e| e.to_string())?;
                println!(
                    "  relabel group '{}' -> '{}' (one tenant owns it, so nothing needs splitting)",
                    args.from_group,
                    tenant.graph_group()
                );
                if args.execute {
                    let (nodes, rels) = client
                        .migration_relabel_group(&args.from_group, tenant.graph_group())
                        .await
                        .map_err(|error| format!("relabelling: {error}"))?;
                    println!("  relabelled {nodes} node(s) and {rels} relationship(s)");
                }
            }
        }
    }

    // ---- 4. Pre-tenancy native nodes --------------------------------------
    println!("\n-- native graph ------------------------------------------");
    let stale = client
        .migration_count_unscoped_native()
        .await
        .map_err(|error| format!("counting native nodes: {error}"))?;
    if stale == 0 {
        println!("  no pre-tenancy native nodes");
    } else {
        println!(
            "  {stale} node(s) carry neither slug nor tenant, so no current query can \
             reach them"
        );
        if args.purge_native && args.execute {
            let purged = client
                .migration_purge_unscoped_native()
                .await
                .map_err(|error| format!("purging native nodes: {error}"))?;
            println!("  deleted {purged}; re-run the native sync with --rebuild to repopulate");
        } else {
            println!("  pass --purge-native --execute to delete them");
        }
    }

    finish(args.execute)
}

fn finish(executed: bool) -> Result<(), String> {
    if executed {
        println!("\ndone. Verify recall per tenant before dropping the source collection.");
    } else {
        println!("\ndry run — nothing was written. Re-run with --execute to apply.");
    }
    Ok(())
}

/// Memory stores present on disk: `<config dir>/projects/*/memory`.
fn stores_on_disk(config_path: &std::path::Path) -> Result<BTreeSet<String>, String> {
    let projects = config_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .join("projects");
    let mut found = BTreeSet::new();
    let entries = match std::fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(error) => return Err(format!("reading {}: {error}", projects.display())),
    };
    for entry in entries.flatten() {
        if entry.path().join("memory").is_dir() {
            found.insert(entry.file_name().to_string_lossy().to_string());
        }
    }
    Ok(found)
}

/// For each episode file, which tenants hold a store containing that filename.
///
/// Zero means the memory was deleted; one is the normal case; more than one is
/// genuinely ambiguous, because the only evidence Graphiti records is the bare
/// filename.
fn attribute(
    config_path: &std::path::Path,
    by_tenant: &BTreeMap<String, Vec<String>>,
    files: &[String],
) -> Result<BTreeMap<String, Vec<String>>, String> {
    // filename -> tenants holding it
    let mut holders: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (tenant, slugs) in by_tenant {
        for slug in slugs {
            let store = engram_paths::store_dir(config_path, slug);
            let Ok(entries) = std::fs::read_dir(&store) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.ends_with(".md") || name == "MEMORY.md" {
                    continue;
                }
                let owners = holders.entry(name).or_default();
                if !owners.contains(tenant) {
                    owners.push(tenant.clone());
                }
            }
        }
    }
    Ok(files
        .iter()
        .map(|file| {
            let owners = holders.get(file).cloned().unwrap_or_default();
            (file.clone(), owners)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        /// `stores` is (slug, filenames).
        fn new(name: &str, stores: &[(&str, &[&str])]) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "engram-migrate-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            for (slug, files) in stores {
                let store = dir.join("projects").join(slug).join("memory");
                std::fs::create_dir_all(&store).unwrap();
                for file in *files {
                    std::fs::write(store.join(file), "---\nname: n\n---\nbody\n").unwrap();
                }
            }
            Self(dir)
        }

        fn config(&self) -> PathBuf {
            self.0.join("engram.yaml")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn tenants(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(tenant, slugs)| {
                (
                    tenant.to_string(),
                    slugs.iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect()
    }

    /// The decision that separates a safe relabel from silently merging two
    /// identities' graphs.
    ///
    /// Graphiti records only the bare FILENAME on an episode — no slug — so
    /// attribution is "which owned store holds a file by this name". One holder
    /// is the normal case; two is genuinely undecidable and must be refused,
    /// because relabelling would move both identities' entities into one group.
    #[test]
    fn an_episode_is_attributed_by_which_store_holds_its_filename() {
        let fixture = Fixture::new(
            "attribute",
            &[
                ("-work", &["payroll.md", "shared-name.md"]),
                ("-home", &["nas.md", "shared-name.md"]),
                ("-work-extra", &["extra.md"]),
            ],
        );
        let by_tenant = tenants(&[("work", &["-work", "-work-extra"]), ("homelab", &["-home"])]);
        let files = [
            "payroll.md".to_string(),
            "nas.md".to_string(),
            "shared-name.md".to_string(),
            "extra.md".to_string(),
            "deleted-long-ago.md".to_string(),
        ];
        let owners = attribute(&fixture.config(), &by_tenant, &files).unwrap();

        assert_eq!(owners["payroll.md"], vec!["work"]);
        assert_eq!(owners["nas.md"], vec!["homelab"]);
        // a second store belonging to the SAME tenant is not ambiguity
        assert_eq!(owners["extra.md"], vec!["work"]);
        // the colliding filename names both, which is what triggers the refusal
        let mut shared = owners["shared-name.md"].clone();
        shared.sort();
        assert_eq!(shared, vec!["homelab", "work"]);
        // an episode whose memory no longer exists is an orphan, not an error
        assert!(owners["deleted-long-ago.md"].is_empty());
    }

    /// `MEMORY.md` is the index, not a memory, and is excluded everywhere else
    /// (`engram_store::load` skips it). Counting it here would invent an owner
    /// for a file Graphiti never holds an episode for — and, since every store
    /// has one, it would make EVERY multi-tenant install look ambiguous.
    #[test]
    fn the_index_file_is_not_treated_as_a_memory() {
        let fixture = Fixture::new(
            "index-file",
            &[("-work", &["MEMORY.md", "a.md"]), ("-home", &["MEMORY.md"])],
        );
        let by_tenant = tenants(&[("work", &["-work"]), ("homelab", &["-home"])]);
        let owners = attribute(
            &fixture.config(),
            &by_tenant,
            &["MEMORY.md".to_string(), "a.md".to_string()],
        )
        .unwrap();
        assert!(
            owners["MEMORY.md"].is_empty(),
            "the index must not be attributed to any tenant"
        );
        assert_eq!(owners["a.md"], vec!["work"]);
    }

    /// A store directory that does not exist must not panic or abort the audit —
    /// a tenant may legitimately declare a slug before its store is created.
    #[test]
    fn a_declared_store_with_no_directory_is_skipped() {
        let fixture = Fixture::new("missing-store", &[("-work", &["a.md"])]);
        let by_tenant = tenants(&[("work", &["-work", "-not-created-yet"])]);
        let owners = attribute(&fixture.config(), &by_tenant, &["a.md".to_string()]).unwrap();
        assert_eq!(owners["a.md"], vec!["work"]);
    }
}
