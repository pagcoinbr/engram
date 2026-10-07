//! Where engram lives, resolved exactly once and exactly like the Python side.
//!
//! Every Rust binary used to carry its own `/root/.claude` clap default and its own
//! `"-root"` slug literal. That is correct on precisely one machine — the host this
//! crate was written on, where the service runs as root — and silently wrong
//! everywhere else: another user got the wrong config, the wrong graph environment
//! and an empty memory store, with no error, because an empty store is also what a
//! store with no matches looks like.
//!
//! The precedence below mirrors `bin/memory_recall.py::resolve_slug` and
//! `bin/memory_lib.sh::memory_slug` line for line. That is the point: the Python
//! writer and the Rust reader must agree on which store they are talking about, or
//! the daemon indexes one store while recall searches another.

use std::path::{Path, PathBuf};

/// `~/.claude` — the engram home, holding `engram.yaml`, `projects/`, `graph/`.
///
/// `ENGRAM_BIN` overrides it (the installer writes that into the daemon
/// environment), then `$HOME/.claude`.
pub fn engram_home() -> PathBuf {
    if let Some(dir) = non_empty_env("ENGRAM_BIN") {
        return PathBuf::from(dir);
    }
    home_dir().join(".claude")
}

/// The active `engram.yaml`: an explicit `--config` wins, then `ENGRAM_CONFIG`,
/// then `<engram home>/engram.yaml`.
pub fn config_path(cli: Option<PathBuf>) -> PathBuf {
    if let Some(path) = cli {
        return path;
    }
    if let Some(path) = non_empty_env("ENGRAM_CONFIG") {
        return PathBuf::from(path);
    }
    engram_home().join("engram.yaml")
}

/// The graph directory — Graphiti's venv, `.env` and `memory_graph_recall.py`.
pub fn graph_dir() -> PathBuf {
    if let Some(dir) = non_empty_env("ENGRAM_GRAPH") {
        return PathBuf::from(dir);
    }
    engram_home().join("graph")
}

/// The vector directory — the Qdrant-side venv and helpers.
pub fn vector_dir() -> PathBuf {
    if let Some(dir) = non_empty_env("ENGRAM_VECTOR") {
        return PathBuf::from(dir);
    }
    engram_home().join("vector")
}

/// Which memory store to use, in the same order as `memory_recall.py`:
/// explicit flag, `CLAUDE_MEMORY_SLUG`, the operator pin in `engram.env`, then a
/// slugified `$HOME`.
///
/// `resolve_slug_in_cwd` adds the Claude-Code-project step for binaries that
/// genuinely run inside a session; a daemon job has an arbitrary cwd, so it must
/// not guess from one.
pub fn resolve_slug(cli: Option<&str>) -> String {
    resolve(cli, None)
}

/// `resolve_slug` plus the cwd-derived project store, for binaries invoked from
/// inside a Claude Code session (the recall hook, the MCP server) where the
/// working directory IS the project whose memories are wanted.
pub fn resolve_slug_in_cwd(cli: Option<&str>) -> String {
    let cwd = std::env::current_dir().ok();
    resolve(cli, cwd.as_deref())
}

/// As [`resolve_slug_in_cwd`], but for a caller that is *told* the directory.
///
/// A hook's process cwd is not necessarily the session's project directory —
/// Claude Code passes the session `cwd` in the hook payload, which is what
/// `bin/hooks/memory-recall-inject.py` uses. Deriving it from
/// `std::env::current_dir()` instead made the Rust hook resolve a different store
/// than its Python sibling whenever the two differed, and a store that does not
/// exist injects nothing at all, silently.
pub fn resolve_slug_in(cli: Option<&str>, cwd: Option<&Path>) -> String {
    match cwd {
        Some(dir) => resolve(cli, Some(dir)),
        None => resolve_slug_in_cwd(cli),
    }
}

fn resolve(cli: Option<&str>, cwd: Option<&Path>) -> String {
    if let Some(slug) = cli.map(str::trim).filter(|s| !s.is_empty()) {
        return slug.to_string();
    }
    if let Some(slug) = non_empty_env("CLAUDE_MEMORY_SLUG") {
        return slug;
    }
    if let Some(slug) = env_file_slug(&engram_home().join("engram.env")) {
        return slug;
    }
    if let Some(dir) = cwd {
        return slugify(dir);
    }
    slugify(&home_dir())
}

/// `/home/u/proj` -> `-home-u-proj`, the naming Claude Code uses for project dirs.
pub fn slugify(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}

/// `<config dir>/projects/<slug>/memory` — the canonical markdown store.
pub fn store_dir(config: &Path, slug: &str) -> PathBuf {
    config
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join("projects")
        .join(slug)
        .join("memory")
}

/// Read `CLAUDE_MEMORY_SLUG` out of an `engram.env`-style file.
///
/// The file is shell-sourced by `memory_lib.sh`, so entries may be bare or
/// `export`ed and the value may be quoted; a missing or unreadable file is simply
/// "no pin", never an error.
fn env_file_slug(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line
            .trim()
            .strip_prefix("export ")
            .unwrap_or(line.trim())
            .trim();
        if let Some(value) = line.strip_prefix("CLAUDE_MEMORY_SLUG=") {
            let value = value.trim().trim_matches('"').trim_matches('\'').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn home_dir() -> PathBuf {
    non_empty_env("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// A hook is TOLD its project directory; it must not guess from its own cwd.
    ///
    /// Claude Code passes the session `cwd` in the hook payload, and the hook
    /// process's working directory is whatever the harness launched it in. The
    /// Python hook has always used the payload value. Reading `current_dir()`
    /// instead resolved a different store, and a store that does not exist
    /// injects nothing at all — silently, with exit 0, looking exactly like
    /// "no relevant memories".
    #[test]
    fn a_supplied_directory_beats_the_process_cwd() {
        let _guard = env_lock();
        // clear(), not just CLAUDE_MEMORY_SLUG: on a host with engram installed the
        // real ~/.claude/engram.env pin otherwise outranks the supplied directory.
        clear();
        let supplied = Path::new("/home/alice/projects/api");
        assert_eq!(
            resolve_slug_in(None, Some(supplied)),
            "-home-alice-projects-api"
        );
        // No directory supplied: fall back to deriving one, not to a panic or a
        // hard-coded store.
        assert_eq!(
            resolve_slug_in(None, None),
            resolve_slug_in_cwd(None),
            "an absent cwd must fall back to the cwd-derived slug"
        );
        // An explicit --slug still wins over both.
        assert_eq!(resolve_slug_in(Some("-pinned"), Some(supplied)), "-pinned");
    }

    /// Env vars are process-global and cargo runs tests on threads, so every test
    /// that touches them has to take the same lock.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn clear() {
        for key in [
            "ENGRAM_BIN",
            "ENGRAM_CONFIG",
            "ENGRAM_GRAPH",
            "ENGRAM_VECTOR",
            "CLAUDE_MEMORY_SLUG",
        ] {
            unsafe { std::env::remove_var(key) };
        }
        unsafe { std::env::set_var("HOME", "/home/tester") };
    }

    #[test]
    fn home_and_config_follow_the_environment_not_root() {
        let _g = env_lock();
        clear();
        assert_eq!(engram_home(), PathBuf::from("/home/tester/.claude"));
        assert_eq!(
            config_path(None),
            PathBuf::from("/home/tester/.claude/engram.yaml")
        );
        assert_eq!(graph_dir(), PathBuf::from("/home/tester/.claude/graph"));

        unsafe { std::env::set_var("ENGRAM_BIN", "/opt/engram") };
        assert_eq!(config_path(None), PathBuf::from("/opt/engram/engram.yaml"));
        assert_eq!(graph_dir(), PathBuf::from("/opt/engram/graph"));

        // an explicit flag outranks every environment variable
        unsafe { std::env::set_var("ENGRAM_CONFIG", "/etc/engram.yaml") };
        assert_eq!(config_path(None), PathBuf::from("/etc/engram.yaml"));
        assert_eq!(
            config_path(Some(PathBuf::from("/tmp/other.yaml"))),
            PathBuf::from("/tmp/other.yaml")
        );
        clear();
    }

    #[test]
    fn slug_precedence_matches_the_python_resolver() {
        let _g = env_lock();
        clear();
        let dir = std::env::temp_dir().join("engram-paths-slug-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("engram.env"),
            "export CLAUDE_MEMORY_SLUG=\"-pinned-store\"\n",
        )
        .unwrap();
        unsafe { std::env::set_var("ENGRAM_BIN", dir.to_str().unwrap()) };

        // 4. slugified $HOME is the last resort
        std::fs::remove_file(dir.join("engram.env")).unwrap();
        assert_eq!(resolve_slug(None), "-home-tester");

        // 3. the engram.env pin beats it
        std::fs::write(
            dir.join("engram.env"),
            "CLAUDE_MEMORY_SLUG='-pinned-store'\n",
        )
        .unwrap();
        assert_eq!(resolve_slug(None), "-pinned-store");

        // 2. the environment beats the pin
        unsafe { std::env::set_var("CLAUDE_MEMORY_SLUG", "-env-store") };
        assert_eq!(resolve_slug(None), "-env-store");

        // 1. an explicit flag beats everything
        assert_eq!(resolve_slug(Some("-flag-store")), "-flag-store");
        // ...but an empty flag is not a choice
        assert_eq!(resolve_slug(Some("  ")), "-env-store");

        std::fs::remove_dir_all(&dir).ok();
        clear();
    }

    #[test]
    fn store_dir_hangs_off_the_config_directory() {
        let _g = env_lock();
        clear();
        assert_eq!(
            store_dir(Path::new("/home/u/.claude/engram.yaml"), "-home-u-proj"),
            PathBuf::from("/home/u/.claude/projects/-home-u-proj/memory")
        );
        // a bare relative --config has no parent; that used to panic on unwrap()
        assert_eq!(
            store_dir(Path::new("engram.yaml"), "-s"),
            PathBuf::from("./projects/-s/memory")
        );
        clear();
    }
}
