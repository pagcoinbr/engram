"""vector_mcp_server.py — MCP server exposing the OPTIONAL Qdrant semantic index to
Claude Code as live tools (parallel to graph/mg_mcp_server.py). Raw dense-vector
inspection over the .md store: "is there a memory about X", and index stats.

Recall itself lives in the `engram-rust` (`engram-mcp`) server, which is the single
recall path (hybrid keyword+vector+graph, degrading to keyword+vector when no graph
is installed). This server used to also expose `memory_vector_recall` and
`memory_recall_fused`; both re-embedded the query on their own, so a recall driven
here plus engram-rust embedded the same text twice against the model. They were
removed and recall consolidated onto engram-rust. What remains are the raw-search
and stats tools nothing else provides.

All tools degrade gracefully: if the vector store is disabled or Qdrant is
unreachable they return a short notice (not an error), so Claude falls back to the
markdown store / graph recall.

Registered (the installer does this for you) with:
    claude mcp add --scope user engram-vector \
        <vector venv python> <vector dir>/vector_mcp_server.py
"""
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent / "bin"))
if str(Path.home() / ".claude") not in sys.path:
    sys.path.append(str(Path.home() / ".claude"))

from mcp.server.fastmcp import FastMCP
import memory_ai
import vector_config as vc
from vector_store import EngramVectorStore

mcp = FastMCP("engram-vector")
_store = None


def _get_store():
    """Lazily build the store; raise VectorUnavailable when off/unreachable."""
    global _store
    if _store is None:
        cfg = memory_ai.load()
        if not memory_ai.vector_enabled(cfg):
            raise vc.VectorUnavailable("vector_store disabled (or local_enabled false)")
        s = EngramVectorStore(cfg)
        s.ensure_collection()
        _store = s
    return _store


def _filters(cfg, mtype: str = "") -> dict | None:
    """Build a payload filter from a `type` arg + the default slug scope."""
    from vector_store import slug
    f = {}
    if mtype:
        f["type"] = mtype
    if memory_ai.scope_to_slug(cfg):
        f["slug"] = slug()
    return f or None


@mcp.tool()
def memory_vector_search(query: str, k: int = 8, type: str = "") -> str:
    """Raw semantic search over memories: returns the top-k matching files with
    their similarity scores (no graph facts). Optionally filter by memory `type`.
    Good for 'is there a memory about X'."""
    try:
        store = _get_store()
    except vc.VectorUnavailable as e:
        return f"(vector store unavailable: {e})"
    try:
        hits = store.search(query, k=k, filters=_filters(store.cfg, type))
    except Exception as e:
        return f"(vector search failed: {e})"
    return "\n".join(f"- `{h['score']:.3f}`  {h['file']}: {(h['description'] or '').strip()}"
                     for h in hits) or "(no matches)"


@mcp.tool()
def memory_vector_stats() -> str:
    """Counts for the vector index: number of indexed memories, collection name,
    embedding dimension, and on-disk mode."""
    try:
        store = _get_store()
    except vc.VectorUnavailable as e:
        return f"(vector store unavailable: {e})"
    try:
        s = store.stats()
    except Exception as e:
        return f"(vector stats failed: {e})"
    return (f"points={s['points']} collection={s['collection']} "
            f"dim={s['dim']} on_disk={s['on_disk']}")


if __name__ == "__main__":
    mcp.run()
