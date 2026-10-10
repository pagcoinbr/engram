"""mg_mcp_server.py — MCP server exposing the Graphiti/Neo4j memory graph to
Claude Code as live tools. Read-side graph admin: fact search,
entity-neighbourhood, stats. All local (Neo4j on loopback, Ollama on the LAN).
Neo4j password is read from .env by mg_config — never passed through the MCP
config.

Recall itself (graph+vector+keyword hybrid) lives in the `engram-rust`
(`engram-mcp`) server, which is the single recall path. This server used to also
expose `memory_recall`/`memory_recall_hybrid`, but those re-embedded the query
independently of engram-rust (the graph leg AND an in-process Qdrant leg each
embedded it), so one recall made two identical calls to the embedding model.
Recall was consolidated onto engram-rust; this server keeps only the graph-native
tools that nothing else provides.

Registered (the installer does this for you) with:
    claude mcp add --scope user engram-graph \
        <engram graph dir>/venv/bin/python <engram graph dir>/mg_mcp_server.py
"""
import logging
import sys
from pathlib import Path

logging.getLogger("neo4j").setLevel(logging.ERROR)
logging.getLogger("neo4j.notifications").setLevel(logging.ERROR)

# mg_config pulls in the shared engine modules (memory_ai / engram_llm), which
# live flat in ~/.claude; make that importable.
if str(Path.home() / ".claude") not in sys.path:
    sys.path.append(str(Path.home() / ".claude"))

from mcp.server.fastmcp import FastMCP
from mg_config import build_graphiti, active_group

mcp = FastMCP("engram-graph")
_g = None
_group = None


async def _graph():
    global _g
    if _g is None:
        _g = build_graphiti()
    return _g


def _active_group() -> str:
    """The Graphiti group_id this server is allowed to read. Resolved from the
    tenant (ENGRAM_TENANT) so EVERY tool below is confined to one identity's
    subgraph — without it `g.search()` / an unscoped Cypher would return another
    tenant's facts once Neo4j holds multiple identities. On a pre-tenancy install
    this is the historical `canonical` group and nothing changes. Resolved once;
    on a tenanted install with no tenant selected active_group() fails loudly,
    which is the point — the server must not fall back to the shared group."""
    global _group
    if _group is None:
        _group = active_group()
    return _group


@mcp.tool()
async def memory_search_facts(query: str, k: int = 8) -> str:
    """Search the memory graph for individual relationship-facts matching a query."""
    g = await _graph()
    hits = await g.search(query, group_ids=[_active_group()], num_results=k)
    return "\n".join(f"- {h.fact}" for h in hits) or "(no facts found)"


@mcp.tool()
async def memory_neighbors(entity: str) -> str:
    """List the facts/relationships connected to a named entity
    (e.g. 'api-1', 'api-service', 'postgres'). Good for 'what do I know about X'."""
    g = await _graph()
    recs, _, _ = await g.driver.execute_query(
        "MATCH (n:Entity)-[r:RELATES_TO]-(m:Entity) WHERE toLower(n.name)=toLower($e) "
        "AND n.group_id=$grp AND m.group_id=$grp AND r.group_id=$grp "
        "RETURN r.name AS rel, m.name AS other, r.fact AS fact LIMIT 50",
        e=entity, grp=_active_group())
    if not recs:
        return f"(no entity named '{entity}')"
    return "\n".join(f"- [{r['rel']}] {r['other']}: {r['fact']}" for r in recs)


@mcp.tool()
async def memory_stats() -> str:
    """Counts of memories (episodes), entities, and facts in the memory graph."""
    g = await _graph()
    # OPTIONAL MATCH so a fresh/empty tenant returns zeros rather than no rows at
    # all: a plain MATCH on a group with no entities yields an empty result set,
    # and recs[0] would then IndexError out of the tool for every new tenant.
    recs, _, _ = await g.driver.execute_query(
        "OPTIONAL MATCH (e:Episodic {group_id:$grp}) WITH count(e) AS eps "
        "OPTIONAL MATCH (n:Entity {group_id:$grp}) WITH eps, count(n) AS ents "
        "OPTIONAL MATCH ()-[r:RELATES_TO {group_id:$grp}]->() "
        "RETURN eps AS episodes, ents AS entities, count(r) AS facts", grp=_active_group())
    r = recs[0]
    return f"episodes={r['episodes']} entities={r['entities']} facts={r['facts']}"


if __name__ == "__main__":
    mcp.run()
