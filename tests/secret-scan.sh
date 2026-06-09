#!/usr/bin/env bash
# secret-scan.sh — gate before publishing engram. Fails (exit 1) on any real
# secret, personal/infra identifier, real IP, the private memory repo, or a
# forbidden file. Run from anywhere: tests/secret-scan.sh [dir]
# Allowlisted (intentional): pagcoinbr/engram (the public target), RFC5737 doc IPs
# (203.0.113.x), RFC1918 example IPs (10.0.0.x), 127.0.0.1/localhost, api_key="ollama".
set -uo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"
FAIL=0
hr(){ printf '%s\n' "------------------------------------------------------------"; }

# Files to scan (skip .git; skip THIS scanner — it legitimately contains the patterns).
mapfile -t FILES < <( { git ls-files 2>/dev/null || find . -type f -not -path './.git/*'; } | grep -vE 'secret-scan\.sh$')

scan(){ # scan <label> <regex>  -> prints hits, sets FAIL
  local label="$1" re="$2" hits
  hits=$(grep -rInE "$re" "${FILES[@]}" 2>/dev/null | grep -vE 'pagcoinbr/engram' || true)
  if [[ -n "$hits" ]]; then echo "❌ $label:"; printf '%s\n' "$hits"; hr; FAIL=1
  else echo "✅ $label: clean"; fi
}

echo "engram secret scan — $ROOT"; hr

# 1. Real secret formats (high confidence).
scan "secret formats (sk-/ghp_/AKIA/private-key/telegram/long-hex)" \
  'sk-[A-Za-z0-9]{20,}|gh[pose]_[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|-----BEGIN [A-Z ]+PRIVATE KEY-----|[0-9]{9,10}:AA[A-Za-z0-9_-]{30,}|\b[a-f0-9]{100,}\b'

# 2. Personal / infra identifiers (the operator's real world).
scan "personal/infra identifiers" \
  'paguebit|alfred|brlnos|dsecbolt|minibolt|sideswap|depix|lnbits|amantikir|hastydev|vault-otc|clube|lfmolinadc|pagcoinbr/claude-memory|-home-pagcoin'
# pagcoin (bare) — allow only the public repo string
PG=$(grep -rInE 'pagcoin' "${FILES[@]}" 2>/dev/null | grep -vE 'pagcoinbr/engram' || true)
if [[ -n "$PG" ]]; then echo "❌ bare 'pagcoin' (non-public-repo):"; printf '%s\n' "$PG"; hr; FAIL=1; else echo "✅ bare 'pagcoin': clean (only pagcoinbr/engram)"; fi

# 3. Real IPs (the operator's LAN / Tailscale / VPS). Doc/RFC ranges allowed.
scan "real IPs (192.168.x / 173.212.213.180 / tailscale 100.64-127.x)" \
  '192\.168\.[0-9]{1,3}\.[0-9]{1,3}|173\.212\.213\.180|100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]{1,3}\.[0-9]{1,3}'

# 4. Forbidden files — HARD-fail only on TRACKED files (those actually publish).
#    Untracked, gitignored cruft (e.g. __pycache__ from a local py_compile) is noted
#    but never fails the gate, since it won't be pushed.
echo "checking for forbidden files…"
BAD=$(printf '%s\n' "${FILES[@]}" | grep -iE '(^|/)\.env$|\.pem$|\.key$|server_creds|\.macaroon$|insert_state\.json$|sync_state\.json$|(^|/)__pycache__/|\.pyc$|/venv/|/neo4j/data/|/extractions/' | grep -vE '\.env\.example$' || true)
if [[ -n "$BAD" ]]; then echo "❌ forbidden files TRACKED (would publish):"; printf '%s\n' "$BAD"; hr; FAIL=1
else echo "✅ forbidden files (tracked): none"; fi
DISK=$(find . -type d \( -name __pycache__ -o -name venv -o -path '*/neo4j/data' \) 2>/dev/null | sed '/^$/d')
[[ -n "$DISK" ]] && echo "ℹ️  on-disk but gitignored (NOT published): $(echo "$DISK" | tr '\n' ' ')"

# 5. Informational: generic secret-ish assignments to eyeball (not a hard fail).
echo; echo "ℹ️  review (generic secret-ish assignments — confirm none hold a real value):"
grep -rInE '(password|passwd|secret|api[_-]?key|token|bearer)["'"'"']?[[:space:]]*[:=]' "${FILES[@]}" 2>/dev/null \
  | grep -vE 'api_key="ollama"|NEO4J_PASSWORD=\$|NEO4J_PASSWORD=$|ANTHROPIC_API_KEY=\$|ANTHROPIC_API_KEY=$|x-service-key|description|# ' | head -40 || true

hr
if [[ "$FAIL" == 0 ]]; then echo "✅ SECRET SCAN PASSED — safe to publish"; else echo "❌ SECRET SCAN FAILED — do NOT publish"; fi
exit $FAIL
