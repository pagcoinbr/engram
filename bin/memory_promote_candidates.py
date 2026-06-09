#!/usr/bin/env python3
"""
memory_promote_candidates.py — rank fixated memories for promotion into skills.

A memory earns *fixation* trust over time (see memory_score.py: suspect ->
provisional -> corroborated -> fixed). Some of those trusted, frequently-recalled
memories don't just record a fact — they encode a **repeatable operational
procedure backed by real tools/scripts** (lncli/bitcoin-cli runbooks, an
emergency-unlock script, a reconcile job, an offload-to-Ollama workflow). Those
are exactly the memories worth graduating from passive prose into a first-class,
invokable Claude Code *skill* (~/.claude/skills/<name>/SKILL.md + bundled scripts).

This script is the read-only RANKER behind the `/memory-to-skill` command. It does
not mutate anything. It:
  1. shells out to `memory_score.py --json` for status/frequency/confidence
     (same source of truth /memory-fixate consumes);
  2. reads each memory body from the canonical store and computes a
     `procedure_score` from concrete signals (code fences, .sh/.py paths, CLI
     tools, numbered steps, systemd units, Usage:);
  3. applies the eligibility gate agreed with the operator —
     status in {corroborated, fixed} AND frequency >= FREQ_MIN AND
     procedure_score > 0 — skipping memories already promoted;
  4. emits a ranked table (or --json) with a suggested kebab-case skill name.

Usage:
  memory_promote_candidates.py                 # human table of eligible candidates
  memory_promote_candidates.py --json          # machine-readable, ranked
  memory_promote_candidates.py --all           # include ineligible (show why)
  memory_promote_candidates.py --memory foo.md # evaluate a single memory
  memory_promote_candidates.py --freq-min 12   # override frequency floor
  memory_promote_candidates.py --proc-min 4    # override runbook-density floor
  memory_promote_candidates.py --top 10        # cap rows shown
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCORER = HERE / "memory_score.py"
SKILLS_DIR = HERE / "skills"

# Eligibility defaults (overridable via flags). Promotion requires earned trust.
FREQ_MIN_DEFAULT = 15
# A lone ".sh" mention scores 1.5; a real runbook (code fence + a tool, or several
# CLI tools / numbered steps) clears 3.0. The floor keeps narrative memories out.
PROC_MIN_DEFAULT = 3.0
ELIGIBLE_STATUS = {"corroborated", "fixed"}
PROMOTED_MARKER = "**Promoted to skill:**"

# Normalisation caps for the blended promote_score.
FREQ_CAP = 40.0           # freq beyond this is "very frequently used"
PROC_CAP = 12.0           # procedure_score beyond this is plenty script-heavy

# Frontmatter slug prefixes stripped when suggesting a skill name.
SLUG_PREFIXES = ("project_", "reference_", "feedback_", "user_")

# Distinct CLI tools / runbook verbs that signal a real procedure.
# (display label, detection regex) — explicit labels keep --json output clean.
CLI_TOOLS = [
    ("docker exec", r"docker\s+exec"),
    ("docker compose", r"docker\s+compose"),
    ("lncli", r"\blncli\b"),
    ("bitcoin-cli", r"\bbitcoin-cli\b"),
    ("elements-cli", r"\belements-cli\b"),
    ("systemctl", r"\bsystemctl\b"),
    ("journalctl", r"\bjournalctl\b"),
    ("psql", r"\bpsql\b"),
    ("curl", r"\bcurl\b"),
    ("ssh", r"\bssh\b"),
    ("scp", r"\bscp\b"),
    ("sudo", r"\bsudo\b"),
    ("gh api", r"\bgh\s+api\b"),
    ("awk", r"\bawk\b"),
    ("jq", r"\bjq\b"),
    ("cron", r"\bcron(?:tab)?\b"),
]


def run_scorer() -> dict:
    """Return the parsed `memory_score.py --json` payload."""
    out = subprocess.run(
        [sys.executable, str(SCORER), "--json"],
        capture_output=True, text=True, check=True,
    ).stdout
    return json.loads(out)


def suggest_skill_name(memory_name: str) -> str:
    """reference_lncli_server-b.md -> lncli-server-b."""
    stem = memory_name[:-3] if memory_name.endswith(".md") else memory_name
    for p in SLUG_PREFIXES:
        if stem.startswith(p):
            stem = stem[len(p):]
            break
    return stem.replace("_", "-")


def already_promoted(body: str, skill_name: str) -> bool:
    """A memory is promoted if it carries the back-pointer marker, or a skill
    directory of the suggested name already exists."""
    if PROMOTED_MARKER in body:
        return True
    return (SKILLS_DIR / skill_name / "SKILL.md").exists()


def procedure_signals(body: str) -> dict:
    """Concrete evidence that a memory describes a runnable procedure."""
    code_fences = body.count("```") // 2
    script_paths = len(re.findall(r"\b[\w./~-]+\.(?:sh|py)\b", body))
    cli_tools = sorted({
        label for label, pat in CLI_TOOLS if re.search(pat, body)
    })
    numbered_steps = len(re.findall(r"(?m)^\s*\d+[.)]\s+\S", body))
    service_units = len(re.findall(r"\b[\w@.-]+\.(?:service|timer)\b", body))
    has_usage = bool(re.search(r"(?im)^\s*#?\s*usage\s*:", body)) or "Usage:" in body
    return {
        "code_fences": code_fences,
        "script_paths": script_paths,
        "cli_tools": cli_tools,
        "numbered_steps": numbered_steps,
        "service_units": service_units,
        "has_usage": has_usage,
    }


def procedure_score(sig: dict) -> float:
    """Weighted blend. Code fences and script paths dominate; CLI tools and
    numbered steps confirm 'this is a runbook'. Capped per-signal so one giant
    code dump can't swamp the gate."""
    return round(
        min(sig["code_fences"], 4) * 2.0
        + min(sig["script_paths"], 4) * 1.5
        + min(len(sig["cli_tools"]), 4) * 1.5
        + min(sig["numbered_steps"], 5) * 1.0
        + min(sig["service_units"], 3) * 0.75
        + (2.0 if sig["has_usage"] else 0.0),
        3,
    )


def clamp(x: float, lo: float, hi: float) -> float:
    return max(lo, min(hi, x))


def promote_score(conf: float, freq: int, proc: float) -> float:
    """Rank eligible candidates: procedure density matters most (that's what
    makes a memory skill-worthy), then how frequently it's used, then trust."""
    proc_n = clamp(proc / PROC_CAP, 0, 1)
    freq_n = clamp(freq / FREQ_CAP, 0, 1)
    return round(0.45 * proc_n + 0.35 * freq_n + 0.20 * conf, 3)


def evaluate(mem: dict, store: Path, freq_min: int, proc_min: float) -> dict:
    name = mem["name"]
    path = store / name
    body = path.read_text(encoding="utf-8", errors="replace") if path.exists() else ""
    skill_name = suggest_skill_name(name)
    sig = procedure_signals(body)
    proc = procedure_score(sig)
    promoted = already_promoted(body, skill_name)

    reasons = []
    if mem["status"] not in ELIGIBLE_STATUS:
        reasons.append(f"status={mem['status']} (need corroborated/fixed)")
    if mem["frequency"] < freq_min:
        reasons.append(f"freq={mem['frequency']} < {freq_min}")
    if proc < proc_min:
        reasons.append(f"procedure_score={proc} < {proc_min} (too little runbook signal)")
    if promoted:
        reasons.append("already promoted")

    eligible = not reasons
    return {
        "name": name,
        "type": mem.get("type"),
        "status": mem["status"],
        "frequency": mem["frequency"],
        "confidence": mem["confidence"],
        "procedure_score": proc,
        "promote_score": promote_score(mem["confidence"], mem["frequency"], proc),
        "suggested_skill_name": skill_name,
        "already_promoted": promoted,
        "eligible": eligible,
        "ineligible_reasons": reasons,
        "signals": sig,
    }


def main() -> None:
    args = sys.argv[1:]
    as_json = "--json" in args
    show_all = "--all" in args

    freq_min = FREQ_MIN_DEFAULT
    if "--freq-min" in args:
        freq_min = int(args[args.index("--freq-min") + 1])

    proc_min = PROC_MIN_DEFAULT
    if "--proc-min" in args:
        proc_min = float(args[args.index("--proc-min") + 1])

    top = None
    if "--top" in args:
        top = int(args[args.index("--top") + 1])

    only = None
    if "--memory" in args:
        only = args[args.index("--memory") + 1]
        if not only.endswith(".md"):
            only += ".md"

    payload = run_scorer()
    store = Path(payload["store"])
    memories = payload["memories"]

    rows = []
    for mem in memories:
        if mem["name"] == "MEMORY.md":
            continue
        if only and mem["name"] != only:
            continue
        rows.append(evaluate(mem, store, freq_min, proc_min))

    if only and not rows:
        print(f"[promote] memory not found in scorer output: {only}", file=sys.stderr)
        sys.exit(1)

    shown = rows if (show_all or only) else [r for r in rows if r["eligible"]]
    shown.sort(key=lambda r: r["promote_score"], reverse=True)
    if top:
        shown = shown[:top]

    if as_json:
        print(json.dumps({
            "store": str(store),
            "freq_min": freq_min,
            "eligible_count": sum(1 for r in rows if r["eligible"]),
            "candidates": shown,
        }, indent=2))
        return

    n_elig = sum(1 for r in rows if r["eligible"])
    print(f"# Memory -> skill promotion candidates  (store: {store})")
    print(f"# gate: status in {{corroborated,fixed}} AND freq >= {freq_min} "
          f"AND procedure_score >= {proc_min}   |   {n_elig} eligible / {len(rows)} scored\n")
    header = f"{'pscore':>6} {'proc':>5} {'freq':>5} {'conf':>5} {'status':<12} {'eligible':<8}  {'suggested skill':<26} memory"
    print(header)
    print("-" * len(header))
    for r in shown:
        elig = "yes" if r["eligible"] else "no"
        print(f"{r['promote_score']:>6.3f} {r['procedure_score']:>5.1f} "
              f"{r['frequency']:>5} {r['confidence']:>5.2f} {r['status']:<12} "
              f"{elig:<8}  {r['suggested_skill_name']:<26} {r['name']}")
        if not r["eligible"] and (show_all or only):
            print(f"       └─ skip: {'; '.join(r['ineligible_reasons'])}")

    if not shown:
        print("(no eligible candidates — lower --freq-min or run --all to see why)")


if __name__ == "__main__":
    main()
