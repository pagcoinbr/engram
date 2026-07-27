#!/usr/bin/env python3
"""Snippet-shelf regressions: snippets get their own index section, are never
auto-merged with each other, and the lookup gate abstains without corroboration.

The three properties that make the shelf safe to reuse code from — run against a
synthetic store, no Neo4j/Qdrant/LLM needed."""
import importlib.util, os, re, sys, tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def _load(name, relpath):
    spec = importlib.util.spec_from_file_location(name, ROOT / relpath)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def test_index_section():
    """A snippet lands under its own heading, not silently in References."""
    with tempfile.TemporaryDirectory() as d:
        slug = "-tmp-snip"
        mem = Path(d) / ".claude" / "projects" / slug / "memory"
        mem.mkdir(parents=True)
        os.environ["HOME"] = d
        os.environ["CLAUDE_MEMORY_SLUG"] = slug
        for t in ("project", "reference", "snippet"):
            (mem / f"{t}_a.md").write_text(
                f"---\nname: {t}_a\ndescription: a {t} memory\nmetadata:\n  type: {t}\n---\nbody\n")
        out = _load("mib", "bin/memory_index_build.py").build(17000)
        assert "## Snippets (code that worked)" in out, out
        # the snippet is linked under ITS heading, not the References one
        sec = out.split("## Snippets (code that worked)")[1]
        assert "snippet_a.md" in sec, out
        assert "snippet_a.md" not in out.split("## Snippets")[0], "snippet leaked into an earlier section"
        print("ok — snippet index section")


def test_never_merged():
    """auto-curate must refuse to merge snippets (near-identical scripts that differ
    only in chain/host would fuse into a plausible, wrong hybrid)."""
    src = (ROOT / "bin" / "memory_auto_curate.py").read_text()
    m = re.search(r'if "snippet" in types:.*?print\(f?"\[auto-curate\] skip snippet cluster', src, re.S)
    assert m, "auto_curate lost its snippet-cluster guard"
    # and the guard must come BEFORE the same-type merge admission
    assert src.index('if "snippet" in types:') < src.index("typed.append(sorted(files))"), \
        "snippet guard must precede the same-type admission"
    print("ok — snippets excluded from auto-merge")


def test_lookup_abstains():
    """The corroboration gate: one ranker is a suggestion, two is a candidate."""
    mf = _load("mf", "bin/memory_fusion.py")
    both = mf.fuse({"vector": ["s1.md", "s2.md"], "keyword": ["s1.md", "s3.md"]})
    picked = mf.select_snippets(both, live_rankers=2)
    assert [p["file"] for p in picked] == ["s1.md"], picked
    assert picked[0]["confirmed"]
    # single ranker on a multi-index install -> abstain entirely
    solo = mf.fuse({"keyword": ["s9.md"]})
    assert mf.select_snippets(solo, live_rankers=3) == []
    # ...but on a single-index install, surface it flagged rather than going silent
    weak = mf.select_snippets(solo, live_rankers=1)
    assert len(weak) == 1 and not weak[0]["confirmed"]
    # never floods the model with options
    many = mf.fuse({"vector": [f"s{i}.md" for i in range(10)],
                    "keyword": [f"s{i}.md" for i in range(10)]})
    assert len(mf.select_snippets(many, live_rankers=2)) == 2
    print("ok — lookup corroboration gate")


def test_render_carries_risk_guidance():
    """The rendered result must always carry the risk/target guidance — it's the
    safety behaviour, and both MCP servers share this one renderer."""
    mf = _load("mf", "bin/memory_fusion.py")
    txt = mf.format_snippet_hits(
        "sweep gas", [{"file": "snippet_x.md", "score": 0.1, "sources": ["vector", "keyword"],
                       "confirmed": True}], lambda f: ("snippet_x", "sweeps gas"))
    for must in ("risk:", "diff and adapt", "snippet_x.md"):
        assert must in txt, txt
    assert "no snippet matched" in mf.format_snippet_hits("x", [], lambda f: ("", ""))
    print("ok — render carries risk guidance")


if __name__ == "__main__":
    home = os.environ.get("HOME")
    try:
        test_index_section()
    finally:
        if home:
            os.environ["HOME"] = home
    test_never_merged()
    test_lookup_abstains()
    test_render_carries_risk_guidance()
    print("ok — snippet shelf")
