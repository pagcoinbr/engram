#!/usr/bin/env bash
# save_memory_content_only.sh — push ONE memory file's content to the canonical
# store + GitHub, WITHOUT touching MEMORY.md. Used for in-place rewrites
# (e.g. reformatting) where the index entry does not change, so the index race
# in memory_index_add_line is avoided entirely and a concurrent index writer
# (the unattended memory pipeline) cannot be clobbered.
# Usage: save_memory_content_only.sh <filename.md> < content
set -euo pipefail
source "${HOME}/.claude/memory_lib.sh"

FILENAME="${1:?Usage: save_memory_content_only.sh <filename.md> < content}"
REPO="$(memory_repo)"
REMOTE_PATH="$(memory_remote_path)"
LOCAL_DIR="$(memory_dir)"

# Same per-memory lock as save_memory.sh — this is a writer too, and a
# /memory-cluster link-rewrite pass calls it for many files in a row.
memory_file_lock_acquire "$FILENAME" || exit 1   # fail-closed: never mutate unlocked
trap 'memory_file_lock_release' EXIT

# MEMORY_EXPECT_SHA=<sha256> — conditional (compare-and-swap) rewrite. A caller
# that READ this file, transformed the text, and is now writing it back passes
# the hash it saw. Without this the read happens before the lock is taken, so a
# concurrent writer's update between the read and this write is silently
# overwritten with stale content. Checked under the lock.
if [[ -n "${MEMORY_EXPECT_SHA:-}" ]]; then
    if [[ ! "$MEMORY_EXPECT_SHA" =~ ^[0-9a-f]{64}$ ]]; then
        echo "[save-content] REFUSING: MEMORY_EXPECT_SHA is set but not a 64-hex sha256" >&2
        exit 1
    fi
    ACTUAL_SHA="$(memory_file_sha "${LOCAL_DIR}/${FILENAME}")"
    if [[ "$ACTUAL_SHA" != "$MEMORY_EXPECT_SHA" ]]; then
        echo "[save-content] REFUSING: ${FILENAME} changed since it was read" >&2
        echo "[save-content]   expected ${MEMORY_EXPECT_SHA:0:12}… got ${ACTUAL_SHA:0:12}…" >&2
        exit 1
    fi
fi

CONTENT=$(cat)
if [[ -z "$CONTENT" ]]; then
    echo "[save-content] ERROR: no content provided via stdin" >&2
    exit 1
fi

# Secret guard: block before writing locally AND before the GitHub push.
memory_guard_secret_content "$CONTENT" "save-content" || exit 1

printf '%s\n' "$CONTENT" > "${LOCAL_DIR}/${FILENAME}"

# Push via the shared retry-on-sha-conflict helper (concurrent writers to the
# same remote path otherwise 409 and silently drop the update).
# Local-first: with no CLAUDE_MEMORY_REPO there is nothing to push. Guarding here
# mirrors save_memory.sh — without it, `_gh_put_file ""` fails and this script
# exits 1 on every call on a local-only install, even though the local write
# succeeded, which makes the exit status useless to callers that check it.
if [[ -z "$REPO" ]]; then
    echo "[save-content] Saved ${FILENAME} locally (no CLAUDE_MEMORY_REPO configured; MEMORY.md index untouched)"
elif _gh_put_file "$REPO" "${REMOTE_PATH}/${FILENAME}" "reformat memory: ${FILENAME}" "${LOCAL_DIR}/${FILENAME}"; then
    echo "[save-content] Pushed ${FILENAME} (MEMORY.md index untouched)"
else
    echo "[save-content] ERROR pushing ${FILENAME}" >&2
    exit 1
fi

# Optional vector index: best-effort, non-blocking re-embed of the rewritten file
# (no-op unless installed with --vector and enabled). Content changed, so the
# sha-synced upsert refreshes its vector.
memory_vector_sync --insert --only "$FILENAME"
