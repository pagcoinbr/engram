"""smoke_test.py — validate the whole local chain end-to-end with ONE real memory:
Neo4j indices -> add_episode (Ollama LLM extraction + Ollama embeddings) -> search.
"""
import os
import asyncio
import datetime as dt
import re
import sys
from pathlib import Path

from graphiti_core.nodes import EpisodeType
from mg_config import build_graphiti, CANONICAL_GROUP

MEM_DIR = Path.home() / ".claude" / "projects" / (os.environ.get("CLAUDE_MEMORY_SLUG") or str(Path.home()).replace("/", "-")) / "memory"
SAMPLE = "feedback_strong_passwords.md"


def parse(md: str):
    body = md
    if md.startswith("---"):
        end = md.find("\n---", 3)
        if end != -1:
            body = md[end + 4:].lstrip("\n")
    return body


async def main():
    g = build_graphiti()
    print("1) building indices/constraints...")
    await g.build_indices_and_constraints()

    body = parse((MEM_DIR / SAMPLE).read_text())
    print(f"2) add_episode for {SAMPLE} ({len(body)} chars) — LLM extraction on Ollama, may take a while...")
    t0 = dt.datetime.now()
    res = await g.add_episode(
        name=SAMPLE.replace(".md", ""),
        episode_body=body,
        source=EpisodeType.text,
        source_description="canonical memory file",
        reference_time=dt.datetime.now(dt.timezone.utc),
        group_id=CANONICAL_GROUP,
        uuid=None,
    )
    dt_s = (dt.datetime.now() - t0).total_seconds()
    nodes = getattr(res, "nodes", []) or []
    edges = getattr(res, "edges", []) or []
    print(f"   extracted {len(nodes)} entities, {len(edges)} relationships in {dt_s:.0f}s")
    for n in nodes[:10]:
        print("     entity:", getattr(n, "name", n))
    for e in edges[:10]:
        print("     edge:", getattr(e, "fact", e))

    print("3) hybrid search: 'what is the policy for database passwords?'")
    hits = await g.search("what is the policy for database passwords?", num_results=5)
    for h in hits:
        print("   hit:", getattr(h, "fact", h))

    await g.close()
    print("OK: end-to-end chain works.")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except Exception as e:
        import traceback
        traceback.print_exc()
        sys.exit(1)
