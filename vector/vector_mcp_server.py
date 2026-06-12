"""vector_mcp_server.py — MCP server exposing the OPTIONAL Qdrant semantic index to
Claude Code as live tools (parallel to graph/mg_mcp_server.py). Dense-vector recall
over the .md store: fast "load the relevant memories" without the graph, and the
primary recall path on vector-only (no-Neo4j) installs.

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


@mcp.tool()
def memory_vector_recall(query: str, k: int = 6) -> str:
    """Recall the most relevant memories for a task/query by dense semantic search
    over the Qdrant index. Returns memory names, descriptions, and similarity scores.
    Use at the start of work to load only the relevant memories instead of all of them.
    (Optional vector store — falls back to a notice if disabled/unreachable.)"""
    try:
        store = _get_store()
    except vc.VectorUnavailable as e:
        return f"(vector store unavailable — using markdown/graph instead: {e})"
    try:
        _, thr = vc.recall_defaults(store.cfg)
        hits = store.search(query, k=k, threshold=thr)
    except Exception as e:
        return f"(vector recall failed: {e})"
    if not hits:
        return f"(no memories matched: {query})"
    out = [f"Recalled {len(hits)} memories for: {query}", ""]
    for h in hits:
        out.append(f"- {h['name'] or h['file']} (score {h['score']:.3f}): {(h['description'] or '').strip()}")
    return "\n".join(out)


@mcp.tool()
def memory_vector_search(query: str, k: int = 8) -> str:
    """Raw semantic search over memories: returns the top-k matching files with
    their similarity scores (no graph facts). Good for 'is there a memory about X'."""
    try:
        store = _get_store()
    except vc.VectorUnavailable as e:
        return f"(vector store unavailable: {e})"
    try:
        hits = store.search(query, k=k)
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
