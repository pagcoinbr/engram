"""mg_mcp_server.py — MCP server exposing the Graphiti/Neo4j memory graph to
Claude Code as live tools. Read-side of the memory brain (Phase 4): hybrid
recall, fact search, entity-neighbourhood, stats. All local (Neo4j on loopback,
Ollama on the LAN). Neo4j password is read from .env by mg_config — never passed
through the MCP config.

Registered (the installer does this for you) with:
    claude mcp add --scope user engram-graph \
        <engram graph dir>/venv/bin/python <engram graph dir>/mg_mcp_server.py
"""
import logging
import sys
from collections import defaultdict
from pathlib import Path

logging.getLogger("neo4j").setLevel(logging.ERROR)
logging.getLogger("neo4j.notifications").setLevel(logging.ERROR)

# Shared engine modules (memory_ai / memory_keyword / memory_fusion) live flat in
# ~/.claude; the optional vector store in ~/.claude/vector. Make both importable so
# the hybrid tool can fuse graph + vector + keyword in-process.
sys.path.insert(0, str(Path.home() / ".claude" / "vector"))
if str(Path.home() / ".claude") not in sys.path:
    sys.path.append(str(Path.home() / ".claude"))

from mcp.server.fastmcp import FastMCP
from mg_config import build_graphiti

mcp = FastMCP("engram-graph")
_g = None


async def _graph():
    global _g
    if _g is None:
        _g = build_graphiti()
    return _g


async def _graph_ranked(g, query: str, k: int, mtype: str = "") -> tuple[list, dict, dict]:
    """Core graph recall: hybrid-search facts, group by episode, rank by fact count.
    Returns (ranked_files, facts_by_file, meta_by_file) where ranked_files is a
    best-first list of UNIQUE .md filenames (duplicate episodes collapsed). Optionally
    filter to a memory `type`. Shared by memory_recall and memory_recall_hybrid."""
    edges = await g.search(query, num_results=k * 3)
    fact_by_ep = defaultdict(list)
    for e in edges:
        for u in (getattr(e, "episodes", None) or []):
            fact_by_ep[u].append(e.fact)
    if not fact_by_ep:
        return [], {}, {}
    recs, _, _ = await g.driver.execute_query(
        "MATCH (e:Episodic) WHERE e.uuid IN $u "
        "RETURN e.uuid AS uuid, e.fm_name AS name, e.fm_description AS desc, "
        "e.file AS file, e.fm_type AS type",
        u=list(fact_by_ep.keys()))
    meta = {r["uuid"]: r for r in recs}
    ranked_uuids = sorted(fact_by_ep, key=lambda u: -len(fact_by_ep[u]))
    # Existence filter: the graph has no delete path, so episodes of deleted/renamed
    # memories linger forever and would otherwise be recalled (and their stale
    # cached name/desc shown). Only surface files that still exist on disk; this
    # also drops raw-uuid pseudo-files (episodes with no fm file).
    try:
        import memory_keyword
        mem_dir = Path(memory_keyword.mem_dir())
    except Exception:
        mem_dir = None
    ranked_files, facts, fmeta, seen = [], {}, {}, set()
    for u in ranked_uuids:
        m = meta.get(u, {})
        f = m.get("file") or u
        if mtype and (m.get("type") or "") != mtype:
            continue
        if f in seen:                      # collapse multiple episodes of one file
            continue
        if not str(f).endswith(".md") or (mem_dir and not (mem_dir / f).is_file()):
            continue                       # deleted/renamed memory or uuid pseudo-file
        seen.add(f)
        ranked_files.append(f)
        facts[f] = fact_by_ep[u]
        fmeta[f] = m
    return ranked_files, facts, fmeta


@mcp.tool()
async def memory_recall(query: str, k: int = 6, type: str = "") -> str:
    """Recall the most relevant memories for a task/query. Delegates to the fused
    graph+vector+keyword ranking (`memory_recall_hybrid`) — kept as a stable name
    for habit/back-compat. The hybrid degrades to graph-only when the vector/keyword
    legs are unavailable, so this is never worse than the old graph-only recall.
    Optionally filter by memory `type` (user|feedback|project|reference|snippet)."""
    return await _recall_hybrid(query, k, type)


@mcp.tool()
async def memory_recall_hybrid(query: str, k: int = 6, type: str = "") -> str:
    """The BEST recall: fuse graph (associative/temporal) + vector (dense semantic) +
    keyword (BM25 lexical) into one ranking via Reciprocal Rank Fusion, keyed by the
    .md filename. Use at the start of work to load the most relevant memories. Each
    ranker degrades independently — a disabled/down vector store or graph just drops
    out. Optionally filter by memory `type` (user|feedback|project|reference|snippet).
    For reusable CODE specifically, prefer `memory_snippet_lookup`."""
    return await _recall_hybrid(query, k, type)


async def _rankings(query: str, want: int, mtype: str = "") -> tuple[dict, dict, dict]:
    """Run all three recall legs and return (rankings, names, facts) for fusion.

    Each leg is independently try/excepted: a dead Neo4j or a disabled vector store
    just drops out of `rankings` instead of failing the call. Shared by hybrid recall
    and snippet lookup so there is exactly ONE retrieval path, not two."""
    import asyncio
    import memory_ai
    import memory_keyword

    cfg = memory_ai.load()
    rankings, names, facts = {}, {}, {}

    # graph leg
    try:
        g = await _graph()
        granked, gfacts, gmeta = await _graph_ranked(g, query, want, mtype)
        rankings["graph"] = granked
        facts.update(gfacts)
        for f, m in gmeta.items():
            names.setdefault(f, (m.get("name"), m.get("desc")))
    except Exception:
        pass  # Neo4j down -> graph drops out

    # vector leg (optional; in-process Qdrant via the shared vector store)
    try:
        if memory_ai.vector_enabled(cfg):
            import vector_config as vc
            from vector_store import EngramVectorStore, slug as _vslug
            store = EngramVectorStore(cfg)
            store.ensure_collection()
            vf = {}
            if mtype:
                vf["type"] = mtype
            if memory_ai.scope_to_slug(cfg):
                vf["slug"] = _vslug()
            vhits = await asyncio.to_thread(store.search, query, want, 0.0, (vf or None))
            rankings["vector"] = [h["file"] for h in vhits]
            for h in vhits:
                names.setdefault(h["file"], (h["name"], h["description"]))
    except Exception:
        pass  # vector store disabled/unreachable -> drops out

    # keyword leg (pure-python; effectively always available)
    try:
        krank = memory_keyword.rank(query, want, mtype or None)
        rankings["keyword"] = [f for f, _ in krank]
    except Exception:
        pass

    return rankings, names, facts


async def _recall_hybrid(query: str, k: int = 6, type: str = "") -> str:
    """Shared implementation for memory_recall + memory_recall_hybrid (a plain
    callable so neither tool depends on the @mcp.tool decorator's return value)."""
    import memory_ai
    import memory_fusion
    import memory_keyword

    rc = memory_ai.recall_cfg(memory_ai.load()).get("hybrid", {})
    rankings, names, facts = await _rankings(query, max(k * 2, 10), type or "")

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
        for fact in facts.get(d["file"], [])[:2]:
            out.append(f"    - {fact}")
    return "\n".join(out)


@mcp.tool()
async def memory_snippet_lookup(task: str, k: int = 2) -> str:
    """Check the snippet shelf BEFORE writing operational code — shell/SSH/Docker
    pipelines, on-chain sends, deploy or recovery scripts, API probes. Returns code
    that has ALREADY been run successfully on this fleet, so a proven script gets
    reused (or diffed and adapted) instead of regenerated from scratch.

    Searches `type=snippet` memories only, and ABSTAINS unless two independent
    rankers agree — "(no snippet matched)" is a normal, useful answer, not a
    failure. Returns pointers, never code: read the returned .md file to get the
    script, its `risk:` tag, and its gotchas before running anything."""
    import memory_ai
    import memory_fusion
    import memory_keyword

    rc = memory_ai.recall_cfg(memory_ai.load()).get("hybrid", {})
    rankings, names, _ = await _rankings(task, max(k * 4, 12), "snippet")
    live = sum(1 for v in rankings.values() if v)
    fused = memory_fusion.fuse(rankings, k_rrf=int(rc.get("k_rrf", 60)),
                               weights=rc.get("weights"))
    picked = memory_fusion.select_snippets(fused, live_rankers=live, k=k)

    def _meta(f):
        if f not in names:
            nm, desc, _t = memory_keyword.meta(f)
            names[f] = (nm, desc)
        return names[f]

    return memory_fusion.format_snippet_hits(task, picked, _meta)


@mcp.tool()
async def memory_search_facts(query: str, k: int = 8) -> str:
    """Search the memory graph for individual relationship-facts matching a query."""
    g = await _graph()
    hits = await g.search(query, num_results=k)
    return "\n".join(f"- {h.fact}" for h in hits) or "(no facts found)"


@mcp.tool()
async def memory_neighbors(entity: str) -> str:
    """List the facts/relationships connected to a named entity
    (e.g. 'api-1', 'api-service', 'postgres'). Good for 'what do I know about X'."""
    g = await _graph()
    recs, _, _ = await g.driver.execute_query(
        "MATCH (n:Entity)-[r:RELATES_TO]-(m:Entity) WHERE toLower(n.name)=toLower($e) "
        "RETURN r.name AS rel, m.name AS other, r.fact AS fact LIMIT 50", e=entity)
    if not recs:
        return f"(no entity named '{entity}')"
    return "\n".join(f"- [{r['rel']}] {r['other']}: {r['fact']}" for r in recs)


@mcp.tool()
async def memory_stats() -> str:
    """Counts of memories (episodes), entities, and facts in the memory graph."""
    g = await _graph()
    recs, _, _ = await g.driver.execute_query(
        "MATCH (e:Episodic) WITH count(e) AS eps MATCH (n:Entity) WITH eps, count(n) AS ents "
        "MATCH ()-[r:RELATES_TO]->() RETURN eps AS episodes, ents AS entities, count(r) AS facts")
    r = recs[0]
    return f"episodes={r['episodes']} entities={r['entities']} facts={r['facts']}"


if __name__ == "__main__":
    mcp.run()
