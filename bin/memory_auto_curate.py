#!/usr/bin/env python3
"""memory_auto_curate.py — DETERMINISTIC, lossless, recoverable auto-consolidation.

The interactive /memory-curate uses LLM JUDGMENT + a human gate to decide what to
merge. This is the unattended sibling: it merges only what is DETERMINISTICALLY a
near-duplicate (Qdrant ANN cosine >= a high threshold, SAME type), so no judgment
call is delegated to a model. The merge is:
  * LOSSLESS   — distilled with preserve_sources=True (every source body appended
                 verbatim), and only applied if sentence_coverage proves no prose loss.
  * RECOVERABLE — source files are removed via delete_memory.sh, which snapshots to
                 .trash/ (90-day undo) first.
  * BOUNDED    — at most `max_merges_per_run` clusters per run.
Everything a model can't make safe (pruning distinct memories, deleting suspects)
stays in the human-gated /memory-curate. This never deletes a memory that isn't
absorbed into an umbrella first.

Gated by auto_curate.enabled in engram.yaml AND --apply. Dry-run otherwise.

Usage:
  memory_auto_curate.py                 # dry-run: print the merges it would do
  memory_auto_curate.py --apply         # apply (needs auto_curate.enabled: true)
"""
from __future__ import annotations
import json, os, re, subprocess, sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(Path.home() / ".claude"))
sys.path.insert(0, str(Path.home() / ".claude" / "vector"))
import memory_ai
import engram_secrets
import memory_distill_verified as mdv

HOME = Path.home()
SLUG = os.environ.get("CLAUDE_MEMORY_SLUG") or str(HOME).replace("/", "-")
MEM = HOME / ".claude" / "projects" / SLUG / "memory"
SAVE = HOME / ".claude" / "save_memory.sh"
DELETE = HOME / ".claude" / "delete_memory.sh"

AC_DEFAULTS = {
    "enabled": False,
    "merge_threshold": 0.92,      # cosine >= this to auto-merge (stricter than the 0.86 finder)
    "max_merges_per_run": 2,
    "min_sentence_coverage": 0.98,  # refuse to apply a merge that would drop prose
}


def ac_cfg(cfg):
    out = dict(AC_DEFAULTS); out.update((cfg.get("auto_curate") or {})); return out


def _fm(p: Path):
    t = p.read_text(errors="ignore")
    def g(k):
        m = re.search(rf"^\s*{k}:\s*(.+)$", t, re.M)
        return m.group(1).strip().strip('"\'') if m else ""
    return g("name") or p.stem, g("description"), g("type")


def _clusters(pairs, threshold):
    """Union-find over (score, a, b) pairs above threshold -> list of file-name sets."""
    parent = {}
    def find(x):
        parent.setdefault(x, x)
        while parent[x] != x:
            parent[x] = parent[parent[x]]; x = parent[x]
        return x
    def union(a, b):
        parent[find(a)] = find(b)
    for score, a, b in pairs:
        if score >= threshold:
            union(a, b)
    groups = {}
    for x in list(parent):
        groups.setdefault(find(x), set()).add(x)
    return [g for g in groups.values() if len(g) >= 2]


def _body_of(distilled: str) -> str:
    """Strip a leading frontmatter block from the distilled text, keep the body."""
    if distilled.startswith("---"):
        end = distilled.find("\n---", 3)
        if end != -1:
            return distilled[end + 4:].lstrip("\n")
    return distilled


def _score(p: Path, scores) -> float:
    return float(scores.get(p.name, 0.0))


def main():
    apply = "--apply" in sys.argv
    cfg = memory_ai.load()
    ac = ac_cfg(cfg)
    if apply and not ac["enabled"]:
        print("[auto-curate] --apply ignored: auto_curate.enabled is false. Dry-run.", file=sys.stderr)
        apply = False

    # fixation scores (to pick which member becomes the umbrella)
    try:
        st = json.loads((MEM / ".fixation_state.json").read_text()).get("memories", {})
        scores = {k: (v.get("last_score") or 0.0) for k, v in st.items()}
    except Exception:
        scores = {}

    # deterministic near-dup pairs via the Qdrant ANN finder
    try:
        from vector_store import EngramVectorStore
        store = EngramVectorStore(cfg); store.ensure_collection()
        pairs = store.find_duplicates(threshold=ac["merge_threshold"], max_pairs=100)
    except Exception as e:
        print(f"[auto-curate] vector store unavailable ({e}) — nothing to do.")
        return

    clusters = _clusters(pairs, ac["merge_threshold"])
    # SAME-type only (never merge a feedback into a project); require all files present
    typed = []
    for c in clusters:
        files = [f for f in c if (MEM / f).is_file()]
        if len(files) < 2:
            continue
        types = {_fm(MEM / f)[2] for f in files}
        if len(types) == 1:
            typed.append(sorted(files))
        else:
            print(f"[auto-curate] skip mixed-type cluster {files}")
    typed = typed[:ac["max_merges_per_run"]]

    print(f"# memory_auto_curate — apply={apply} enabled={ac['enabled']} "
          f"threshold={ac['merge_threshold']} cap={ac['max_merges_per_run']}")
    print(f"near-dup clusters this run: {len(typed)}")
    done = 0
    for files in typed:
        paths = [MEM / f for f in files]
        umbrella = max(paths, key=lambda p: _score(p, scores))   # keep the highest-trust member's name
        name, desc, typ = _fm(umbrella)
        key = umbrella.stem
        final, report = mdv.distill_cluster(key, files, cfg=cfg, preserve_sources=True)
        cov = report.get("sentence_coverage", 0.0)
        print(f"\n· cluster {files}  -> umbrella {umbrella.name}  sentence_cov={cov}")
        if cov < ac["min_sentence_coverage"]:
            print(f"  HOLD: sentence_coverage {cov} < {ac['min_sentence_coverage']} — not lossless, skipping")
            continue
        body = _body_of(final)
        if engram_secrets.looks_secret(body):
            print("  HOLD: umbrella tripped secret scan — skipping")
            continue
        doc = f"---\nname: {umbrella.stem}\ndescription: {desc}\nmetadata:\n  type: {typ}\n---\n\n{body}"
        absorbed = [f for f in files if f != umbrella.name]
        if not apply:
            print(f"  would WRITE umbrella {umbrella.name} + TRASH {absorbed}")
            continue
        r = subprocess.run([str(SAVE), umbrella.name, desc or umbrella.stem],
                           input=doc, capture_output=True, text=True)
        if r.returncode != 0:
            print(f"  ERROR writing umbrella: {r.stderr.strip()[:120]} — NOT deleting sources"); continue
        for a in absorbed:                    # only after the umbrella is safely written
            subprocess.run([str(DELETE), a], capture_output=True, text=True)
        print(f"  MERGED: wrote {umbrella.name}, trashed {absorbed} (recoverable in .trash/)")
        done += 1
    print(f"\napplied {done} merge(s)." if apply else "\n(dry-run — set auto_curate.enabled + --apply)")


if __name__ == "__main__":
    main()
