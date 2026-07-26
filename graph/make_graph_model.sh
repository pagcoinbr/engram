#!/usr/bin/env bash
# make_graph_model.sh — build the Ollama variant used for graph extraction.
#
# Why this exists: Graphiti talks OpenAI-compat /v1, which has no num_ctx knob.
# Ollama therefore serves extraction prompts at its server default (commonly
# 4096) no matter how large a context the base model supports — long episodes
# are silently truncated, and truncated input produces plausible-looking but
# incomplete entity extraction. Baking num_ctx into a derived model is the only
# per-model way to set it.
#
# The base is whatever engram.yaml already resolves for the `distill` role, so
# this stays correct when you change models instead of pinning a stale name.
#
#   ./make_graph_model.sh                 # create/update `engram-graph`
#   MG_NUM_CTX=65536 ./make_graph_model.sh
#
# Then point the bootstrap at it:
#   export MG_LLM_MODEL=engram-graph MG_SMALL_MODEL=engram-graph
set -euo pipefail

NAME="${MG_GRAPH_MODEL:-engram-graph}"
NUM_CTX="${MG_NUM_CTX:-32768}"
BIN="${ENGRAM_BIN:-$HOME/.claude}"

BASE="${MG_BASE_MODEL:-$(python3 -c "
import sys; sys.path.insert(0, '$BIN')
import engram_llm; print(engram_llm.model_for('distill'))
")}"

[ -n "$BASE" ] || { echo "could not resolve a base model (set MG_BASE_MODEL)" >&2; exit 1; }
if ! ollama list | awk '{print $1}' | grep -qx "$BASE"; then
    echo "base model not present in ollama: $BASE" >&2
    echo "pull it first, or set MG_BASE_MODEL to one you have." >&2
    exit 1
fi

echo "building $NAME from $BASE (num_ctx=$NUM_CTX)"
# `ollama create -f -` does not read stdin, so stage a real file.
MF="$(mktemp)"; trap 'rm -f "$MF"' EXIT
# NOTE: no `PARAMETER think` — Ollama rejects it as an unknown parameter (0.32).
# Reasoning is injected per-request instead; see MG_REASONING_EFFORT in mg_config.py.
cat > "$MF" <<EOF
FROM $BASE
PARAMETER num_ctx $NUM_CTX
PARAMETER temperature 0
EOF
ollama create "$NAME" -f "$MF"

echo "done — export MG_LLM_MODEL=$NAME MG_SMALL_MODEL=$NAME"
