#!/usr/bin/env bash
# delete_memory.sh — remove a memory file locally + on your-org/engram-memory,
# and strip its line from MEMORY.md (local + remote).
# Usage: delete_memory.sh <filename.md>
# Mirrors the slug/path conventions of save_memory.sh.

set -euo pipefail

FILENAME="${1:?Usage: delete_memory.sh <filename.md>}"

# Shared canonical-store resolution (kills the old $PWD coupling).
source "${HOME}/.claude/memory_lib.sh"
REPO="$(memory_repo)"
REMOTE_PATH="$(memory_remote_path)"
LOCAL_DIR="$(memory_dir)"

# Hold this memory's own lock for the WHOLE deletion (CAS check + snapshot +
# rm + remote delete + index strip). Lock order: file lock OUTER, index lock
# INNER — memory_index_remove_line below takes the index lock.
memory_file_lock_acquire "$FILENAME" || exit 1   # fail-closed: never mutate unlocked
trap 'memory_file_lock_release' EXIT

# MEMORY_EXPECT_SHA=<sha256> — conditional (compare-and-swap) delete. A caller
# that read this file earlier passes the hash it saw; if the content changed
# since, ABORT rather than retire an update it never merged. Checked under the
# lock, so the value cannot drift between the check and the rm.
if [[ -n "${MEMORY_EXPECT_SHA:-}" ]]; then
    # Reject a malformed value rather than treating it as "no CAS requested" — a
    # caller whose hash lookup silently produced garbage must not get an
    # unconditional delete.
    if [[ ! "$MEMORY_EXPECT_SHA" =~ ^[0-9a-f]{64}$ ]]; then
        echo "[delete-memory] REFUSING: MEMORY_EXPECT_SHA is set but not a 64-hex sha256" >&2
        exit 1
    fi
    ACTUAL_SHA="$(memory_file_sha "${LOCAL_DIR}/${FILENAME}")"
    if [[ "$ACTUAL_SHA" != "$MEMORY_EXPECT_SHA" ]]; then
        echo "[delete-memory] REFUSING: ${FILENAME} changed since it was read" >&2
        echo "[delete-memory]   expected ${MEMORY_EXPECT_SHA:0:12}… got ${ACTUAL_SHA:0:12}…" >&2
        exit 1
    fi
fi

# 0a. Remote CAS PRE-CHECK — must happen BEFORE anything local is touched.
# Resolving the remote sha here (rather than inside the delete block below) means a
# remote that moved on, or an unreachable API, aborts with the local canonical file
# still in place. Checking it after the local rm would leave the store showing only
# the merged memory while an updated payout/custody record sat in .trash.
REMOTE_SHA=""
if [[ -n "$REPO" ]]; then
    if RESP=$(gh api "repos/${REPO}/contents/${REMOTE_PATH}/${FILENAME}" 2>/dev/null); then
        REMOTE_SHA=$(echo "$RESP" | jq -r '.sha // empty')
    elif [[ -n "${MEMORY_EXPECT_REMOTE_SHA:-}" ]]; then
        # A caller that asked for remote CAS cannot be served if the API is
        # unreachable — fail closed rather than deleting blind.
        echo "[delete-memory] REFUSING: cannot read remote ${FILENAME} to honour MEMORY_EXPECT_REMOTE_SHA" >&2
        exit 1
    fi
    if [[ -n "${MEMORY_EXPECT_REMOTE_SHA:-}" && "$REMOTE_SHA" != "$MEMORY_EXPECT_REMOTE_SHA" ]]; then
        echo "[delete-memory] REFUSING: remote ${FILENAME} changed since it was read" >&2
        echo "[delete-memory]   expected ${MEMORY_EXPECT_REMOTE_SHA:0:12}… got ${REMOTE_SHA:0:12}…" >&2
        exit 1
    fi
fi

# 0b. Local recoverability snapshot. Remote deletes are git-recoverable ONLY when
# CLAUDE_MEMORY_REPO is set; with no remote (local-first) a plain rm is permanent.
# Always snapshot to .trash/ first so every deletion has a local undo. .trash is a
# subdir, so MEM_DIR.glob("*.md") (non-recursive) never picks these up.
TRASH_DIR="${LOCAL_DIR}/.trash"
if [[ -f "${LOCAL_DIR}/${FILENAME}" ]]; then
    mkdir -p "$TRASH_DIR"
    TS="$(date -u +%Y%m%dT%H%M%SZ)"
    cp -p "${LOCAL_DIR}/${FILENAME}" "${TRASH_DIR}/${TS}-${FILENAME}"
    echo "[delete-memory] Backed up to .trash/${TS}-${FILENAME} (restore: mv it back, then save_memory.sh)"
    find "$TRASH_DIR" -type f -name '*.md' -mtime +90 -delete 2>/dev/null || true
fi

# 1. Remove the local file
if [[ -f "${LOCAL_DIR}/${FILENAME}" ]]; then
    rm -f "${LOCAL_DIR}/${FILENAME}"
    echo "[delete-memory] Removed local ${FILENAME}"
else
    echo "[delete-memory] Local file already absent: ${FILENAME}"
fi

# 2. Delete the remote file (needs its current sha) — ONLY if a remote is
# configured. With no CLAUDE_MEMORY_REPO the unguarded `gh api "repos//contents/…"`
# PUT/DELETE aborts the whole script under `set -e`, so the index strip + vector
# cleanup below never run. Local-first: skip cleanly.
if [[ -n "$REPO" ]]; then
    # REMOTE_SHA was resolved and CAS-checked in step 0a, before anything local was
    # touched. Deleting at that exact sha means GitHub itself rejects the call if the
    # blob moved in between, so the remote delete is conditional end to end.
    if [[ -n "$REMOTE_SHA" ]]; then
        gh api "repos/${REPO}/contents/${REMOTE_PATH}/${FILENAME}" \
            --method DELETE \
            --field message="delete memory: ${FILENAME}" \
            --field sha="$REMOTE_SHA" > /dev/null
        echo "[delete-memory] Removed remote ${FILENAME}"
    else
        echo "[delete-memory] Remote file already absent: ${FILENAME}"
    fi
fi

# 3. Strip the MEMORY.md index line using the LOCKED helper (a raw
# `grep -v > tmp && mv` here races the pipeline's index writers — the exact
# clobber memory_lib.sh documents fixing), then push via memory_push_index
# (which no-ops when no remote is set).
MEMORY_MD="${LOCAL_DIR}/MEMORY.md"
if memory_index_remove_line "$MEMORY_MD" "$FILENAME"; then
    memory_push_index "update MEMORY.md: remove ${FILENAME}"
    echo "[delete-memory] Updated MEMORY.md index"
else
    echo "[delete-memory] No MEMORY.md index line for ${FILENAME}"
fi

# Optional vector index: best-effort, non-blocking removal of this file's point
# from Qdrant (no-op unless installed with --vector and enabled).
memory_vector_sync --delete "$FILENAME"
