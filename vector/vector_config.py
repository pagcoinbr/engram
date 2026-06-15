"""vector_config.py — resolve the OPTIONAL Qdrant vector store config and build a
lazy client. Mirrors graph/mg_config.py's role for Neo4j.

The vector store is a *rebuildable* semantic index over the .md store (the source
of truth). It is OFF by default: when `vector_store.enabled` is false, or Qdrant is
unreachable, callers fall back to pure markdown (the in-memory cosine duplicate
finder + graph recall) — nothing breaks.

Embeddings always route through engram_llm.embed() (768-dim: Ollama nomic when
reachable, else CPU fastembed), so the vector space matches the graph's exactly.

Resolution order for each setting: env override > engram.yaml `vector_store.*` >
built-in default.
"""
from __future__ import annotations
import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
# engram_llm + memory_ai live in the engine dir: ../bin in the repo, ~/.claude installed.
sys.path.insert(0, str(HERE.parent / "bin"))
if str(Path.home() / ".claude") not in sys.path:
    sys.path.append(str(Path.home() / ".claude"))
import memory_ai   # config loader
import engram_llm  # embed() + embed_dim()


class VectorUnavailable(RuntimeError):
    """Raised when the vector store is disabled or Qdrant cannot be reached.
    Callers should catch this and fall back to the pure-markdown path."""


def _vcfg(cfg=None) -> dict:
    return (cfg or memory_ai.load()).get("vector_store", {}) or {}


def enabled(cfg=None) -> bool:
    """Master gate. Both engram's local switch AND vector_store.enabled must be on."""
    cfg = cfg or memory_ai.load()
    return bool(memory_ai.local_enabled(cfg)) and bool(_vcfg(cfg).get("enabled", False))


def provider(cfg=None) -> str:
    return (_vcfg(cfg).get("provider") or "qdrant").strip().lower()


def url(cfg=None) -> str:
    return (os.environ.get("ENGRAM_QDRANT_URL")
            or _vcfg(cfg).get("url") or "http://127.0.0.1:6333")


def api_key(cfg=None) -> str:
    return (os.environ.get("ENGRAM_QDRANT_API_KEY")
            or _vcfg(cfg).get("api_key") or "").strip()


def collection_name(cfg=None) -> str:
    return (os.environ.get("ENGRAM_VECTOR_COLLECTION")
            or _vcfg(cfg).get("collection") or "engram_memory")


def on_disk(cfg=None) -> bool:
    return bool(_vcfg(cfg).get("on_disk", False))


def timeout_seconds(cfg=None) -> int:
    return int(_vcfg(cfg).get("timeout_seconds", 30))


def dim(cfg=None) -> int:
    """Vector size. Derived from the embedding backend so the collection always
    matches engram_llm.embed() (768-dim nomic)."""
    return int(engram_llm.embed_dim(cfg))


def recall_defaults(cfg=None) -> tuple[int, float]:
    r = _vcfg(cfg).get("recall", {}) or {}
    return int(r.get("default_k", 6)), float(r.get("threshold", 0.0))


def use_vector_dupes(cfg=None) -> bool:
    """Should the light pass use Qdrant ANN for the duplicate finder?"""
    df = _vcfg(cfg).get("duplicate_finder", {}) or {}
    return enabled(cfg) and bool(df.get("use_vector_store", True))


def build_client(cfg=None):
    """Construct a QdrantClient. Lazy + defensive: raises VectorUnavailable when the
    feature is off, the client lib is missing, or the server can't be reached."""
    cfg = cfg or memory_ai.load()
    if not enabled(cfg):
        raise VectorUnavailable("vector_store.enabled is false")
    if provider(cfg) != "qdrant":
        raise VectorUnavailable(f"unsupported vector provider: {provider(cfg)!r}")
    try:
        from qdrant_client import QdrantClient
    except ImportError as e:
        raise VectorUnavailable(
            "qdrant-client not installed — `pip install qdrant-client`") from e
    kwargs = {"url": url(cfg), "timeout": timeout_seconds(cfg)}
    if api_key(cfg):
        kwargs["api_key"] = api_key(cfg)
    try:
        return QdrantClient(**kwargs)
    except Exception as e:  # network/config error at construction time
        raise VectorUnavailable(f"cannot reach Qdrant at {url(cfg)}: {e}") from e
