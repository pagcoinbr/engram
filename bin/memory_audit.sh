#!/usr/bin/env bash
# memory_audit.sh — non-destructive detector for memory files that may need
# review. Outputs a plain-text report grouped by issue type. Read-only.
#
# Usage:
#   memory_audit.sh                    # full report
#   memory_audit.sh --filenames-only   # one filename per line (for piping)
#   memory_audit.sh --json             # JSON report
#
# Resolves the canonical store via memory_lib.sh (independent of $PWD).

set -eo pipefail
source "${HOME}/.claude/memory_lib.sh"

MODE="${1:-report}"

DIR="$(memory_dir)"
INDEX="$(memory_index)"

if [[ ! -d "$DIR" ]]; then
    echo "[memory-audit] no memory dir at $DIR" >&2
    exit 1
fi

FILES_ORPHAN_INDEX=()
FILES_MISSING_INDEX=()
FILES_TINY=()
FILES_STALE_MARKED=()
FILES_OLD_MTIME=()
FILES_DUPLICATE_DESC=()
FILES_MALFORMED_FM=()
FILES_BROKEN_LINKS=()
FILES_DUP_DESC_NORM=()
CLUSTER_CANDIDATES=()

NOW=$(date +%s)
SIX_MONTHS=$((180 * 24 * 3600))

# --- 1. index drift ---
if [[ -f "$INDEX" ]]; then
    while IFS= read -r f; do
        [[ -z "$f" ]] && continue
        if [[ ! -f "${DIR}/${f}" ]]; then
            FILES_ORPHAN_INDEX+=("$f")
        fi
    done < <(grep -oE '\([a-zA-Z0-9_.-]+\.md\)' "$INDEX" | tr -d '()' | sort -u || true)

    for f in "$DIR"/*.md; do
        [[ -f "$f" ]] || continue
        name=$(basename "$f")
        [[ "$name" == "MEMORY.md" ]] && continue
        if ! grep -qF "](${name})" "$INDEX"; then
            FILES_MISSING_INDEX+=("$name")
        fi
    done

    # exact duplicate description lines (everything after " — ")
    while IFS= read -r dup_line; do
        [[ -z "$dup_line" ]] && continue
        FILES_DUPLICATE_DESC+=("$dup_line")
    done < <(grep -oE ' — .*$' "$INDEX" | sort | uniq -d || true)
fi

# --- 2. per-file checks ---
declare -A CLUSTER_COUNT

# pre-pass: collect known memory identities (filename stems + name slugs),
# normalized to lowercase with '-' and '_' folded, so [[wiki-links]] that point
# at a memory's name slug (the store's convention) validate correctly.
declare -A NAME_SET
for f in "$DIR"/*.md; do
    [[ -f "$f" ]] || continue
    nm=$(basename "$f"); [[ "$nm" == "MEMORY.md" ]] && continue
    NAME_SET["$(printf '%s' "${nm%.md}" | tr 'A-Z' 'a-z' | tr '-' '_')"]=1
    ns=$(memory_frontmatter_field "$f" name)
    if [[ -n "$ns" ]]; then
        NAME_SET["$(printf '%s' "$ns" | tr 'A-Z' 'a-z' | tr '-' '_')"]=1
    fi
done

for f in "$DIR"/*.md; do
    [[ -f "$f" ]] || continue
    name=$(basename "$f")
    [[ "$name" == "MEMORY.md" ]] && continue

    size=$(stat -c %s "$f")
    if (( size < 200 )); then
        FILES_TINY+=("$name ($size bytes)")
    fi

    if grep -qE '\b(STALE|DEPRECATED|OBSOLETE)\b' "$f"; then
        FILES_STALE_MARKED+=("$name")
    fi

    mtime=$(stat -c %Y "$f")
    age=$((NOW - mtime))
    if (( age > SIX_MONTHS )); then
        days=$((age / 86400))
        FILES_OLD_MTIME+=("$name (${days}d)")
    fi

    # malformed frontmatter: missing name / description (top-level in both
    # conventions) or type (top-level OR nested under metadata:)
    miss=""
    for field in name description; do
        [[ -n "$(memory_frontmatter_field "$f" "$field")" ]] || miss="${miss}${miss:+,}${field}"
    done
    [[ -n "$(memory_frontmatter_type "$f")" ]] || miss="${miss}${miss:+,}type"
    [[ -n "$miss" ]] && FILES_MALFORMED_FM+=("$name (missing: $miss)")

    # broken [[wiki-links]] — ref must match a known name slug or filename stem
    # (folding '-'/'_' and case). Only genuinely-unresolvable links are flagged.
    while IFS= read -r ref; do
        [[ -z "$ref" ]] && continue
        refkey="$(printf '%s' "$ref" | tr 'A-Z' 'a-z' | tr '-' '_')"
        [[ -n "${NAME_SET[$refkey]:-}" ]] || FILES_BROKEN_LINKS+=("$name -> [[${ref}]]")
    done < <(grep -oE '\[\[[a-zA-Z0-9_.-]+\]\]' "$f" 2>/dev/null | sed -E 's/^\[\[//; s/\]\]$//' | sort -u || true)

    # prefix-cluster key = first two underscore tokens (curate candidates)
    stem="${name%.md}"
    IFS='_' read -r t1 t2 _rest <<< "$stem"
    key="$t1"; [[ -n "$t2" ]] && key="${t1}_${t2}"
    CLUSTER_COUNT["$key"]=$(( ${CLUSTER_COUNT["$key"]:-0} + 1 ))
done

# clusters with 3+ members are consolidation candidates
for k in "${!CLUSTER_COUNT[@]}"; do
    (( ${CLUSTER_COUNT[$k]} >= 3 )) && CLUSTER_CANDIDATES+=("${k}_* (${CLUSTER_COUNT[$k]} memories)")
done
IFS=$'\n' CLUSTER_CANDIDATES=($(sort <<<"${CLUSTER_CANDIDATES[*]}")); unset IFS

# normalized duplicate descriptions across files (lowercased, ws-collapsed)
while IFS= read -r d; do
    [[ -z "$d" ]] && continue
    FILES_DUP_DESC_NORM+=("$d")
done < <(
    for f in "$DIR"/*.md; do
        name=$(basename "$f"); [[ "$name" == "MEMORY.md" ]] && continue
        memory_frontmatter_field "$f" description | tr 'A-Z' 'a-z' | sed -E 's/[[:space:]]+/ /g; s/^ //; s/ $//'
    done | sort | uniq -d || true
)

emit_section() {
    local title="$1"; shift
    local count=$#
    if (( count == 0 )); then return; fi
    echo
    echo "## $title ($count)"
    printf -- '- %s\n' "$@"
}

if [[ "$MODE" == "--filenames-only" ]]; then
    {
        (( ${#FILES_ORPHAN_INDEX[@]} ))  && printf '%s\n' "${FILES_ORPHAN_INDEX[@]}"
        (( ${#FILES_MISSING_INDEX[@]} )) && printf '%s\n' "${FILES_MISSING_INDEX[@]}"
        (( ${#FILES_TINY[@]} ))          && for x in "${FILES_TINY[@]}";          do echo "${x%% *}"; done
        (( ${#FILES_STALE_MARKED[@]} ))  && printf '%s\n' "${FILES_STALE_MARKED[@]}"
        (( ${#FILES_OLD_MTIME[@]} ))     && for x in "${FILES_OLD_MTIME[@]}";     do echo "${x%% *}"; done
        (( ${#FILES_MALFORMED_FM[@]} ))  && for x in "${FILES_MALFORMED_FM[@]}";  do echo "${x%% *}"; done
        (( ${#FILES_BROKEN_LINKS[@]} ))  && for x in "${FILES_BROKEN_LINKS[@]}";  do echo "${x%% *}"; done
        true
    } | grep -v '^$' | sort -u
    exit 0
fi

if [[ "$MODE" == "--json" ]]; then
    jq -n \
        --argjson orphan_index    "$(printf '%s\n' "${FILES_ORPHAN_INDEX[@]+"${FILES_ORPHAN_INDEX[@]}"}"     | jq -R . | jq -s .)" \
        --argjson missing_index   "$(printf '%s\n' "${FILES_MISSING_INDEX[@]+"${FILES_MISSING_INDEX[@]}"}"   | jq -R . | jq -s .)" \
        --argjson tiny            "$(printf '%s\n' "${FILES_TINY[@]+"${FILES_TINY[@]}"}"                     | jq -R . | jq -s .)" \
        --argjson stale_marked    "$(printf '%s\n' "${FILES_STALE_MARKED[@]+"${FILES_STALE_MARKED[@]}"}"     | jq -R . | jq -s .)" \
        --argjson old_mtime       "$(printf '%s\n' "${FILES_OLD_MTIME[@]+"${FILES_OLD_MTIME[@]}"}"           | jq -R . | jq -s .)" \
        --argjson duplicate_desc  "$(printf '%s\n' "${FILES_DUPLICATE_DESC[@]+"${FILES_DUPLICATE_DESC[@]}"}" | jq -R . | jq -s .)" \
        --argjson malformed_fm    "$(printf '%s\n' "${FILES_MALFORMED_FM[@]+"${FILES_MALFORMED_FM[@]}"}"     | jq -R . | jq -s .)" \
        --argjson broken_links    "$(printf '%s\n' "${FILES_BROKEN_LINKS[@]+"${FILES_BROKEN_LINKS[@]}"}"     | jq -R . | jq -s .)" \
        --argjson dup_desc_norm   "$(printf '%s\n' "${FILES_DUP_DESC_NORM[@]+"${FILES_DUP_DESC_NORM[@]}"}"   | jq -R . | jq -s .)" \
        --argjson clusters        "$(printf '%s\n' "${CLUSTER_CANDIDATES[@]+"${CLUSTER_CANDIDATES[@]}"}"     | jq -R . | jq -s .)" \
        '{orphan_index:$orphan_index, missing_index:$missing_index, tiny:$tiny, stale_marked:$stale_marked, old_mtime:$old_mtime, duplicate_descriptions:$duplicate_desc, malformed_frontmatter:$malformed_fm, broken_links:$broken_links, duplicate_descriptions_normalized:$dup_desc_norm, cluster_candidates:$clusters} | map_values(map(select(. != "")))'
    exit 0
fi

# --- report mode (default) ---
echo "# Memory audit — $DIR"
echo "Generated: $(date -Iseconds)"
emit_section "Orphan index entries (in MEMORY.md but file missing)"        "${FILES_ORPHAN_INDEX[@]+"${FILES_ORPHAN_INDEX[@]}"}"
emit_section "Missing from MEMORY.md (file exists but not indexed)"        "${FILES_MISSING_INDEX[@]+"${FILES_MISSING_INDEX[@]}"}"
emit_section "Malformed frontmatter (missing name/description/type)"       "${FILES_MALFORMED_FM[@]+"${FILES_MALFORMED_FM[@]}"}"
emit_section "Broken [[wiki-links]] (target file absent)"                  "${FILES_BROKEN_LINKS[@]+"${FILES_BROKEN_LINKS[@]}"}"
emit_section "Tiny files (<200 bytes — likely placeholder)"                "${FILES_TINY[@]+"${FILES_TINY[@]}"}"
emit_section "Marked STALE/DEPRECATED/OBSOLETE in body"                    "${FILES_STALE_MARKED[@]+"${FILES_STALE_MARKED[@]}"}"
emit_section "Old mtime (>180 days unchanged)"                             "${FILES_OLD_MTIME[@]+"${FILES_OLD_MTIME[@]}"}"
emit_section "Duplicate description lines in MEMORY.md (exact)"            "${FILES_DUPLICATE_DESC[@]+"${FILES_DUPLICATE_DESC[@]}"}"
emit_section "Duplicate descriptions across files (normalized)"            "${FILES_DUP_DESC_NORM[@]+"${FILES_DUP_DESC_NORM[@]}"}"
emit_section "Cluster candidates for /memory-curate (3+ shared prefix)"    "${CLUSTER_CANDIDATES[@]+"${CLUSTER_CANDIDATES[@]}"}"

TOTAL=$(( ${#FILES_ORPHAN_INDEX[@]} + ${#FILES_MISSING_INDEX[@]} + ${#FILES_TINY[@]} + ${#FILES_STALE_MARKED[@]} + ${#FILES_OLD_MTIME[@]} + ${#FILES_DUPLICATE_DESC[@]} + ${#FILES_MALFORMED_FM[@]} + ${#FILES_BROKEN_LINKS[@]} + ${#FILES_DUP_DESC_NORM[@]} ))
echo
echo "Total flagged (excl. cluster candidates): $TOTAL"
echo "Cluster candidates: ${#CLUSTER_CANDIDATES[@]}"
