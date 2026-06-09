#!/usr/bin/env python3
"""memory_distill.py — DRAFT (never apply) cluster distillations for the fixation
maintenance report. As of 2026-05-30 this delegates to the VERIFIED pipeline
(memory_distill_verified.distill_cluster): the LLM writes prose, then code
GUARANTEES hard-fact coverage (~1.0 by construction), preserves caveats, drops
secrets, and flags possible hallucinations. Replaces the old freeform call that
silently dropped ports/paths/caveats and often timed out.

Best-effort: notes and exits 0 if the local LLM is unreachable. Bounded by
MEM_CLUSTER_MAX so a slow model can't run unbounded. Writes a coverage signal to
memory/.distill_coverage.json so memory_grade.py can score distillation quality.
"""
from __future__ import annotations
import json, os, re, sys
from pathlib import Path
from collections import defaultdict

sys.path.insert(0, str(Path.home() / ".claude"))
import memory_ai
import memory_distill_verified as mdv

HOME = Path.home()
def slug() -> str:
    return os.environ.get("CLAUDE_MEMORY_SLUG") or str(HOME).replace("/", "-")
MEM_DIR     = HOME / ".claude" / "projects" / slug() / "memory"
MIN_CLUSTER = int(os.environ.get("MEM_CLUSTER_MIN", "3"))
MAX_CLUSTER = int(os.environ.get("MEM_CLUSTER_MAX", "3"))
COV_FILE    = MEM_DIR / ".distill_coverage.json"

def main():
    cfg = memory_ai.load()
    if not memory_ai.local_enabled(cfg):
        print("_local_enabled is false — distillation skipped._"); return
    if not MEM_DIR.is_dir():
        print(f"_no memory dir at {MEM_DIR}_"); return
    files = [p for p in MEM_DIR.glob("*.md") if p.name != "MEMORY.md"]
    clusters = defaultdict(list)
    for p in files:
        toks = re.split(r"_", p.stem)
        key = "_".join(toks[:2]) if len(toks) >= 2 else toks[0]
        clusters[key].append(p)
    cands = sorted(((k, v) for k, v in clusters.items() if len(v) >= MIN_CLUSTER),
                   key=lambda kv: -len(kv[1]))[:MAX_CLUSTER]
    if not cands:
        print(f"_No clusters of >= {MIN_CLUSTER} members to distill._"); return
    try:
        memory_ai.ollama_generate("reply ok", role="triage", cfg=cfg)
    except Exception as e:
        print(f"_distill expert unreachable: {e}. Drafts skipped._"); return

    model = memory_ai.expert_model("distill", cfg)
    best_cov = 0.0; cov_clusters = {}
    for key, ps in cands:
        names = [p.name for p in ps]
        try:
            draft, rep = mdv.distill_cluster(key, names, cfg=cfg)
        except Exception as e:
            print(f"## Proposed distillation: `{key}_*` ({len(ps)} notes) — via {model}\n")
            print(f"_(distill failed: {e})_\n")
            continue
        cov = rep.get("final_fact_coverage", 0.0)
        best_cov = max(best_cov, cov)
        cov_clusters[key] = {"natural": rep.get("natural_fact_coverage"),
                             "final": cov, "hallucinations": len(rep.get("possible_hallucinations", []))}
        print(f"## Proposed distillation: `{key}_*` ({len(ps)} notes) — via {model} "
              f"[verified: facts {rep['natural_fact_coverage']:.0%}→{cov:.0%}, "
              f"{rep['appendix_facts']} appended, {len(rep['possible_hallucinations'])} hallucination-flags]\n")
        print("Sources: " + ", ".join(names) + "\n")
        print("```markdown\n" + draft.strip() + "\n```\n")

    # persist coverage signal for memory_grade.py
    try:
        COV_FILE.write_text(json.dumps(
            {"best_final_coverage": best_cov, "clusters": cov_clusters,
             "engine": "memory_distill_verified.py"}, indent=1))
    except Exception:
        pass

if __name__ == "__main__":
    main()
