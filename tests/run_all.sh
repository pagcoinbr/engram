#!/usr/bin/env bash
# Run the whole suite: Rust workspace, then the Python and shell tests.
#
# The suites are a deliberate mix — plain scripts with a main(), module-level
# asserts, and bash e2e harnesses — and there was no single entry point, so CI had
# nothing to call and "the tests pass" meant whatever the person saying it had run.
# Everything here is self-contained: the Python tests build a throwaway $HOME and
# load modules by path, so no venv or install is required.
#
# Tests needing a live service (Neo4j, Qdrant, an LLM endpoint) must SKIP, not
# fail, when it is absent — they print "skip — ..." and exit 0.
#
# Usage:
#   tests/run_all.sh            # everything
#   tests/run_all.sh --rust     # Rust only
#   tests/run_all.sh --python   # Python + shell only
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Tests that exercise engram_llm.generate() now write LLM-audit events; keep them
# out of the operator's real ~/.claude/logs/llm_events.jsonl by pointing the audit
# log at a throwaway dir for the whole run.
ENGRAM_AUDIT_TMP="$(mktemp -d)"
export ENGRAM_LOG_DIR="$ENGRAM_AUDIT_TMP"
trap 'rm -rf "$ENGRAM_AUDIT_TMP"' EXIT

WANT_RUST=yes
WANT_PYTHON=yes
case "${1:-}" in
  --rust)   WANT_PYTHON=no ;;
  --python) WANT_RUST=no ;;
  "") ;;
  *) echo "usage: $0 [--rust|--python]" >&2; exit 2 ;;
esac

PASS=0
FAIL=0
SKIP=0
FAILED_NAMES=()

run_one() {
  local name="$1"; shift
  local output status
  output="$("$@" 2>&1)"
  status=$?
  if [[ $status -ne 0 ]]; then
    FAIL=$((FAIL + 1)); FAILED_NAMES+=("$name")
    printf 'FAIL  %s\n' "$name"
    printf '%s\n' "$output" | sed 's/^/        /'
  elif printf '%s' "$output" | grep -qi '^skip'; then
    SKIP=$((SKIP + 1))
    printf 'SKIP  %s — %s\n' "$name" "$(printf '%s' "$output" | grep -i '^skip' | head -1)"
  else
    PASS=$((PASS + 1))
    printf 'ok    %s\n' "$name"
  fi
}

if [[ "$WANT_RUST" == yes ]]; then
  echo "── Rust workspace ────────────────────────────────────────────"
  if command -v cargo >/dev/null; then
    run_one "cargo fmt --check" cargo fmt --all --check
    run_one "cargo clippy" cargo clippy --workspace --all-targets --offline
    run_one "cargo test" cargo test --workspace --offline
  else
    SKIP=$((SKIP + 1)); echo "SKIP  cargo not installed"
  fi
fi

if [[ "$WANT_PYTHON" == yes ]]; then
  echo "── Python ────────────────────────────────────────────────────"
  # The detectors' self-checks are the cheapest real assertions we have.
  run_one "engram_secrets self-check" python3 bin/engram_secrets.py
  for t in tests/test_*.py; do
    run_one "$(basename "$t")" python3 "$t"
  done

  echo "── Shell ─────────────────────────────────────────────────────"
  for t in tests/test_*.sh; do
    run_one "$(basename "$t")" bash "$t"
  done
  # e2e_*.sh build a sandbox $HOME and are slower; opt in with ENGRAM_E2E=1.
  if [[ "${ENGRAM_E2E:-0}" == 1 ]]; then
    for t in tests/e2e_*.sh; do
      run_one "$(basename "$t")" bash "$t"
    done
  else
    echo "note  e2e_*.sh skipped (set ENGRAM_E2E=1 to include them)"
  fi
fi

echo "──────────────────────────────────────────────────────────────"
printf '%d passed, %d skipped, %d failed\n' "$PASS" "$SKIP" "$FAIL"
if [[ $FAIL -gt 0 ]]; then
  printf 'failed: %s\n' "${FAILED_NAMES[*]}"
  exit 1
fi
