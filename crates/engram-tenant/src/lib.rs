//! Agent identity, and the boundary between identities.
//!
//! A tenant is one agent's world: the memory stores it owns, the Obsidian vault
//! it reads, the Qdrant collections it searches and the Neo4j group it writes.
//! The requirement this crate exists to enforce is absolute — a work agent must
//! never recall a homelab memory or wiki page — and the chosen configuration
//! shape makes that harder, not easier: ONE `engram.yaml` lists every tenant, so
//! every process holds every tenant's vault path and could reach it.
//!
//! The compensating control is this type. A [`Tenant`] is the only thing that
//! can name a collection, a graph group or a vault path, so every store, vector
//! and graph call takes one. Omitting the scope is then a compile error rather
//! than a silent cross-tenant read, which is the only form of this guarantee
//! that survives someone adding a twentieth call site in a hurry.
//!
//! Two consequences worth stating, because both were deliberate:
//!
//! - **An absent tenant is never a default.** With tenancy configured, a
//!   missing `--tenant` is refused even when exactly one tenant exists.
//!   "Obviously the only one" is how a default gets established that silently
//!   becomes wrong the day a second tenant is added.
//! - **Legacy lives inside the type.** An install with no `tenants:` block —
//!   which is every install that exists today — resolves to a legacy tenant
//!   that reads the configured collection and Graphiti's historical
//!   `canonical` group. Encoding that here rather than branching at each call
//!   site means the upgrade path is one code path, exercised by the same tests.

use engram_config::Config;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

/// Graphiti's historical single group, used by every memory inserted before
/// tenancy existed. Retained ONLY for the legacy tenant; a named tenant uses its
/// own name as the group, which is what stops the graph leg crossing identities.
pub const LEGACY_GRAPH_GROUP: &str = "canonical";

/// Everything a binary needs before it may touch a store.
pub struct Resolved {
    pub config: Config,
    pub tenant: Tenant,
    pub slug: String,
}

/// Where the default store comes from when `--slug` is omitted.
#[derive(Clone, Copy, PartialEq)]
pub enum Derive {
    /// The working directory is the project whose memories are wanted — a CLI
    /// the operator typed, the MCP server its client launched in-project.
    Cwd,
    /// The working directory is arbitrary and must not be guessed from: a daemon
    /// job, a systemd unit.
    Environment,
}

/// The opening move of every engram binary: load the config, resolve the agent
/// identity, then pick one of its stores.
///
/// Shared rather than copied into each binary, because the ordering is the part
/// that matters and seven near-identical copies is how one of them ends up
/// resolving the slug before the tenant, or skipping the ownership check.
pub fn resolve_for_cli(
    config_path: &Path,
    requested_tenant: Option<&str>,
    requested_slug: Option<&str>,
    derive: Derive,
) -> Result<Resolved, String> {
    let config = Config::load(config_path).map_err(|error| error.to_string())?;
    let derived = match derive {
        Derive::Environment => engram_paths::resolve_slug(None),
        Derive::Cwd => engram_paths::resolve_slug_in_cwd(None),
    };
    let tenant = resolve_tenant(&config, requested_tenant, &derived, requested_slug)
        .map_err(|error| error.to_string())?;
    let slug = tenant
        .choose_slug(requested_slug, &derived)
        .map_err(|error| error.to_string())?;
    Ok(Resolved {
        config,
        tenant,
        slug,
    })
}

/// The identity for a store-scoped operation.
///
/// **The tenant is a function of the store.** If the store can be named, the
/// identity is determined: the slug→tenant mapping is declared by the operator
/// and total, so there is exactly one right answer and no way to pick wrongly.
/// `--tenant` is then needed only where no store can be determined, and acts as
/// a cross-check when both are supplied (a slug belonging to another identity is
/// still refused, by [`Tenant::choose_slug`]).
///
/// This rule exists because the alternative does not work. The prompt hook and
/// the MCP server are registered ONCE in `settings.json`, and the save path fires
/// `engram-index --slug <slug>` backgrounded with its output discarded — a host
/// with four identities needs all four served from those fixed call sites, and a
/// hard-coded `--tenant` could serve one. Requiring the flag there did not make
/// anything safer; it made memories silently stop being indexed.
///
/// It is NOT the "pick the only tenant" default [`Tenant::resolve`] refuses.
/// That default is unrelated to what the caller is doing and becomes silently
/// wrong the day a second tenant appears. This tracks the store actually being
/// read or written, and a store no tenant claims is refused rather than assigned
/// to whoever happens to be running.
///
/// Exposed separately so the prompt hook can pass the session `cwd` from its
/// payload rather than the hook process's own working directory — those differ,
/// and the payload is authoritative.
pub fn resolve_tenant(
    config: &Config,
    requested: Option<&str>,
    derived_slug: &str,
    requested_slug: Option<&str>,
) -> Result<Tenant, TenantError> {
    let named = requested.map(str::trim).filter(|name| !name.is_empty());
    if named.is_some() || !config.tenancy_enabled() {
        return Tenant::resolve(config, requested);
    }
    // An explicit --slug outranks the derived one: a caller may legitimately
    // name another of its own tenant's stores.
    let slug = requested_slug
        .map(str::trim)
        .filter(|slug| !slug.is_empty())
        .unwrap_or(derived_slug);
    match config.tenant_of_slug(slug) {
        Some(owner) => Tenant::resolve(config, Some(owner)),
        None => Err(TenantError::UnownedStore {
            slug: slug.to_string(),
            available: Tenant::available(config),
        }),
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum TenantError {
    #[error(
        "--tenant is required: this install defines tenants ({available}). \
         Pass --tenant <name> or set ENGRAM_TENANT"
    )]
    Required { available: String },
    #[error("unknown tenant '{requested}'; configured tenants are {available}")]
    Unknown {
        requested: String,
        available: String,
    },
    #[error(
        "--tenant '{requested}' was given but no tenants are configured; \
         add a 'tenants:' block to engram.yaml first"
    )]
    NotConfigured { requested: String },
    #[error(
        "tenant '{tenant}' does not own memory store '{slug}'; \
         it belongs to another identity"
    )]
    ForeignSlug { tenant: String, slug: String },
    #[error(
        "tenant '{tenant}' owns several memory stores ({slugs}) and none matches \
         the current directory ('{derived}'); pass --slug to choose one"
    )]
    AmbiguousSlug {
        tenant: String,
        derived: String,
        slugs: String,
    },
    #[error(
        "no tenant owns memory store '{slug}', so there is no identity to act as \
         here; add it to a tenant's `slugs:` in engram.yaml (tenants: {available}) \
         or pass --tenant explicitly"
    )]
    UnownedStore { slug: String, available: String },
    #[error("tenant '{tenant}' has no vault configured")]
    NoVault { tenant: String },
    #[error("vault for tenant '{tenant}' is not readable at {path}: {detail}")]
    VaultUnreadable {
        tenant: String,
        path: String,
        detail: String,
    },
    #[error("'{path}' must be a relative path inside the vault")]
    NotRelative { path: String },
    #[error("'{path}' escapes the vault of tenant '{tenant}'")]
    Escapes { tenant: String, path: String },
    #[error(
        "'{path}' is outside the agent-writable subtree '{subtree}'; \
         agents may only write there"
    )]
    NotWritable { path: String, subtree: String },
}

/// Permission to query one store, within one tenant.
///
/// Every graph call takes one of these instead of a bare `slug: &str`, and the
/// only way to get one is [`Tenant::graph_scope`], which refuses a slug the
/// tenant does not own. That makes the ownership check happen exactly once, at
/// construction, rather than being a thing each of ~20 call sites has to
/// remember — and it means a cross-tenant query cannot be *expressed*, rather
/// than being expressible and merely incorrect.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphScope {
    tenant: String,
    slug: String,
}

impl GraphScope {
    /// Build a scope without a [`Tenant`], for operator probe binaries only.
    ///
    /// `engram-graph-facts` and friends connect to a raw Neo4j with explicit
    /// `--uri`/`--password` and no `engram.yaml` at all, so there is no config
    /// from which to resolve a tenant. They still must not query unpartitioned,
    /// hence this.
    ///
    /// It takes the ownership check away, so the caller owns it. Deliberately
    /// named `unchecked` and deliberately ugly: every use is a place where the
    /// compile-time guarantee this module exists for does NOT apply, and `grep
    /// -rn unchecked` has to find all of them. Do not use it to make an ordinary
    /// call site compile — resolve a `Tenant` and call
    /// [`Tenant::graph_scope`] instead.
    pub fn unchecked(tenant: impl Into<String>, slug: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            slug: slug.into(),
        }
    }

    /// The `$tenant` parameter: the Graphiti `group_id` and the native `tenant`
    /// node property.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// The `$slug` parameter: which memory store inside the tenant.
    pub fn slug(&self) -> &str {
        &self.slug
    }
}

/// A resolved agent identity. Construct through [`Tenant::resolve`].
#[derive(Clone, Debug, PartialEq)]
pub struct Tenant {
    /// `None` is the legacy tenant: no `tenants:` block in the config.
    name: Option<String>,
    slugs: Vec<String>,
    vault: Option<PathBuf>,
    agent_subtree: String,
    extract_facts: bool,
    memory_collection: String,
    wiki_collection: String,
}

impl Tenant {
    /// Resolve the active tenant, or explain precisely why there isn't one.
    ///
    /// `requested` is the `--tenant` flag or `ENGRAM_TENANT`. Every refusal
    /// lists the configured tenants, because the operator's next action is
    /// always to pick one and the alternative is a second command to find out
    /// what the options were.
    pub fn resolve(config: &Config, requested: Option<&str>) -> Result<Self, TenantError> {
        let requested = requested.map(str::trim).filter(|name| !name.is_empty());
        if !config.tenancy_enabled() {
            // A --tenant on an install with no tenants is a mistake worth
            // naming: silently ignoring it would run the command against the
            // shared legacy index while the operator believed it was scoped.
            if let Some(requested) = requested {
                return Err(TenantError::NotConfigured {
                    requested: requested.to_string(),
                });
            }
            return Ok(Self::legacy(config));
        }
        let available = Self::available(config);
        let Some(requested) = requested else {
            return Err(TenantError::Required { available });
        };
        let Some(tenant) = config.tenants.get(requested) else {
            return Err(TenantError::Unknown {
                requested: requested.to_string(),
                available,
            });
        };
        let vault = tenant.vault.trim();
        Ok(Self {
            name: Some(requested.to_string()),
            slugs: tenant.slugs.iter().map(|s| s.trim().to_string()).collect(),
            vault: (!vault.is_empty()).then(|| PathBuf::from(vault)),
            agent_subtree: tenant.agent_subtree.trim().to_string(),
            extract_facts: tenant.extract_facts,
            memory_collection: format!("{}__{requested}", config.vector_store.collection),
            wiki_collection: format!("{}__{requested}", config.vector_store.wiki_collection),
        })
    }

    /// The pre-tenancy world: the configured collection and Graphiti's single
    /// `canonical` group, with no vault and no slug ownership.
    fn legacy(config: &Config) -> Self {
        Self {
            name: None,
            slugs: Vec::new(),
            vault: None,
            agent_subtree: String::new(),
            extract_facts: false,
            memory_collection: config.vector_store.collection.clone(),
            wiki_collection: config.vector_store.wiki_collection.clone(),
        }
    }

    pub(crate) fn available(config: &Config) -> String {
        config
            .tenants
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The tenant's name, or `None` on a pre-tenancy install.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// A name safe to put in a log line or an error.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or("(untenanted)")
    }

    pub fn is_legacy(&self) -> bool {
        self.name.is_none()
    }

    pub fn slugs(&self) -> &[String] {
        &self.slugs
    }

    /// Whether this tenant may read `slug`.
    ///
    /// The legacy tenant owns everything, because on a pre-tenancy install
    /// there is no boundary to enforce and every existing caller passes a slug
    /// resolved the usual way. A NAMED tenant owns only what it declares, so an
    /// explicit `--slug` belonging to another identity is refusable.
    pub fn owns_slug(&self, slug: &str) -> bool {
        self.is_legacy() || self.slugs.iter().any(|owned| owned == slug)
    }

    /// Which of this tenant's stores to use, given operator input and the
    /// store `engram-paths` derived from the environment.
    ///
    /// The derived slug comes from the working directory, which on a
    /// multi-tenant host is frequently some *other* tenant's project — so it
    /// cannot simply be trusted, and it cannot simply be ignored either, since
    /// it is the right answer whenever an agent is working inside its own tree.
    ///
    /// Order: an explicit `--slug` must be owned, or it is refused outright. A
    /// derived slug is used when owned. Otherwise, a tenant declaring exactly
    /// one store has no ambiguity to resolve, so that store is used — note this
    /// is a choice strictly *inside* the boundary, unlike defaulting the tenant
    /// itself, which is never done. A tenant with several stores and no usable
    /// hint is told to pick.
    pub fn choose_slug(&self, cli: Option<&str>, derived: &str) -> Result<String, TenantError> {
        if let Some(slug) = cli.map(str::trim).filter(|slug| !slug.is_empty()) {
            if !self.owns_slug(slug) {
                return Err(TenantError::ForeignSlug {
                    tenant: self.label().to_string(),
                    slug: slug.to_string(),
                });
            }
            return Ok(slug.to_string());
        }
        if self.is_legacy() || self.owns_slug(derived) {
            return Ok(derived.to_string());
        }
        if let [only] = self.slugs.as_slice() {
            return Ok(only.clone());
        }
        Err(TenantError::AmbiguousSlug {
            tenant: self.label().to_string(),
            derived: derived.to_string(),
            slugs: self.slugs.join(", "),
        })
    }

    /// Permission to query one of this tenant's stores.
    ///
    /// Refuses a slug belonging to another identity. That refusal is the reason
    /// this returns a `Result` rather than a plain value: an explicit `--slug`
    /// is operator input, and on a multi-tenant install it is the one input that
    /// could ask one agent's process to read another's store.
    pub fn graph_scope(&self, slug: &str) -> Result<GraphScope, TenantError> {
        let slug = slug.trim();
        if !self.owns_slug(slug) {
            return Err(TenantError::ForeignSlug {
                tenant: self.label().to_string(),
                slug: slug.to_string(),
            });
        }
        Ok(GraphScope {
            tenant: self.graph_group().to_string(),
            slug: slug.to_string(),
        })
    }

    /// Qdrant collection holding this tenant's memories.
    pub fn memory_collection(&self) -> &str {
        &self.memory_collection
    }

    /// Qdrant collection holding this tenant's wiki chunks.
    pub fn wiki_collection(&self) -> &str {
        &self.wiki_collection
    }

    /// The Graphiti `group_id` / native `tenant` property for this identity.
    pub fn graph_group(&self) -> &str {
        self.name.as_deref().unwrap_or(LEGACY_GRAPH_GROUP)
    }

    pub fn vault(&self) -> Option<&Path> {
        self.vault.as_deref()
    }

    pub fn extract_facts(&self) -> bool {
        self.extract_facts
    }

    /// The vault root, resolved through symlinks.
    ///
    /// Canonicalized here and nowhere else, so every containment check compares
    /// against the same real path. Comparing against the *configured* path would
    /// be defeated by a symlinked vault root.
    pub fn vault_root(&self) -> Result<PathBuf, TenantError> {
        let vault = self.vault.as_deref().ok_or_else(|| TenantError::NoVault {
            tenant: self.label().to_string(),
        })?;
        vault
            .canonicalize()
            .map_err(|error| TenantError::VaultUnreadable {
                tenant: self.label().to_string(),
                path: vault.display().to_string(),
                detail: error.to_string(),
            })
    }

    /// Resolve a vault-relative path to a real path, refusing anything that
    /// leaves the vault.
    ///
    /// This is the function that makes the isolation claim true on disk. A
    /// symlink inside one tenant's vault pointing at another's — or a `..`
    /// walked up out of it — would otherwise hand an agent the other identity's
    /// documents while every database-level filter remained perfectly correct.
    pub fn resolve_in_vault(&self, relative: &Path) -> Result<PathBuf, TenantError> {
        let root = self.vault_root()?;
        self.contain(&root, relative)
    }

    /// As [`Tenant::resolve_in_vault`], and additionally require the path to sit
    /// under the agent-writable subtree.
    ///
    /// Two separate checks, deliberately: containment keeps an agent inside its
    /// own vault, and this keeps it out of the human-authored part of that
    /// vault. Collapsing them would make a future change to one silently widen
    /// the other.
    pub fn resolve_for_agent_write(&self, relative: &Path) -> Result<PathBuf, TenantError> {
        let root = self.vault_root()?;
        let resolved = self.contain(&root, relative)?;
        // Resolve the subtree the same way, so a symlinked `_agent` is compared
        // as its real location rather than its name.
        let writable = self.contain(&root, Path::new(&self.agent_subtree))?;
        if !resolved.starts_with(&writable) {
            return Err(TenantError::NotWritable {
                path: relative.display().to_string(),
                subtree: self.agent_subtree.clone(),
            });
        }
        Ok(resolved)
    }

    /// The agent-writable root, as a real path. `None` for the legacy tenant.
    pub fn agent_root(&self) -> Option<PathBuf> {
        self.vault
            .as_ref()
            .map(|vault| vault.join(&self.agent_subtree))
    }

    fn contain(&self, root: &Path, relative: &Path) -> Result<PathBuf, TenantError> {
        // Shape first. It is cheap, it needs no filesystem, and it rejects the
        // ordinary `../../etc/passwd` case before anything is opened.
        if relative.is_absolute() {
            return Err(TenantError::NotRelative {
                path: relative.display().to_string(),
            });
        }
        for part in relative.components() {
            match part {
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(TenantError::NotRelative {
                        path: relative.display().to_string(),
                    });
                }
                _ => {}
            }
        }
        let joined = root.join(relative);
        // Then resolve symlinks and re-check. Checking only the joined path
        // would pass a `leak -> /vaults/other` symlink, since textually it IS
        // under the root.
        let resolved = canonicalize_existing_prefix(&joined).map_err(|error| {
            TenantError::VaultUnreadable {
                tenant: self.label().to_string(),
                path: joined.display().to_string(),
                detail: error.to_string(),
            }
        })?;
        // Path::starts_with is component-wise, so `/vaults/work-other` does not
        // start with `/vaults/work`. A string prefix test would accept it.
        if !resolved.starts_with(root) {
            return Err(TenantError::Escapes {
                tenant: self.label().to_string(),
                path: relative.display().to_string(),
            });
        }
        Ok(resolved)
    }
}

/// Canonicalize as much of `path` as exists, keeping the missing tail.
///
/// A write target does not exist yet, so plain `canonicalize` fails outright on
/// it — but the containment check still has to see through symlinks on every
/// component that DOES exist, because that is where an escape gets planted. The
/// tail is safe to re-append un-resolved: `..` was already rejected by shape.
fn canonicalize_existing_prefix(path: &Path) -> std::io::Result<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match current.canonicalize() {
            Ok(real) => {
                let mut resolved = real;
                for part in tail.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = current.file_name().map(std::ffi::OsStr::to_os_string) else {
                    return Err(error);
                };
                tail.push(name);
                if !current.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(yaml: &str) -> Config {
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        config.validate().unwrap();
        config
    }

    const TWO: &str = "tenants:\n  \
         homelab:\n    slugs: ['-root']\n    vault: /vaults/homelab\n  \
         work-company-x:\n    slugs: ['-root-MJSV']\n    vault: /vaults/work\n";

    /// With tenancy configured, an absent `--tenant` must REFUSE — including
    /// when there is exactly one tenant to choose from.
    ///
    /// A convenience default here is how a cross-tenant read gets introduced
    /// later: it is correct while one tenant exists and silently wrong the day a
    /// second is added, with no error to notice.
    #[test]
    fn an_absent_tenant_is_refused_even_when_only_one_exists() {
        let one = config("tenants:\n  homelab:\n    slugs: ['-root']\n");
        let error = Tenant::resolve(&one, None).expect_err("must not default");
        assert_eq!(
            error,
            TenantError::Required {
                available: "homelab".into()
            }
        );
        // the refusal names the options, so the next command is the right one
        assert!(format!("{error}").contains("homelab"));

        // blank and whitespace are not a choice either
        assert!(Tenant::resolve(&one, Some("")).is_err());
        assert!(Tenant::resolve(&one, Some("   ")).is_err());
    }

    #[test]
    fn an_unknown_tenant_is_refused_and_lists_the_real_ones() {
        let two = config(TWO);
        let error = Tenant::resolve(&two, Some("work")).unwrap_err();
        let message = format!("{error}");
        assert!(message.contains("unknown tenant 'work'"), "{message}");
        assert!(message.contains("homelab, work-company-x"), "{message}");
    }

    /// The upgrade path: no `tenants:` block at all.
    #[test]
    fn a_pre_tenancy_install_resolves_to_the_legacy_tenant() {
        let legacy = config("vector_store: {collection: engram_memory}\n");
        let tenant = Tenant::resolve(&legacy, None).unwrap();
        assert!(tenant.is_legacy());
        assert_eq!(tenant.name(), None);
        // it reads the collection and group it always did
        assert_eq!(tenant.memory_collection(), "engram_memory");
        assert_eq!(tenant.graph_group(), LEGACY_GRAPH_GROUP);
        assert_eq!(tenant.vault(), None);
        // and it owns every slug, because there is no boundary yet
        assert!(tenant.owns_slug("-root"));
        assert!(tenant.owns_slug("-anything-at-all"));

        // ...but asking for a tenant that cannot exist is still an error, not a
        // silent fall back to the shared index.
        let error = Tenant::resolve(&legacy, Some("work")).unwrap_err();
        assert_eq!(
            error,
            TenantError::NotConfigured {
                requested: "work".into()
            }
        );
    }

    /// Named tenants must not collide anywhere a query is addressed.
    #[test]
    fn each_tenant_addresses_its_own_collections_and_graph_group() {
        let two = config(TWO);
        let work = Tenant::resolve(&two, Some("work-company-x")).unwrap();
        let home = Tenant::resolve(&two, Some("homelab")).unwrap();

        assert_eq!(work.memory_collection(), "engram_memory__work-company-x");
        assert_eq!(home.memory_collection(), "engram_memory__homelab");
        assert_eq!(work.wiki_collection(), "engram_wiki__work-company-x");
        assert_eq!(home.wiki_collection(), "engram_wiki__homelab");
        assert_eq!(work.graph_group(), "work-company-x");
        assert_eq!(home.graph_group(), "homelab");

        // nothing a named tenant addresses may equal what another addresses, and
        // none of it may equal the shared legacy names
        for (left, right) in [
            (work.memory_collection(), home.memory_collection()),
            (work.wiki_collection(), home.wiki_collection()),
            (work.graph_group(), home.graph_group()),
        ] {
            assert_ne!(left, right);
        }
        assert_ne!(work.graph_group(), LEGACY_GRAPH_GROUP);
        assert_ne!(work.memory_collection(), "engram_memory");
    }

    #[test]
    fn a_named_tenant_owns_only_its_declared_slugs() {
        let two = config(TWO);
        let work = Tenant::resolve(&two, Some("work-company-x")).unwrap();
        assert!(work.owns_slug("-root-MJSV"));
        assert!(
            !work.owns_slug("-root"),
            "the other tenant's store must not be claimed"
        );
        assert!(!work.owns_slug("-root-unassigned"));
        assert_eq!(work.slugs(), ["-root-MJSV"]);
    }

    /// Asking for another identity's store must be refused at the point the
    /// query is authorised, not filtered out afterwards.
    #[test]
    fn a_scope_cannot_be_built_for_another_tenants_store() {
        let two = config(TWO);
        let work = Tenant::resolve(&two, Some("work-company-x")).unwrap();

        let mine = work.graph_scope("-root-MJSV").unwrap();
        assert_eq!(mine.slug(), "-root-MJSV");
        assert_eq!(mine.tenant(), "work-company-x");

        // '-root' is the homelab tenant's store
        let error = work.graph_scope("-root").unwrap_err();
        assert_eq!(
            error,
            TenantError::ForeignSlug {
                tenant: "work-company-x".into(),
                slug: "-root".into()
            }
        );
        // and an unassigned store belongs to nobody, so it is refused too
        assert!(work.graph_scope("-root-unassigned").is_err());
    }

    /// The identity follows the store.
    ///
    /// The hook and MCP server are registered once in `settings.json`, and the
    /// save path fires `engram-index --slug <slug>` with its output discarded —
    /// a fixed `--tenant` at those call sites could serve one of four
    /// identities, and requiring it made memories silently stop being indexed.
    /// Since the slug→tenant mapping is operator-declared and total, naming the
    /// store determines the identity, with exactly one right answer.
    #[test]
    fn the_identity_follows_the_store_being_read_or_written() {
        let config = config(TWO);

        // in the work project -> work; in the homelab project -> homelab
        let work = resolve_tenant(&config, None, "-root-MJSV", None).unwrap();
        assert_eq!(work.name(), Some("work-company-x"));
        let home = resolve_tenant(&config, None, "-root", None).unwrap();
        assert_eq!(home.name(), Some("homelab"));

        // an explicit --tenant still wins over the directory
        let forced = resolve_tenant(&config, Some("homelab"), "-root-MJSV", None).unwrap();
        assert_eq!(forced.name(), Some("homelab"));

        // a directory no tenant claims is REFUSED, not assigned to anyone
        let error = resolve_tenant(&config, None, "-root-unclaimed", None).unwrap_err();
        assert!(
            matches!(error, TenantError::UnownedStore { .. }),
            "got {error:?}"
        );
        assert!(format!("{error}").contains("-root-unclaimed"));

        // an explicit --slug picks the identity too, and `choose_slug` still
        // enforces ownership afterwards
        let by_slug = resolve_tenant(&config, None, "-root", Some("-root-MJSV")).unwrap();
        assert_eq!(by_slug.name(), Some("work-company-x"));
    }

    /// A store no tenant claims is refused, never adopted.
    ///
    /// This is the half of the rule that keeps "the identity follows the store"
    /// from becoming "the identity is whatever is nearby". An unassigned store
    /// has no collection to be written to, and adopting it into whichever tenant
    /// happens to be running would put one identity's memories inside another's
    /// boundary.
    #[test]
    fn an_unowned_store_has_no_identity_to_borrow() {
        let two = config(TWO);
        let error = resolve_tenant(&two, None, "-root-nobodys", None).unwrap_err();
        let message = format!("{error}");
        assert!(message.contains("no tenant owns"), "{message}");
        // the refusal lists the tenants, so the fix is one edit away
        assert!(message.contains("homelab"), "{message}");

        // with tenancy off there is no boundary, so any store resolves
        let legacy = config("backend: ollama\n");
        assert!(
            resolve_tenant(&legacy, None, "-root-anything", None)
                .unwrap()
                .is_legacy()
        );
    }

    /// The cwd-derived slug is a hint, not an authority: on a multi-tenant host
    /// it is routinely some other tenant's project directory.
    #[test]
    fn choosing_a_store_never_leaves_the_tenant() {
        let two = config(TWO);
        let work = Tenant::resolve(&two, Some("work-company-x")).unwrap();

        // an explicit flag is honoured when owned...
        assert_eq!(
            work.choose_slug(Some("-root-MJSV"), "-root").unwrap(),
            "-root-MJSV"
        );
        // ...and refused when it is another identity's store, even though it is
        // a perfectly real store that exists on disk
        assert!(matches!(
            work.choose_slug(Some("-root"), "-root-MJSV").unwrap_err(),
            TenantError::ForeignSlug { .. }
        ));

        // the derived slug wins when the agent IS in its own tree
        assert_eq!(work.choose_slug(None, "-root-MJSV").unwrap(), "-root-MJSV");
        // and when it is not, the tenant's single store is unambiguous — a
        // choice inside the boundary, unlike defaulting the tenant itself
        assert_eq!(work.choose_slug(None, "-root").unwrap(), "-root-MJSV");
    }

    #[test]
    fn several_stores_and_no_usable_hint_asks_rather_than_guesses() {
        let config = config(
            "tenants:\n  work:\n    slugs: ['-root-a', '-root-b']\n  \
             homelab:\n    slugs: ['-root']\n",
        );
        let work = Tenant::resolve(&config, Some("work")).unwrap();
        let error = work.choose_slug(None, "-somewhere-else").unwrap_err();
        let message = format!("{error}");
        assert!(message.contains("-root-a, -root-b"), "{message}");
        assert!(message.contains("--slug"), "{message}");
        // a hint that does match is still used
        assert_eq!(work.choose_slug(None, "-root-b").unwrap(), "-root-b");
    }

    /// A pre-tenancy install must keep taking whatever slug it derived.
    #[test]
    fn the_legacy_tenant_accepts_the_derived_store() {
        let tenant = Tenant::resolve(&config("backend: ollama\n"), None).unwrap();
        assert_eq!(
            tenant.choose_slug(None, "-root-anything").unwrap(),
            "-root-anything"
        );
        assert_eq!(
            tenant.choose_slug(Some("-explicit"), "-x").unwrap(),
            "-explicit"
        );
    }

    /// The legacy tenant has to keep working against any slug, because a
    /// pre-tenancy install resolves every store the usual way and none of them
    /// are declared anywhere.
    #[test]
    fn the_legacy_tenant_scopes_any_slug_to_the_historical_group() {
        let legacy = config("backend: ollama\n");
        let tenant = Tenant::resolve(&legacy, None).unwrap();
        let scope = tenant.graph_scope("-root-anything").unwrap();
        assert_eq!(scope.slug(), "-root-anything");
        assert_eq!(scope.tenant(), LEGACY_GRAPH_GROUP);
    }

    /// A vault fixture, so the containment tests exercise real symlinks rather
    /// than string manipulation.
    struct Vaults {
        dir: PathBuf,
    }

    impl Vaults {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "engram-tenant-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            for sub in ["work/Runbooks", "work/_agent", "homelab/Secrets"] {
                std::fs::create_dir_all(dir.join(sub)).unwrap();
            }
            std::fs::write(dir.join("work/Runbooks/DNS.md"), "# DNS\n").unwrap();
            std::fs::write(dir.join("homelab/Secrets/Keys.md"), "# Keys\n").unwrap();
            Self { dir }
        }

        fn tenant(&self, name: &str, vault: &str) -> Tenant {
            let config = config(&format!(
                "tenants:\n  {name}:\n    slugs: ['-s']\n    vault: {}\n",
                self.dir.join(vault).display()
            ));
            Tenant::resolve(&config, Some(name)).unwrap()
        }
    }

    impl Drop for Vaults {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    #[test]
    fn a_path_inside_the_vault_resolves() {
        let vaults = Vaults::new("inside");
        let work = vaults.tenant("work", "work");
        let resolved = work.resolve_in_vault(Path::new("Runbooks/DNS.md")).unwrap();
        assert!(resolved.ends_with("Runbooks/DNS.md"));
        assert!(resolved.starts_with(work.vault_root().unwrap()));
    }

    /// The breach this function exists to stop: a symlink in one vault pointing
    /// into another tenant's.
    ///
    /// Every database filter can be perfectly correct and this would still hand
    /// the work agent the homelab vault, because the leak is on the filesystem,
    /// below the level any query sees.
    #[test]
    #[cfg(unix)] // the whole test is about symlinks, not just its setup
    fn a_symlink_out_of_the_vault_is_refused() {
        let vaults = Vaults::new("symlink");
        let work = vaults.tenant("work", "work");

        std::os::unix::fs::symlink(vaults.dir.join("homelab"), vaults.dir.join("work/leak"))
            .unwrap();

        // the symlink itself
        let error = work.resolve_in_vault(Path::new("leak")).unwrap_err();
        assert!(
            matches!(error, TenantError::Escapes { .. }),
            "expected an escape, got {error:?}"
        );
        // and a path THROUGH it, which is the form that actually reads a file
        let error = work
            .resolve_in_vault(Path::new("leak/Secrets/Keys.md"))
            .unwrap_err();
        assert!(
            matches!(error, TenantError::Escapes { .. }),
            "expected an escape, got {error:?}"
        );
    }

    #[test]
    fn traversal_and_absolute_paths_are_refused_before_touching_the_disk() {
        let vaults = Vaults::new("traversal");
        let work = vaults.tenant("work", "work");
        for path in [
            "../homelab/Secrets/Keys.md",
            "Runbooks/../../homelab/Secrets/Keys.md",
            "..",
            "/etc/passwd",
        ] {
            let error = work.resolve_in_vault(Path::new(path)).unwrap_err();
            assert!(
                matches!(
                    error,
                    TenantError::NotRelative { .. } | TenantError::Escapes { .. }
                ),
                "{path} gave {error:?}"
            );
        }
    }

    /// A sibling directory sharing a name prefix must not count as inside.
    /// `Path::starts_with` gets this right and a string prefix test does not,
    /// so the containment check is pinned to the component-wise behaviour.
    #[test]
    fn a_prefix_sharing_sibling_is_not_inside_the_vault() {
        let vaults = Vaults::new("prefix");
        std::fs::create_dir_all(vaults.dir.join("work-other/Deep")).unwrap();
        std::fs::write(vaults.dir.join("work-other/Deep/x.md"), "x").unwrap();
        let work = vaults.tenant("work", "work");
        let root = work.vault_root().unwrap();
        let sibling = vaults.dir.join("work-other").canonicalize().unwrap();
        assert!(
            !sibling.starts_with(&root),
            "{} must not be inside {}",
            sibling.display(),
            root.display()
        );
    }

    /// Writes are confined twice: inside the vault, and inside the agent
    /// subtree. The second is what keeps an agent out of human-authored pages.
    #[test]
    fn agent_writes_are_confined_to_the_agent_subtree() {
        let vaults = Vaults::new("writes");
        let work = vaults.tenant("work", "work");

        // a new file under _agent is allowed even though it does not exist yet
        let target = work
            .resolve_for_agent_write(Path::new("_agent/findings/new.md"))
            .unwrap();
        assert!(target.ends_with("_agent/findings/new.md"));

        // the curated part of the vault is refused, though it IS in the vault
        let error = work
            .resolve_for_agent_write(Path::new("Runbooks/DNS.md"))
            .unwrap_err();
        assert!(
            matches!(error, TenantError::NotWritable { .. }),
            "got {error:?}"
        );
        // and climbing out of the subtree is refused as traversal
        let error = work
            .resolve_for_agent_write(Path::new("_agent/../Runbooks/DNS.md"))
            .unwrap_err();
        assert!(
            matches!(
                error,
                TenantError::NotRelative { .. } | TenantError::NotWritable { .. }
            ),
            "got {error:?}"
        );
        // reading that same curated path is fine — the two checks are separate
        work.resolve_in_vault(Path::new("Runbooks/DNS.md")).unwrap();
    }

    #[test]
    fn a_tenant_with_no_vault_says_so_rather_than_guessing() {
        let config = config("tenants:\n  work:\n    slugs: ['-root-MJSV']\n");
        let work = Tenant::resolve(&config, Some("work")).unwrap();
        assert_eq!(work.vault(), None);
        assert_eq!(
            work.resolve_in_vault(Path::new("x.md")).unwrap_err(),
            TenantError::NoVault {
                tenant: "work".into()
            }
        );
        // it still addresses its own collections — a tenant may own stores and
        // no vault, which is every tenant's state during the migration
        assert_eq!(work.memory_collection(), "engram_memory__work");
    }
}
