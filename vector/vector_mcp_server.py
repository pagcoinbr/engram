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
def memory_vector_recall(query: str, k: int = 6, type: str = "") -> str:
    """Recall the most relevant memories for a task/query by dense semantic search
    over the Qdrant index. Returns memory names, descriptions, and similarity scores.
    Optionally filter by memory `type` (user|feedback|project|reference). Use at the
    start of work to load only the relevant memories instead of all of them.
    (Optional vector store — falls back to a notice if disabled/unreachable.)"""
    try:
        store = _get_store()
    except vc.VectorUnavailable as e:
        return f"(vector store unavailable — using markdown/graph instead: {e})"
    try:
        _, thr = vc.recall_defaults(store.cfg)
        hits = store.search(query, k=k, threshold=thr, filters=_filters(store.cfg, type))
    except Exception as e:
        return f"(vector recall failed: {e})"
    if not hits:
        return f"(no memories matched: {query})"
    out = [f"Recalled {len(hits)} memories for: {query}", ""]
    for h in hits:
        out.append(f"- {h['name'] or h['file']} (score {h['score']:.3f}): {(h['description'] or '').strip()}")
    return "\n".join(out)


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
def memory_recall_fused(query: str, k: int = 6, type: str = "") -> str:
    """Hybrid recall WITHOUT the graph: fuse dense vector search + keyword (BM25)
    over the .md store via Reciprocal Rank Fusion. Best single recall tool when the
    Neo4j graph isn't installed; for the full graph+vector+keyword fusion use the
    engram-graph `memory_recall_hybrid` tool instead. Optionally filter by `type`."""
    import memory_keyword
    import memory_fusion
    cfg = memory_ai.load()
    rc = memory_ai.recall_cfg(cfg).get("hybrid", {})
    mtype = type or None

    rankings, names = {}, {}
    # vector leg (optional — degrades to keyword-only if unavailable)
    try:
        store = _get_store()
        vhits = store.search(query, k=max(k * 2, 10), filters=_filters(store.cfg, mtype))
        rankings["vector"] = [h["file"] for h in vhits]
        for h in vhits:
            names.setdefault(h["file"], (h["name"], h["description"]))
    except vc.VectorUnavailable:
        pass
    except Exception as e:
        return f"(fused recall: vector leg failed: {e})"
    # keyword leg (pure-python, effectively always available)
    krank = memory_keyword.rank(query, k=max(k * 2, 10), mtype=mtype)
    rankings["keyword"] = [f for f, _ in krank]

    fused = memory_fusion.fuse(rankings, k_rrf=int(rc.get("k_rrf", 60)),
                               weights=rc.get("weights"))[:k]
    if not fused:
        return f"(no memories matched: {query})"
    out = [f"Recalled {len(fused)} memories for: {query}", ""]
    for d in fused:
        if d["file"] not in names:                  # keyword-only hit -> read frontmatter
            nm, desc, _ = memory_keyword.meta(d["file"])
            names[d["file"]] = (nm, desc)
        nm, desc = names[d["file"]]
        out.append(f"- {nm or d['file']} [{'+'.join(d['sources'])}]: {(desc or '').strip()}")
    return "\n".join(out)


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
