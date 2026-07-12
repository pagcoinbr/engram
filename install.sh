#!/usr/bin/env bash
# install.sh — install engram into ~/.claude. Idempotent + re-runnable.
#
# Interactive by default; non-interactive with flags:
#   --backend ollama|claude     --tier cpu|small|medium|large   --ollama-host URL
#   --storage local|github      --repo owner/name (implies github)
#   --daemon none|systemd|docker
#   --graph | --no-graph        (build the Neo4j graph venv + register the MCP server)
#   --vector | --no-vector      (build the Qdrant vector venv + register the MCP server)
#   --yes                       (accept defaults, no prompts)
#
# What it does: copies the engine into ~/.claude, writes engram.yaml, merges the
# Stop/SessionStart hooks into settings.json, optionally builds the graph venv +
# registers the recall MCP server, seeds synthetic examples (only if the store is
# empty), and sets up the chosen daemon. Apply-gates ship OFF (dry-run).
set -eo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLAUDE="${ENGRAM_CLAUDE_HOME:-$HOME/.claude}"
SETTINGS="$CLAUDE/settings.json"

BACKEND=""; TIER="small"; OLLAMA_HOST="http://localhost:11434"
STORAGE="local"; REPO_REMOTE=""; DAEMON="none"; YES=0; WANT_GRAPH="auto"; WANT_VECTOR="auto"

while [[ $# -gt 0 ]]; do case "$1" in
  --backend) BACKEND="$2"; shift 2;;
  --tier) TIER="$2"; shift 2;;
  --ollama-host) OLLAMA_HOST="$2"; shift 2;;
  --storage) STORAGE="$2"; shift 2;;
  --repo) REPO_REMOTE="$2"; STORAGE="github"; shift 2;;
  --daemon) DAEMON="$2"; shift 2;;
  --graph) WANT_GRAPH="yes"; shift;;
  --no-graph) WANT_GRAPH="no"; shift;;
  --vector) WANT_VECTOR="yes"; shift;;
  --no-vector) WANT_VECTOR="no"; shift;;
  --yes|-y) YES=1; shift;;
  -h|--help) sed -n '2,18p' "$0"; exit 0;;
  *) echo "unknown arg: $1" >&2; exit 2;;
esac; done

say()  { printf '\033[1;36m[engram]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[engram] warning:\033[0m %s\n' "$*"; }
ask()  { local __v="$1" __p="$2" __d="$3" __a; if [[ "$YES" == 1 || ! -t 0 ]]; then printf -v "$__v" '%s' "${!__v:-$__d}"; return; fi; read -r -p "$__p [$__d]: " __a || true; printf -v "$__v" '%s' "${__a:-${!__v:-$__d}}"; }

# ---- prereqs ----
command -v python3 >/dev/null || { echo "python3 required" >&2; exit 1; }
command -v jq >/dev/null || warn "jq not found — settings.json hook merge will be skipped (install jq + re-run)"

# ---- choices ----
[[ -z "$BACKEND" ]] && ask BACKEND "LLM backend (ollama=GPU / claude=no-GPU)" "ollama"
if [[ "$BACKEND" == ollama ]]; then
  ask TIER "Ollama hardware tier (cpu/small/medium/large)" "$TIER"
  ask OLLAMA_HOST "Ollama host URL" "$OLLAMA_HOST"
fi
ask STORAGE "Storage (local / github)" "$STORAGE"
[[ "$STORAGE" == github && -z "$REPO_REMOTE" ]] && ask REPO_REMOTE "GitHub memory repo (owner/name)" ""
ask DAEMON "24h daemon (none / systemd / docker)" "$DAEMON"
if [[ "$WANT_GRAPH" == auto ]]; then WANT_GRAPH="yes"; ask WANT_GRAPH "Build the Neo4j graph (yes/no)" "yes"; fi
if [[ "$WANT_VECTOR" == auto ]]; then WANT_VECTOR="no"; ask WANT_VECTOR "Build the Qdrant vector index (yes/no)" "no"; fi
say "backend=$BACKEND tier=$TIER storage=$STORAGE daemon=$DAEMON graph=$WANT_GRAPH vector=$WANT_VECTOR"

# ---- place files ----
mkdir -p "$CLAUDE/commands" "$CLAUDE/graph" "$CLAUDE/vector" "$CLAUDE/logs"
install -m 0755 "$REPO"/bin/*.sh "$CLAUDE"/ 2>/dev/null || true
install -m 0755 "$REPO"/bin/*.py "$CLAUDE"/ 2>/dev/null || true
install -m 0644 "$REPO"/commands/*.md "$CLAUDE"/commands/ 2>/dev/null || true
for f in "$REPO"/graph/*.py "$REPO"/graph/*.md "$REPO"/graph/docker-compose.yml; do [[ -e "$f" ]] && install -m 0644 "$f" "$CLAUDE/graph/"; done
for f in "$REPO"/vector/*.py "$REPO"/vector/docker-compose.yml; do [[ -e "$f" ]] && install -m 0644 "$f" "$CLAUDE/vector/"; done
install -m 0755 "$REPO"/daemon/engram-daemon.py "$CLAUDE"/ 2>/dev/null || true
mkdir -p "$CLAUDE/ui"; [[ -f "$REPO/ui/index.html" ]] && install -m 0644 "$REPO/ui/index.html" "$CLAUDE/ui/"
say "engine installed into $CLAUDE (GUI: run engram-ui.sh)"

# ---- engram.yaml ----
if [[ -f "$CLAUDE/engram.yaml" ]]; then
  say "engram.yaml exists — preserving it (edit by hand to change backend/tier)"
else
  sed -e "s|^backend: .*|backend: $BACKEND|" \
      -e "s|^tier: .*|tier: $TIER|" \
      -e "s|host: \"http://localhost:11434\"|host: \"$OLLAMA_HOST\"|" \
      "$REPO/engram.yaml.example" > "$CLAUDE/engram.yaml"
  # Flip the optional vector store on only when --vector was chosen: rewrite the
  # FIRST `enabled:` line inside the vector_store: block (robust to spacing).
  if [[ "$WANT_VECTOR" == yes ]]; then
    awk '/^vector_store:/{inv=1} inv && /^[[:space:]]+enabled:/{sub(/enabled:[[:space:]]*false/,"enabled: true"); inv=0} {print}' \
        "$CLAUDE/engram.yaml" > "$CLAUDE/engram.yaml.tmp" && mv "$CLAUDE/engram.yaml.tmp" "$CLAUDE/engram.yaml"
  fi
  say "wrote $CLAUDE/engram.yaml"
fi

# ---- storage env (opt-in GitHub sync) ----
if [[ "$STORAGE" == github && -n "$REPO_REMOTE" ]]; then
  printf 'export CLAUDE_MEMORY_REPO=%q\n' "$REPO_REMOTE" > "$CLAUDE/engram.env"
  say "GitHub sync -> $REPO_REMOTE (wrote $CLAUDE/engram.env; add the same export to your shell profile for interactive use)"
  command -v gh >/dev/null || warn "gh CLI not found — needed for GitHub sync"
else
  rm -f "$CLAUDE/engram.env" 2>/dev/null || true
  say "storage: local-only (no remote sync)"
fi

# ---- python deps (engine) ----
if ! python3 -c "import yaml" 2>/dev/null; then
  say "installing pyyaml (engine dep)..."; python3 -m pip install --user -q pyyaml || warn "pip install pyyaml failed"
fi

# ---- graph venv + MCP ----
if [[ "$WANT_GRAPH" == yes ]]; then
  VENV="$CLAUDE/graph/venv"
  if [[ ! -x "$VENV/bin/python" ]]; then
    say "building graph venv (graphiti-core, neo4j, fastembed)... this can take a few minutes"
    if python3 -m venv "$VENV" && "$VENV/bin/pip" install -q --upgrade pip && \
       "$VENV/bin/pip" install -q graphiti-core neo4j fastembed pyyaml; then
      say "graph venv ready"
    else
      warn "graph venv build failed — install graphiti-core/neo4j/fastembed manually into $VENV"
    fi
  fi
  [[ -f "$CLAUDE/graph/.env" ]] || { printf 'NEO4J_PASSWORD=%s\n' "$(openssl rand -hex 24 2>/dev/null || date +%s)" > "$CLAUDE/graph/.env"; chmod 600 "$CLAUDE/graph/.env"; say "generated graph/.env (Neo4j password)"; }
  if command -v claude >/dev/null && [[ -x "$VENV/bin/python" ]]; then
    if ! claude mcp list 2>/dev/null | grep -q engram-graph; then
      claude mcp add --scope user engram-graph "$VENV/bin/python" "$CLAUDE/graph/mg_mcp_server.py" \
        && say "registered engram-graph MCP server" || warn "claude mcp add failed (register manually later)"
    else say "engram-graph MCP already registered"; fi
  else warn "claude CLI or graph venv missing — skipping MCP registration (run 'claude mcp add' later)"; fi
  say "start Neo4j: cd $CLAUDE/graph && NEO4J_PASSWORD=\$(grep -oP 'NEO4J_PASSWORD=\\K.*' .env) docker compose up -d"
fi

# ---- vector venv + MCP (optional Qdrant index) ----
if [[ "$WANT_VECTOR" == yes ]]; then
  VVENV="$CLAUDE/vector/venv"
  if [[ ! -x "$VVENV/bin/python" ]]; then
    say "building vector venv (mcp, qdrant-client, fastembed)... this can take a few minutes"
    if python3 -m venv "$VVENV" && "$VVENV/bin/pip" install -q --upgrade pip && \
       "$VVENV/bin/pip" install -q "mcp[cli]" qdrant-client fastembed pyyaml; then
      say "vector venv ready"
    else
      warn "vector venv build failed — install mcp/qdrant-client/fastembed manually into $VVENV"
    fi
  fi
  if command -v claude >/dev/null && [[ -x "$VVENV/bin/python" ]]; then
    if ! claude mcp list 2>/dev/null | grep -q engram-vector; then
      claude mcp add --scope user engram-vector "$VVENV/bin/python" "$CLAUDE/vector/vector_mcp_server.py" \
        && say "registered engram-vector MCP server" || warn "claude mcp add failed (register manually later)"
    else say "engram-vector MCP already registered"; fi
  else warn "claude CLI or vector venv missing — skipping MCP registration (run 'claude mcp add' later)"; fi
  # Hybrid recall: the warm engram-graph server queries Qdrant in-process, so the
  # GRAPH venv needs qdrant-client too. Idempotent; harmless if graph isn't built.
  if [[ -x "$CLAUDE/graph/venv/bin/python" ]]; then
    "$CLAUDE/graph/venv/bin/pip" install -q qdrant-client \
      && say "added qdrant-client to graph venv (enables memory_recall_hybrid)" \
      || warn "could not add qdrant-client to graph venv (hybrid will fall back to graph+keyword)"
  fi
  say "start Qdrant: cd $CLAUDE/vector && docker compose up -d"
  # Seed the index from any memories already on disk (best-effort; no-op if Qdrant is down).
  [[ -x "$VVENV/bin/python" ]] && "$VVENV/bin/python" "$CLAUDE/vector/vector_sync.py" --rebuild 2>/dev/null || true
fi

# ---- hooks ----
[[ -f "$SETTINGS" ]] || echo '{}' > "$SETTINGS"
if command -v jq >/dev/null; then
  merge_hook(){ jq --arg e "$1" --arg c "$2" '.hooks //= {} | .hooks[$e] //= [] |
      if ([.hooks[$e][]?|.hooks[]?|.command]|index($c))==null then .hooks[$e] += [{"hooks":[{"type":"command","command":$c}]}] else . end' \
      "$SETTINGS" > "$SETTINGS.tmp" && mv "$SETTINGS.tmp" "$SETTINGS"; }
  merge_hook SessionStart "$CLAUDE/memory_curate_check.sh"
  merge_hook SessionStart "$CLAUDE/codex-availability-warn.sh"
  merge_hook Stop "$CLAUDE/memory_agent.sh"
  merge_hook Stop "$CLAUDE/memory_session_curate.sh"
  say "hooks merged into settings.json"
fi

# ---- seed synthetic examples (only if store empty) ----
SLUG="${CLAUDE_MEMORY_SLUG:-$(printf '%s' "$HOME" | sed 's|/|-|g')}"
STORE="$CLAUDE/projects/$SLUG/memory"; mkdir -p "$STORE"
if ! ls "$STORE"/*.md >/dev/null 2>&1; then
  if ls "$REPO"/examples/memory/*.md >/dev/null 2>&1; then
    cp "$REPO"/examples/memory/*.md "$STORE"/; say "seeded ${STORE} with synthetic examples"
  fi
fi

# ---- daemon ----
case "$DAEMON" in
  systemd)
    mkdir -p "$HOME/.config/systemd/user" "$HOME/.config/engram"
    DAEMON_ENV="$HOME/.config/engram/daemon.env"
    # PRESERVE operator secrets across re-installs (do NOT clobber them): the ccg key
    # and the Telegram approval-gate token/chat id live here and must survive.
    PRESERVED="$(grep -E '^(ENGRAM_CCG_KEY|ANTHROPIC_BASE_URL|TELEGRAM_BOT_TOKEN|TELEGRAM_CHAT_ID)=' "$DAEMON_ENV" 2>/dev/null || true)"
    { echo "ENGRAM_BIN=$CLAUDE"; echo "ENGRAM_GRAPH=$CLAUDE/graph"; echo "ENGRAM_CONFIG=$CLAUDE/engram.yaml"; echo "ENGRAM_LOG_DIR=$CLAUDE/logs";
      [[ -x "$CLAUDE/graph/venv/bin/python" ]] && echo "ENGRAM_GRAPH_PYTHON=$CLAUDE/graph/venv/bin/python";
      [[ -x "$CLAUDE/vector/venv/bin/python" ]] && echo "ENGRAM_VECTOR_PYTHON=$CLAUDE/vector/venv/bin/python"; } > "$DAEMON_ENV"
    if [[ -n "$PRESERVED" ]]; then
      printf '%s\n' "$PRESERVED" >> "$DAEMON_ENV"
    else
      cat >> "$DAEMON_ENV" <<'DENV'

# ── Async approval gate (Telegram) — for the RISKY autonomous ops (skill installs,
#    Codex-deferred / lossy merges, purges). Optional but recommended.
#    1) Telegram: message @BotFather -> /newbot -> copy the token.
#    2) Uncomment + set below (message your bot once so it can learn your chat id;
#       engram_telegram_gate.py --poll will pick up the first message's chat id, or
#       set TELEGRAM_CHAT_ID explicitly). Keep this file mode 600.
# TELEGRAM_BOT_TOKEN=123456:AA...
# TELEGRAM_CHAT_ID=123456789
#
# ── cc-gateway backend key (only if engram.yaml has `backend: ccg`) ──
# ENGRAM_CCG_KEY=...
DENV
    fi
    chmod 600 "$DAEMON_ENV"
    sed "s|^ExecStart=.*|ExecStart=$(command -v python3) $CLAUDE/engram-daemon.py --once|" "$REPO/daemon/engram.service" > "$HOME/.config/systemd/user/engram.service"
    cp "$REPO/daemon/engram.timer" "$HOME/.config/systemd/user/engram.timer"
    # Optional nightly Codex-gated curate+fixate APPLY (headless Claude). ExecStart is
    # templated to the real $CLAUDE path (honours ENGRAM_CLAUDE_HOME). Enabled (NOT
    # started) only when MEMORY_NIGHTLY_APPLY=1 — it moves memory unattended; opt in
    # once you trust the Codex gate + DRYRUN output.
    # Plain copy — the unit reads ENGRAM_BIN at runtime (no path templated into it).
    # daemon.env (written above) carries ENGRAM_BIN=$CLAUDE for alternate homes.
    if [[ -f "$REPO/daemon/memory-nightly-apply.timer" && -f "$REPO/daemon/memory-nightly-apply.service" ]]; then
      cp "$REPO/daemon/memory-nightly-apply.service" "$HOME/.config/systemd/user/memory-nightly-apply.service"
      cp "$REPO/daemon/memory-nightly-apply.timer" "$HOME/.config/systemd/user/memory-nightly-apply.timer"
    elif [[ "${MEMORY_NIGHTLY_APPLY:-0}" == "1" ]]; then
      warn "MEMORY_NIGHTLY_APPLY=1 but nightly units missing from repo — NOT enabling"
    fi
    if systemctl --user daemon-reload 2>/dev/null && systemctl --user enable --now engram.timer 2>/dev/null; then
      say "systemd timer enabled (engram.timer); 'sudo loginctl enable-linger $USER' to run when logged out"
      if [[ "${MEMORY_NIGHTLY_APPLY:-0}" == "1" ]]; then
        # enable --now is safe here: the timer is Persistent=false with a future
        # OnCalendar, so --now starts it ticking toward the next 03:37 and never
        # catch-up-runs an APPLY during install.
        if [[ -x "$CLAUDE/memory_nightly_apply.sh" && -f "$HOME/.config/systemd/user/memory-nightly-apply.timer" ]]; then
          systemctl --user enable --now memory-nightly-apply.timer 2>/dev/null \
            && say "nightly Codex-gated apply ENABLED (active; fires nightly at 03:37)" \
            || warn "could not enable memory-nightly-apply.timer"
        else warn "MEMORY_NIGHTLY_APPLY=1 but runner/timer artifacts missing — NOT enabled"; fi
      elif systemctl --user is-enabled memory-nightly-apply.timer >/dev/null 2>&1; then
        # Reinstall without opt-in must not silently leave a previously-enabled timer running.
        systemctl --user disable --now memory-nightly-apply.timer >/dev/null 2>&1 || true
        say "nightly apply timer DISABLED (MEMORY_NIGHTLY_APPLY not set this run)"
      else
        say "nightly apply installed but NOT enabled — set MEMORY_NIGHTLY_APPLY=1 to turn on"
      fi
    else warn "systemd --user unavailable here — units written; enable on the target host"; fi;;
  docker)
    say "docker daemon: cd $REPO/daemon && cp .env.example .env && \$EDITOR .env && docker compose up -d";;
  none) say "no daemon (run /memory-* commands manually, or set one up later)";;
esac

say "done. Restart Claude Code so it loads the new commands + MCP server."
say "To run engram AUTONOMOUSLY (unattended harvest/graduate/curate + Telegram approvals),"
say "see AUTONOMY.md — set the backend, the auto_* flags in engram.yaml, and the Telegram token in $HOME/.config/engram/daemon.env."
