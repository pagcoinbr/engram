"""mg_config.py — build a Graphiti instance wired to local infra only:
Neo4j on loopback; embeddings + reranking routed through engram_llm so the graph
works on EITHER backend (Ollama nomic, or CPU fastembed when there's no GPU).
Nothing leaves the machine. Neo4j password from env or the chmod-600 .env.
"""
import asyncio
import json
import math
import os
import sys
import time
from pathlib import Path

from graphiti_core import Graphiti
from graphiti_core.driver.neo4j_driver import Neo4jDriver
from graphiti_core.llm_client.openai_generic_client import OpenAIGenericClient
from graphiti_core.llm_client.config import LLMConfig
from graphiti_core.embedder.client import EmbedderClient
from graphiti_core.cross_encoder.client import CrossEncoderClient

HERE = Path(__file__).resolve().parent
# engram_llm lives in the engine dir: ../bin in the repo, ~/.claude once installed.
sys.path.insert(0, str(HERE.parent / "bin"))
if str(Path.home() / ".claude") not in sys.path:
    sys.path.append(str(Path.home() / ".claude"))
import engram_llm  # provider abstraction (ollama | claude); embed() = ollama nomic | fastembed
try:
    import engram_llm_audit as _audit  # loop-visibility audit; guarded, never fatal
except Exception:  # pragma: no cover
    _audit = None

# LLM is only used by the (retired) Graphiti add_episode bootstrap path — insert/recall
# do not call it, so constructing it never reaches out. Kept for the bootstrap option.
OLLAMA_BASE = os.environ.get("OLLAMA_BASE_URL", "http://localhost:11434/v1")


def _configured_model(role: str) -> str:
    """Resolve the extraction model the way every other engram component does:
    `experts` override > tier preset. These used to default to the literal
    "llama3.1:8b" — a frozen copy of the tier-small preset that ignored the
    resolution chain, so an operator who pinned `experts:` to their own model
    (and pruned the presets) got a 404 on every extraction call."""
    try:
        return engram_llm.model_for(role)
    except Exception:
        return "llama3.1:8b"


# MG_LLM_MODEL wins when set — that's the hook for a num_ctx-tuned Modelfile
# variant (see graph/make_graph_model.sh), which /v1 cannot configure per-request.
LLM_MODEL = os.environ.get("MG_LLM_MODEL") or _configured_model("distill")
SMALL_MODEL = os.environ.get("MG_SMALL_MODEL") or _configured_model("triage")
# Thinking budget for extraction. Graphiti's OpenAIGenericClient never sends
# reasoning_effort and Ollama rejects `PARAMETER think` in a Modelfile, so the
# only place left to inject it is the request body (see _instrument_llm).
# Empty string disables the injection entirely (non-reasoning models).
REASONING_EFFORT = os.environ.get("MG_REASONING_EFFORT", "")


def _neo4j_uri() -> str:
    """Resolve the Neo4j bolt URI: NEO4J_URI env override > engram.yaml
    `graph.neo4j_uri` > loopback default. Honors the operator's engram.yaml so the
    setting isn't decorative; defensive so a missing/unreadable config still works."""
    if os.environ.get("NEO4J_URI"):
        return os.environ["NEO4J_URI"]
    try:
        import memory_ai  # in ~/.claude (already on sys.path above)
        uri = (memory_ai.load().get("graph", {}) or {}).get("neo4j_uri")
        if uri:
            return str(uri).strip()
    except Exception:
        pass
    return "bolt://127.0.0.1:7687"


NEO4J_URI = _neo4j_uri()
NEO4J_USER = os.environ.get("NEO4J_USER", "neo4j")
CANONICAL_GROUP = "canonical"   # the PRE-TENANCY group_id — see active_group()


def active_group(tenant: str = None) -> str:
    """The Graphiti ``group_id`` for the active agent identity.

    `CANONICAL_GROUP` was written as a literal for every memory in every store,
    so Graphiti had no partition at all and the graph leg returned one project's
    facts to another. This resolves the group from the configured tenant instead;
    on an install with no ``tenants:`` block it returns `CANONICAL_GROUP`, so a
    pre-tenancy graph is read and written exactly as before.

    Deliberately NOT exception-swallowing. Falling back to the shared group when
    a tenant cannot be resolved is the cross-tenant write this function exists to
    prevent — a missing ``--tenant`` has to fail loudly, not quietly pick the
    group everything used to share.
    """
    import engram_tenant

    return engram_tenant.resolve(requested=tenant).graph_group


def group_from_argv(argv=None) -> str:
    """`active_group` for a script invoked with ``--tenant NAME``.

    These scripts already parameterised their Cypher with ``grp=`` and bound it
    to the shared literal, so switching the binding is the whole change. Resolved
    at import so an unresolvable tenant fails before any statement runs — a
    maintenance pass that silently operated on the shared group would be far
    worse than one that refused to start.
    """
    argv = list(sys.argv[1:] if argv is None else argv)
    tenant = None
    if "--tenant" in argv:
        index = argv.index("--tenant")
        if index + 1 < len(argv):
            tenant = argv[index + 1]
    return active_group(tenant)


def neo4j_password() -> str:
    if os.environ.get("NEO4J_PASSWORD"):
        return os.environ["NEO4J_PASSWORD"]
    envf = HERE / ".env"
    if envf.exists():
        for line in envf.read_text().splitlines():
            if line.startswith("NEO4J_PASSWORD="):
                return line.split("=", 1)[1].strip()
    raise RuntimeError("NEO4J_PASSWORD not set (export it, or put it in graph/.env)")


def _cos(a, b) -> float:
    s = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a)) or 1.0
    nb = math.sqrt(sum(y * y for y in b)) or 1.0
    return s / (na * nb)


class EngramEmbedder(EmbedderClient):
    """Graphiti embedder backed by engram_llm.embed() — Ollama nomic when reachable,
    else CPU fastembed. 768-dim in both paths, so the graph never needs re-embedding
    when you switch backends."""
    async def create(self, input_data):
        text = input_data if isinstance(input_data, str) else " ".join(map(str, input_data))
        return await asyncio.to_thread(engram_llm.embed, text)

    async def create_batch(self, input_data_list):
        return [await asyncio.to_thread(engram_llm.embed, t) for t in input_data_list]


class EngramReranker(CrossEncoderClient):
    """Embedding-cosine reranker — needs no generation LLM, so recall works in
    claude-only mode (no Ollama) without calling out or spending tokens."""
    async def rank(self, query: str, passages):
        if not passages:
            return []
        qv = await asyncio.to_thread(engram_llm.embed, query)
        scored = []
        for p in passages:
            pv = await asyncio.to_thread(engram_llm.embed, p)
            scored.append((p, _cos(qv, pv)))
        scored.sort(key=lambda x: -x[1])
        return scored


def _audit_graphiti(call_id, attempt, model, canon, t0, outcome, detail=""):
    """Record one Graphiti generation attempt. Best-effort; never raises."""
    if _audit is None:
        return
    try:
        _audit.record(src="graphiti", kind="generation", call_id=call_id, attempt=attempt,
                      backend="graphiti", endpoint=OLLAMA_BASE, model=model or "", role="",
                      chars=len(canon or ""), digest=_audit.digest(canon or ""),
                      ms=int((time.time() - t0) * 1000), outcome=outcome, detail=detail)
    except Exception:
        pass


def _instrument_llm(llm) -> None:
    """Wrap graphiti's chat.completions.create to (1) AUDIT every generation call for
    loop visibility and (2) inject reasoning_effort when configured.

    Installed ALWAYS (the audit must see ingest even with reasoning off). Reasoning
    injection stays conditional on REASONING_EFFORT. The empty-`length` retry logs a
    second attempt (same call_id) — exactly the loop signal we want. The wrapper never
    mutates the caller's dicts, tolerates extra_body=None, and passes *args through.
    """
    create = llm.client.chat.completions.create

    def _canon_and_model(kwargs):
        try:
            # Canonical JSON of the messages, not concatenated text: collision-safe
            # and tolerant of structured/multimodal content.
            canon = json.dumps(kwargs.get("messages"), sort_keys=True,
                               separators=(",", ":"), default=str)
        except Exception:
            canon = ""
        return canon, (kwargs.get("model") or LLM_MODEL)

    async def _logged_create(*args, **kwargs):
        call_id = _audit.new_call_id() if _audit else ""
        inject = bool(REASONING_EFFORT)
        if inject:
            extra = dict(kwargs.get("extra_body") or {})   # copy; don't mutate caller
            extra["reasoning_effort"] = REASONING_EFFORT
            kwargs = {**kwargs, "extra_body": extra}
        canon, model = _canon_and_model(kwargs)

        async def _one(kw, attempt):
            t0 = time.time()
            try:
                result = await create(*args, **kw)
            except Exception as exc:
                _audit_graphiti(call_id, attempt, model, canon, t0, "error", type(exc).__name__)
                raise
            _audit_graphiti(call_id, attempt, model, canon, t0, "ok")
            return result

        r = await _one(kwargs, 1)
        if inject:
            # A long memory at high effort can spend the ENTIRE max_tokens budget on hidden
            # CoT and return finish_reason=length with empty content. Graphiti's tenacity
            # wrapper then retries the same call 4x (~12 min each here) and the memory ends
            # up absent from the graph. Degrade the thinking budget instead of re-losing it.
            # "none" and not "low": measured on Qwen3.6 via ollama 0.32 /v1, high and low
            # return byte-identical CoT (493 chars) — the effort levels are a no-op here.
            c = r.choices[0]
            if c.finish_reason == "length" and not (c.message.content or ""):
                extra2 = dict(kwargs.get("extra_body") or {}); extra2["reasoning_effort"] = "none"
                r = await _one({**kwargs, "extra_body": extra2}, 2)
        return r

    llm.client.chat.completions.create = _logged_create


def _llm_timeout() -> float:
    """Per-request timeout for graph extraction, honouring ollama.timeout_seconds.

    The openai SDK defaults to read=600s with max_retries=2, and graphiti constructs
    its AsyncOpenAI with neither overridden. On a CPU-offloaded local model a single
    extraction can legitimately exceed 10 minutes, so the call fails after ~41 min
    (3 attempts + backoff), the episode is logged as an error, and — because it never
    reaches the state file — the memory is SILENTLY ABSENT from the graph. Observed on
    3 of the first 50 memories. engram.yaml's ollama.timeout_seconds does not apply
    here: that governs engram_llm's /api/generate path, a different client.
    """
    if os.environ.get("MG_LLM_TIMEOUT"):
        return float(os.environ["MG_LLM_TIMEOUT"])
    try:
        import memory_ai
        return float((memory_ai.load().get("ollama") or {}).get("timeout_seconds") or 600)
    except Exception:
        return 600.0


def build_graphiti() -> Graphiti:
    # Explicit client so the timeout above actually applies. max_retries=0 on purpose:
    # with a long timeout a retry burns hours, and the bootstrap's own resume loop is
    # the better retry — it is idempotent and skips everything already done.
    from openai import AsyncOpenAI
    http = AsyncOpenAI(api_key="ollama", base_url=OLLAMA_BASE,
                       timeout=_llm_timeout(), max_retries=0)
    llm = OpenAIGenericClient(
        client=http,
        config=LLMConfig(api_key="ollama", model=LLM_MODEL, small_model=SMALL_MODEL,
                         base_url=OLLAMA_BASE, temperature=0.0, max_tokens=8192))
    _instrument_llm(llm)   # always: audits every generation; injects reasoning if set
    driver = Neo4jDriver(uri=NEO4J_URI, user=NEO4J_USER, password=neo4j_password())
    return Graphiti(graph_driver=driver, llm_client=llm,
                    embedder=EngramEmbedder(), cross_encoder=EngramReranker())
