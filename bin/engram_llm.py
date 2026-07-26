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
import json, os, re, subprocess, sys, urllib.request
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
def fallback(cfg=None) -> str:
    return (_cfg(cfg).get("fallback") or "").strip().lower()

def _ccg_generate(prompt: str, role: str, cfg) -> str:
    """Generation via cc-gateway (ccg): the Claude Code CLI pointed at the ccg OAuth
    proxy (ANTHROPIC_BASE_URL) with the ccg client key (ANTHROPIC_API_KEY). ccg swaps
    the client key for the real Claude.ai OAuth token, so this works HEADLESS (no local
    OAuth session needed) — unlike raw `claude`. The key is read from an env var
    (default ENGRAM_CCG_KEY) so it lives in the service EnvironmentFile, never the repo."""
    gc = cfg.get("ccg", {})
    base_url = gc.get("base_url") or os.environ.get("ANTHROPIC_BASE_URL")
    if not base_url:
        raise RuntimeError("ccg backend: no base_url configured (ccg.base_url or ANTHROPIC_BASE_URL)")
    # Require the configured key env EXPLICITLY. No implicit ANTHROPIC_API_KEY
    # fallback: that would ship the REAL Anthropic key to the gateway as the client
    # key (a compromised gateway could then impersonate/charge the account). An
    # operator who genuinely wants that must set api_key_env: ANTHROPIC_API_KEY.
    key_env = gc.get("api_key_env", "ENGRAM_CCG_KEY")
    key = os.environ.get(key_env)
    if not key:
        raise RuntimeError(f"ccg backend: api key env {key_env!r} not set")
    # ccg reuses the claude CLI path/flags; override model from the ccg block if given.
    sub = dict(cfg)
    if gc.get("model") or gc.get("bin") or gc.get("timeout_seconds"):
        cc = dict(cfg.get("claude", {}))
        for k in ("model", "bin", "timeout_seconds", "max_turns"):
            if gc.get(k) is not None:
                cc[k] = gc[k]
        sub = {**cfg, "claude": cc}
    return _claude_generate(prompt, role, sub,
                            env_extra={"ANTHROPIC_BASE_URL": base_url, "ANTHROPIC_API_KEY": key},
                            label="ccg")


def generate(prompt: str, role: str = "distill", cfg=None) -> str:
    cfg = _cfg(cfg)
    b = backend(cfg)
    fb = fallback(cfg)
    if b == "claude":
        return _claude_generate(prompt, role, cfg)
    if b == "ccg":
        # Route through cc-gateway; fall back to RAW claude (OAuth) ONLY on transport
        # unavailability (gateway down/unreachable) — NEVER on an auth/policy denial,
        # which would route the prompt around the gateway's auth/audit/DLP boundary.
        try:
            return _ccg_generate(prompt, role, cfg)
        except BackendAuthError:
            raise                                   # fail closed
        except Exception:
            if fb == "claude":
                return _claude_generate(prompt, role, cfg)
            raise
    if b == "llama_cpp":
        try:
            return _llamacpp_generate(prompt, role, cfg)
        except Exception:
            return _fallback_generate(prompt, role, cfg, fb)
    # ollama primary; optional ccg/claude fallback when the GPU box is unreachable.
    try:
        return _ollama_generate(prompt, role, cfg)
    except Exception:
        return _fallback_generate(prompt, role, cfg, fb)


def _fallback_generate(prompt, role, cfg, fb):
    """Run the configured fallback backend. A ccg auth/policy denial fails closed
    (propagates) rather than degrading to another path."""
    if fb == "ccg":
        return _ccg_generate(prompt, role, cfg)     # BackendAuthError propagates
    if fb == "claude":
        return _claude_generate(prompt, role, cfg)
    raise RuntimeError("primary backend failed and no fallback configured")


# Qwen3 and other reasoning models emit <think>...</think> before the answer; with
# format-constrained or JSON-expecting callers that collides. Strip it defensively.
_THINK_RE = re.compile(r"<think>.*?</think>\s*", re.S | re.I)

def _strip_think(text: str) -> str:
    t = _THINK_RE.sub("", text or "")
    # A DANGLING </think> with no opener is the common ollama case: the chat
    # template emits the opening tag itself, so `response` starts mid-CoT and ends
    # with just the closer. Neither the regex nor the unclosed-opener branch below
    # catches that, which let whole reasoning blocks through. Cut at the LAST
    # closer whenever one is present, regardless of an opener.
    low = t.lower()
    if "</think>" in low:
        t = t[low.rfind("</think>") + len("</think>"):]
    # tolerate an unclosed <think> (truncated CoT): drop from the opener.
    elif "<think>" in low:
        t = t[: low.find("<think>")]
    return t.strip()


def _llamacpp_generate(prompt: str, role: str, cfg) -> str:
    """Generation via an OpenAI-compatible llama.cpp server (llama-server /v1).
    Single user message, no tools — pure text. Honors num_predict as max_tokens."""
    lc = cfg.get("llama_cpp", {})
    url = (lc.get("url") or "http://localhost:8080/v1").rstrip("/")
    oc = cfg.get("ollama", {})
    body = {
        "model": lc.get("model") or "local",
        "messages": [{"role": "user", "content": prompt}],
        "temperature": float(lc.get("temperature", oc.get("temperature", 0.2))),
        "max_tokens": int(lc.get("max_tokens", oc.get("num_predict", 8000))),
        "stream": False,
    }
    headers = {"Content-Type": "application/json"}
    if lc.get("api_key"):
        headers["Authorization"] = f"Bearer {lc['api_key']}"
    req = urllib.request.Request(f"{url}/chat/completions",
                                 data=json.dumps(body).encode(), headers=headers)
    timeout = int(lc.get("timeout_seconds", _timeout(cfg)))
    with urllib.request.urlopen(req, timeout=timeout) as r:
        data = json.loads(r.read().decode())
    return _strip_think(data["choices"][0]["message"]["content"])


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
    # think: ollama's thinking toggle, config-driven (default False, unchanged).
    # reasoning_effort below is a NO-OP while think is False, so pinning a
    # reasoning model to "medium" previously did nothing at all.
    body = {"model": model_for(role, cfg), "prompt": prompt, "stream": False,
            "think": bool(oc.get("think", False)), "options": options}
    if oc.get("keep_alive"):
        body["keep_alive"] = oc["keep_alive"]
    if oc.get("reasoning_effort"):
        body["reasoning_effort"] = oc["reasoning_effort"]
    req = urllib.request.Request(f"{_ollama_host(cfg)}/api/generate",
                                 data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=_timeout(cfg)) as r:
        # Strip CoT here too. The llama.cpp path already did this; ollama did not,
        # so a reasoning model's <think> block leaked into JSON-expecting callers
        # (harvest/distill) — which is why `think` was pinned False upstream.
        return _strip_think(json.loads(r.read().decode())["response"])


# A "not authenticated" failure is PERMANENT within a run (expired OAuth with no
# session to refresh it, or a bad/missing api key) — retrying it 3× just burns time
# and buries the real cause under empty exit-1s (this is what produced 169k silent
# "claude -p failed (exit 1)" lines). Detect it and fail fast + clearly.
_AUTH_FAIL_RE = re.compile(
    r"not logged in|please run /login|invalid[_ ]?api[_ ]?key|unauthor|authentication|forbidden|"
    r"\b40[13]\b|policy|blocked",
    re.IGNORECASE)


class BackendAuthError(RuntimeError):
    """Auth / authorization / policy denial from a backend. Distinct from transport
    failure: callers must NOT silently fall back to another backend on this, or they
    route around the gateway's auth/audit/DLP boundary (a data-exfil path)."""


def _claude_generate(prompt: str, role: str, cfg, env_extra=None, label="claude -p") -> str:
    """Headless generation via the Claude Code CLI. No tools, single turn — pure text.
    Flags are configurable (cfg['claude']) since they can vary by CLI version.
    `env_extra` injects env vars into the subprocess (used by the ccg backend to set
    ANTHROPIC_BASE_URL / ANTHROPIC_API_KEY so the call routes through cc-gateway)."""
    cc = cfg.get("claude", {})
    claude_bin = cc.get("bin", "claude")
    timeout = int(cc.get("timeout_seconds", 600))
    cmd = [claude_bin, "-p", prompt, "--output-format", "text",
           "--max-turns", str(cc.get("max_turns", 1))]
    if cc.get("model"):
        cmd += ["--model", cc["model"]]
    # Restrict tools to nothing — this is pure text generation in an unattended loop.
    if cc.get("allowed_tools_flag", "--allowedTools"):
        cmd += [cc.get("allowed_tools_flag", "--allowedTools"), cc.get("allowed_tools", "")]
    env = None
    if env_extra:
        env = dict(os.environ)
        env.update({k: v for k, v in env_extra.items() if v is not None})
    # The unattended timer window can hit transient failures (OAuth token refresh,
    # a brief usage cap, a flaky spawn) that surface as exit 1 with empty stderr —
    # retry those. But an AUTH failure is permanent within the run: fail fast.
    import time as _time
    attempts = max(1, int(cc.get("retries", 3)))
    backoff = float(cc.get("retry_backoff_seconds", 5))
    last_err = None
    for attempt in range(attempts):
        try:
            out = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout,
                                 check=True, env=env)
            break
        except FileNotFoundError:
            raise RuntimeError(f"claude CLI not found (configured bin: {claude_bin!r})")
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as e:
            blob = ((getattr(e, "stderr", "") or "") + (getattr(e, "stdout", "") or ""))[:400]
            kind = "timeout" if isinstance(e, subprocess.TimeoutExpired) else f"exit {e.returncode}"
            if _AUTH_FAIL_RE.search(blob):
                raise BackendAuthError(
                    f"{label} auth/policy denied ({blob.strip() or 'no detail'}) — non-retryable; "
                    f"check credentials. NOT falling back (would bypass the gateway boundary).")
            last_err = RuntimeError(f"{label} failed ({kind}): {blob.strip()}")
            if attempt < attempts - 1:
                _time.sleep(backoff * (attempt + 1))
    else:
        raise last_err
    # With --output-format text the CLI prints only the final result text. Some CLI
    # versions still emit stream-json (a LIST of events, or a single dict) even so —
    # extract the result defensively so the caller never ingests raw event wrappers.
    text = out.stdout.strip()
    if text[:1] in ("[", "{"):
        try:
            data = json.loads(text)
            if isinstance(data, dict):
                return data.get("result", text)
            if isinstance(data, list):
                for e in reversed(data):
                    if isinstance(e, dict) and e.get("type") == "result" \
                            and isinstance(e.get("result"), str):
                        return e["result"]
        except json.JSONDecodeError:
            pass
    return text


# ---------------------------------------------------------------------------
# Embeddings — always local, never a paid API. 768-dim in both paths.
# ---------------------------------------------------------------------------
_FE_MODEL = None

def _embed_provider(cfg) -> str:
    """Embedding provider, chosen INDEPENDENTLY of the generation backend so a
    llama.cpp/claude backend can still embed via Ollama. Explicit `embed.provider`
    wins; otherwise default to ollama when the generation backend is ollama, else
    the local CPU fastembed path."""
    p = (cfg.get("embed", {}).get("provider") or "").strip().lower()
    if p in ("ollama", "fastembed"):
        return p
    return "ollama" if backend(cfg) == "ollama" else "fastembed"

def embed(text: str, cfg=None):
    cfg = _cfg(cfg)
    if _embed_provider(cfg) == "ollama":
        try:
            return _ollama_embed(text, cfg)
        except Exception:
            pass  # Ollama unreachable -> fall back to CPU fastembed
    return _fastembed_embed(text, cfg)

def embed_dim(cfg=None) -> int:
    return int(_cfg(cfg).get("embed", {}).get("dim", DEFAULT_EMBED_DIM))

def _ollama_embed(text: str, cfg):
    # explicit embed.model wins (e.g. bge-m3); else the MoE similarity role; else nomic.
    model = cfg.get("embed", {}).get("model") or model_for("similarity", cfg) or "nomic-embed-text"
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

_HEALTH_CACHE = {"t": 0.0, "result": None}
_HEALTH_TTL = int(os.environ.get("ENGRAM_HEALTH_TTL", "600"))   # seconds

def health(cfg=None, force=False) -> dict:
    """Reachability of the active generation backend + the embedding path. For the
    daemon/GUI. CACHED for _HEALTH_TTL so a `ccg`/`claude` probe isn't a real LLM
    round-trip on every 30-min tick (each burns OAuth quota and can hang) — and so
    task_maintenance's _generate_available() check reuses the tick's probe."""
    import time as _time
    if not force and _HEALTH_CACHE["result"] is not None \
            and (_time.time() - _HEALTH_CACHE["t"]) < _HEALTH_TTL:
        return _HEALTH_CACHE["result"]
    cfg = _cfg(cfg)
    b = backend(cfg)
    out = {"backend": b, "tier": tier(cfg), "generate": False, "embed": False, "detail": ""}
    try:
        if b == "claude":
            cc = cfg.get("claude", {})
            subprocess.run([cc.get("bin", "claude"), "--version"],
                           capture_output=True, timeout=30, check=True)
        elif b == "ccg":
            # real round-trip through the proxy — a --version check wouldn't exercise auth
            _ccg_generate("reply ok", "triage", cfg)
        elif b == "llama_cpp":
            _llamacpp_generate("reply ok", "triage", cfg)
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
    _HEALTH_CACHE["t"] = _time.time(); _HEALTH_CACHE["result"] = out
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
