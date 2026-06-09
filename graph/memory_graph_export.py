"""memory_graph_export.py — Phase 2: regenerate .md files FROM the graph.

In the graph-authoritative model the files are a faithful, git-committed export
of the graph (your condition #2: automatic, versioned, round-trip-exact). Each
canonical episode stores its verbatim source_md, so export is byte-identical to
the original. Writes to a SEPARATE export dir (never the live store) until you
choose to cut over.

Usage:
  python3 memory_graph_export.py [--dir PATH] [--verify] [--no-git]
"""
import asyncio
import logging
import os
import subprocess
import sys
from pathlib import Path

logging.getLogger("neo4j").setLevel(logging.ERROR)
logging.getLogger("neo4j.notifications").setLevel(logging.ERROR)

from mg_config import build_graphiti, CANONICAL_GROUP

DEFAULT_EXPORT = Path(__file__).resolve().parent / "export"
LIVE_DIR = Path.home() / ".claude" / "projects" / (os.environ.get("CLAUDE_MEMORY_SLUG") or str(Path.home()).replace("/", "-")) / "memory"


async def fetch_canonical(g):
    recs, _, _ = await g.driver.execute_query(
        "MATCH (e:Episodic) WHERE e.group_id=$g AND e.source_md IS NOT NULL "
        "RETURN e.file AS file, e.source_md AS md ORDER BY e.file",
        g=CANONICAL_GROUP,
    )
    return [(r["file"], r["md"]) for r in recs if r["file"]]


async def main():
    args = sys.argv[1:]
    export_dir = Path(args[args.index("--dir") + 1]) if "--dir" in args else DEFAULT_EXPORT
    do_git = "--no-git" not in args
    verify = "--verify" in args
    export_dir.mkdir(parents=True, exist_ok=True)

    g = build_graphiti()
    rows = await fetch_canonical(g)
    await g.close()

    for file, md in rows:
        (export_dir / file).write_text(md, encoding="utf-8")   # verbatim, no mangling
    print(f"[export] wrote {len(rows)} memories -> {export_dir}")

    if verify:
        ok = diff = miss = 0
        for file, md in rows:
            lf = LIVE_DIR / file
            if not lf.exists():
                miss += 1
                continue
            if lf.read_text() == md:
                ok += 1
            else:
                diff += 1
                print(f"  DIFF round-trip: {file}")
        print(f"[verify] identical={ok}  differ={diff}  live-missing={miss}  (of {len(rows)})")

    if do_git:
        if not (export_dir / ".git").exists():
            subprocess.run(["git", "init", "-q"], cwd=export_dir)
            subprocess.run(["git", "config", "user.email", "memory@local"], cwd=export_dir)
            subprocess.run(["git", "config", "user.name", "memory-learner"], cwd=export_dir)
        subprocess.run(["git", "add", "-A"], cwd=export_dir)
        r = subprocess.run(["git", "commit", "-q", "-m", f"export {len(rows)} memories from graph"],
                           cwd=export_dir, capture_output=True, text=True)
        print("[git] committed" if r.returncode == 0 else "[git] nothing to commit")


if __name__ == "__main__":
    asyncio.run(main())
