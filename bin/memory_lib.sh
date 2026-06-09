#!/usr/bin/env bash
# memory_lib.sh — shared resolution + helpers for the Claude memory store.
#
# Source this from save_memory.sh / delete_memory.sh / memory_audit.sh /
# memory_agent.sh / memory_reindex.sh / memory_curate_check.sh so every tool
# targets ONE canonical store and shares index/frontmatter logic.
#
# Canonical store resolution (priority order):
#   1. $CLAUDE_MEMORY_SLUG  — explicit override (e.g. "-home-user" or a
#                             per-project slug like "-home-user-myproj")
#   2. home-scoped slug     — $HOME with "/" -> "-"   (DEFAULT; ignores $PWD)
#
# Why: the tools used to derive the slug from $PWD, so a session running in a
# subdirectory wrote/read a *different* (usually empty) store and fragmented
# memories away from the canonical one. We pin to a single store by default;
# opt into another via CLAUDE_MEMORY_SLUG.
#
# CONCURRENCY (added 2026-06-03): every mutation of MEMORY.md MUST hold the
# index lock (memory_with_index_lock) and use a PER-PID temp file. Previously
# three writers (memory_index_add_line here, memory_fixate_cron.sh's quarantine
# de-index, memory_reindex.sh) each did `... > "${idx}.tmp" && mv` to the SAME
# MEMORY.md.tmp with no lock; concurrent runs (the unattended fixate/pipeline
# cron racing a session's save_memory.sh) clobbered the shared temp and
# truncated the index. GitHub PUTs also retry on sha-conflict (concurrent
# writers to the same remote path otherwise 409 and silently lose the update).

MEMORY_AUTO_SECTION="## Uncategorized (auto-added)"

# Optional install-time env (e.g. CLAUDE_MEMORY_REPO for opt-in GitHub sync).
[[ -f "${HOME}/.claude/engram.env" ]] && source "${HOME}/.claude/engram.env"

memory_repo()        { printf '%s' "${CLAUDE_MEMORY_REPO:-}"; }   # local-first: empty = no remote sync
memory_username()    { printf '%s' "${CLAUDE_MEMORY_USERNAME:-$(basename "$HOME")}"; }  # override for shared/central stores

memory_slug() {
    if [[ -n "${CLAUDE_MEMORY_SLUG:-}" ]]; then
        printf '%s' "$CLAUDE_MEMORY_SLUG"
    else
        printf '%s' "$(printf '%s' "$HOME" | sed 's|/|-|g')"
    fi
}

memory_dir()         { printf '%s' "${HOME}/.claude/projects/$(memory_slug)/memory"; }
memory_index()       { printf '%s' "$(memory_dir)/MEMORY.md"; }
memory_remote_path() { printf '%s' "$(memory_username)/projects/$(memory_slug)/memory"; }
memory_state_file()  { printf '%s' "$(memory_dir)/.curator_state"; }

# memory_frontmatter_field <file> <field> — echo a top-level frontmatter value.
memory_frontmatter_field() {
    awk -v f="$2" '
        BEGIN { infm = 0 }
        /^---[[:space:]]*$/ { infm++; if (infm > 1) exit; next }
        infm == 1 && index($0, f ":") == 1 {
            sub("^" f ":[[:space:]]*", "")
            gsub(/^"|"$/, "")
            print
            exit
        }
    ' "$1" 2>/dev/null
}

# memory_frontmatter_type <file> — echo the memory's type, tolerating BOTH the
# top-level `type:` convention and the nested `metadata:\n  type:` convention.
memory_frontmatter_type() {
    awk '
        BEGIN { infm = 0 }
        /^---[[:space:]]*$/ { infm++; if (infm > 1) exit; next }
        infm == 1 && $0 ~ /^[[:space:]]*type:[[:space:]]*/ {
            sub(/^[[:space:]]*type:[[:space:]]*/, "")
            gsub(/^"|"$/, "")
            print
            exit
        }
    ' "$1" 2>/dev/null
}

# ---------------------------------------------------------------------------
# Concurrency primitives
# ---------------------------------------------------------------------------
memory_index_lockfile() { printf '%s' "$(memory_index).lock"; }

# memory_with_index_lock <cmd> [args...] — run cmd while holding an exclusive
# advisory lock on the index lockfile, so all MEMORY.md mutations serialize.
# Falls back to running unlocked (best-effort) if flock is unavailable. The
# command's exit status is propagated to the caller.
#
# NOTE: the lock is taken on a dedicated fd in the CURRENT shell (not via a
# `( ... ) 9>>lock` subshell). Under Claude Code's shell snapshot `grep`/others
# are functions that `exec` the bundled binary when they detect a subshell
# (BASHPID != $$); running the mutation inside a subshell would let that exec
# replace the subshell mid-function and abort the write. Keeping cmd in the
# current shell avoids that.
memory_with_index_lock() {
    if ! command -v flock >/dev/null 2>&1; then
        "$@"; return $?
    fi
    local lock fd rc
    lock="$(memory_index_lockfile)"
    exec {fd}>>"$lock" || { "$@"; return $?; }
    flock -w 30 "$fd" || true
    "$@"; rc=$?
    exec {fd}>&-
    return $rc
}

# _gh_put_file <repo> <remote_path> <message> <local_file> — create/update a
# file on GitHub via the contents API, refreshing the sha and retrying on a
# 409/422 conflict (a concurrent writer changed the file between our GET and
# PUT). Returns 0 on success.
_gh_put_file() {
    local repo="$1" path="$2" msg="$3" file="$4"
    local enc sha out attempt
    enc=$(base64 -w 0 < "$file") || return 1
    for attempt in 1 2 3 4 5; do
        sha=""
        if out=$(gh api "repos/${repo}/contents/${path}" 2>/dev/null); then
            sha=$(printf '%s' "$out" | jq -r '.sha // empty')
        fi
        if [[ -n "$sha" ]]; then
            if gh api "repos/${repo}/contents/${path}" --method PUT \
                    --field message="$msg" --field content="$enc" \
                    --field sha="$sha" --jq '.content.name' >/dev/null 2>&1; then
                return 0
            fi
        else
            if gh api "repos/${repo}/contents/${path}" --method PUT \
                    --field message="$msg" --field content="$enc" \
                    --jq '.content.name' >/dev/null 2>&1; then
                return 0
            fi
        fi
        sleep $(( attempt ))   # linear backoff before re-fetching sha
    done
    echo "[gh-put] ERROR: failed to PUT ${path} after retries" >&2
    return 1
}

# memory_index_add_line <index_file> <filename.md> <description>
# Add a "- [name](file) — desc" line under the auto section if the file is not
# already indexed. LOCAL ONLY (caller is responsible for pushing).
# Returns 0 if a line was added, 1 if the file was already indexed.
_memory_index_add_line_unlocked() {
    local idx="$1" fname="$2" desc="$3"
    local line="- [${fname%.*}](${fname}) — ${desc}"
    if [[ -f "$idx" ]] && grep -qF "](${fname})" "$idx"; then
        return 1
    fi
    local tmp="${idx}.tmp.$$"
    if [[ ! -f "$idx" ]]; then
        printf '# Memory Index\n\n%s\n%s\n' "$MEMORY_AUTO_SECTION" "$line" > "$idx"
        return 0
    fi
    if ! grep -qF "$MEMORY_AUTO_SECTION" "$idx"; then
        printf '\n%s\n' "$MEMORY_AUTO_SECTION" >> "$idx"
    fi
    awk -v hdr="$MEMORY_AUTO_SECTION" -v line="$line" '
        { print }
        $0 == hdr && !done { print line; done = 1 }
    ' "$idx" > "$tmp" && mv "$tmp" "$idx"
    return 0
}
memory_index_add_line() { memory_with_index_lock _memory_index_add_line_unlocked "$@"; }

# memory_index_remove_line <index_file> <filename.md>
# Remove the index line linking to <filename.md>. LOCAL ONLY (caller pushes).
# Returns 0 if a line was removed, 1 if there was nothing to remove.
_memory_index_remove_line_unlocked() {
    local idx="$1" fname="$2"
    [[ -f "$idx" ]] || return 1
    grep -qF "](${fname})" "$idx" || return 1
    local tmp="${idx}.tmp.$$"
    grep -vF "](${fname})" "$idx" > "$tmp" && mv "$tmp" "$idx"
    return 0
}
memory_index_remove_line() { memory_with_index_lock _memory_index_remove_line_unlocked "$@"; }

# memory_push_index [message] — push the local MEMORY.md to the remote (create
# or update), retrying on sha-conflict. Reads the local index under the lock so
# it never uploads a half-written file.
memory_push_index() {
    local repo remote idx
    repo="$(memory_repo)"; remote="$(memory_remote_path)"; idx="$(memory_index)"
    [[ -n "$repo" ]] || return 0   # local-first: no remote configured -> nothing to push
    [[ -f "$idx" ]] || return 0
    memory_with_index_lock _gh_put_file "$repo" "${remote}/MEMORY.md" "${1:-update MEMORY.md index}" "$idx"
}
