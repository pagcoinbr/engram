#!/usr/bin/env bash
# save_memory.sh — save a memory to the local store (and push to the remote, if one is configured).
# Usage: save_memory.sh <filename.md> <"one-line description"> < content
# Example: echo "content" | save_memory.sh project_foo.md "what foo does"
#
# Body format convention (see memory feedback_memory_file_format): after the
# name/description/type frontmatter, write Summary -> numbered Index -> Body
# (## Summary, ## Index, then one "## <n>. <Title>" section per index entry) so a
# file's content is graspable at a glance. This script writes stdin verbatim — the
# caller is responsible for the shape.

set -euo pipefail

# Shared canonical-store resolution + index helpers (kills $PWD coupling).
source "${HOME}/.claude/memory_lib.sh"

FILENAME="${1:?Usage: save_memory.sh <filename.md> <description>}"
DESCRIPTION="${2:?Provide a one-line description}"

REPO="$(memory_repo)"
REMOTE_PATH="$(memory_remote_path)"
LOCAL_DIR="$(memory_dir)"
MEMORY_MD="$(memory_index)"

mkdir -p "$LOCAL_DIR"

# Hold this memory's own lock for the WHOLE mutation (existence check + write +
# push + index), so a concurrent writer to the same filename cannot interleave.
# Released on any exit path. Lock order: file lock OUTER, index lock INNER.
memory_file_lock_acquire "$FILENAME" || exit 1   # fail-closed: never mutate unlocked
trap 'memory_file_lock_release' EXIT

# Track whether this is a brand-new memory (drives the consolidation counter).
WAS_NEW=0
[[ -f "${LOCAL_DIR}/${FILENAME}" ]] || WAS_NEW=1

# MEMORY_NOCLOBBER=1 — create-only. An overwrite gets no .trash snapshot, so a
# caller that means "this must be a NEW memory" (e.g. /memory-cluster writing a
# merged file) can demand it. Checked UNDER the lock, so it is atomic against
# another writer creating the same name.
if [[ "${MEMORY_NOCLOBBER:-0}" == "1" && "$WAS_NEW" != "1" ]]; then
    echo "[save-memory] REFUSING: ${FILENAME} exists and MEMORY_NOCLOBBER=1 (an overwrite has no .trash snapshot)" >&2
    exit 1
fi

# Read content from stdin
CONTENT=$(cat)
if [[ -z "$CONTENT" ]]; then
    echo "[save-memory] ERROR: no content provided via stdin" >&2
    exit 1
fi

# Secret guard: block before writing locally AND before any GitHub push. Scan
# the body AND the description (the description is written into MEMORY.md and
# pushed, so a secret there leaks via the index path).
memory_guard_secret_content "$CONTENT" "save-memory" || exit 1
memory_guard_secret_content "$DESCRIPTION" "save-memory-desc" || exit 1

# Save locally
printf '%s\n' "$CONTENT" > "${LOCAL_DIR}/${FILENAME}"

# Push to the remote store only if one is configured (local-first by default).
# With no CLAUDE_MEMORY_REPO set, the memory lives purely on this machine.
if [[ -n "$REPO" ]]; then
    # retry-on-sha-conflict helper, so a concurrent writer can't silently 409.
    if _gh_put_file "$REPO" "${REMOTE_PATH}/${FILENAME}" "update memory: ${FILENAME}" "${LOCAL_DIR}/${FILENAME}"; then
        echo "[save-memory] Pushed ${FILENAME} to ${REPO}"
    else
        echo "[save-memory] ERROR: failed to push ${FILENAME}" >&2
        exit 1
    fi
else
    echo "[save-memory] Saved ${FILENAME} locally (no CLAUDE_MEMORY_REPO configured)"
fi

# Index integrity: guarantee MEMORY.md has an entry. New auto-saved memories
# land under a stable "Uncategorized (auto-added)" section so a manual reorg of
# the index can't silently drop them (the old flat-append + substring grep is
# what orphaned files before). memory_index_add_line returns 0 when it adds.
if memory_index_add_line "$MEMORY_MD" "$FILENAME" "$DESCRIPTION"; then
    memory_push_index "update MEMORY.md: add ${FILENAME}"
    echo "[save-memory] Updated MEMORY.md index"
fi

# Optional vector index: best-effort, non-blocking upsert of just this file into
# Qdrant (no-op unless installed with --vector and enabled). Markdown stays the
# source of truth; this only refreshes the semantic index.
memory_vector_sync --insert --only "$FILENAME"

# Auto-consolidate bookkeeping: count NEW memories since the last curate pass so
# memory_curate_check.sh can nudge a consolidation when the store has grown.
if [[ "$WAS_NEW" == "1" ]] && command -v jq >/dev/null 2>&1; then
    STATE="$(memory_state_file)"
    if [[ -f "$STATE" ]]; then
        tmp=$(mktemp) && jq '.saves_since_curate = ((.saves_since_curate // 0) + 1)' "$STATE" > "$tmp" 2>/dev/null \
            && mv "$tmp" "$STATE" || rm -f "$tmp"
    else
        printf '{"schema":1,"last_curate_at":null,"saves_since_curate":1}\n' > "$STATE"
    fi
fi
