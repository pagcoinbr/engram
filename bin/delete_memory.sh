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
