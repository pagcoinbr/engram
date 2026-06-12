#!/usr/bin/env python3
"""memory_light_curate.py — the twice-daily LIGHT local pass (analysis only).

A small mixture-of-experts, all local:
  • CURATION side  — the *similarity* expert (embeddings, e.g. nomic-embed-text)
    finds semantic near-duplicates / merge clusters the keyword pass misses.
  • FIXATION side  — reuses memory_score.py for trust signals (status mix,
    suspects, lowest-confidence) so one report shows both lenses.

Prints a labeled markdown report. NO mutation (the cron handles quarantine;
human-gated /memory-curate|fixate apply handles merges/deletes). Respects
local_enabled and routes models via memory_ai.yaml.
"""
from __future__ import annotations
import json, math, os, re, subprocess, sys
from pathlib import Path
from collections import Counter

sys.path.insert(0, str(Path.home() / ".claude"))
import memory_ai

HOME = Path.home()
def slug() -> str:
    return os.environ.get("CLAUDE_MEMORY_SLUG") or str(HOME).replace("/", "-")
MEM_DIR = HOME / ".claude" / "projects" / slug() / "memory"

def cosine(a, b) -> float:
    s = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a)); nb = math.sqrt(sum(y * y for y in b))
    return s / (na * nb) if na and nb else 0.0

def embed_text(p: Path) -> str:
    t = p.read_text(errors="ignore")
    nm = re.search(r"^name:\s*(.+)$", t, re.M)
    ds = re.search(r"^description:\s*(.+)$", t, re.M)
    s = ((nm.group(1) if nm else "") + " " + (ds.group(1) if ds else "")).strip()
    return s or p.stem


def _vector_dupes(cfg, thr):
    """ANN duplicate finder via the OPTIONAL Qdrant index — O(n·log n) instead of
    O(n²). Returns a list of (score, a, b) pairs, or None when the vector store is
    disabled/unreachable (so the caller falls back to the cosine path)."""
    if not memory_ai.vector_enabled(cfg):
        return None
    try:
        sys.path.insert(0, str(Path.home() / ".claude" / "vector"))
        sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "vector"))
        import vector_config as vc
        from vector_store import EngramVectorStore
    except Exception:
        return None
    try:
        store = EngramVectorStore(cfg)
        store.ensure_collection()
        print("_(duplicate finder: using Qdrant ANN index)_\n")
        return store.find_duplicates(threshold=thr)
    except vc.VectorUnavailable as e:
        print(f"_vector store unreachable ({e}) — falling back to cosine._\n")
        return None
    except Exception as e:
        print(f"_vector duplicate finder failed ({e}) — falling back to cosine._\n")
        return None


def _cosine_dupes(files, cfg, thr):
    """The pure-markdown fallback: embed each file's name+description and do the
    O(n²) pairwise cosine. Returns (score, a, b) pairs, or None if embeddings are
    unreachable."""
    embs = {}
    try:
        for p in files:
            embs[p.name] = memory_ai.ollama_embed(embed_text(p), cfg=cfg)
    except Exception as e:
        print(f"_similarity expert unreachable: {e}_\n")
        return None
    names = list(embs)
    pairs = []
    for i in range(len(names)):
        for j in range(i + 1, len(names)):
            c = cosine(embs[names[i]], embs[names[j]])
            if c >= thr:
                pairs.append((c, names[i], names[j]))
    pairs.sort(reverse=True)
    return pairs

def main():
    cfg = memory_ai.load()
    if not memory_ai.local_enabled(cfg):
        print("_local_enabled is false — light pass skipped._")
        return
    if not MEM_DIR.is_dir():
        print(f"_no memory dir at {MEM_DIR}_")
        return
    lp = cfg.get("light_pass", {})
    files = [p for p in MEM_DIR.glob("*.md") if p.name != "MEMORY.md"]

    # ---------- DUPLICATE FINDER (semantic structure) ----------
    cur = lp.get("duplicate_finder", lp.get("curation", {}))
    if cur.get("enabled", True):
        thr = float(cur.get("dup_threshold", 0.86))
        print(f"## Duplicate Finder — semantic near-duplicate / merge candidates (cosine ≥ {thr:.2f})\n")
        pairs = _vector_dupes(cfg, thr)          # Qdrant ANN when the optional vector store is on
        if pairs is None:                        # disabled/unreachable -> pure-markdown cosine fallback
            pairs = _cosine_dupes(files, cfg, thr)
        if pairs is None:
            pass                                 # similarity expert unreachable (already reported)
        elif not pairs:
            print(f"_No semantic near-duplicates above {thr:.2f}._\n")
        else:
            for c, a, b in pairs[:30]:
                print(f"- `{c:.3f}`  {a}  ⇄  {b}")
            print(f"\n_{len(pairs)} candidate pair(s). Run `/memory-curate` (Duplicate Finder) to merge — human-gated._\n")

    # ---------- INJECTION GUARD (trust signals + suspects) ----------
    fx = lp.get("injection_guard", lp.get("fixation", {}))
    if fx.get("enabled", True):
        print("## Injection Guard — trust scoring + suspect detection\n")
        try:
            out = subprocess.run([sys.executable, str(HOME / ".claude" / "memory_score.py"), "--json"],
                                 capture_output=True, text=True, timeout=180).stdout
            sc = json.loads(out)
            dist = Counter(m["status"] for m in sc["memories"])
            print(f"status distribution: `{dict(dist)}`\n")
            susp = [m for m in sc["memories"] if m["suspicion"]]
            print(f"suspect (injection-gated): {len(susp)}")
            for m in susp:
                print(f"  - ⚠ `{m['name']}`")
            print("\nlowest confidence (review soonest):")
            for m in sorted(sc["memories"], key=lambda x: x["confidence"])[:8]:
                print(f"  - {m['name']}  conf={m['confidence']} freq={m['frequency']} surv={m['survival']} ({m['status']})")
            print("\n_Run `/memory-fixate` to distill/quarantine (human-gated)._")
        except Exception as e:
            print(f"_fixation scoring failed: {e}_")

if __name__ == "__main__":
    main()
