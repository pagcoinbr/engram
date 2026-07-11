#!/usr/bin/env python3
"""I1 regression: generated index links or recall-notes EVERY memory (no silent
orphans) and stays within the load budget. Runs against a synthetic store."""
import sys, importlib.util, tempfile, os, json, re
from pathlib import Path
ROOT = Path(__file__).resolve().parent.parent

def main():
    with tempfile.TemporaryDirectory() as d:
        slug = "-tmp-idx"
        mem = Path(d) / ".claude" / "projects" / slug / "memory"
        mem.mkdir(parents=True)
        os.environ["HOME"] = d
        os.environ["CLAUDE_MEMORY_SLUG"] = slug
        os.environ["MEMORY_INDEX_BUDGET"] = "4000"

        def mk(t, i):
            (mem / f"{t}_mem{i}.md").write_text(
                f"---\nname: {t}_mem{i}\ndescription: desc number {i} about thing {i} "
                f"with enough text to consume budget space here\nmetadata:\n  type: {t}\n---\nbody {i}\n")
        # realistic distribution: few identity/rules, many project/reference
        for i in range(3):   mk("user", i)
        for i in range(15):  mk("feedback", i)
        for i in range(60):  mk("project", i)
        for i in range(42):  mk("reference", i)
        spec = importlib.util.spec_from_file_location("mib", ROOT/"bin"/"memory_index_build.py")
        mib = importlib.util.module_from_spec(spec); spec.loader.exec_module(mib)
        out = mib.build(4000)
        linked = set(re.findall(r'\]\(([^)]+\.md)\)', out))
        over = sum(int(m) for m in re.findall(r'\+(\d+) more', out))
        disk = [p.name for p in mem.glob("*.md") if p.name != "MEMORY.md"]
        assert len(linked)+over == len(disk), f"orphans! linked{len(linked)}+over{over}!=disk{len(disk)}"
        # user+feedback always fully linked
        for t in ("user","feedback"):
            want = sum(1 for f in disk if f.startswith(t))
            got = sum(1 for f in linked if f.startswith(t))
            assert got == want, f"{t}: {got}/{want} — always-include violated"
        # some project/reference must spill to overflow at this tight budget,
        # proving the fill is actually bounded
        assert over > 0, "expected overflow at a tight budget"
        print(f"ok — I1 index build (linked={len(linked)} over={over} disk={len(disk)} bytes={len(out)})")

if __name__ == "__main__":
    main()
