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

CONTENT=$(cat)
if [[ -z "$CONTENT" ]]; then
    echo "[save-content] ERROR: no content provided via stdin" >&2
    exit 1
fi

printf '%s\n' "$CONTENT" > "${LOCAL_DIR}/${FILENAME}"

# Push via the shared retry-on-sha-conflict helper (concurrent writers to the
# same remote path otherwise 409 and silently drop the update).
if _gh_put_file "$REPO" "${REMOTE_PATH}/${FILENAME}" "reformat memory: ${FILENAME}" "${LOCAL_DIR}/${FILENAME}"; then
    echo "[save-content] Pushed ${FILENAME} (MEMORY.md index untouched)"
else
    echo "[save-content] ERROR pushing ${FILENAME}" >&2
    exit 1
fi
