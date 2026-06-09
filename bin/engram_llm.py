#!/usr/bin/env python3
"""engram_llm.py — backend-agnostic LLM + embedding provider for engram.

Every pipeline LLM/embedding call routes through here, so the same engine code
runs identically whether you have a GPU (Ollama) or not (Claude-only).

Two generation backends + one always-local embedding path:
  backend: ollama   -> Ollama native API; model per mixture-of-experts ROLE,
                       scaled by a hardware TIER preset (cpu/small/medium/large).
  backend: claude   -> shells `claude -p` (no GPU; cost = Claude usage). Used by
                       the always-on loop container.
  embed()           -> Ollama `nomic-embed-text` when on the ollama backend and
                       reachable, ELSE CPU `fastembed` (nomic-embed-text-v1.5).
                       Both are 768-dim, so the graph's vector space is stable
                       across backends and never needs a paid embeddings API.

Config is read via memory_ai.load() (engram.yaml). Relevant keys:
  backend: ollama|claude
  tier:    cpu|small|medium|large           # ollama model preset
  experts: { <role>: { model: "<name>" } }  # OPTIONAL per-role override (wins over tier)
  ollama:  { host, timeout_seconds, num_ctx, num_predict, keep_alive, reasoning_effort }
  claude:  { bin, model, timeout_seconds, max_turns }
  embed:   { fastembed_model, dim }

CLI:
  engram_llm.py --check                 # probe the active backend + embeddings
  engram_llm.py --model <role>          # print the resolved model for a role
  engram_llm.py --generate <role>       # read prompt on stdin, print completion
  engram_llm.py --embed                 # read text on stdin, print JSON vector
"""
from __future__ import annotations
import json, os, subprocess, sys, urllib.request
from pathlib import Path

# Make sibling modules importable; the sibling (this dir) wins over ~/.claude so
# local testing uses the engram copy. Post-install both are ~/.claude anyway.
sys.path.insert(0, str(Path(__file__).resolve().parent))
if str(Path.home() / ".claude") not in sys.path:
    sys.path.append(str(Path.home() / ".claude"))
import memory_ai  # config loader (no engram_llm import there = no import cycle)

# ---------------------------------------------------------------------------
# Tier presets: hardware class -> {role: ollama model}. A per-role override in
# engram.yaml `experts.<role>.model` always wins. Roles: harvest/triage/distill/
# injection/verify/similarity (similarity is the embedding model).
# ---------------------------------------------------------------------------
TIER_PRESETS = {
    "cpu": {  # no GPU — tiny models (slow); most cpu users should prefer backend=claude
        "harvest": "llama3.2:1b", "triage": "llama3.2:1b", "distill": "llama3.2:3b",
        "injection": "llama3.2:3b", "verify": "llama3.2:3b", "similarity": "nomic-embed-text",
    },
    "small": {  # ~8 GB VRAM
        "harvest": "qwen2.5-coder:7b", "triage": "llama3.2:3b", "distill": "llama3.1:8b",
        "injection": "llama3.1:8b", "verify": "llama3.1:8b", "similarity": "nomic-embed-text",
    },
    "medium": {  # ~16-24 GB VRAM
        "harvest": "qwen2.5-coder:7b", "triage": "llama3.2:3b", "distill": "gpt-oss:20b",
        "injection": "deepseek-r1:14b", "verify": "deepseek-r1:14b", "similarity": "nomic-embed-text",
    },
    "large": {  # >= 32 GB VRAM
        "harvest": "qwen2.5-coder:7b", "triage": "llama3.2:3b", "distill": "qwen3-coder:30b",
        "injection": "deepseek-r1:32b", "verify": "deepseek-r1:32b", "similarity": "nomic-embed-text",
    },
}
_FALLBACK_MODEL = "llama3.1:8b"
DEFAULT_EMBED_MODEL = "nomic-ai/nomic-embed-text-v1.5"  # fastembed; 768-dim, matches Ollama nomic
DEFAULT_EMBED_DIM = 768


def _cfg(cfg=None):
    return cfg or memory_ai.load()

def backend(cfg=None) -> str:
    return (_cfg(cfg).get("backend") or "ollama").strip().lower()

def tier(cfg=None) -> str:
    return (_cfg(cfg).get("tier") or "small").strip().lower()

def model_for(role: str, cfg=None) -> str:
    """Resolve the model for a MoE role: explicit experts override > tier preset > fallback."""
    cfg = _cfg(cfg)
    e = cfg.get("experts", {}).get(role)
    if isinstance(e, dict) and e.get("model"):
        return e["model"]
    if isinstance(e, str) and e:
        return e
    preset = TIER_PRESETS.get(tier(cfg), TIER_PRESETS["small"])
    return preset.get(role) or preset.get("distill") or _FALLBACK_MODEL


# ---------------------------------------------------------------------------
# Generation
# ---------------------------------------------------------------------------
def generate(prompt: str, role: str = "distill", cfg=None) -> str:
    cfg = _cfg(cfg)
    b = backend(cfg)
    if b == "claude":
        return _claude_generate(prompt, role, cfg)
    return _ollama_generate(prompt, role, cfg)


def _ollama_generate(prompt: str, role: str, cfg) -> str:
    oc = cfg.get("ollama", {})
    options = {
        "temperature": float(oc.get("temperature", 0.2)),
        # num_ctx: cluster-distill prompts run ~6-8k tokens; Ollama's 4096 default
        # would silently TRUNCATE them. num_predict: reasoning models spend output
        # tokens on hidden CoT first; too low a cap returns an EMPTY response.
        "num_ctx": int(oc.get("num_ctx", 16384)),
        "num_predict": int(oc.get("num_predict", 8000)),
    }
    body = {"model": model_for(role, cfg), "prompt": prompt, "stream": False,
            "think": False, "options": options}  # think=False: standing convention
    if oc.get("keep_alive"):
        body["keep_alive"] = oc["keep_alive"]
    if oc.get("reasoning_effort"):
        body["reasoning_effort"] = oc["reasoning_effort"]
    req = urllib.request.Request(f"{_ollama_host(cfg)}/api/generate",
                                 data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=_timeout(cfg)) as r:
        return json.loads(r.read().decode())["response"]


def _claude_generate(prompt: str, role: str, cfg) -> str:
    """Headless generation via the Claude Code CLI. No tools, single turn — pure text.
    Flags are configurable (cfg['claude']) since they can vary by CLI version."""
    cc = cfg.get("claude", {})
    claude_bin = cc.get("bin", "claude")
    timeout = int(cc.get("timeout_seconds", 600))
    cmd = [claude_bin, "-p", prompt, "--output-format", "json",
           "--max-turns", str(cc.get("max_turns", 1))]
    if cc.get("model"):
        cmd += ["--model", cc["model"]]
    # Restrict tools to nothing — this is pure text generation in an unattended loop.
    if cc.get("allowed_tools_flag", "--allowedTools"):
        cmd += [cc.get("allowed_tools_flag", "--allowedTools"), cc.get("allowed_tools", "")]
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, check=True)
    except FileNotFoundError:
        raise RuntimeError(f"claude CLI not found (configured bin: {claude_bin!r})")
    except subprocess.CalledProcessError as e:
        raise RuntimeError(f"claude -p failed (exit {e.returncode}): {(e.stderr or '')[:300]}")
    try:
        data = json.loads(out.stdout)
        return data.get("result", "") if isinstance(data, dict) else str(data)
    except json.JSONDecodeError:
        return out.stdout.strip()  # tolerate plain-text output


# ---------------------------------------------------------------------------
# Embeddings — always local, never a paid API. 768-dim in both paths.
# ---------------------------------------------------------------------------
_FE_MODEL = None

def embed(text: str, cfg=None):
    cfg = _cfg(cfg)
    if backend(cfg) == "ollama":
        try:
            return _ollama_embed(text, cfg)
        except Exception:
            pass  # GPU/Ollama down -> fall back to CPU fastembed (still 768-dim nomic)
    return _fastembed_embed(text, cfg)

def embed_dim(cfg=None) -> int:
    return int(_cfg(cfg).get("embed", {}).get("dim", DEFAULT_EMBED_DIM))

def _ollama_embed(text: str, cfg):
    model = model_for("similarity", cfg) or "nomic-embed-text"
    payload = {"model": model, "prompt": text}
    if cfg.get("ollama", {}).get("keep_alive"):
        payload["keep_alive"] = cfg["ollama"]["keep_alive"]
    req = urllib.request.Request(f"{_ollama_host(cfg)}/api/embeddings",
                                 data=json.dumps(payload).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=_timeout(cfg)) as r:
        return json.loads(r.read().decode())["embedding"]

def _fastembed_embed(text: str, cfg):
    global _FE_MODEL
    name = cfg.get("embed", {}).get("fastembed_model", DEFAULT_EMBED_MODEL)
    if _FE_MODEL is None or getattr(_FE_MODEL, "_engram_name", None) != name:
        try:
            from fastembed import TextEmbedding
        except ImportError:
            raise RuntimeError("fastembed not installed — `pip install fastembed` for the CPU embedding path")
        _FE_MODEL = TextEmbedding(model_name=name)
        _FE_MODEL._engram_name = name
    return [float(x) for x in next(iter(_FE_MODEL.embed([text])))]


# ---------------------------------------------------------------------------
# Shared helpers + health
# ---------------------------------------------------------------------------
def _ollama_host(cfg) -> str:
    return cfg.get("ollama", {}).get("host", "http://localhost:11434")

def _timeout(cfg) -> int:
    return int(cfg.get("ollama", {}).get("timeout_seconds", 600))

def health(cfg=None) -> dict:
    """Reachability of the active generation backend + the embedding path. For the daemon/GUI."""
    cfg = _cfg(cfg)
    b = backend(cfg)
    out = {"backend": b, "tier": tier(cfg), "generate": False, "embed": False, "detail": ""}
    try:
        if b == "claude":
            cc = cfg.get("claude", {})
            subprocess.run([cc.get("bin", "claude"), "--version"],
                           capture_output=True, timeout=30, check=True)
        else:
            _ollama_generate("reply ok", "triage", cfg)
        out["generate"] = True
    except Exception as ex:
        out["detail"] = f"generate: {ex}"
    try:
        v = embed("ping", cfg)
        out["embed"] = bool(v)
        out["embed_dim"] = len(v)
    except Exception as ex:
        out["detail"] = (out["detail"] + f"; embed: {ex}").strip("; ")
    return out


def main():
    args = sys.argv[1:]
    cfg = memory_ai.load()
    if "--model" in args:
        print(model_for(args[args.index("--model") + 1], cfg)); return
    if "--generate" in args:
        role = args[args.index("--generate") + 1] if len(args) > args.index("--generate") + 1 else "distill"
        print(generate(sys.stdin.read(), role, cfg)); return
    if "--embed" in args:
        print(json.dumps(embed(sys.stdin.read().strip(), cfg))); return
    # default / --check
    h = health(cfg)
    print(f"backend: {h['backend']}   tier: {h['tier']}")
    if h["backend"] == "ollama":
        print(f"ollama host: {_ollama_host(cfg)}")
        for role in ("harvest", "triage", "distill", "injection", "verify", "similarity"):
            print(f"  {role:<11} -> {model_for(role, cfg)}")
    print(f"generate reachable: {h['generate']}")
    print(f"embed reachable: {h['embed']}" + (f" (dim={h.get('embed_dim')})" if h.get("embed_dim") else ""))
    if h["detail"]:
        print(f"detail: {h['detail']}")


if __name__ == "__main__":
    main()
