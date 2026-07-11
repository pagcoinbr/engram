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

# 0. Local recoverability snapshot. Remote deletes are git-recoverable ONLY when
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
    REMOTE_SHA=""
    if RESP=$(gh api "repos/${REPO}/contents/${REMOTE_PATH}/${FILENAME}" 2>/dev/null); then
        REMOTE_SHA=$(echo "$RESP" | jq -r '.sha // empty')
    fi
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
