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

# 2. Delete the remote file (needs its current sha)
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

# 3. Strip the MEMORY.md index line (match the link target exactly)
MEMORY_MD="${LOCAL_DIR}/MEMORY.md"
if [[ -f "$MEMORY_MD" ]] && grep -qF "](${FILENAME})" "$MEMORY_MD"; then
    grep -vF "](${FILENAME})" "$MEMORY_MD" > "${MEMORY_MD}.tmp"
    mv "${MEMORY_MD}.tmp" "$MEMORY_MD"

    MEMORY_SHA=""
    if MEM_RESP=$(gh api "repos/${REPO}/contents/${REMOTE_PATH}/MEMORY.md" 2>/dev/null); then
        MEMORY_SHA=$(echo "$MEM_RESP" | jq -r '.sha // empty')
    fi
    MEMORY_ENCODED=$(base64 -w 0 < "$MEMORY_MD")

    gh api "repos/${REPO}/contents/${REMOTE_PATH}/MEMORY.md" \
        --method PUT \
        --field message="update MEMORY.md: remove ${FILENAME}" \
        --field content="$MEMORY_ENCODED" \
        --field sha="$MEMORY_SHA" \
        --jq '.content.name' > /dev/null

    echo "[delete-memory] Updated MEMORY.md index"
else
    echo "[delete-memory] No MEMORY.md index line for ${FILENAME}"
fi

# Optional vector index: best-effort, non-blocking removal of this file's point
# from Qdrant (no-op unless installed with --vector and enabled).
memory_vector_sync --delete "$FILENAME"
