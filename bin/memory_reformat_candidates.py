#!/usr/bin/env python3
"""
memory_reformat_candidates.py — list memories that need converting to the
Summary -> numbered Index -> Body shape (see feedback_memory_file_format).

Read-only lister behind the `/memory-reformat` command. A memory is a candidate
when it is "big" (enough body that a summary + index actually helps) AND not
already in the new shape (missing a `## Summary` or `## Index` heading). Small
one-fact memories are left alone — the convention lets them keep just a summary.

The actual reformatting prose is produced by the LOCAL Ollama MCP (qwen3.6:35b),
never here — this script only decides *what* to reformat and reports size.

Usage:
  memory_reformat_candidates.py                 # table of candidates (biggest first)
  memory_reformat_candidates.py --json          # machine-readable
  memory_reformat_candidates.py --all           # include already-formatted / small (show why)
  memory_reformat_candidates.py --big-min 2000  # override the size floor (chars)
  memory_reformat_candidates.py --memory foo.md # check a single memory
"""
from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCORER = HERE / "memory_score.py"

# Body big enough that a summary + index earns its keep. Tunable via --big-min.
BIG_MIN_CHARS = 1500


def store_path() -> Path:
    """Reuse the scorer's canonical, $PWD-independent store resolution."""
    out = subprocess.run(
        [sys.executable, str(SCORER), "--json"],
        capture_output=True, text=True, check=True,
    ).stdout
    return Path(json.loads(out)["store"])


def split_frontmatter(text: str) -> str:
    """Return the body (everything after the closing --- of the frontmatter)."""
    if text.startswith("---"):
        end = text.find("\n---", 3)
        if end != -1:
            nl = text.find("\n", end + 1)
            return text[nl + 1:] if nl != -1 else ""
    return text


def is_formatted(body: str) -> bool:
    """New shape requires both a Summary and an Index heading."""
    return ("## Summary" in body) and ("## Index" in body)


def main() -> None:
    args = sys.argv[1:]
    as_json = "--json" in args
    show_all = "--all" in args

    big_min = BIG_MIN_CHARS
    if "--big-min" in args:
        big_min = int(args[args.index("--big-min") + 1])

    only = None
    if "--memory" in args:
        only = args[args.index("--memory") + 1]
        if not only.endswith(".md"):
            only += ".md"

    store = store_path()
    rows = []
    for path in sorted(store.glob("*.md")):
        if path.name == "MEMORY.md":
            continue
        if only and path.name != only:
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        body = split_frontmatter(text)
        chars = len(text)
        formatted = is_formatted(body)
        big = chars >= big_min

        reasons = []
        if formatted:
            reasons.append("already Summary/Index/Body")
        if not big:
            reasons.append(f"small ({chars} < {big_min} chars)")

        rows.append({
            "name": path.name,
            "chars": chars,
            "lines": text.count("\n") + 1,
            "formatted": formatted,
            "big": big,
            "candidate": big and not formatted,
            "skip_reasons": reasons,
        })

    if only and not rows:
        print(f"[reformat] memory not found: {only}", file=sys.stderr)
        sys.exit(1)

    shown = rows if (show_all or only) else [r for r in rows if r["candidate"]]
    shown.sort(key=lambda r: r["chars"], reverse=True)

    if as_json:
        print(json.dumps({
            "store": str(store),
            "big_min_chars": big_min,
            "candidate_count": sum(1 for r in rows if r["candidate"]),
            "candidates": shown,
        }, indent=2))
        return

    n = sum(1 for r in rows if r["candidate"])
    print(f"# Memories needing Summary/Index/Body reformat  (store: {store})")
    print(f"# candidate = chars >= {big_min} AND not already formatted   |   "
          f"{n} candidate / {len(rows)} scanned\n")
    header = f"{'chars':>6} {'lines':>5} {'fmt':>4} {'cand':>5}  memory"
    print(header)
    print("-" * len(header))
    for r in shown:
        print(f"{r['chars']:>6} {r['lines']:>5} {('yes' if r['formatted'] else 'no'):>4} "
              f"{('yes' if r['candidate'] else 'no'):>5}  {r['name']}")
        if not r["candidate"] and (show_all or only):
            print(f"       └─ skip: {'; '.join(r['skip_reasons'])}")

    if not shown:
        print("(no candidates — every big memory is already Summary/Index/Body)")


if __name__ == "__main__":
    main()
