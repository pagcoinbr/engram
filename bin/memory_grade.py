#!/usr/bin/env python3
"""memory_grade.py — grade the memory SYSTEM (not individual memories) against a
model of human cognition, emitting ONE "Memory Quotient" point per run so the
score can be tracked and improved over time.

The per-memory trust scorer is memory_score.py; THIS script consumes its --json
output plus a few structural checks and rolls them into four cognitive dimensions
the operator named: short-term memory, long-term memory, learning, tooling.

Each dimension scores 0..25 -> total /100 (the "Memory Quotient", MQ). A history
line is appended to memory/.grade_history.jsonl each run so deltas are visible
(the "give it a point each run, compare to last" loop).

Human-memory mapping
  short_term : working buffer (~7+-2, recent, pre-consolidation). Healthy = active
               intake but the provisional buffer isn't overflowing/unconsolidated.
  long_term  : consolidated semantic+episodic store. Depth & trust of corroborated
               /fixed memories, confidence, age.
  learning   : consolidation (short->long maturation, survival growth), skill
               formation, and distillation QUALITY (fact coverage), plus delta vs
               the previous run.
  tooling    : metacognition / self-maintenance. Machinery present, index integrity,
               no secrets stored (source monitoring), verified-distiller available.

Usage:
  memory_grade.py            # human scorecard + append history
  memory_grade.py --json     # machine-readable
  memory_grade.py --no-save  # don't append to history
"""
from __future__ import annotations
import json, os, re, subprocess, sys
from pathlib import Path
from datetime import datetime, timezone

HOME = Path.home()
def slug() -> str:
    return os.environ.get("CLAUDE_MEMORY_SLUG") or str(HOME).replace("/", "-")
MEM_DIR = HOME / ".claude" / "projects" / slug() / "memory"
HIST = MEM_DIR / ".grade_history.jsonl"
SKILLS_DIR = HOME / ".claude" / "skills"

# Secret/cruft signatures that should NEVER live in long-term memory (source
# monitoring — see the security-mindset memories). Tuned 2026-05-30 to stop
# false positives: public LN pubkeys (66-hex, legit), txids, ENV_VAR *names*,
# and <placeholder>/$VAR/... docs are NOT secrets — only assigned VALUES are.
# A line is a leak only if a secret-ish KEY is set to a concrete literal.
_ASSIGN = r"(?:client[_-]?secret|webhook[_-]?secret|[A-Za-z0-9_]*secret|password|passwd|" \
          r"[A-Za-z0-9_]*api[_-]?key|access[_-]?token|bearer)"
SECRET_RE = re.compile(
    r"(?i)\b" + _ASSIGN + r"\s*[:=]\s*['\"`]?"
    r"(?![<$]|\.\.\.|x\b|any\b|redacted|placeholder|\*{2,})"   # skip <ph>, $VAR, ..., x, redacted, ***
    r"[A-Za-z0-9+/_\-]{8,}"                                     # an actual literal of length >=8
    r"|-----BEGIN [A-Z ]*PRIVATE KEY-----")

def clamp(x, lo=0.0, hi=1.0): return max(lo, min(hi, x))

def load_scores() -> list[dict]:
    out = subprocess.run([sys.executable, str(HOME/".claude"/"memory_score.py"), "--json"],
                         capture_output=True, text=True)
    d = json.loads(out.stdout)
    return d["memories"] if isinstance(d, dict) and "memories" in d else d

def prev_run() -> dict | None:
    if not HIST.exists(): return None
    last = None
    for line in HIST.read_text().splitlines():
        line = line.strip()
        if line:
            try: last = json.loads(line)
            except Exception: pass
    return last

def grade():
    mems = load_scores()
    n = len(mems)
    by_status = {}
    for m in mems:
        by_status.setdefault(m["status"], []).append(m)
    prov = by_status.get("provisional", [])
    corr = by_status.get("corroborated", [])
    fixed = by_status.get("fixed", [])
    suspect = by_status.get("suspect", [])
    trusted = corr + fixed

    def mean(xs): return sum(xs)/len(xs) if xs else 0.0

    # ---- structural checks (files on disk) --------------------------------
    files = [p for p in MEM_DIR.glob("*.md") if p.name != "MEMORY.md"]
    index_txt = (MEM_DIR/"MEMORY.md").read_text(errors="ignore") if (MEM_DIR/"MEMORY.md").exists() else ""
    indexed = sum(1 for p in files if f"]({p.name})" in index_txt)
    index_integrity = indexed / max(1, len(files))           # 1.0 = every file indexed
    # broken wikilinks
    names = {p.stem for p in files}
    broken = 0; total_links = 0
    for p in files:
        for m in re.findall(r"\[\[([\w\-./]+?)\]\]", p.read_text(errors="ignore")):
            total_links += 1
            if m.replace(".md","") not in names: broken += 1
    link_integrity = 1.0 - (broken/total_links if total_links else 0.0)
    # secrets stored in memory (source-monitoring failure)
    secret_hits = sum(1 for p in files if SECRET_RE.search(p.read_text(errors="ignore")))
    secret_clean = 1.0 - clamp(secret_hits / max(1, len(files)) * 4)   # a few poison the score
    # self-maintenance tooling present
    tools = ["memory_score.py","memory_distill_verified.py","memory_fixate_cron.sh",
             "memory_light_curate.py","save_memory.sh","delete_memory.sh","memory_audit.sh"]
    tools_present = sum(1 for t in tools if (HOME/".claude"/t).exists()) / len(tools)
    has_verified_distiller = (HOME/".claude"/"memory_distill_verified.py").exists()
    skills_promoted = len([d for d in SKILLS_DIR.glob("*") if d.is_dir()]) if SKILLS_DIR.exists() else 0

    # ---- DIMENSION 1: SHORT-TERM MEMORY (working buffer) -----------------
    # Healthy intake = there IS recent capture, but the provisional buffer is not
    # overflowing (Miller's 7+-2 as a soft target for un-consolidated items).
    prov_frac = len(prov)/max(1, n)
    buffer_pressure = clamp(len(prov)/12.0)            # >12 unconsolidated = pressure
    has_recent = 1.0 if any(m.get("age_days",999) < 3 for m in mems) else 0.4
    # reward active-but-controlled buffer: peak when prov_frac ~0.10-0.25
    if prov_frac <= 0.25: buffer_quality = clamp(prov_frac/0.20)        # ramp up to healthy
    else: buffer_quality = clamp(1.0 - (prov_frac-0.25)/0.5)            # decline when bloated
    short_term = 25 * (0.5*buffer_quality + 0.3*has_recent + 0.2*(1-buffer_pressure))

    # ---- DIMENSION 2: LONG-TERM MEMORY (consolidated store) --------------
    trusted_frac = len(trusted)/max(1, n)
    depth = clamp(len(trusted)/60.0)                   # ~60 trusted = mature store
    conf = mean([m.get("confidence",0) for m in trusted])
    fixed_bonus = clamp(len(fixed)/20.0)               # graduating to 'fixed' is the gold tier
    long_term = 25 * (0.4*trusted_frac + 0.3*depth + 0.2*conf + 0.1*fixed_bonus)

    # ---- DIMENSION 3: LEARNING (consolidation + skill formation) ---------
    survived = sum(1 for m in mems if m.get("survival",0) > 0)
    survival_rate = survived/max(1, n)                 # are memories surviving distillation?
    skill_score = clamp(skills_promoted/4.0)           # promoted procedures -> skills
    # distillation quality: best fact-coverage recorded by memory_distill_verified
    cov = 0.0
    covlog = MEM_DIR/".distill_coverage.json"
    if covlog.exists():
        try: cov = float(json.loads(covlog.read_text()).get("best_final_coverage",0))
        except Exception: pass
    learning = 25 * (0.4*survival_rate + 0.3*skill_score + 0.3*cov)

    # ---- DIMENSION 4: TOOLING / METACOGNITION ---------------------------
    tooling = 25 * (0.30*tools_present + 0.25*index_integrity + 0.15*link_integrity
                    + 0.20*secret_clean + 0.10*(1.0 if has_verified_distiller else 0.0))

    mq = round(short_term + long_term + learning + tooling, 1)
    dims = {"short_term": round(short_term,1), "long_term": round(long_term,1),
            "learning": round(learning,1), "tooling": round(tooling,1)}
    detail = {
        "memories": n, "provisional": len(prov), "corroborated": len(corr),
        "fixed": len(fixed), "suspect": len(suspect), "trusted_frac": round(trusted_frac,3),
        "mean_confidence_trusted": round(conf,3), "survival_rate": round(survival_rate,3),
        "skills_promoted": skills_promoted, "distill_best_coverage": cov,
        "index_integrity": round(index_integrity,3), "link_integrity": round(link_integrity,3),
        "secrets_in_memory_files": secret_hits,
    }
    return mq, dims, detail

def bar(score, width=20):
    fill = int(round(score/25*width))
    return "█"*fill + "·"*(width-fill)

def main():
    as_json = "--json" in sys.argv
    save = "--no-save" not in sys.argv
    mq, dims, detail = grade()
    prev = prev_run()
    delta = round(mq - prev["mq"], 1) if prev else None

    ts = datetime.now(timezone.utc).isoformat(timespec="seconds") if not os.environ.get("MEM_GRADE_TS") \
         else os.environ["MEM_GRADE_TS"]
    rec = {"ts": ts, "mq": mq, "dims": dims, "detail": detail}

    if as_json:
        rec["delta_vs_prev"] = delta
        print(json.dumps(rec, indent=2))
    else:
        human = {"short_term":"Short-term memory (working buffer)",
                 "long_term":"Long-term memory (consolidated)",
                 "learning":"Learning (consolidation + skills)",
                 "tooling":"Tooling (metacognition)"}
        print(f"\n  MEMORY QUOTIENT: {mq}/100", end="")
        if delta is not None:
            arrow = "▲" if delta>0 else ("▼" if delta<0 else "▬")
            print(f"   {arrow} {delta:+} vs last run ({prev['mq']})")
        else:
            print("   (baseline — first graded run)")
        print("  " + "─"*46)
        for k in ("short_term","long_term","learning","tooling"):
            print(f"  {human[k]:<40} {dims[k]:>5}/25  {bar(dims[k])}")
        print("  " + "─"*46)
        print(f"  store: {detail['memories']} memories  "
              f"({detail['provisional']}P / {detail['corroborated']}C / {detail['fixed']}F / {detail['suspect']}S)")
        print(f"  trusted={detail['trusted_frac']:.0%}  conf={detail['mean_confidence_trusted']:.2f}  "
              f"survival={detail['survival_rate']:.0%}  skills={detail['skills_promoted']}  "
              f"distill_cov={detail['distill_best_coverage']:.0%}")
        print(f"  index={detail['index_integrity']:.0%}  links={detail['link_integrity']:.0%}  "
              f"secrets_in_memory={detail['secrets_in_memory_files']}")
        # cheapest next point
        worst = min(dims, key=dims.get)
        print(f"\n  weakest dimension: {human[worst]} ({dims[worst]}/25) — best place to earn the next point.\n")

    if save:
        with HIST.open("a") as f:
            f.write(json.dumps(rec) + "\n")

if __name__ == "__main__":
    main()
