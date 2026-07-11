#!/usr/bin/env bash
# memory_reindex.sh — reconcile the memory store with its MEMORY.md index.
#
# Adds index lines for files missing from the index (under the auto section,
# description pulled from each file's frontmatter), and reports — or with
# --prune-orphans, removes — index lines that point to files no longer on disk.
#
# Usage:
#   memory_reindex.sh                       # dry-run: report drift only
#   memory_reindex.sh --apply               # add missing entries (local + push)
#   memory_reindex.sh --apply --prune-orphans  # also strip dead index lines
#
# Read-only by default. Never fabricates content; only repairs the index.

set -uo pipefail
source "${HOME}/.claude/memory_lib.sh"

APPLY=0; PRUNE=0; REBUILD=0
for a in "$@"; do
    case "$a" in
        --apply) APPLY=1 ;;
        --prune-orphans) PRUNE=1 ;;
        --rebuild) REBUILD=1 ;;
    esac
done

# --rebuild: regenerate the WHOLE index deterministically from frontmatter under
# a byte budget (memory_index_build.py), instead of incrementally patching drift.
# This is the durable fix for the flat-index overflow — nothing is ever orphaned
# and the file stays under the session load limit. Pushes the regenerated index.
if [[ "$REBUILD" == "1" ]]; then
    BUILD="${HOME}/.claude/memory_index_build.py"
    [[ -f "$BUILD" ]] || BUILD="$(dirname "$0")/memory_index_build.py"
    if [[ "$APPLY" == "1" ]]; then
        python3 "$BUILD" --write && memory_push_index "rebuild MEMORY.md (deterministic)"
    else
        echo "[reindex] --rebuild dry-run (add --apply to write). Preview:" >&2
        python3 "$BUILD"
    fi
    exit $?
fi

DIR="$(memory_dir)"
INDEX="$(memory_index)"
[[ -d "$DIR" ]]   || { echo "[reindex] no memory dir at $DIR" >&2; exit 1; }
[[ -f "$INDEX" ]] || { echo "[reindex] no MEMORY.md at $INDEX" >&2; exit 1; }

# 1. files on disk that are missing from the index
missing=()
for f in "$DIR"/*.md; do
    [[ -f "$f" ]] || continue
    name=$(basename "$f")
    [[ "$name" == "MEMORY.md" ]] && continue
    grep -qF "](${name})" "$INDEX" || missing+=("$name")
done

# 2. index lines whose target file is gone (orphans)
orphans=()
while IFS= read -r tgt; do
    [[ -z "$tgt" ]] && continue
    [[ -f "${DIR}/${tgt}" ]] || orphans+=("$tgt")
done < <(grep -oE '\]\([a-zA-Z0-9_.-]+\.md\)' "$INDEX" | sed -E 's/^\]\(//; s/\)$//' | sort -u)

echo "# Reindex — $DIR"
echo "missing from index: ${#missing[@]}"
(( ${#missing[@]} )) && printf '  - %s\n' "${missing[@]}"
echo "orphan index lines: ${#orphans[@]}"
(( ${#orphans[@]} )) && printf '  - %s\n' "${orphans[@]}"

if [[ "$APPLY" != "1" ]]; then
    echo "(dry-run — re-run with --apply to add missing entries)"
    exit 0
fi

changed=0

# Add missing entries (description from each file's own frontmatter).
for name in "${missing[@]+"${missing[@]}"}"; do
    desc=$(memory_frontmatter_field "${DIR}/${name}" description)
    [[ -z "$desc" ]] && desc="(no description — review)"
    if memory_index_add_line "$INDEX" "$name" "$desc"; then
        echo "[reindex] indexed $name"
        changed=1
    fi
done

# Prune dead index lines if asked.
if [[ "$PRUNE" == "1" && ${#orphans[@]} -gt 0 ]]; then
    for tgt in "${orphans[@]}"; do
        if memory_index_remove_line "$INDEX" "$tgt"; then
            echo "[reindex] pruned dead index line: $tgt"
            changed=1
        fi
    done
fi

if [[ "$changed" == "1" ]]; then
    memory_push_index "reindex: reconcile MEMORY.md with store"
    echo "[reindex] pushed updated index"
else
    echo "[reindex] nothing to change"
fi
