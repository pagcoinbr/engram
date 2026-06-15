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


if __name__ == "__main__":  # tiny self-check
    demo = fuse({"graph": ["a.md", "b.md"], "vector": ["b.md", "c.md"],
                 "keyword": ["a.md", "c.md"]})
    for d in demo:
        print(f"{d['score']:.5f}  {d['file']}  ({'+'.join(d['sources'])})")
