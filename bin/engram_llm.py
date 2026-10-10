#!/usr/bin/env python3
"""engram_llm.py — backend-agnostic LLM + embedding provider for engram.

Every pipeline LLM/embedding call routes through here, so the same engine code
runs identically whether you have a GPU (Ollama) or not (Claude-only).

Two generation backends + one always-local embedding path:
  backend: ollama   -> Ollama native API; model per mixture-of-experts ROLE,
                       scaled by a hardware TIER preset (cpu/small/medium/large).
  backend: claude   -> shells `claude -p` (no GPU; cost = Claude usage). Used by
                       the always-on loop container.
  embed()           -> the provider named in `embed.provider` (ollama, fastembed,
                       or llama_cpp/openai for an OpenAI-compatible server such as
                       bge-m3 under llama-server). With no provider set, Ollama
                       `nomic-embed-text` when on the ollama backend and reachable,
                       ELSE CPU `fastembed` — both 768-dim, so the auto path never
                       changes the vector space and never needs a paid API. An
                       EXPLICIT provider never falls back: see embed().

Config is read via memory_ai.load() (engram.yaml). Relevant keys:
  backend: ollama|claude
  tier:    cpu|small|medium|large           # ollama model preset
  experts: { <role>: { model: "<name>" } }  # OPTIONAL per-role override (wins over tier)
  ollama:  { host, timeout_seconds, num_ctx, num_predict, keep_alive, reasoning_effort }
  claude:  { bin, model, timeout_seconds, max_turns }
  embed:   { provider, url, model, api_key, timeout_seconds, fastembed_model, dim,
             query_prefix, document_prefix }

CLI:
  engram_llm.py --check                 # probe the active backend + embeddings
  engram_llm.py --model <role>          # print the resolved model for a role
  engram_llm.py --generate <role>       # read prompt on stdin, print completion
  engram_llm.py --embed                 # read text on stdin, print JSON vector
"""
from __future__ import annotations
import hashlib, json, os, re, shutil, struct, subprocess, sys, time, urllib.request
from pathlib import Path

# Make sibling modules importable; the sibling (this dir) wins over ~/.claude so
# local testing uses the engram copy. Post-install both are ~/.claude anyway.
sys.path.insert(0, str(Path(__file__).resolve().parent))
if str(Path.home() / ".claude") not in sys.path:
    sys.path.append(str(Path.home() / ".claude"))
import memory_ai  # config loader (no engram_llm import there = no import cycle)

# Audit of generation calls (loop visibility). Guarded: a missing module must never
# break generation — see bin/engram_llm_audit.py.
try:
    import engram_llm_audit as _audit
except Exception:  # pragma: no cover - defensive
    _audit = None


def _err_detail(exc) -> str:
    """A sanitized error label — class name (+ HTTP code), never raw stderr/reason,
    which could echo prompt or response text into the shared, cross-tenant log."""
    code = getattr(exc, "code", None)
    return f"{type(exc).__name__}:{code}" if code else type(exc).__name__


def _audit_gen(*, kind, backend, endpoint, model, role, prompt, call_id,
               attempt, t0, outcome, detail=""):
    """Record one generation attempt. Best-effort; never raises into the caller."""
    if _audit is None:
        return
    try:
        _audit.record(src="engram_llm", kind=kind, call_id=call_id, attempt=attempt,
                      backend=backend, endpoint=endpoint or "", model=model or "",
                      role=role or "", chars=len(prompt or ""),
                      digest=_audit.digest(prompt or ""),
                      ms=int((time.time() - t0) * 1000), outcome=outcome, detail=detail)
    except Exception:
        pass


def _call_id() -> str:
    return _audit.new_call_id() if _audit else ""

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

def _ccg_gateway_from_cli() -> str | None:
    """The gateway URL from the installed `ccg` launcher, if present. It's the single
    source of truth for where the working gateway lives, so when ccg.base_url isn't
    pinned we reuse it — engram then can't drift from a stale origin baked into the
    yaml (e.g. a raw IP that moved). Returns None if no `ccg` is on PATH / unreadable."""
    launcher = shutil.which("ccg")
    if not launcher:
        return None
    try:
        m = re.search(r'^GATEWAY_URL="?([^"\n]+?)"?\s*$', Path(launcher).read_text(), re.M)
    except (OSError, UnicodeDecodeError):   # a compiled `ccg` on PATH is not text
        return None
    return m.group(1) if m else None


def _ccg_generate(prompt: str, role: str, cfg, *, audit_kind: str = "generation") -> str:
    """Generation via cc-gateway (ccg): the Claude Code CLI pointed at the ccg OAuth
    proxy (ANTHROPIC_BASE_URL) with the ccg client key (ANTHROPIC_API_KEY). ccg swaps
    the client key for the real Claude.ai OAuth token, so this works HEADLESS (no local
    OAuth session needed) — unlike raw `claude`. The key is read from an env var
    (default ENGRAM_CCG_KEY) so it lives in the service EnvironmentFile, never the repo."""
    gc = cfg.get("ccg", {})
    # Explicit config wins; fall back to the installed `ccg` CLI's gateway host.
    base_url = gc.get("base_url") or os.environ.get("ANTHROPIC_BASE_URL") or _ccg_gateway_from_cli()
    if not base_url:
        # Preflight rejection happens before _claude_generate would log it — record
        # it here so a misconfigured ccg is visible in the audit, not just absent.
        _audit_gen(kind=audit_kind, backend="ccg", endpoint="", model="", role=role,
                   prompt=prompt, call_id=_call_id(), attempt=1, t0=time.time(),
                   outcome="error", detail="preflight_no_base_url")
        raise RuntimeError("ccg backend: no base_url configured (ccg.base_url, ANTHROPIC_BASE_URL, or an installed `ccg` CLI)")
    # Require the configured key env EXPLICITLY. No implicit ANTHROPIC_API_KEY
    # fallback: that would ship the REAL Anthropic key to the gateway as the client
    # key (a compromised gateway could then impersonate/charge the account). An
    # operator who genuinely wants that must set api_key_env: ANTHROPIC_API_KEY.
    key_env = gc.get("api_key_env", "ENGRAM_CCG_KEY")
    key = os.environ.get(key_env)
    if not key:
        _audit_gen(kind=audit_kind, backend="ccg", endpoint=base_url, model="", role=role,
                   prompt=prompt, call_id=_call_id(), attempt=1, t0=time.time(),
                   outcome="error", detail="preflight_no_key")
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
                            label="ccg", audit_kind=audit_kind)


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


def _llamacpp_generate(prompt: str, role: str, cfg, *, audit_kind: str = "generation") -> str:
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
    # engram callers want structured/JSON output, never chain-of-thought. Qwen3 and
    # other reasoning models emit <think> by default, which can exhaust the token
    # budget (truncated JSON) or return pure reasoning (empty after stripping).
    # Disable thinking via the chat template unless cfg explicitly opts in.
    if not bool(lc.get("enable_thinking", False)):
        body["chat_template_kwargs"] = {"enable_thinking": False}
    headers = {"Content-Type": "application/json"}
    if lc.get("api_key"):
        headers["Authorization"] = f"Bearer {lc['api_key']}"
    req = urllib.request.Request(f"{url}/chat/completions",
                                 data=json.dumps(body).encode(), headers=headers)
    timeout = int(lc.get("timeout_seconds", _timeout(cfg)))
    call_id, t0 = _call_id(), time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            data = json.loads(r.read().decode())
        text = _strip_think(data["choices"][0]["message"]["content"])
    except Exception as exc:
        _audit_gen(kind=audit_kind, backend="llama_cpp", endpoint=url, model=body["model"],
                   role=role, prompt=prompt, call_id=call_id, attempt=1, t0=t0,
                   outcome="error", detail=_err_detail(exc))
        raise
    _audit_gen(kind=audit_kind, backend="llama_cpp", endpoint=url, model=body["model"],
               role=role, prompt=prompt, call_id=call_id, attempt=1, t0=t0, outcome="ok")
    return text


def _ollama_generate(prompt: str, role: str, cfg, *, audit_kind: str = "generation") -> str:
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
    endpoint, model = _ollama_host(cfg), body["model"]
    call_id, t0 = _call_id(), time.time()
    try:
        with urllib.request.urlopen(req, timeout=_timeout(cfg)) as r:
            # Strip CoT here too. The llama.cpp path already did this; ollama did not,
            # so a reasoning model's <think> block leaked into JSON-expecting callers
            # (harvest/distill) — which is why `think` was pinned False upstream.
            text = _strip_think(json.loads(r.read().decode())["response"])
    except Exception as exc:
        _audit_gen(kind=audit_kind, backend="ollama", endpoint=endpoint, model=model,
                   role=role, prompt=prompt, call_id=call_id, attempt=1, t0=t0,
                   outcome="error", detail=_err_detail(exc))
        raise
    _audit_gen(kind=audit_kind, backend="ollama", endpoint=endpoint, model=model,
               role=role, prompt=prompt, call_id=call_id, attempt=1, t0=t0, outcome="ok")
    return text


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


def _claude_generate(prompt: str, role: str, cfg, env_extra=None, label="claude -p",
                     *, audit_kind: str = "generation") -> str:
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
    # One audit line per real subprocess attempt (so 3 retries = 3 lines sharing a
    # call_id). backend "ccg" vs "claude" is carried via the label; detail is a
    # sanitized token (exit code / timeout / auth), never the subprocess blob.
    _backend = "ccg" if label == "ccg" else "claude"
    _endpoint = (env_extra or {}).get("ANTHROPIC_BASE_URL") or "claude-cli"
    _model = cc.get("model") or "claude"
    _cid = _call_id()
    for attempt in range(attempts):
        t0 = time.time()
        try:
            out = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout,
                                 check=True, env=env)
            _audit_gen(kind=audit_kind, backend=_backend, endpoint=_endpoint, model=_model,
                       role=role, prompt=prompt, call_id=_cid, attempt=attempt + 1, t0=t0,
                       outcome="ok")
            break
        except FileNotFoundError:
            _audit_gen(kind=audit_kind, backend=_backend, endpoint=_endpoint, model=_model,
                       role=role, prompt=prompt, call_id=_cid, attempt=attempt + 1, t0=t0,
                       outcome="error", detail="FileNotFoundError")
            raise RuntimeError(f"claude CLI not found (configured bin: {claude_bin!r})")
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as e:
            blob = ((getattr(e, "stderr", "") or "") + (getattr(e, "stdout", "") or ""))[:400]
            kind = "timeout" if isinstance(e, subprocess.TimeoutExpired) else f"exit {e.returncode}"
            if _AUTH_FAIL_RE.search(blob):
                _audit_gen(kind=audit_kind, backend=_backend, endpoint=_endpoint, model=_model,
                           role=role, prompt=prompt, call_id=_cid, attempt=attempt + 1, t0=t0,
                           outcome="error", detail="auth")
                raise BackendAuthError(
                    f"{label} auth/policy denied ({blob.strip() or 'no detail'}) — non-retryable; "
                    f"check credentials. NOT falling back (would bypass the gateway boundary).")
            last_err = RuntimeError(f"{label} failed ({kind}): {blob.strip()}")
            _audit_gen(kind=audit_kind, backend=_backend, endpoint=_endpoint, model=_model,
                       role=role, prompt=prompt, call_id=_cid, attempt=attempt + 1, t0=t0,
                       outcome="error",
                       detail="timeout" if isinstance(e, subprocess.TimeoutExpired)
                       else f"exit_{e.returncode}")
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

PROVIDERS = ("ollama", "fastembed", "llama_cpp", "openai")

def _embed_provider(cfg) -> str:
    """Embedding provider, chosen INDEPENDENTLY of the generation backend so a
    llama.cpp/claude backend can still embed via Ollama. Explicit `embed.provider`
    wins; otherwise default to ollama when the generation backend is ollama, else
    the local CPU fastembed path."""
    p = (cfg.get("embed", {}).get("provider") or "").strip().lower()
    if p in PROVIDERS:
        return "llama_cpp" if p == "openai" else p
    return "ollama" if backend(cfg) == "ollama" else "fastembed"

def _embed_provider_is_explicit(cfg) -> bool:
    """True when the operator named a provider in `embed.provider`. An explicit
    choice is binding: we fail rather than silently answering from another one."""
    return (cfg.get("embed", {}).get("provider") or "").strip().lower() in PROVIDERS

def _check_embed_dim(vec, cfg):
    """Refuse a vector that does not belong to the configured embedding space.

    Qdrant and the graph both store one dimension per collection. Returning a
    768-dim vector into a 1024-dim index does not fail loudly at the call site —
    it fails much later as unexplainably bad recall, or as a rejected upsert — so
    the mismatch is caught here, where the cause is still visible.
    """
    want = (cfg.get("embed") or {}).get("dim")
    if want and int(want) != len(vec):
        raise RuntimeError(f"embedding dimension mismatch: got {len(vec)}, "
                           f"embed.dim is {int(want)} — check embed.model/provider")
    return vec

def _embed_endpoint(cfg) -> str:
    """Where embeddings are requested: `embed.url`, else the generation endpoint.
    Mirrors `Config::embed_endpoint` so both halves agree on the space."""
    ec = cfg.get("embed") or {}
    return (ec.get("url") or (cfg.get("llama_cpp") or {}).get("url") or "").strip()


def embedding_space_id(cfg=None) -> str:
    """Fingerprint the embedding space: provider, endpoint, model, prefixes, dim.

    Byte-for-byte identical to `Config::embedding_space_id` in Rust — the two
    implementations index the same Qdrant collection, so a disagreement here
    would make each one consider the other's records stale forever. Pinned from
    both sides by `tests/test_embed_space.py` and the Rust unit test of the same
    name.

    Content hashes alone could not tell that the *model* had changed, so swapping
    to a different model of the same dimension left every record "current" while
    the vectors were no longer comparable.
    """
    cfg = _cfg(cfg)
    ec = cfg.get("embed") or {}
    digest = hashlib.sha256()
    for part in (_embed_provider(cfg), _embed_endpoint(cfg), ec.get("model") or "",
                 ec.get("query_prefix") or "", ec.get("document_prefix") or ""):
        digest.update(str(part).encode())
        digest.update(b"\0")
    digest.update(struct.pack("<I", int(ec.get("dim") or DEFAULT_EMBED_DIM)))
    return digest.hexdigest()[:16]


def document_text(text: str, cfg=None) -> str:
    """Prefix a document before indexing it (asymmetric models need this)."""
    return f"{(_cfg(cfg).get('embed') or {}).get('document_prefix') or ''}{text}"


def query_text(text: str, cfg=None) -> str:
    """Prefix a query before searching with it."""
    return f"{(_cfg(cfg).get('embed') or {}).get('query_prefix') or ''}{text}"


def embed(text: str, cfg=None, kind: str | None = None):
    """Embed `text` in the configured space.

    An explicitly configured provider NEVER falls back: a dead llama-server used to
    silently hand back CPU fastembed vectors from a different model, in a different
    dimension, poisoning the index with no error anywhere. Only the auto-selected
    default (no `embed.provider` set) is allowed to degrade to fastembed.

    `kind` selects the asymmetric-model prefix: `"document"` applies
    `embed.document_prefix`, `"query"` applies `embed.query_prefix`. Both keys
    were configurable, round-tripped by the editor, and applied by nothing — so an
    asymmetric model indexed and queried in two different spaces.

    `kind=None` (the default) applies NEITHER, which is what callers that cannot
    distinguish the two sides need. Defaulting to the document side instead would
    have silently prefixed queries too: Graphiti's embedder and the reranker in
    `graph/mg_config.py` route both passages and queries through one call, and
    `memory_ai.ollama_embed` discards its role argument entirely. A wrong prefix
    is worse than no prefix — it moves the query out of the index's space.
    """
    cfg = _cfg(cfg)
    if kind == "document":
        text = document_text(text, cfg)
    elif kind == "query":
        text = query_text(text, cfg)
    prov = _embed_provider(cfg)
    explicit = _embed_provider_is_explicit(cfg)
    if prov == "llama_cpp":
        if explicit:
            return _check_embed_dim(_llama_embed(text, cfg), cfg)
        try:
            return _check_embed_dim(_llama_embed(text, cfg), cfg)
        except Exception:
            pass
    elif prov == "ollama":
        if explicit:
            return _check_embed_dim(_ollama_embed(text, cfg), cfg)
        try:
            return _check_embed_dim(_ollama_embed(text, cfg), cfg)
        except Exception:
            pass
    elif prov == "fastembed" and explicit:
        return _check_embed_dim(_fastembed_embed(text, cfg), cfg)
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

def _llama_embed(text: str, cfg):
    """Embeddings via a llama.cpp / OpenAI-compatible server (POST {url}/embeddings).
    Used when embed.provider is 'llama_cpp' (e.g. bge-m3 served by llama-server)."""
    ec = cfg.get("embed", {})
    base = (ec.get("url") or "http://127.0.0.1:8091/v1").rstrip("/")
    payload = {"model": ec.get("model") or "bge-m3", "input": text}
    headers = {"Content-Type": "application/json"}
    if ec.get("api_key"):
        headers["Authorization"] = f"Bearer {ec['api_key']}"
    req = urllib.request.Request(f"{base}/embeddings",
                                 data=json.dumps(payload).encode(), headers=headers)
    with urllib.request.urlopen(req, timeout=int(ec.get("timeout_seconds", 120))) as r:
        return json.loads(r.read().decode())["data"][0]["embedding"]


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
            _ccg_generate("reply ok", "triage", cfg, audit_kind="health")
        elif b == "llama_cpp":
            _llamacpp_generate("reply ok", "triage", cfg, audit_kind="health")
        else:
            _ollama_generate("reply ok", "triage", cfg, audit_kind="health")
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
