#!/usr/bin/env bash
# test_install_store_slug.sh — install.sh must resolve ONE canonical store and must
# not destroy operator pins in engram.env across re-installs.
#
# Regressions covered:
#   1. `--storage github` truncated engram.env with `>`, dropping CLAUDE_MEMORY_SLUG.
#   2. `--storage local` deleted engram.env outright, same effect.
#   3. SLUG was computed AFTER the vector rebuild, so the rebuild (and any child
#      process) targeted the $HOME-derived store even when a pin said otherwise —
#      silently building a near-empty index.
set -uo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
fails=0
ok()   { printf '  ✅ %s\n' "$1"; }
bad()  { printf '  ❌ %s\n' "$1"; fails=$((fails + 1)); }

run_install() {   # run_install <home> <args...>
    local h="$1"; shift
    HOME="$h" ENGRAM_CLAUDE_HOME="$h/.claude" \
        "$REPO/install.sh" --no-graph --no-vector --daemon none "$@" >"$h/install.log" 2>&1
}

# ── 1. a CLAUDE_MEMORY_SLUG pin survives a --storage github (re)install ────────
T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
mkdir -p "$T/.claude"
printf 'export CLAUDE_MEMORY_SLUG=-pinned-store\n' > "$T/.claude/engram.env"
run_install "$T" --backend claude --storage github --repo owner/name --yes
if grep -q 'CLAUDE_MEMORY_SLUG=-pinned-store' "$T/.claude/engram.env"; then
    ok "github install preserves an existing CLAUDE_MEMORY_SLUG pin"
else
    bad "github install DROPPED the CLAUDE_MEMORY_SLUG pin"; cat "$T/.claude/engram.env"
fi
grep -q 'CLAUDE_MEMORY_REPO=owner/name' "$T/.claude/engram.env" \
    && ok "github install still writes CLAUDE_MEMORY_REPO" \
    || bad "CLAUDE_MEMORY_REPO missing"

# the pinned store — not the \$HOME-derived one — is what gets created/seeded
[[ -d "$T/.claude/projects/-pinned-store/memory" ]] \
    && ok "seeded/created the PINNED store" \
    || bad "pinned store not created (install used the \$HOME-derived slug)"

# ── 2. the pin survives a --storage local (re)install ─────────────────────────
T2="$(mktemp -d)"; trap 'rm -rf "$T" "$T2"' EXIT
mkdir -p "$T2/.claude"
printf 'export CLAUDE_MEMORY_SLUG=-pinned-store\nexport CLAUDE_MEMORY_REPO=old/remote\n' \
    > "$T2/.claude/engram.env"
run_install "$T2" --backend claude --storage local --yes
if [[ -f "$T2/.claude/engram.env" ]] && grep -q 'CLAUDE_MEMORY_SLUG=-pinned-store' "$T2/.claude/engram.env"; then
    ok "local install keeps the pin (file not deleted)"
else
    bad "local install DELETED engram.env and the pin with it"
fi
grep -q 'CLAUDE_MEMORY_REPO' "$T2/.claude/engram.env" 2>/dev/null \
    && bad "local install should have dropped CLAUDE_MEMORY_REPO" \
    || ok "local install drops only CLAUDE_MEMORY_REPO"

# ── 3. no pin anywhere -> unchanged \$HOME-derived default, no stray file ─────
T3="$(mktemp -d)"; trap 'rm -rf "$T" "$T2" "$T3"' EXIT
run_install "$T3" --backend claude --storage local --yes
DEFAULT_SLUG="$(printf '%s' "$T3" | sed 's|/|-|g')"
[[ -d "$T3/.claude/projects/$DEFAULT_SLUG/memory" ]] \
    && ok "no pin -> \$HOME-derived slug (behaviour unchanged)" \
    || bad "default slug store missing: $DEFAULT_SLUG"
[[ -f "$T3/.claude/engram.env" ]] \
    && bad "local install with no pins should leave no engram.env" \
    || ok "no pins + local -> no engram.env left behind"

echo "── test_install_store_slug: $([[ $fails -eq 0 ]] && echo PASS || echo "FAIL ($fails)") ──"
exit $((fails > 0))
