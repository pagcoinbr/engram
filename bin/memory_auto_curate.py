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
    # Who reviews a merge before it can auto-apply:
    #   auto   (default) — use Codex if it's installed; otherwise fall back to HUMAN
    #                      (every merge -> Telegram approval queue). Best for portability:
    #                      most users have Claude but NOT Codex.
    #   codex  — force Codex review (near-lossless+clean auto-applies, else human queue).
    #   human  — NO Codex; EVERY merge goes to the human Telegram queue (nothing auto-
    #            applies). Safe default when you have only one agent.
    # There is intentionally no "no reviewer, just auto-apply" mode — a compressed merge
    # always gets either Codex or a human before it touches the store.
    "review_gate": "auto",
    # Orphan pruning: PROPOSE (never auto) stale memories with no merge target for a
    # one-tap Telegram prune. frequency==0 means "never discussed", NOT "worthless" (a
    # disaster-recovery runbook is freq-0 for a year), so this is human-gated by design.
    "prune_orphans": True,
    "orphan_age_days": 90,
    "max_prunes_per_run": 5,
    # When Codex DOES review: auto-apply only a near-LOSSLESS merge (a fact outside
    # Codex's slice could be dropped otherwise); everything lossy -> the human queue.
    "min_auto_coverage": 0.90,
}


def _resolve_gate(ac) -> str:
    """codex | human. 'auto' -> codex iff the advisor script AND the codex CLI are both
    present; else human (Telegram approval). So a user without Codex still gets a safe
    reviewer (themselves, one tap) instead of unreviewed auto-merges."""
    import shutil
    g = (ac.get("review_gate") or "auto").lower()
    if g in ("codex", "human"):
        return g
    return "codex" if (ADVISOR.exists() and shutil.which("codex")) else "human"

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

    gate_mode = _resolve_gate(ac)     # "codex" or "human"
    print(f"# memory_auto_curate — apply={apply} enabled={ac['enabled']} "
          f"threshold={ac['merge_threshold']} cap={ac['max_merges_per_run']} review_gate={gate_mode}")
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
        # HUMAN gate (no Codex): every merge goes to the Telegram approval queue.
        if gate_mode == "human":
            gate.propose("merge_apply", params, f"Needs your approval (no Codex). {preview}",
                         files=files, codex_verdict="human-gate")
            print("  QUEUED for human approval (human gate — no Codex)"); queued += 1
            continue
        # CODEX gate: auto-apply only a near-LOSSLESS + clean merge; lossy or non-clean
        # -> human queue. coverage is on the FULL sources (not Codex's truncated slice).
        verdict = _codex_verdict(doc, files)
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

    pruned = _orphan_prune_pass(cfg, ac, store, gate, apply) if ac.get("prune_orphans") else 0
    print(f"\napplied {done}, queued {queued}, orphan-prune proposed {pruned}."
          if apply else "\n(dry-run — set auto_curate.enabled + --apply)")


def _orphan_prune_pass(cfg, ac, store, gate, apply) -> int:
    """PROPOSE stale orphans for a one-tap Telegram prune (never auto). An orphan is:
    type∈{project,reference}, age>=orphan_age_days, frequency==0 (never discussed), NO
    close neighbor (no merge target), not a suspect, and not a skill source."""
    try:
        r = subprocess.run([sys.executable, str(HOME / ".claude" / "memory_score.py"), "--json"],
                           capture_output=True, text=True, timeout=300)
        scored = json.loads(r.stdout).get("memories", [])
    except Exception as e:
        print(f"[orphan-prune] scorer unavailable ({e}) — skipping"); return 0
    # set of files that HAVE a merge target (close neighbor >= 0.80) — those are not orphans
    close = set()
    try:
        for _s, a, b in store.find_duplicates(threshold=0.80, max_pairs=2000):
            close.add(a); close.add(b)
    except Exception:
        pass
    age_min = float(ac.get("orphan_age_days", 90))
    prunable = []
    for m in scored:
        name = m.get("name", "")
        if m.get("type") not in ("project", "reference"):     # never user/feedback
            continue
        if float(m.get("age_days", 0)) < age_min or m.get("frequency", 1) != 0 or m.get("suspicion"):
            continue
        if name in close:                                     # has a merge target -> not an orphan
            continue
        try:
            if "Promoted to skill:" in (MEM / name).read_text(errors="ignore"):
                continue                                      # skill sources are load-bearing
        except Exception:
            continue
        prunable.append(name)
    n = 0
    for name in prunable[:int(ac.get("max_prunes_per_run", 5))]:
        preview = f"Prune stale orphan {name}: aged ≥{int(age_min)}d, never recalled, no merge target. → .trash (recoverable)."
        if apply:
            gate.propose("orphan_prune", {"name": name}, preview, files=[name])
        else:
            print(f"  would propose prune: {name}")
        n += 1
    if prunable:
        print(f"[orphan-prune] {len(prunable)} orphan(s); proposed {n} (cap {ac.get('max_prunes_per_run', 5)})")
    return n


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
