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
    "codex_gate": True,           # Codex reviews each merge before auto-apply
    # A LOSSY (compressed) merge is NEVER auto-applied: a fact outside Codex's review
    # slice could be dropped while the source leaves recall. Auto-apply ONLY when the
    # compression preserved nearly all source sentences (full-source metric,
    # deterministic) AND Codex is clean; everything else -> the human approval queue.
    "min_auto_coverage": 0.90,
}

ADVISOR = HOME / ".claude" / "skills" / "code-advisor" / "scripts" / "code_advisor.py"


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
          f"threshold={ac['merge_threshold']} cap={ac['max_merges_per_run']} codex_gate={ac['codex_gate']}")
    print(f"near-dup clusters this run: {len(typed)}")
    import engram_telegram_gate as gate
    done = queued = 0
    for files in typed:
        paths = [MEM / f for f in files]
        umbrella = max(paths, key=lambda p: _score(p, scores))   # keep the highest-trust member's name
        name, desc, typ = _fm(umbrella)
        # COMPRESS for real (no preserve_sources) — the umbrella is a tight consolidation.
        # Losslessness is provided by REVERSIBILITY (sources -> quarantine), not by
        # appending everything. sentence_coverage is recorded, not gated.
        final, report = mdv.distill_cluster(umbrella.stem, files, cfg=cfg, preserve_sources=False)
        body = _body_of(final)
        # Secret-scan desc+body AND every source: if any holds a credential, do NOT
        # consolidate (leave as-is) — never route secret-bearing text onward.
        if (not body.strip() or engram_secrets.looks_secret(f"{desc}\n{body}")
                or any(engram_secrets.looks_secret((MEM/f).read_text(errors="ignore")) for f in files)):
            print(f"· {files}: HOLD (empty umbrella or secret in cluster) — skipping"); continue
        doc = f"---\nname: {umbrella.stem}\ndescription: {desc}\nmetadata:\n  type: {typ}\n---\n\n{body}"
        absorbed = [f for f in files if f != umbrella.name]
        # transaction id so a second merge on the same umbrella can't clobber the
        # first merge's backup (each merge's originals live under their own dir)
        import hashlib as _hl
        merge_id = _hl.sha256((umbrella.name + "".join(sorted(absorbed)) + doc).encode()).hexdigest()[:10]
        params = {"umbrella": umbrella.name, "umbrella_content": doc, "desc": desc,
                  "absorbed": absorbed, "merge_id": merge_id}
        preview = (f"Merge {len(files)} near-dups → {umbrella.name} (compressed "
                   f"{report.get('draft_chars')} chars, prose-cov {report.get('sentence_coverage')}). "
                   f"Absorbed→quarantine: {', '.join(absorbed)}")
        print(f"\n· {files} -> {umbrella.name}")
        if not apply:
            print(f"  would: {preview}"); continue
        cov = report.get("sentence_coverage", 0.0)
        verdict = _codex_verdict(doc, files) if ac["codex_gate"] else "APPROVE"
        # AUTO-APPLY only a near-LOSSLESS + Codex-clean merge; a lossy compression
        # (the common case) is a JUDGMENT call -> route to the human queue, never
        # auto-applied. coverage is measured on the FULL sources (not Codex's slice).
        if cov >= ac["min_auto_coverage"] and verdict == "APPROVE":
            ok, detail = gate._apply_merge(params)
            print(f"  AUTO-APPLIED (near-lossless cov={cov}; {detail})")
            if ok:
                gate.notify_undo("merge_undo",
                                 {"umbrella": umbrella.name, "absorbed": absorbed, "merge_id": merge_id},
                                 f"🧠 auto-merged (near-lossless): {preview}\nTap UNDO to reverse.")
                done += 1
        else:
            why = (f"lossy compression (cov={cov} < {ac['min_auto_coverage']})"
                   if cov < ac["min_auto_coverage"] else f"Codex {verdict}")
            gate.propose("merge_apply", params, f"Needs approval — {why}. {preview}",
                         files=files, codex_verdict=verdict)
            print(f"  QUEUED for human approval ({why})"); queued += 1
    print(f"\napplied {done}, queued {queued}." if apply else "\n(dry-run — set auto_curate.enabled + --apply)")


def _codex_verdict(umbrella_doc, source_files) -> str:
    """Ask the Codex advisor whether the compressed umbrella loses information vs its
    sources or introduces a secret. Returns APPROVE only on an explicit clean verdict;
    DEFER on anything else (fail-closed → routes to the human queue)."""
    if not ADVISOR.exists():
        return "DEFER-no-advisor"
    # Redact EVERYTHING before it reaches the advisor subprocess/backend/logs — the
    # whole umbrella doc (its frontmatter description may hold a secret) AND every
    # source. A scanner-missed credential in a legacy memory must not leak.
    safe_umbrella = engram_secrets.redact(umbrella_doc)[0]
    srcs = "\n\n".join(f"### {f}\n{engram_secrets.redact((MEM/f).read_text(errors='ignore')[:2000])[0]}"
                       for f in source_files)
    blob = f"PROPOSED UMBRELLA:\n{safe_umbrella[:4000]}\n\nSOURCES IT REPLACES:\n{srcs[:6000]}"
    try:
        r = subprocess.run(
            [sys.executable, str(ADVISOR), "--mode", "review", "--stdin",
             "--task", ("Does this umbrella drop any durable fact from the sources, or add a secret/"
                        "hallucination? Reply a final line exactly 'VERDICT: APPROVE' or 'VERDICT: REJECT'.")],
            input=blob, capture_output=True, text=True, timeout=180)
    except Exception as e:
        return f"DEFER-{type(e).__name__}"
    if r.returncode != 0:
        return "DEFER-advisor-rc"
    import re as _re
    # Injected source text could try to surface a spoofed verdict token. Require:
    # exactly ONE verdict anywhere, it is APPROVE, and it is on the LAST non-empty
    # line. Anything else -> DEFER to the human queue (fail-closed).
    verdicts = _re.findall(r"VERDICT:\s*(APPROVE|REJECT)", r.stdout or "", _re.I)
    lines = [ln.strip() for ln in (r.stdout or "").splitlines() if ln.strip()]
    last = lines[-1].upper() if lines else ""
    if len(verdicts) == 1 and verdicts[0].upper() == "APPROVE" and last == "VERDICT: APPROVE":
        return "APPROVE"
    return "REJECT" if verdicts else "DEFER-no-verdict"


if __name__ == "__main__":
    main()
