#!/usr/bin/env python3
"""memory_fusion.py — Reciprocal Rank Fusion (RRF) for engram hybrid recall.

Combine several ranked lists of memories (each keyed by the `.md` filename) into a
single ranking. Used by the hybrid recall tools to fuse graph + vector + keyword
results so the indexes are greater than the sum of their parts.

Zero dependencies (stdlib only) so this module imports cleanly from BOTH the graph
venv and the vector venv — it must never import qdrant_client or graphiti.

RRF (Cormack et al. 2009): a memory's score is the sum, over every ranker that
returned it, of weight / (k_rrf + rank), where rank is the memory's 1-based
position in that ranker's list. A ranker that didn't return a memory contributes
nothing for it; an empty ranker contributes nothing at all. This makes fusion
robust to a dead/disabled backend — it just drops out.
"""
from __future__ import annotations


def fuse(named_rankings: dict[str, list[str]], k_rrf: int = 60,
         weights: dict[str, float] | None = None) -> list[dict]:
    """Fuse named ranked filename lists via RRF.

    named_rankings: {ranker_name: [filename, ...]} ordered best-first. Each list
        should already be deduped within itself; duplicates are tolerated (first
        occurrence wins for that ranker).
    k_rrf: the RRF constant (60 is standard) — damps the contribution of low ranks.
    weights: optional {ranker_name: weight}; missing rankers default to 1.0.

    Returns [{"file", "score", "sources"}] sorted by (-score, file) — deterministic.
    """
    weights = weights or {}
    scores: dict[str, float] = {}
    sources: dict[str, list[str]] = {}
    for ranker, files in (named_rankings or {}).items():
        w = float(weights.get(ranker, 1.0))
        seen = set()
        for rank, f in enumerate(files or [], start=1):
            if f in seen:        # first occurrence wins within a single ranker
                continue
            seen.add(f)
            scores[f] = scores.get(f, 0.0) + w / (k_rrf + rank)
            sources.setdefault(f, []).append(ranker)
    fused = [{"file": f, "score": scores[f], "sources": sources[f]} for f in scores]
    fused.sort(key=lambda d: (-d["score"], d["file"]))   # stable tie-break by filename
    return fused


def select_snippets(fused: list[dict], live_rankers: int, k: int = 2,
                    min_sources: int = 2) -> list[dict]:
    """Pick snippet candidates from a fused ranking, or return [] (abstain).

    Recall is relevance-ranked, not confidence-ranked: RRF always puts *something*
    first, so "top hit" is far too weak a bar for "reuse this code". The bar here is
    CORROBORATION — at least `min_sources` independent rankers had to surface the
    same file. One ranker liking it is a suggestion; two agreeing is a candidate.

    live_rankers: how many rankers actually returned anything this call. On an install
        with only one index alive, two-way agreement is impossible, so the threshold
        drops to what's achievable and every hit comes back `confirmed: False` —
        surfaced, but explicitly marked as a single-index guess rather than suppressed.

    Returns at most `k` dicts (the fused entry + `confirmed`), best-first. Deliberately
    small: a long list invites the model to shop for the answer it already wanted.
    """
    need = min(max(1, int(min_sources)), max(1, int(live_rankers)))
    out = []
    for d in fused:
        if len(d.get("sources") or []) < need:
            continue
        out.append({**d, "confirmed": len(d["sources"]) >= min_sources})
        if len(out) >= k:
            break
    return out


NO_MATCH = ("(no snippet matched: {task})\n"
            "Nothing proven on the shelf for this — write it fresh, and consider "
            "saving it as a `snippet` memory once it works.")

NEXT_STEPS = ("Next: READ the file(s) above. Reuse verbatim only if the target (host, "
              "chain, container, asset) matches; otherwise diff and adapt. Honour the "
              "snippet's `risk:` tag — `money`/`write` need the resolved command shown "
              "and confirmed before it runs.")


def format_snippet_hits(task: str, picked: list[dict], meta_fn) -> str:
    """Render snippet candidates identically from either MCP server.

    Lives here (the one module both the graph venv and the vector venv already
    import) so the guidance text can't drift between the two — it's the part that
    tells the model to check the target and honour `risk:`, so two versions of it is
    two safety behaviours. meta_fn(file) -> (name, description)."""
    if not picked:
        return NO_MATCH.format(task=task)
    out = [f"{len(picked)} snippet candidate(s) for: {task}", ""]
    for d in picked:
        nm, desc = meta_fn(d["file"])
        flag = "corroborated" if d.get("confirmed") else "SINGLE-INDEX GUESS — verify harder"
        out.append(f"- {nm or d['file']}  [{'+'.join(d['sources'])}; {flag}]")
        out.append(f"    file: {d['file']}")
        if desc:
            out.append(f"    {desc.strip()}")
    return "\n".join(out + ["", NEXT_STEPS])


if __name__ == "__main__":  # tiny self-check
    demo = fuse({"graph": ["a.md", "b.md"], "vector": ["b.md", "c.md"],
                 "keyword": ["a.md", "c.md"]})
    for d in demo:
        print(f"{d['score']:.5f}  {d['file']}  ({'+'.join(d['sources'])})")

    # select_snippets: corroboration gate
    picked = select_snippets(demo, live_rankers=3)
    assert [d["file"] for d in picked] == ["a.md", "b.md"], picked   # c.md is 2-source too, k=2 caps
    assert all(d["confirmed"] for d in picked)
    solo = fuse({"keyword": ["x.md", "y.md"]})
    assert select_snippets(solo, live_rankers=3) == [], "1 source must not pass a 3-ranker install"
    weak = select_snippets(solo, live_rankers=1)
    assert [d["file"] for d in weak] == ["x.md", "y.md"] and not any(d["confirmed"] for d in weak)
    assert select_snippets([], live_rankers=3) == []
    print("ok — select_snippets corroboration gate")
