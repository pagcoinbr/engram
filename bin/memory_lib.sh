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

# ── PER-MEMORY locking (added 2026-08) ──────────────────────────────────────
# The index lock serialises MEMORY.md, but nothing serialised the MEMORY FILES
# themselves: save_memory.sh overwrites and delete_memory.sh removes with no
# compare-and-swap, so two writers touching the same filename (a session's
# save racing the unattended curator, or a long /memory-cluster run) could lose
# an update outright. These give every memory its own lock file under .locks/.
#
# LOCK ORDER — always file lock OUTER, index lock INNER. Both writers take the
# file lock for their whole mutation and only then call memory_index_* (which
# takes the index lock). Never invert, or two writers deadlock.
#
# Same fd-in-the-CURRENT-shell discipline as memory_with_index_lock: under
# Claude Code's shell snapshot, running the mutation in a `( … ) 9>>lock`
# subshell lets a function-shadowed builtin exec away mid-write.
memory_file_lockfile() {
    local d; d="$(memory_dir)/.locks"
    mkdir -p "$d" 2>/dev/null || true
    printf '%s' "${d}/${1}.lock"
}

# memory_file_lock_acquire <filename.md> — take the per-memory lock for the rest
# of this shell. FAIL-CLOSED: returns non-zero if the lock cannot be opened or
# acquired, and every caller MUST abort.
#
# Deliberately unlike memory_with_index_lock, which falls back to running
# unlocked. That is defensible for the index (a best-effort append), but NOT
# here: MEMORY_NOCLOBBER and MEMORY_EXPECT_SHA are compare-and-swap primitives,
# and a CAS that proceeds unlocked after a timeout silently stops being a CAS —
# two writers would both pass their check and one update would be lost. If flock
# is missing entirely, that guarantee cannot be offered at all, so a caller that
# explicitly asked for CAS is refused rather than quietly downgraded.
memory_file_lock_acquire() {
    MEMORY_FILE_LOCK_FD=""
    local wait="${MEMORY_LOCK_WAIT:-30}"
    if ! command -v flock >/dev/null 2>&1; then
        if [[ -n "${MEMORY_NOCLOBBER:-}" || -n "${MEMORY_EXPECT_SHA:-}" ]]; then
            echo "[memory] REFUSING: flock unavailable, cannot honour MEMORY_NOCLOBBER/MEMORY_EXPECT_SHA" >&2
            return 1
        fi
        return 0   # no CAS requested: proceed as before
    fi
    local lock; lock="$(memory_file_lockfile "$1")"
    if ! exec {MEMORY_FILE_LOCK_FD}>>"$lock" 2>/dev/null; then
        MEMORY_FILE_LOCK_FD=""
        echo "[memory] REFUSING: cannot open lock for $1" >&2
        return 1
    fi
    if ! flock -w "$wait" "$MEMORY_FILE_LOCK_FD"; then
        echo "[memory] REFUSING: timed out after ${wait}s waiting for the lock on $1 (another writer holds it)" >&2
        exec {MEMORY_FILE_LOCK_FD}>&- 2>/dev/null || true
        MEMORY_FILE_LOCK_FD=""
        return 1
    fi
    return 0
}

memory_file_lock_release() {
    [[ -n "${MEMORY_FILE_LOCK_FD:-}" ]] || return 0
    exec {MEMORY_FILE_LOCK_FD}>&- 2>/dev/null || true
    MEMORY_FILE_LOCK_FD=""
}

# memory_file_sha <path> — sha256 of a memory file, or "" when absent. Used for
# the conditional (compare-and-swap) delete.
memory_file_sha() {
    [[ -f "$1" ]] || { printf ''; return 0; }
    sha256sum "$1" 2>/dev/null | awk '{print $1}'
}

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
    local repo remote idx bad
    repo="$(memory_repo)"; remote="$(memory_remote_path)"; idx="$(memory_index)"
    [[ -n "$repo" ]] || return 0   # local-first: no remote configured -> nothing to push
    [[ -f "$idx" ]] || return 0
    # Backstop: never publish an index that trips the secret scanner (a secret in
    # a memory's name/description would otherwise reach GitHub via the index).
    # Fail closed on the PUSH only — the local index stays usable.
    if bad=$(memory_scan_secret "$idx"); then
        echo "[memory] NOT pushing MEMORY.md — possible secret at index line(s): ${bad}. Redact the offending memory's name/description, then re-run memory_reindex.sh --rebuild --apply." >&2
        return 1
    fi
    memory_with_index_lock _gh_put_file "$repo" "${remote}/MEMORY.md" "${1:-update MEMORY.md index}" "$idx"
}

# ---------------------------------------------------------------------------
# Secret guard — every write path (save_memory.sh, save_memory_content_only.sh,
# distill output) MUST scan before writing locally AND before the GitHub PUT.
# Mirrors SECRET_RE in memory_distill_verified.py, plus the bearer / PEM /
# vendor-token / xprv forms the review flagged. High-precision on purpose so it
# rarely false-blocks; override a genuine false positive with MEMORY_ALLOW_SECRET=1.
# ---------------------------------------------------------------------------
# HIGH-PRECISION only. Generic high-entropy nets (bare 32+ hex, 40+ base64) were
# tried and rejected: they false-flag git SHAs, session UUIDs, and base64 examples
# in 60% of legit memories (measured on the live store). A keyless raw-blob secret
# will therefore pass — that ceiling is accepted; the named/prefixed forms below
# cover the credentials people actually paste. ponytail: upgrade to an entropy
# analyzer only if a real keyless-secret leak is observed.
# A named-credential match needs an ASSIGNMENT and an 8+ char value, so prose like
# "the DB password lives in Vault" doesn't trip it.
_MEMORY_SECRET_ERE='((mnemonic|seed[_-]?phrase|recovery[_-]?phrase)[[:space:]]*[:=][[:space:]]*([a-z]+[[:space:],]+){5,}[a-z]+|(client[_-]?secret|webhook[_-]?secret|api[_-]?key|apikey|password|passwd|secret|token|access[_-]?token|mnemonic|seed[_-]?phrase|recovery[_-]?phrase|private[_-]?key|priv[_-]?key|macaroon)[[:space:]]*[:=][[:space:]]*.?[[:alnum:]_/+-]{8,}|BEGIN[[:space:]]+[A-Z ]*PRIVATE KEY|-----BEGIN[[:space:]]|Bearer[[:space:]]+[A-Za-z0-9._-]{20,}|xprv[a-zA-Z0-9]{20,}|[5KL][1-9A-HJ-NP-Za-km-z]{50,51}|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{20,}|sk-[A-Za-z0-9_-]{20,}|[0-9a-f]{80,})'

# memory_scan_secret <file> — echo the offending LINE NUMBERS (never the content,
# so a secret can't leak into the transcript). Returns 0 if a secret was found,
# 1 if the file is clean. Respects MEMORY_ALLOW_SECRET=1 (always reports clean).
memory_scan_secret() {
    local file="$1" hits
    [[ "${MEMORY_ALLOW_SECRET:-0}" == "1" ]] && return 1
    [[ -f "$file" ]] || return 1
    hits=$(grep -niE "$_MEMORY_SECRET_ERE" "$file" 2>/dev/null | cut -d: -f1 | tr '\n' ' ')
    if [[ -n "${hits// /}" ]]; then
        printf '%s' "$hits"
        return 0
    fi
    return 1
}

# memory_guard_stdin_secret <label> — read stdin into $CONTENT-safe temp, block on
# secret. Caller uses: CONTENT=$(cat); memory_guard_secret_content "$CONTENT" tag.
memory_guard_secret_content() {
    local content="$1" label="${2:-memory}" tmp bad
    [[ "${MEMORY_ALLOW_SECRET:-0}" == "1" ]] && return 0   # explicit override, short-circuit
    # Fail CLOSED on scanner setup failure — a full/unwritable tmp must not let an
    # unscanned save through.
    tmp=$(mktemp) || { echo "[${label}] BLOCKED: secret-scan setup failed (mktemp)" >&2; return 1; }
    if ! printf '%s\n' "$content" > "$tmp" 2>/dev/null; then
        rm -f "$tmp"; echo "[${label}] BLOCKED: secret-scan temp write failed" >&2; return 1
    fi
    if bad=$(memory_scan_secret "$tmp"); then
        rm -f "$tmp"
        echo "[${label}] BLOCKED: possible live secret at line(s): ${bad}" >&2
        echo "[${label}] refusing to write/push. Redact it, or set MEMORY_ALLOW_SECRET=1 to override." >&2
        return 1
    fi
    rm -f "$tmp"
    return 0
}

# memory_vector_sync <args...> — fire a best-effort, NON-BLOCKING vector_sync.py
# call (e.g. "--insert --only foo.md" or "--delete foo.md"). The OPTIONAL Qdrant
# index is only installed with `./install.sh --vector`; when it's absent this is a
# zero-cost no-op, and when present vector_sync re-checks vector_store.enabled and
# no-ops itself if disabled/unreachable. Backgrounded with output discarded so a
# slow or down Qdrant never delays (or fails) a save/delete. Mirrors the local-first
# posture of the optional GitHub push above.
memory_vector_sync() {
    local script="${HOME}/.claude/vector/vector_sync.py"
    [[ -f "$script" ]] || return 0
    # Prefer the vector venv (has qdrant-client) > env override > graph venv > python3.
    local py="${ENGRAM_VECTOR_PYTHON:-}"
    [[ -z "$py" && -x "${HOME}/.claude/vector/venv/bin/python" ]] && py="${HOME}/.claude/vector/venv/bin/python"
    [[ -z "$py" ]] && py="${ENGRAM_GRAPH_PYTHON:-python3}"
    command -v "$py" >/dev/null 2>&1 || [[ -x "$py" ]] || py="python3"
    ( "$py" "$script" "$@" >/dev/null 2>&1 & ) 2>/dev/null || true
}
