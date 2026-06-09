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
from collections import defaultdict

logging.getLogger("neo4j").setLevel(logging.ERROR)
logging.getLogger("neo4j.notifications").setLevel(logging.ERROR)

from mcp.server.fastmcp import FastMCP
from mg_config import build_graphiti

mcp = FastMCP("engram-graph")
_g = None


async def _graph():
    global _g
    if _g is None:
        _g = build_graphiti()
    return _g


@mcp.tool()
async def memory_recall(query: str, k: int = 6) -> str:
    """Recall the most relevant memories for a task/query using hybrid graph+vector
    search. Returns the top memory names, their descriptions, and the matched facts.
    Use at the start of work to load only the relevant memories instead of all of them."""
    g = await _graph()
    edges = await g.search(query, num_results=k * 3)
    fact_by_ep = defaultdict(list)
    for e in edges:
        for u in (getattr(e, "episodes", None) or []):
            fact_by_ep[u].append(e.fact)
    if not fact_by_ep:
        return f"(no memories matched: {query})"
    recs, _, _ = await g.driver.execute_query(
        "MATCH (e:Episodic) WHERE e.uuid IN $u "
        "RETURN e.uuid AS uuid, e.fm_name AS name, e.fm_description AS desc, e.file AS file",
        u=list(fact_by_ep.keys()))
    meta = {r["uuid"]: r for r in recs}
    ranked = sorted(fact_by_ep, key=lambda u: -len(fact_by_ep[u]))[:k]
    out = [f"Recalled {len(ranked)} memories for: {query}", ""]
    for u in ranked:
        m = meta.get(u, {})
        out.append(f"- {m.get('name') or m.get('file')}: {(m.get('desc') or '').strip()}")
        for f in fact_by_ep[u][:3]:
            out.append(f"    - {f}")
    return "\n".join(out)


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
