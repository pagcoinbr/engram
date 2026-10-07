#!/usr/bin/env bash
# update.sh — bring an existing engram install up to date. Dry-run by default.
#
#   ./update.sh                  # show what would change; touches nothing
#   ./update.sh --apply          # pull, reinstall, restore overrides, rebuild, verify
#   ./update.sh --apply --daemon none -- --no-graph   # args after -- go to install.sh
#
# install.sh already is the idempotent installer/updater (it preserves engram.yaml).
# This wraps the steps it does not cover, each of which bit a real update:
#   1. pull safely: fast-forward only, clean tree, default branch only;
#   2. re-run install.sh with the daemon mode the box ALREADY uses (detected), so a
#      manual-only box (engram.timer off) is not switched back to a scheduled one;
#   3. restore local overrides: every file under ~/.claude/engram-local-overrides/
#      is copied back over ~/.claude after the install, which otherwise overwrites
#      hand-customised files (e.g. commands/*.md) with the repo's copies;
#   4. rebuild + restart the Atlas web UI and restart the Rust API when they are
#      installed (install.sh knows neither);
#   5. verify at the user layer: the registered recall hook still injects memories,
#      the Atlas still answers. Exit 1 if not, with the rollback command.
set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
CLAUDE="${ENGRAM_CLAUDE_HOME:-$HOME/.claude}"
INSTALL="${ENGRAM_INSTALL_CMD:-$REPO/install.sh}"   # overridable for tests only
OVERRIDES="$CLAUDE/engram-local-overrides"
APPLY=0; DAEMON=""; INSTALL_ARGS=()
while [[ $# -gt 0 ]]; do case "$1" in
  --apply) APPLY=1; shift;;
  --daemon) DAEMON="$2"; shift 2;;
  --) shift; INSTALL_ARGS=("$@"); break;;
  -h|--help) sed -n '2,20p' "$0"; exit 0;;
  *) echo "unknown argument: $1 (see --help)" >&2; exit 2;;
esac; done

say()  { printf '\033[1;36m[update]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[update] warning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[update] error:\033[0m %s\n' "$*" >&2; exit 1; }
user_unit() { systemctl --user cat "$1" >/dev/null 2>&1; }

cd "$REPO"
# ── 1. safe pull ──────────────────────────────────────────────────────────────
DEFAULT="$(git symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null | sed 's|^origin/||')"
DEFAULT="${DEFAULT:-main}"
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[[ "$BRANCH" == "$DEFAULT" ]] \
  || die "on '$BRANCH', not '$DEFAULT': updates deploy the default branch only (git switch $DEFAULT)"
git diff --quiet && git diff --cached --quiet \
  || die "uncommitted changes in $REPO; commit or stash them first (an update must not mix them in)"
git fetch -q origin "$DEFAULT"
OLD="$(git rev-parse HEAD)"
NEW="$(git rev-parse "origin/$DEFAULT")"
git merge-base --is-ancestor "$OLD" "$NEW" \
  || die "local $DEFAULT has commits that origin/$DEFAULT lacks; refusing to update a diverged tree"

if [[ "$OLD" == "$NEW" ]]; then
  say "already at origin/$DEFAULT (${OLD:0:7}); reinstall + verify only"
else
  say "$(git rev-list --count "$OLD..$NEW") new commit(s) on origin/$DEFAULT:"
  git log --oneline --no-decorate "$OLD..$NEW" | sed 's/^/    /'
fi

# ── 2. daemon mode the box already uses ────────────────────────────────────────
if [[ -z "$DAEMON" ]]; then
  if systemctl --user is-enabled engram.timer >/dev/null 2>&1; then DAEMON=systemd; else DAEMON=none; fi
fi
say "install.sh --yes --daemon $DAEMON ${INSTALL_ARGS[*]:-}"

# ── 3/4. what else will happen ─────────────────────────────────────────────────
OVR_FILES=()
[[ -d "$OVERRIDES" ]] && mapfile -t OVR_FILES < <(cd "$OVERRIDES" && find . -type f | sed 's|^\./||' | sort)
((${#OVR_FILES[@]})) && say "local overrides to restore after install: ${OVR_FILES[*]}"
ATLAS=0; user_unit engram-atlas.service && ATLAS=1
ATLAS_BUILD=0
if ((ATLAS)); then
  if [[ ! -f "$REPO/atlas/dist/index.html" ]] || ! git diff --quiet "$OLD" "$NEW" -- atlas/; then ATLAS_BUILD=1; fi
  say "Atlas: $( ((ATLAS_BUILD)) && echo 'rebuild + restart' || echo 'restart (frontend unchanged)')"
fi
API=0; systemctl --user is-active --quiet engram-api.service 2>/dev/null && API=1
((API)) && say "Rust API (engram-api.service): restart"

if ((!APPLY)); then
  say "dry-run: nothing changed. Re-run with --apply."
  exit 0
fi

# ── apply ─────────────────────────────────────────────────────────────────────
[[ "$OLD" != "$NEW" ]] && git merge -q --ff-only "$NEW"
"$INSTALL" --yes --daemon "$DAEMON" "${INSTALL_ARGS[@]}"

if ((${#OVR_FILES[@]})); then
  cp -a "$OVERRIDES/." "$CLAUDE/"
  say "restored ${#OVR_FILES[@]} local override(s)"
fi

if ((ATLAS)); then
  if ((ATLAS_BUILD)); then
    # npm ci, not install: package.json pins "latest", only the lockfile is reproducible.
    (cd "$REPO/atlas" && npm ci --ignore-scripts --no-audit --no-fund >/dev/null && npm run build >/dev/null) \
      || die "Atlas build failed; the running Atlas still serves the previous dist/"
  fi
  "$CLAUDE/graph/venv/bin/python" -c 'import fastapi' 2>/dev/null \
    || "$CLAUDE/graph/venv/bin/python" -m pip install -q fastapi
  systemctl --user restart engram-atlas.service
fi
((API)) && systemctl --user restart engram-api.service

# ── 5. verify at the user layer ────────────────────────────────────────────────
fail=0
HOOK="$(jq -r '.hooks.UserPromptSubmit[]?.hooks[]?.command' "$CLAUDE/settings.json" 2>/dev/null \
        | grep -E 'engram-recall-hook|memory-recall-inject' | head -1 || true)"
if [[ -z "$HOOK" ]]; then
  warn "no engram recall hook registered in settings.json"; fail=1
else
  SID="update-probe-$(date +%s%N)"
  OUT="$(printf '{"prompt":"how is the engram memory recall and graph configured on this machine","session_id":"%s","cwd":"%s"}' \
         "$SID" "$HOME" | timeout 30 "$HOOK" 2>/dev/null || true)"
  rm -f "$CLAUDE"/logs/*recall-inject/"$SID".json
  MEMS="$(sed '/Graph facts/,$d' <<<"$OUT" | grep -c '^- ' || true)"
  FACTS="$(sed -n '/Graph facts/,$p' <<<"$OUT" | grep -c '^- ' || true)"
  if ((MEMS > 0)); then say "recall hook ($(basename "$HOOK")): $MEMS memories, $FACTS graph facts"
  else warn "recall hook ($HOOK) injected NOTHING for a probe prompt"; fail=1; fi
fi
if ((ATLAS)); then
  for _ in $(seq 1 30); do curl -sf -o /dev/null -m 2 http://127.0.0.1:8765/login && break; sleep 1; done
  if curl -sf -o /dev/null -m 2 http://127.0.0.1:8765/login; then say "Atlas: up on 127.0.0.1:8765"
  else warn "Atlas did not come back (journalctl --user -u engram-atlas)"; fail=1; fi
fi

if ((fail)); then
  # install.sh directly: update.sh would just pull the new commits again
  warn "update finished with problems. Roll back: git -C $REPO reset --hard ${OLD:0:12} && $INSTALL --yes --daemon $DAEMON"
  exit 1
fi
say "updated ${OLD:0:7} -> $(git rev-parse --short HEAD)"
