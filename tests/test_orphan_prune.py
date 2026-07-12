#!/usr/bin/env python3
"""zero-cmd #3: orphan-prune proposer selectivity. Only stale project/reference memories
with no merge target, freq 0, aged, not a suspect, not a skill source get PROPOSED."""
import sys, types, importlib.util, json, tempfile, shutil, os
from pathlib import Path
ROOT = Path(__file__).resolve().parent.parent


def main():
    for m in ("memory_ai", "engram_secrets", "memory_distill_verified"):
        sys.modules[m] = types.ModuleType(m)
    d = tempfile.mkdtemp(); os.environ["HOME"] = d; os.environ["CLAUDE_MEMORY_SLUG"] = "-t"
    sp = importlib.util.spec_from_file_location("ac", ROOT / "bin" / "memory_auto_curate.py")
    ac = importlib.util.module_from_spec(sp); sp.loader.exec_module(ac)
    mem = Path(d) / ".claude/projects/-t/memory"; mem.mkdir(parents=True); ac.MEM = mem
    (mem / "project_old.md").write_text("---\nname: project_old\n---\nstale\n")
    (mem / "project_skill.md").write_text("---\nname: project_skill\n---\nsteps\n\n**Promoted to skill:** auto/x\n")
    scored = {"memories": [
        {"name": "project_old.md", "type": "project", "age_days": 120, "frequency": 0, "suspicion": False},
        {"name": "reference_young.md", "type": "reference", "age_days": 5, "frequency": 0, "suspicion": False},
        {"name": "project_hot.md", "type": "project", "age_days": 200, "frequency": 40, "suspicion": False},
        {"name": "project_skill.md", "type": "project", "age_days": 300, "frequency": 0, "suspicion": False},
        {"name": "feedback_rule.md", "type": "feedback", "age_days": 400, "frequency": 0, "suspicion": False},
    ]}
    proposals = []
    gate = types.SimpleNamespace(propose=lambda op, p, prev, files=None, codex_verdict=None: proposals.append(p["name"]))
    ac.subprocess = types.SimpleNamespace(run=lambda *a, **k: types.SimpleNamespace(stdout=json.dumps(scored), returncode=0))
    store = types.SimpleNamespace(find_duplicates=lambda threshold, max_pairs: [])
    ac._orphan_prune_pass({}, ac.AC_DEFAULTS, store, gate, apply=True)
    shutil.rmtree(d)
    assert proposals == ["project_old.md"], f"only the stale orphan should be proposed, got {proposals}"
    print("ok — orphan-prune selectivity (young/hot/skill-source/feedback all skipped)")


if __name__ == "__main__":
    main()
