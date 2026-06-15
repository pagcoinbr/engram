#!/usr/bin/env python3
"""memory_ai.py — local-AI config + routing for the memory system.

Reads ~/.claude/memory_ai.yaml: the local on/off switch, the Ollama endpoint,
the mixture-of-experts (role -> model) map, light-pass settings, and declared
local MCP servers. Other scripts (memory_distill.py, memory_light_curate.py,
the fixation cron) import these helpers so model/endpoint choices live in ONE
editable place.

CLI:
  memory_ai.py --get <dotted.key>   # print a value (for bash; bools -> true/false)
  memory_ai.py --check              # summarize config + probe model reachability
"""
from __future__ import annotations
import json, os, re, sys, urllib.request
from pathlib import Path

HOME = Path.home()
CONFIG_PATH = Path(os.environ.get("ENGRAM_CONFIG")
                   or os.environ.get("MEMORY_AI_CONFIG")          # back-compat
                   or (HOME / ".claude" / "engram.yaml"))

_DEFAULTS = {
    "local_enabled": True,
    "ollama": {"host": "http://localhost:11434", "timeout_seconds": 1200,
               "num_ctx": 16384, "num_predict": 8000, "reasoning_effort": "low"},
    # backend + tier drive model choice (see engram_llm.TIER_PRESETS). `experts`
    # is an OPTIONAL per-role override map, e.g. {"distill": {"model": "..."}}.
    "backend": "ollama",                 # ollama | claude | llama_cpp
    "fallback": "",                      # "" | claude — used when the primary backend is unreachable
    "tier": "small",                     # cpu | small | medium | large  (ollama only)
    "experts": {},
    "claude": {"bin": "claude", "timeout_seconds": 600, "max_turns": 1},
    # OpenAI-compatible llama.cpp server (llama-server /v1). Used when backend=llama_cpp.
    "llama_cpp": {"url": "http://localhost:8080/v1", "model": "local",
                  "timeout_seconds": 600, "max_tokens": 8000, "api_key": ""},
    # embed.provider chooses the embedding path independently of the generation backend
    # (ollama | fastembed). embed.model overrides the Ollama embedding model (e.g. bge-m3).
    "embed": {"provider": "", "model": "", "fastembed_model": "nomic-ai/nomic-embed-text-v1.5", "dim": 768},
    "light_pass": {
        "enabled": True,
        "duplicate_finder": {"enabled": True, "expert": "similarity", "dup_threshold": 0.86},
        "injection_guard": {"enabled": True, "quarantine_suspects": True},
        "draft_distill": True,
        "max_items": 60,
    },
    "session_curate": {"enabled": True, "min_minutes": 15, "min_new_memories": 3, "max_per_day": 2},
    # Hybrid recall: fuse graph + vector + keyword (BM25) via Reciprocal Rank Fusion.
    "recall": {
        "hybrid": {"enabled": True, "k_rrf": 60, "default_k": 6,
                   "weights": {"graph": 1.0, "vector": 1.0, "keyword": 1.0}},
        "scope_to_slug": True,   # restrict vector/hybrid recall to the current store's slug
    },
    # OPTIONAL Qdrant vector index over the .md store (semantic recall + fast dedup).
    # OFF by default: disabled or unreachable => fall back to pure markdown.
    "vector_store": {
        "enabled": False,
        "provider": "qdrant",
        "url": "http://127.0.0.1:6333",
        "api_key": "",
        "collection": "engram_memory",
        "on_disk": False,
        "timeout_seconds": 30,
        "recall": {"default_k": 6, "threshold": 0.0},
        "duplicate_finder": {"use_vector_store": True},
    },
    "schedule": {"times": ["03:30"]},
}

def _deep_merge(base: dict, over: dict) -> dict:
    for k, v in (over or {}).items():
        if isinstance(v, dict) and isinstance(base.get(k), dict):
            _deep_merge(base[k], v)
        else:
            base[k] = v
    return base

def load() -> dict:
    cfg = json.loads(json.dumps(_DEFAULTS))   # deep copy
    if CONFIG_PATH.exists():
        try:
            import yaml
            _deep_merge(cfg, yaml.safe_load(CONFIG_PATH.read_text()) or {})
        except Exception as e:
            print(f"[memory_ai] warning: cannot parse {CONFIG_PATH}: {e}", file=sys.stderr)
    return cfg

def local_enabled(cfg=None) -> bool:
    return bool((cfg or load()).get("local_enabled", True))

def vector_enabled(cfg=None) -> bool:
    """True only when the local switch AND the optional vector store are both on.
    The single place callers check before touching Qdrant (else: markdown fallback)."""
    cfg = cfg or load()
    return bool(local_enabled(cfg)) and bool(cfg.get("vector_store", {}).get("enabled", False))

def recall_cfg(cfg=None) -> dict:
    """The hybrid-recall config block (k_rrf, default_k, weights, scope_to_slug).
    Used by the graph/vector MCP servers to fuse + scope recall."""
    return (cfg or load()).get("recall", {}) or {}

def scope_to_slug(cfg=None) -> bool:
    return bool(recall_cfg(cfg).get("scope_to_slug", True))

def ollama_host(cfg=None) -> str:
    return (cfg or load())["ollama"]["host"]

def expert_model(role: str, cfg=None) -> str:
    # Resolve via the provider abstraction: explicit experts override > tier preset.
    import engram_llm
    return engram_llm.model_for(role, cfg or load())

def _timeout(cfg) -> int:
    return int(cfg.get("ollama", {}).get("timeout_seconds", 180))

def ollama_generate(prompt: str, role: str = "distill", cfg=None) -> str:
    # Delegated to the backend-agnostic provider (ollama | claude). Kept under the
    # old name so every existing caller routes through engram_llm with no edits.
    import engram_llm
    return engram_llm.generate(prompt, role, cfg or load())

def ollama_embed(text: str, role: str = "similarity", cfg=None):
    # Delegated: Ollama nomic when reachable, else CPU fastembed (both 768-dim).
    import engram_llm
    return engram_llm.embed(text, cfg or load())

# A harvested candidate that the model self-labels as momentary STATUS is session
# noise, not a durable memory. This is a VALUE filter (worth-remembering), distinct
# from the injection/confidence SAFETY gates — the model is often 0.9-confident that
# "container is listening" is a durable project fact, so safety screening won't drop it.
_TRANSIENT_RE = re.compile(
    r"\b(status|listening|reachable|uptime|running|healthy|health[- ]?check|"
    r"state[- ]?check|final[- ]?state|recreat(?:e|ed|ion)|restarted|confirmation|is up)\b",
    re.I)


def is_transient_fact(name: str = "", description: str = "") -> bool:
    """True if a candidate looks like transient session status rather than a durable
    fact. Used to keep noise out of staging/recall (callers may still allow a human
    to save such a fact manually)."""
    return bool(_TRANSIENT_RE.search(f"{name} {description}"))


def _dig(cfg, dotted):
    cur = cfg
    for part in dotted.split("."):
        if isinstance(cur, dict) and part in cur:
            cur = cur[part]
        else:
            return None
    return cur

def main():
    args = sys.argv[1:]
    cfg = load()
    if "--get" in args:
        val = _dig(cfg, args[args.index("--get") + 1])
        if isinstance(val, bool):
            print("true" if val else "false")
        elif isinstance(val, (dict, list)):
            print(json.dumps(val))
        elif val is None:
            print("")
        else:
            print(val)
        return
    # default / --check
    print(f"config: {CONFIG_PATH} ({'loaded' if CONFIG_PATH.exists() else 'defaults only'})")
    print(f"local_enabled: {cfg.get('local_enabled')}")
    print(f"ollama host: {ollama_host(cfg)}")
    print("experts (MoE roles -> model):")
    for role, e in cfg.get("experts", {}).items():
        m = e.get("model") if isinstance(e, dict) else e
        print(f"  {role:<11} -> {m}")
    print(f"light_pass.enabled: {_dig(cfg, 'light_pass.enabled')}  "
          f"schedule: {_dig(cfg, 'schedule.times')}")
    if "--check" in args:
        try:
            ollama_generate("reply ok", role="triage", cfg=cfg)
            print("ollama generate (triage): reachable")
        except Exception as ex:
            print(f"ollama generate: UNREACHABLE ({ex})")
        try:
            v = ollama_embed("ping", cfg=cfg)
            print(f"ollama embeddings (similarity): reachable (dim={len(v)})")
        except Exception as ex:
            print(f"ollama embeddings: UNREACHABLE ({ex})")

if __name__ == "__main__":
    main()
