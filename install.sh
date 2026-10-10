#!/usr/bin/env bash
# install.sh — install engram into ~/.claude. Idempotent + re-runnable.
#
# Interactive by default; non-interactive with flags:
#   --backend ollama|claude     --tier cpu|small|medium|large   --ollama-host URL
#   --storage local|github      --repo owner/name (implies github)
#   --daemon none|systemd|docker
#   --graph | --no-graph        (build the Neo4j graph venv + register the MCP server)
#   --vector | --no-vector      (build the Qdrant vector venv + register the MCP server)
#   --hermes | --no-hermes      (also register the MCP servers with hermes; default: auto)
#   --no-start-services         (do NOT `docker compose up` Neo4j/Qdrant; just print how)
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
WANT_HERMES="auto"   # auto = register only if the hermes CLI is on PATH
WANT_BACKUP="no"     # off unless asked: backups need an operator-provided bucket
WANT_SILVERBULLET="no"  # off unless asked: stands up a web editor per vault
WANT_WEB_AUTH="no"      # off unless asked: one web credential for atlas + SilverBullet
START_SERVICES="yes" # bring Neo4j/Qdrant UP (they were only ever printed as a hint)

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
  --backup) WANT_BACKUP="yes"; shift;;
  --no-backup) WANT_BACKUP="no"; shift;;
  --silverbullet) WANT_SILVERBULLET="yes"; shift;;
  --no-silverbullet) WANT_SILVERBULLET="no"; shift;;
  --web-auth) WANT_WEB_AUTH="yes"; shift;;
  --no-web-auth) WANT_WEB_AUTH="no"; shift;;
  --hermes) WANT_HERMES="yes"; shift;;
  --no-hermes) WANT_HERMES="no"; shift;;
  --start-services) START_SERVICES="yes"; shift;;
  --no-start-services) START_SERVICES="no"; shift;;
  --yes|-y) YES=1; shift;;
  -h|--help) sed -n '2,17p' "$0"; exit 0;;
  *) echo "unknown arg: $1" >&2; exit 2;;
esac; done

say()  { printf '\033[1;36m[engram]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[engram] warning:\033[0m %s\n' "$*"; }
ask()  { local __v="$1" __p="$2" __d="$3" __a; if [[ "$YES" == 1 || ! -t 0 ]]; then printf -v "$__v" '%s' "${!__v:-$__d}"; return; fi; read -r -p "$__p [$__d]: " __a || true; printf -v "$__v" '%s' "${__a:-${!__v:-$__d}}"; }

# compose_up <dir> <container> <label> — actually START a dependency instead of just
# printing the command. Both compose files already declare `restart: unless-stopped`,
# so once up they survive reboots — PROVIDED the docker daemon starts at boot. Both
# conditions are verified rather than assumed: a Neo4j that is merely down does not
# error loudly, it silently pauses every graph insert (7 nights of that, 2026-08).
# `docker compose` picks up the sibling .env for ${NEO4J_PASSWORD} on its own.
compose_up() {
  local dir="$1" cname="$2" label="$3" pol
  if [[ "$START_SERVICES" != yes ]]; then
    say "not starting $label (--no-start-services); run: cd $dir && docker compose up -d"; return
  fi
  if ! command -v docker >/dev/null; then
    warn "docker not found — start $label yourself: cd $dir && docker compose up -d"; return
  fi
  if docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$cname"; then
    say "$label already running ($cname)"
  elif ( cd "$dir" && docker compose up -d ) >/dev/null 2>&1; then
    say "$label started ($cname)"
  else
    warn "could not start $label — run manually: cd $dir && docker compose up -d"; return
  fi
  pol="$(docker inspect -f '{{.HostConfig.RestartPolicy.Name}}' "$cname" 2>/dev/null || true)"
  case "$pol" in
    unless-stopped|always) ;;
    *) warn "$cname restart policy is '${pol:-unknown}' — it will NOT return after a reboot."
       warn "  fix: docker update --restart unless-stopped $cname";;
  esac
  systemctl is-enabled docker >/dev/null 2>&1 \
    || warn "docker is not enabled at boot — $label will not auto-start (sudo systemctl enable docker)"
}

# hermes_register <server-name> <python> <server.py> — also expose an engram MCP
# server to hermes, an MCP client that drives a LOCAL Ollama model, so the local
# LLM gets the same recall tools Claude Code has. No-op when hermes isn't installed.
# Two non-obvious constraints, both load-bearing:
#   * hermes sanitizes the child env down to PATH/HOME/USER/LANG/LC_ALL/TERM/SHELL/
#     TMPDIR/XDG_*, so CLAUDE_MEMORY_SLUG must travel in the entry's OWN env map —
#     exporting it here would NOT reach the server, and a wrong slug silently
#     recalls from the wrong store.
#   * `hermes mcp add --args` is argparse.REMAINDER: it must be the LAST flag.
hermes_register() {
  [[ "$WANT_HERMES" == no ]] && return 0
  if ! command -v hermes >/dev/null; then
    [[ "$WANT_HERMES" == yes ]] && warn "hermes not found on PATH — skipping $1"
    return 0
  fi
  [[ -x "$2" ]] || return 0
  if hermes mcp list 2>/dev/null | grep -q "$1"; then say "$1 already registered with hermes"; return 0; fi
  if hermes mcp add "$1" --env "CLAUDE_MEMORY_SLUG=$SLUG" --command "$2" --args "$3" >/dev/null 2>&1; then
    say "registered $1 with hermes (local LLM gets recall too)"
  else
    warn "hermes mcp add $1 failed — add it under mcp_servers: in ~/.hermes/config.yaml manually"
  fi
}

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
say "backend=$BACKEND tier=$TIER storage=$STORAGE daemon=$DAEMON graph=$WANT_GRAPH vector=$WANT_VECTOR hermes=$WANT_HERMES"

# ---- place files ----
mkdir -p "$CLAUDE/commands" "$CLAUDE/graph" "$CLAUDE/vector" "$CLAUDE/logs"
install -m 0755 "$REPO"/bin/*.sh "$CLAUDE"/ 2>/dev/null || true
install -m 0755 "$REPO"/bin/*.py "$CLAUDE"/ 2>/dev/null || true
install -m 0644 "$REPO"/commands/*.md "$CLAUDE"/commands/ 2>/dev/null || true
# natural-language skills (memory-tidy / memory-promote) — the zero-command entry points
for skdir in "$REPO"/skills/*/; do
  [[ -f "$skdir/SKILL.md" ]] || continue
  sk="$(basename "$skdir")"; mkdir -p "$CLAUDE/skills/$sk"
  install -m 0644 "$skdir/SKILL.md" "$CLAUDE/skills/$sk/"
done
for f in "$REPO"/graph/*.py "$REPO"/graph/*.md "$REPO"/graph/docker-compose.yml; do [[ -e "$f" ]] && install -m 0644 "$f" "$CLAUDE/graph/"; done
for f in "$REPO"/vector/*.py "$REPO"/vector/docker-compose.yml; do [[ -e "$f" ]] && install -m 0644 "$f" "$CLAUDE/vector/"; done
install -m 0755 "$REPO"/daemon/engram-daemon.py "$CLAUDE"/ 2>/dev/null || true
mkdir -p "$CLAUDE/hooks"; install -m 0755 "$REPO"/bin/hooks/*.py "$CLAUDE/hooks/" 2>/dev/null || true
# Remove the legacy SPA; the optional Atlas imports the installed API module.
rm -rf "$CLAUDE/ui" "$CLAUDE/engram-ui.sh" 2>/dev/null || true
say "engine installed into $CLAUDE (console: run $CLAUDE/engram-tui.py)"

# Keep a copy of engram-app that a systemd unit runs, but that this installer does
# not own, in step with the one it just built.
#
# On an SELinux-enforcing host, systemd cannot exec out of /root/.claude
# (admin_home_t is not an entrypoint), so those deployments run the API from a
# bin_t path such as /usr/local/bin and the unit's ExecStart points there. This
# script only ever wrote $CLAUDE/rust, so every install produced a new binary that
# the service never ran: `systemctl restart` succeeded, the API kept serving the
# old code, and nothing said so. That is the worst shape for this failure — the
# deploy looks clean.
#
# Only refresh a path that ALREADY exists. Creating one would quietly add a
# system-wide install on hosts that never asked for it.
# Provision encrypted off-host backups (restic). Called only under --backup.
# Writes the three secrets into daemon.env (mode 600, never into engram.yaml),
# flips backup.enabled, and inits the repo. Secrets already present (preserved
# across a re-install) are not re-prompted. Non-interactive runs (--yes, or no
# TTY) write commented placeholders and print instructions rather than blocking.
provision_backup(){
  local envf="$HOME/.config/engram/daemon.env"
  say "provisioning encrypted backups (restic)"
  if ! command -v restic >/dev/null 2>&1; then
    if command -v dnf >/dev/null 2>&1; then sudo dnf install -y restic >/dev/null 2>&1 || true
    elif command -v apt-get >/dev/null 2>&1; then sudo apt-get install -y restic >/dev/null 2>&1 || true; fi
  fi
  command -v restic >/dev/null 2>&1 || warn "restic not installed — install it, then set backup secrets in $envf"
  mkdir -p "$(dirname "$envf")"; touch "$envf"; chmod 600 "$envf"

  local have_pw have_bucket
  have_pw="$(grep -c '^ENGRAM_BACKUP_PASSWORD=' "$envf" 2>/dev/null || echo 0)"
  local endpoint bucket key_id key pw
  if [[ -t 0 && "$YES" != "1" ]]; then
    read -r -p "  B2/S3 endpoint [s3.us-west-004.backblazeb2.com]: " endpoint
    read -r -p "  bucket name: " bucket
    read -r -p "  application key id: " key_id
    read -r -s -p "  application key: " key; echo
    if [[ "$have_pw" == "0" ]]; then
      read -r -s -p "  restic password (blank = generate a strong one): " pw; echo
    fi
  fi
  endpoint="${endpoint:-s3.us-west-004.backblazeb2.com}"
  # Generate a password only when none exists AND none was typed. Print it ONCE.
  if [[ "$have_pw" == "0" && -z "${pw:-}" ]]; then
    pw="$(openssl rand -base64 33 2>/dev/null || head -c 33 /dev/urandom | base64)"
    warn "GENERATED restic password — store it NOW, it is the ONLY key to your backups:"
    printf '      %s\n' "$pw"
  fi
  {
    [[ "$have_pw" == "0" && -n "${pw:-}" ]] && echo "ENGRAM_BACKUP_PASSWORD=$pw"
    [[ -n "${key_id:-}" ]] && echo "ENGRAM_BACKUP_S3_KEY_ID=$key_id"
    [[ -n "${key:-}" ]] && echo "ENGRAM_BACKUP_S3_KEY=$key"
  } >> "$envf"
  chmod 600 "$envf"

  # Non-secret knobs into engram.yaml: enable, and the endpoint/bucket if given.
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$CLAUDE/engram.yaml" "$endpoint" "${bucket:-}" <<'PYB' 2>/dev/null || warn "could not update engram.yaml backup block"
import sys
try:
    import yaml
except Exception:
    sys.exit(0)
path, endpoint, bucket = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    cfg = yaml.safe_load(open(path)) or {}
except Exception:
    cfg = {}
b = cfg.get("backup") or {}
b["enabled"] = True
b.setdefault("provider", "b2")
b["endpoint"] = endpoint
if bucket:
    b["bucket"] = bucket
b.setdefault("prefix", "engram")
b.setdefault("retention", {"daily": 7, "weekly": 4, "monthly": 6})
b.setdefault("check_every_days", 7)
cfg["backup"] = b
yaml.safe_dump(cfg, open(path, "w"), default_flow_style=False, sort_keys=False)
PYB
  fi

  # Initialise the repo when we have enough to reach it.
  if command -v restic >/dev/null 2>&1 && grep -q '^ENGRAM_BACKUP_PASSWORD=' "$envf" && [[ -n "${bucket:-}" || -n "$(grep '^ENGRAM_BACKUP_REPO=' "$envf" 2>/dev/null)" ]]; then
    ( set -a; . "$envf"; set +a; ENGRAM_CONFIG="$CLAUDE/engram.yaml" python3 "$CLAUDE/engram_backup.py" init ) \
      && say "backup repository ready — the daemon will snapshot daily" \
      || warn "backup repo init did not complete; check the bucket + keys, then run: engram_backup.py init"
  else
    say "backup secrets recorded in $envf; set the bucket and run: engram_backup.py init"
  fi
}

# Stand up a SilverBullet browser editor per vault. Called only under
# --silverbullet. One container per tenant that has a vault, each mounting ONLY
# its own vault (isolation at the mount layer), bound to loopback and fronted by
# the Traefik gateway over the LAN/tailnet. Secrets (per-tenant SB_USER) live in
# ~/.config/engram/silverbullet.env (mode 600), NOT in the repo. The work tenants
# are read-only by default (set in the compose file).
provision_silverbullet(){
  local compose="$REPO/silverbullet/docker-compose.yml"
  local envf="$HOME/.config/engram/silverbullet.env"
  local suffix="${SB_HOST_SUFFIX:-home.arpa}"
  local dyn="${TRAEFIK_DYNAMIC_DIR:-/opt/gateway-stack/traefik/dynamic}"
  say "provisioning SilverBullet vault editors"
  if ! command -v docker >/dev/null 2>&1; then warn "docker not found — cannot run SilverBullet"; return; fi
  [[ -f "$compose" ]] || { warn "missing $compose"; return; }
  mkdir -p "$(dirname "$envf")"; touch "$envf"; chmod 600 "$envf"

  # Which tenants have a vault? Ask the config, same resolver the rest uses.
  local tenants
  tenants="$(python3 - "$CLAUDE/engram.yaml" <<'PYT' 2>/dev/null || true
import sys
try:
    import yaml; cfg = yaml.safe_load(open(sys.argv[1])) or {}
except Exception:
    sys.exit(0)
for name, entry in (cfg.get("tenants") or {}).items():
    if str((entry or {}).get("vault") or "").strip():
        print(name)
PYT
)"
  [[ -n "$tenants" ]] || { warn "no tenant has a 'vault:' configured — nothing to serve"; return; }

  # One SB_USER per tenant, generated once and preserved. Service name wiki-<t>.
  #
  # The compose file ships a fixed set of services (the known tenants). A tenant
  # the compose has no service for is SKIPPED with a warning rather than silently
  # breaking `docker compose up` — and without this guard an all-unknown set would
  # leave `services` empty, which makes `up -d` start EVERY service. To add a new
  # tenant: add its block to silverbullet/docker-compose.yml and a port here.
  local services=() t var pw known
  for t in $tenants; do
    case "$t" in homelab|mjsv|bbhost|dseclab) known=1;; *) known=0;; esac
    if [[ "$known" == 0 ]]; then
      warn "tenant '$t' has no service in silverbullet/docker-compose.yml — skipping (add one to serve it)"
      continue
    fi
    var="SB_USER_$(printf '%s' "$t" | tr '[:lower:]-' '[:upper:]_')"
    if ! grep -q "^${var}=" "$envf" 2>/dev/null; then
      # Generate and STORE the password; do NOT echo it. Printing it would land
      # the secret in terminal scrollback, CI logs and transcript capture. The
      # operator retrieves it deliberately from the 0600 file.
      pw="$(openssl rand -base64 18 2>/dev/null || head -c 18 /dev/urandom | base64)"
      echo "${var}=admin:${pw}" >> "$envf"
      say "SilverBullet '$t': generated a login (user 'admin') — read it with: grep '^${var}=' $envf"
    fi
    services+=("wiki-${t}")
  done
  chmod 600 "$envf"
  if [[ ${#services[@]} -eq 0 ]]; then
    warn "no servable tenant (none match a compose service) — not starting SilverBullet"
    return 1
  fi

  # Order matters (codex #8): bring the backends UP first, then publish routes —
  # otherwise a failed `up` leaves live Traefik routes pointing at dead backends.
  docker compose --env-file "$envf" -f "$compose" pull "${services[@]}" >/dev/null 2>&1 || true
  if ! docker compose --env-file "$envf" -f "$compose" up -d "${services[@]}"; then
    warn "docker compose up did not complete for SilverBullet; routes NOT published. Check 'docker compose -f $compose logs'"
    return 1
  fi

  # Publish the Traefik routes only now, and ATOMICALLY (write + rename) so the
  # gateway's file-watcher never reads a half-written config.
  if [[ -d "$dyn" && -w "$dyn" ]]; then
    local tmp="$dyn/.silverbullet.yml.$$"
    { echo "http:"; echo "  services:"
      for t in $tenants; do
        local port; case "$t" in homelab) port=3011;; mjsv) port=3012;; bbhost) port=3013;; dseclab) port=3014;; *) port=0;; esac
        [[ "$port" == 0 ]] && continue
        echo "    sb-${t}: {loadBalancer: {servers: [{url: \"http://127.0.0.1:${port}\"}]}}"
      done
      echo "  routers:"
      for t in $tenants; do
        case "$t" in homelab|mjsv|bbhost|dseclab) ;; *) continue;; esac
        echo "    sb-${t}: {rule: \"Host(\`wiki-${t}.${suffix}\`)\", entryPoints: [websecure, websecure-v6], tls: {}, service: sb-${t}}"
      done
    } > "$tmp" && mv -f "$tmp" "$dyn/silverbullet.yml"
    say "published Traefik routes to $dyn/silverbullet.yml (wiki-<tenant>.${suffix})"
  else
    warn "gateway dynamic dir $dyn not writable — wire routes by hand from silverbullet/traefik-silverbullet.example.yml"
  fi

  for t in $tenants; do
    case "$t" in homelab|mjsv|bbhost|dseclab) say "  SilverBullet [$t] -> https://wiki-${t}.${suffix}";; esac
  done
  say "SilverBullet is tailnet-only (loopback + gateway). Edits are indexed on the daemon's next wiki pass."
}

# One web credential for the whole engram web surface. Called under --web-auth.
#
# The model (see silverbullet/engram-web-auth.example.yml): ONE user:password,
# enforced at the gateway for engram's atlas dashboard (whose config editor is
# otherwise open on the LAN) and as SilverBullet's own SB_USER for the editors —
# so a single credential logs into both, with no double prompt. It lives ONLY at
# the web layer: the loopback API, CLI, MCP and daemon never cross it.
provision_web_auth(){
  local dyn="${TRAEFIK_DYNAMIC_DIR:-/opt/gateway-stack/traefik/dynamic}"
  local sbenv="$HOME/.config/engram/silverbullet.env"
  local failed=0
  say "provisioning shared web auth (atlas + SilverBullet)"

  local user pw
  if [[ -t 0 && "$YES" != "1" ]]; then
    read -r -p "  web username [admin]: " user
    read -r -s -p "  web password (blank = generate): " pw; echo
  fi
  user="${user:-admin}"
  if [[ -z "${pw:-}" ]]; then
    pw="$(openssl rand -base64 15 2>/dev/null | tr -d '/+=' || head -c 15 /dev/urandom | base64 | tr -d '/+=')"
    say "generated the web password — read it with: grep '^ENGRAM_WEB_PASSWORD=' $sbenv"
  fi

  # Record the plaintext ONCE in the 0600 env file (not printed), so the operator
  # can retrieve it; it is also what SilverBullet's SB_USER needs (user:pass).
  # umask 077 so the temp never exists even briefly world-readable (it holds the
  # plaintext password).
  mkdir -p "$(dirname "$sbenv")"
  local old_umask; old_umask="$(umask)"; umask 077
  touch "$sbenv"; chmod 600 "$sbenv"
  local tmp; tmp="$(mktemp "$(dirname "$sbenv")/.sbenv.XXXXXX")"
  { echo "ENGRAM_WEB_USER=$user"; echo "ENGRAM_WEB_PASSWORD=$pw"
    grep -v '^ENGRAM_WEB_USER=\|^ENGRAM_WEB_PASSWORD=\|^SB_USER_' "$sbenv" 2>/dev/null || true
    # Sink 2: SilverBullet's SB_USER for every known tenant = the SAME credential.
    local T; for T in HOMELAB MJSV BBHOST DSECLAB; do echo "SB_USER_${T}=${user}:${pw}"; done
  } > "$tmp"
  mv -f "$tmp" "$sbenv"; chmod 600 "$sbenv"; umask "$old_umask"
  say "SilverBullet logins synced to the same credential (in $sbenv)"

  # Sink 1: the gateway. The middleware carries the bcrypt hash INLINE (like the
  # gateway's hermes.yml), NOT via usersFile — the gateway mounts individual
  # files, so a separate htpasswd would not be visible inside the traefik
  # container. Inline users in a file already under the mounted dynamic/ dir
  # needs no new mount. The '$' in a bcrypt hash is literal in a file-provider
  # config (unlike docker labels), so it is embedded verbatim.
  local hashline
  if command -v htpasswd >/dev/null 2>&1; then
    hashline="$(htpasswd -nbB "$user" "$pw")"
  else
    hashline="${user}:$(openssl passwd -apr1 "$pw")"
  fi
  if [[ -d "$dyn" && -w "$dyn" ]]; then
    # Atomic publish: write a temp in the SAME dir, then rename, so Traefik's
    # watcher never reads a half-written middleware.
    local mwtmp; mwtmp="$(mktemp "$dyn/.engram-web-auth.XXXXXX")"
    cat > "$mwtmp" <<EOF
# Generated by install.sh --web-auth. Inline bcrypt (no usersFile: the gateway
# mounts individual files, so a separate htpasswd would be invisible here).
http:
  middlewares:
    engram-auth:
      basicAuth:
        users:
          - "${hashline}"
        removeHeader: true
EOF
    mv -f "$mwtmp" "$dyn/engram-web-auth.yml"
    # Attach it to the EXISTING engram router in tls.yml (idempotent, backed up,
    # written atomically; a backup that cannot be taken aborts the patch).
    local tls="$dyn/tls.yml"
    if [[ -f "$tls" ]] && command -v python3 >/dev/null; then
      if cp -a "$tls" "$tls.engram-bak.$(date +%s)"; then
      python3 - "$tls" <<'PY' && say "attached engram-auth to the engram router" || { warn "did NOT secure the atlas router at $tls — add 'engram-auth' to its middlewares by hand (see message above)"; failed=1; }
import sys
p = sys.argv[1]
raw = open(p).read()

def apply(load, dump):
    d = load(raw)
    r = ((d.get("http") or {}).get("routers") or {}).get("engram")
    if r is None:
        print(f"no 'engram' router in {p}; nothing to patch", file=sys.stderr)
        return None
    mw = r.setdefault("middlewares", [])
    if "engram-auth" in mw:
        return raw  # idempotent: already attached, leave the file byte-for-byte
    mw.append("engram-auth")
    return dump(d)

# Prefer ruamel (round-trip: preserves comments, quoting and key order). This is
# a shared GATEWAY file that may carry operator comments, so a lossy rewrite is
# not acceptable.
out = None
try:
    from ruamel.yaml import YAML
    import io
    y = YAML()
    def _load(s):
        return y.load(s)
    def _dump(d):
        buf = io.StringIO(); y.dump(d, buf); return buf.getvalue()
    out = apply(_load, _dump)
except Exception:
    import yaml
    # pyyaml strips comments. Only safe when the file has none — otherwise refuse
    # and tell the operator exactly what to add, rather than silently mangling it.
    if any(line.lstrip().startswith("#") for line in raw.splitlines()):
        print("tls.yml has comments and ruamel.yaml is not installed; refusing a lossy\n"
              "rewrite. Add these two lines under the 'engram' router:\n"
              "      middlewares:\n        - engram-auth", file=sys.stderr)
        sys.exit(1)
    out = apply(yaml.safe_load, lambda d: yaml.safe_dump(d, default_flow_style=False, sort_keys=False))

if out is None:
    sys.exit(1)
# Atomic: write a sibling temp then rename, so Traefik's watcher never reads a
# partial router config.
import os, tempfile
fd, t = tempfile.mkstemp(dir=os.path.dirname(p) or ".", prefix=".tls.")
with os.fdopen(fd, "w") as f:
    f.write(out)
os.replace(t, p)
PY
      else
        warn "could not back up $tls — NOT patching it (atlas left unsecured). Add 'engram-auth' to the engram router by hand."; failed=1
      fi
    else
      warn "no $tls to patch — attach the engram-auth middleware to your engram router manually"; failed=1
    fi
  else
    warn "gateway dir $dyn not writable — atlas NOT secured. See silverbullet/engram-web-auth.example.yml to wire it by hand."; failed=1
  fi

  if [[ "$failed" == 0 ]]; then
    say "atlas dashboard now requires login at https://engram.${SB_HOST_SUFFIX:-home.arpa}"
  else
    warn "WEB AUTH INCOMPLETE: the atlas dashboard is NOT protected yet — act on the message(s) above."
  fi

  # If SilverBullet instances are already running, their SB_USER is baked into the
  # live container env; rotating the file does nothing until they are recreated.
  if command -v docker >/dev/null 2>&1 && [[ -f "$REPO/silverbullet/docker-compose.yml" ]]; then
    local running; running="$(docker compose --env-file "$sbenv" -f "$REPO/silverbullet/docker-compose.yml" ps --services --status running 2>/dev/null || true)"
    if [[ -n "$running" ]]; then
      say "recreating running SilverBullet instances so the new login takes effect"
      # shellcheck disable=SC2086
      docker compose --env-file "$sbenv" -f "$REPO/silverbullet/docker-compose.yml" up -d --force-recreate $running >/dev/null 2>&1 \
        || warn "could not recreate SilverBullet — restart it so the new SB_USER applies"
    fi
  fi

  say "web auth is gateway + web-app only — the loopback API, CLI, MCP and daemon are unaffected"
  return "$failed"
}

sync_service_binaries(){
  local unit path updated=0
  for unit in /etc/systemd/system/engram-*.service "$HOME/.config/systemd/user"/engram-*.service; do
    [[ -f "$unit" ]] || continue
    while read -r path; do
      [[ -n "$path" ]] || continue
      # Already the installer-managed copy, or not ours to touch.
      [[ "$path" == "$CLAUDE/rust/engram-app" ]] && continue
      [[ -f "$path" ]] || continue
      install -m 0755 "$REPO/target/release/engram-app" "$path" || continue
      command -v restorecon >/dev/null && restorecon "$path" 2>/dev/null
      say "refreshed $path (run by $(basename "$unit"))"
      updated=1
    done < <(grep -hoE '^ExecStart=[^ ]*/engram-app' "$unit" 2>/dev/null | sed 's/^ExecStart=//')
  done
  [[ "$updated" == 1 ]] && say "restart the API to pick it up: systemctl restart engram-api"
  return 0
}

# Build the Rust API and command-line migration tools when Cargo is available.
# Python services remain installed during the staged cutover, so a missing Rust
# toolchain never turns an update into an outage.
if command -v cargo >/dev/null; then
  say "building Rust API and recall tools"
  if (cd "$REPO" && cargo build --release -q -p engram-app --bins); then
    mkdir -p "$CLAUDE/rust"
    for rust_bin in engram-app engram-graph-sync engram-graph-recall-eval engram-index engram-lifecycle engram-mcp engram-native-graph-sync engram-recall engram-recall-hook engram-tenant-migrate engram-wiki-index; do
      [[ -x "$REPO/target/release/$rust_bin" ]] && install -m 0755 "$REPO/target/release/$rust_bin" "$CLAUDE/rust/$rust_bin"
    done
    # engram-graph-compat was removed: the bounded Graphiti spawn now lives inside
    # engram-hybrid, so there is one compatibility implementation, not two. Drop a
    # copy left behind by an earlier install.
    rm -f "$CLAUDE/rust/engram-graph-compat"
    install -m 0644 "$REPO/tests/graph_recall_eval.json" "$CLAUDE/rust/graph_recall_eval.json"
    say "Rust executables installed into $CLAUDE/rust"
    sync_service_binaries
  else
    warn "Rust build failed — retaining the installed Python services"
  fi
else
  warn "cargo not found — Rust API not installed; install Rust and re-run this installer"
fi

# ---- engram.yaml ----
if [[ -f "$CLAUDE/engram.yaml" ]]; then
  say "engram.yaml exists — preserving it (edit by hand to change backend/tier)"
  # UPDATE aid: surface top-level config keys that exist in the shipped example but
  # NOT in the user's config, so they can opt into new features after an update.
  # (Non-destructive — we never edit their file; missing keys fall back to code defaults.)
  NEWKEYS=""
  for k in $(grep -oE '^[a-z_]+:' "$REPO/engram.yaml.example" | tr -d ':' | sort -u); do
    grep -qE "^${k}:" "$CLAUDE/engram.yaml" || NEWKEYS="$NEWKEYS $k"
  done
  if [[ -n "$NEWKEYS" ]]; then
    warn "new config keys available since your engram.yaml was written:${NEWKEYS}"
    warn "  -> compare $REPO/engram.yaml.example and add the blocks you want (e.g. auto_curate, telegram)."
  fi
  # A missing `graph:` block means the graph backend comes from code defaults, and
  # that default decides which INDEX recall reads. It is graphiti_compat on both
  # sides (Rust and the daemon) precisely so an upgrade keeps using the Graphiti
  # index it already populated, but say so rather than leaving it implicit.
  if ! grep -qE '^graph:' "$CLAUDE/engram.yaml"; then
    say "no 'graph:' block in engram.yaml — staying on graphiti_compat (your existing index)"
    say "  -> to try the native graph, add:  graph:\\n  backend: native"
  fi
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
# engram.env is sourced by memory_lib.sh, so it's where operators put pins —
# notably CLAUDE_MEMORY_SLUG (canonical store) and CLAUDE_MEMORY_USERNAME. We own
# only the CLAUDE_MEMORY_REPO line: PRESERVE every other line, because a re-install
# used to truncate the file (`>`) or delete it outright, silently dropping pins.
ENVF="$CLAUDE/engram.env"
PRESERVED_ENV="$(grep -vE '^[[:space:]]*export[[:space:]]+CLAUDE_MEMORY_REPO=' "$ENVF" 2>/dev/null || true)"
if [[ "$STORAGE" == github && -n "$REPO_REMOTE" ]]; then
  { printf 'export CLAUDE_MEMORY_REPO=%q\n' "$REPO_REMOTE"
    [[ -n "$PRESERVED_ENV" ]] && printf '%s\n' "$PRESERVED_ENV"; } > "$ENVF"
  say "GitHub sync -> $REPO_REMOTE (wrote $CLAUDE/engram.env; add the same export to your shell profile for interactive use)"
  command -v gh >/dev/null || warn "gh CLI not found — needed for GitHub sync"
elif [[ -n "$PRESERVED_ENV" ]]; then
  printf '%s\n' "$PRESERVED_ENV" > "$ENVF"      # drop only the remote, keep the pins
  say "storage: local-only (no remote sync; kept your other engram.env pins)"
else
  rm -f "$ENVF" 2>/dev/null || true
  say "storage: local-only (no remote sync)"
fi

# ---- canonical store slug ----
# Resolve ONCE, up here, so every later step targets the SAME store. This used to
# be computed far below (just before seeding), which meant the vector rebuild
# ran against whatever slug the child process defaulted to — on any install whose
# memories don't live under the $HOME-derived slug, that silently built a
# near-empty index and reported success.
# Precedence matches memory_lib.sh: operator pin (engram.env, sourced above via
# PRESERVED_ENV) > $CLAUDE_MEMORY_SLUG > $HOME-derived default.
[[ -f "$ENVF" ]] && source "$ENVF"
SLUG="${CLAUDE_MEMORY_SLUG:-$(printf '%s' "$HOME" | sed 's|/|-|g')}"
STORE="$CLAUDE/projects/$SLUG/memory"; mkdir -p "$STORE"
export CLAUDE_MEMORY_SLUG="$SLUG"   # child processes (vector_sync) must agree
say "memory store: $STORE"

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
       "$VENV/bin/pip" install -q "mcp[cli]" "graphiti-core==0.29.2" neo4j fastembed pyyaml; then
      say "graph venv ready"
    else
      warn "graph venv build failed — install graphiti-core/neo4j/fastembed manually into $VENV"
    fi
  fi
  [[ -f "$CLAUDE/graph/.env" ]] || { printf 'NEO4J_PASSWORD=%s\n' "$(openssl rand -hex 24 2>/dev/null || date +%s)" > "$CLAUDE/graph/.env"; chmod 600 "$CLAUDE/graph/.env"; say "generated graph/.env (Neo4j password)"; }
  # mg_mcp_server.py imports mcp at line 26. Venvs built before that dep was listed
  # here have graphiti-core but no mcp, so the server dies at import and engram-graph
  # is SILENTLY absent from every client. Heal them (no-op once satisfied).
  if [[ -x "$VENV/bin/python" ]] && ! "$VENV/bin/python" -c "import mcp" 2>/dev/null; then
    "$VENV/bin/pip" install -q "mcp[cli]" \
      && say "added mcp to graph venv (engram-graph could not start without it)" \
      || warn "could not install mcp into graph venv — engram-graph will not start"
  fi
  if [[ -x "$VENV/bin/python" ]] && ! "$VENV/bin/python" -c "import importlib.metadata as m; assert m.version('graphiti-core') == '0.29.2'" 2>/dev/null; then
    "$VENV/bin/pip" install -q "graphiti-core==0.29.2" \
      && say "pinned graphiti-core to 0.29.2 for recall compatibility" \
      || warn "could not pin graphiti-core — graphiti_compat recall may differ from the tested version"
  fi
  if command -v claude >/dev/null && [[ -x "$VENV/bin/python" ]]; then
    if ! claude mcp list 2>/dev/null | grep -q engram-graph; then
      claude mcp add --scope user engram-graph \
        -e "ENGRAM_BIN=$CLAUDE" -e "ENGRAM_CONFIG=$CLAUDE/engram.yaml" -e "ENGRAM_GRAPH=$CLAUDE/graph" \
        -- "$VENV/bin/python" "$CLAUDE/graph/mg_mcp_server.py" \
        && say "registered engram-graph MCP server" || warn "claude mcp add failed (register manually later)"
    else say "engram-graph MCP already registered"; fi
  else warn "claude CLI or graph venv missing — skipping MCP registration (run 'claude mcp add' later)"; fi
  hermes_register engram-graph "$VENV/bin/python" "$CLAUDE/graph/mg_mcp_server.py"
  say "start Neo4j: cd $CLAUDE/graph && NEO4J_PASSWORD=\$(grep -oP 'NEO4J_PASSWORD=\\K.*' .env) docker compose up -d"
fi

# Register Rust hybrid recall while retaining the graph server's admin tools.
# The INSTALLATION paths are pinned into the entry (this install's config and graph
# dir, which the server cannot otherwise know); the memory slug deliberately is NOT,
# because it is per-project and the server resolves it from the session's directory.
if command -v claude >/dev/null && [[ -x "$CLAUDE/rust/engram-mcp" ]]; then
  if ! claude mcp list 2>/dev/null | grep -q '^engram-rust'; then
    claude mcp add --scope user engram-rust \
      -e "ENGRAM_BIN=$CLAUDE" -e "ENGRAM_CONFIG=$CLAUDE/engram.yaml" -e "ENGRAM_GRAPH=$CLAUDE/graph" \
      -- "$CLAUDE/rust/engram-mcp" \
      && say "registered Rust hybrid recall MCP server" \
      || warn "could not register engram-rust MCP server"
  else say "engram-rust MCP already registered"; fi
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
  hermes_register engram-vector "$VVENV/bin/python" "$CLAUDE/vector/vector_mcp_server.py"
  # Hybrid recall (graph+vector+keyword) now lives in the engram-rust server, which
  # embeds the query once. The engram-graph/engram-vector servers no longer run their
  # own recall legs, so the graph venv no longer needs qdrant-client for it.
  say "start Qdrant: cd $CLAUDE/vector && docker compose up -d"
  # Seed the index from any memories already on disk (best-effort; no-op if Qdrant
  # is down). Output is REPORTED, not swallowed: `2>/dev/null || true` hid both a
  # dead Qdrant and an empty-store rebuild, so a broken index looked like success.
  if [[ -x "$VVENV/bin/python" ]]; then
    if VOUT="$("$VVENV/bin/python" "$CLAUDE/vector/vector_sync.py" --rebuild 2>&1)"; then
      say "vector rebuild: ${VOUT##*$'\n'}"
      ONDISK="$(find "$STORE" -maxdepth 1 -name '*.md' ! -name 'MEMORY.md' 2>/dev/null | wc -l)"
      [[ "$ONDISK" -gt 0 && "$VOUT" != *"$ONDISK memory"* ]] && \
        warn "store has $ONDISK memories but the rebuild indexed a different count — check CLAUDE_MEMORY_SLUG (currently $SLUG)"
    else
      warn "vector rebuild failed (Qdrant not up yet?) — start it, then: CLAUDE_MEMORY_SLUG=$SLUG $VVENV/bin/python $CLAUDE/vector/vector_sync.py --rebuild"
    fi
  fi
fi

# ---- hooks ----
[[ -f "$SETTINGS" ]] || echo '{}' > "$SETTINGS"
if command -v jq >/dev/null; then
  merge_hook(){ jq --arg e "$1" --arg c "$2" '.hooks //= {} | .hooks[$e] //= [] |
      if ([.hooks[$e][]?|.hooks[]?|.command]|index($c))==null then .hooks[$e] += [{"hooks":[{"type":"command","command":$c}]}] else . end' \
      "$SETTINGS" > "$SETTINGS.tmp" && mv "$SETTINGS.tmp" "$SETTINGS"; }
  # replace_hook EVENT WANTED OBSOLETE...: add WANTED and remove the named
  # alternatives in ONE atomic rewrite.
  #
  # merge_hook only ever appends. An install that registered the Python recall hook
  # and later gained the Rust binary ended up running BOTH, each with its own
  # dedup state — duplicate work and duplicate injection on every prompt.
  replace_hook(){
    local event="$1" wanted="$2"; shift 2
    local drop; drop="$(printf '%s\n' "$@" | jq -R . | jq -s .)"
    jq --arg e "$event" --arg c "$wanted" --argjson drop "$drop" '
      .hooks //= {} | .hooks[$e] //= []
      # drop the obsolete commands wherever they sit in the nested shape
      | .hooks[$e] = [ .hooks[$e][]
          | if type == "object" and has("hooks")
            then .hooks = [ .hooks[] | select(.command as $cmd | ($drop | index($cmd)) == null) ]
            else . end
          | select((type == "object" and has("hooks") and (.hooks | length) == 0) | not) ]
      | if ([.hooks[$e][]?|.hooks[]?|.command]|index($c))==null
        then .hooks[$e] += [{"hooks":[{"type":"command","command":$c}]}] else . end' \
      "$SETTINGS" > "$SETTINGS.tmp" && mv "$SETTINGS.tmp" "$SETTINGS"
  }
  merge_hook SessionStart "$CLAUDE/memory_curate_check.sh"
  merge_hook SessionStart "$CLAUDE/codex-availability-warn.sh"
  merge_hook Stop "$CLAUDE/memory_agent.sh"
  merge_hook Stop "$CLAUDE/memory_session_curate.sh"
  # auto-recall: inject the memories relevant to each prompt (deduped per session).
  # Turn off with `recall.inject.enabled: false` in engram.yaml — no need to unmerge.
  # Exactly ONE recall hook must be registered. Whichever implementation is
  # chosen, the other is removed in the same rewrite.
  if [[ -x "$CLAUDE/rust/engram-recall-hook" ]]; then
    replace_hook UserPromptSubmit "$CLAUDE/rust/engram-recall-hook" \
      "$CLAUDE/hooks/memory-recall-inject.py"
  else
    replace_hook UserPromptSubmit "$CLAUDE/hooks/memory-recall-inject.py" \
      "$CLAUDE/rust/engram-recall-hook"
  fi
  say "hooks merged into settings.json"
fi

# ---- seed synthetic examples (only if store empty) ----
# SLUG/STORE resolved above, before the vector rebuild that depends on them.
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
    PRESERVED="$(grep -E '^(ENGRAM_CCG_KEY|ANTHROPIC_BASE_URL|TELEGRAM_BOT_TOKEN|TELEGRAM_CHAT_ID|ENGRAM_BACKUP_PASSWORD|ENGRAM_BACKUP_S3_KEY_ID|ENGRAM_BACKUP_S3_KEY|ENGRAM_BACKUP_REPO)=' "$DAEMON_ENV" 2>/dev/null || true)"
    # CLAUDE_MEMORY_SLUG must be here too: without it the daemon re-derives the
    # slug from $HOME and ignores an operator pin, so scheduled jobs index a
    # different store than the one interactive recall searches.
    # ENGRAM_TENANT, when the config declares tenants and one of them owns $SLUG.
    #
    # The API serves ONE identity per process and has no project directory to
    # derive it from — a service's working directory, and even its $HOME, say
    # nothing about which agent it belongs to. (The live engram-api unit sets no
    # HOME at all, so the derived slug there was "-", the slugification of "/".)
    # Naming it here keeps that explicit and visible to anyone reading the unit.
    # Store-scoped callers do not need it: the identity follows the slug.
    API_TENANT=""
    if [[ -f "$CLAUDE/engram.yaml" ]] && command -v python3 >/dev/null; then
      API_TENANT="$(python3 - "$CLAUDE/engram.yaml" "$SLUG" <<'PYT' 2>/dev/null || true
import sys
try:
    import yaml
except Exception:
    sys.exit(0)
try:
    cfg = yaml.safe_load(open(sys.argv[1])) or {}
except Exception:
    sys.exit(0)
for name, entry in (cfg.get("tenants") or {}).items():
    if sys.argv[2] in [str(s).strip() for s in ((entry or {}).get("slugs") or [])]:
        print(name)
        break
PYT
)"
    fi
    { echo "ENGRAM_BIN=$CLAUDE"; echo "ENGRAM_GRAPH=$CLAUDE/graph"; echo "ENGRAM_CONFIG=$CLAUDE/engram.yaml"; echo "ENGRAM_LOG_DIR=$CLAUDE/logs"; echo "CLAUDE_MEMORY_SLUG=$SLUG";
      [[ -n "$API_TENANT" ]] && echo "ENGRAM_TENANT=$API_TENANT";
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
    sed "s|%h/.claude|$CLAUDE|g" "$REPO/daemon/engram-api.service" > "$HOME/.config/systemd/user/engram-api.service"
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
      if [[ -x "$CLAUDE/rust/engram-app" ]]; then
        systemctl --user enable --now engram-api.service 2>/dev/null \
          && say "Rust API enabled on 127.0.0.1:8787" \
          || warn "could not enable engram-api.service"
      fi
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

# Optional add-ons, gated ONLY on their own flags — NOT on the daemon mode. They
# were previously inside the systemd arm above, so `--backup`/`--silverbullet`
# silently did nothing under the default `--daemon none`. Each provision function
# creates and 0600s its own env file, so neither depends on daemon.env existing.
[[ "$WANT_BACKUP" == "yes" ]] && provision_backup
# Web auth BEFORE SilverBullet: it writes the shared SB_USER_* into
# silverbullet.env, which provision_silverbullet then honours (it only generates
# a password when one is absent), so a single credential logs into both.
[[ "$WANT_WEB_AUTH" == "yes" ]] && provision_web_auth
[[ "$WANT_SILVERBULLET" == "yes" ]] && provision_silverbullet

say "done. Restart Claude Code so it loads the new commands + MCP server."
say "To run engram AUTONOMOUSLY (unattended harvest/graduate/curate + Telegram approvals),"
say "see AUTONOMY.md — set the backend, the auto_* flags in engram.yaml, and the Telegram token in $HOME/.config/engram/daemon.env."
