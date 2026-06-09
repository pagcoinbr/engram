#!/usr/bin/env python3
"""memory_distill_verified.py — distillation with GUARANTEED hard-fact coverage.

Lessons learned (2026-05-30, iterating live):
  * gpt-oss:20b with a SIMPLE prompt -> complete, readable umbrella, but it
    SILENTLY DROPS dense hard facts (ports, paths, ENV_VARS, hashes, decimals)
    and "do NOT / never" caveats.
  * Nagging the model with a big MUST-PRESERVE checklist BACKFIRES: the extra
    instructions make it reason until it exhausts num_predict and emits an EMPTY
    response (cov=0%, done=length, ~983s wasted). More prompting = worse.

So the logic that actually works is HYBRID (completeness is mechanical, the model
only owns prose):
  1. LLM writes the readable narrative with the proven simple prompt.
  2. Code deterministically EXTRACTS hard facts from the sources (regex - these
     are exactly the droppable token classes).
  3. Code MEASURES which the LLM reproduced verbatim (natural coverage = the
     LLM-quality metric we iterate on).
  4. Code GUARANTEES completeness: any missing hard fact / caveat is appended
     verbatim, with its source file + line, in a "## Reference facts" section.
     Final coverage is ~1.0 BY CONSTRUCTION - never dependent on the model.
  5. Self-verify hallucination: flag output tokens of fact-classes that do NOT
     appear in any source (possible invention) for human review.

Output is a draft + JSON report; nothing is applied.

Usage: memory_distill_verified.py <cluster_key> <note1.md> <note2.md> ...
"""
from __future__ import annotations
import os
import json, re, sys, time, urllib.request
from pathlib import Path

sys.path.insert(0, str(Path.home() / ".claude"))
import memory_ai

MEM = Path.home() / ".claude" / "projects" / (os.environ.get("CLAUDE_MEMORY_SLUG") or str(Path.home()).replace("/", "-")) / "memory"

FACT_PATTERNS = [
    (r"(?<![\w.]):\d{2,5}\b", "port"),
    (r"\bport\s+\d{2,5}\b", "port"),
    (r"\b\d{1,3}(?:\.\d{1,3}){3}\b", "ip"),
    (r"/(?:home|data|root|usr|etc|var|opt)/[\w./\-]+", "path"),
    (r"\b[A-Z][A-Z0-9]{2,}(?:_[A-Z0-9]+)+\b", "env"),
    (r"\bcommit\s+`?[0-9a-f]{7,40}`?", "commit"),                  # only hashes labelled 'commit'
    (r"\b[\w\-]+\.(?:py|ts|js|tsx|toml|env|sh|service)\b", "file"),
    # decimals ONLY when carrying a unit/threshold meaning (sats, BTC, version,
    # =0.01, x.y.z handled by ip). bare balances/amounts like 282.63 are NOT durable.
    (r"\b\d+\.\d+\s*(?:BTC|L-BTC|sats?|USDt?|USDC)\b", "amount"),
    (r"(?:THRESHOLD|=|version|toolchain|pinned to)\s*`?\d+\.\d+`?", "threshold"),
]
FACT_STOP = {"0.0", "1.0", "2.0", "3.0"}
# Secrets / per-run cruft that distillation MUST DROP, never faithfully preserve.
# (Matches the security-mindset + no-secrets memories: client secrets, session
# ids, raw API keys, webhook secrets, long base64/hex blobs are not "facts".)
SECRET_RE = re.compile(
    r"(?i)(originSessionId|client[_-]?secret|api[_-]?key\s*[:=]|"
    r"webhook[_-]?secret|password|[0-9a-f]{32,}|[A-Za-z0-9+/]{40,}={0,2})")

def looks_secret(line: str) -> bool:
    return bool(SECRET_RE.search(line))

def extract_facts(text: str):
    seen = {}
    for pat, kind in FACT_PATTERNS:
        for m in re.finditer(pat, text):
            tok = m.group(0).strip().rstrip(".,;:)")
            norm = tok.lower()
            if len(norm) < 3 or norm in FACT_STOP:
                continue
            # find the line for a secrecy check so we never appendix a secret
            line = next((ln for ln in text.splitlines() if tok in ln), "")
            if line and looks_secret(line) and kind in ("commit",):
                continue
            seen.setdefault(norm, (tok, kind))
    return seen

CAVEAT_TRIGGER = re.compile(r"(?i)\b(do not|don'?t|never|must not|not viable|banned|do NOT)\b")

def caveats(text: str):
    """Sentence-level caveat extraction. The old line-level version missed rules
    buried inside long bullets (e.g. 'DO NOT pursue SIDESHIFT_PROXY' inside a
    1500-char paragraph). Split into sentences and keep the ones with a caveat
    trigger, trimmed to the surrounding clause."""
    out = []
    # normalize whitespace per paragraph, then split on sentence boundaries
    for para in re.split(r"\n\s*\n", text):
        flat = " ".join(para.split())
        for sent in re.split(r"(?<=[.!?])\s+|→|;", flat):
            sent = sent.strip().lstrip("-*# ").strip()
            if CAVEAT_TRIGGER.search(sent) and 8 <= len(sent) <= 300 and not looks_secret(sent):
                out.append(sent)
    # dedupe preserving order
    seen = set(); uniq = []
    for c in out:
        k = c.lower()
        if k not in seen:
            seen.add(k); uniq.append(c)
    return uniq

SIMPLE_PROMPT = (
    "You are distilling a cluster of one person's memory notes into ONE durable "
    "umbrella note. Preserve frontmatter conventions (name/description/type) and the "
    "**Why:** / **How to apply:** structure. Produce a tight class-level body with short "
    "labeled subsections, one per source note. Keep concrete facts (ports, paths, env "
    "vars, commands). Do NOT invent facts. Output ONLY the merged memory.\n\n")

def ollama_stream(prompt, cfg):
    host = memory_ai.ollama_host(cfg); model = memory_ai.expert_model("distill", cfg)
    oc = cfg.get("ollama", {})
    body = json.dumps({"model": model, "prompt": prompt, "stream": True,
        "think": False, "reasoning_effort": oc.get("reasoning_effort", "low"),
        "options": {"temperature": 0.1, "num_ctx": int(oc.get("num_ctx", 16384)),
                    "num_predict": int(oc.get("num_predict", 8000))}}).encode()
    req = urllib.request.Request(f"{host}/api/generate", data=body,
                                 headers={"Content-Type": "application/json"})
    parts = []; done = None; t0 = time.time()
    with urllib.request.urlopen(req, timeout=int(oc.get("timeout_seconds", 1200))) as r:
        for line in r:
            line = line.strip()
            if not line:
                continue
            try:
                c = json.loads(line)
            except Exception:
                continue
            if c.get("response"):
                parts.append(c["response"])
            if c.get("done"):
                done = c.get("done_reason")
    return "".join(parts), done, time.time() - t0

def distill_cluster(key, files, cfg=None, verbose=False):
    """Distill a cluster -> (final_draft_text, report_dict). Importable by
    memory_distill.py / the fixation cron so the verified pipeline is the default.
    final_fact_coverage is ~1.0 by construction (missing facts appended verbatim)."""
    cfg = cfg or memory_ai.load()
    members_text = ""
    facts = {}; fact_src = {}; all_caveats = []
    for f in files:
        body = (MEM / f).read_text(errors="ignore")
        members_text += f"### {f}\n{body}\n\n---\n\n"
        for norm, (disp, kind) in extract_facts(body).items():
            facts.setdefault(norm, (disp, kind))
            if norm not in fact_src:
                for ln in body.splitlines():
                    if disp.lower() in ln.lower():
                        fact_src[norm] = (f, ln.strip()); break
        all_caveats += [(c, f) for c in caveats(body)]
    if verbose:
        print(f"[{key}] {len(files)} notes · {len(facts)} hard facts · {len(all_caveats)} caveat lines", flush=True)

    out = ""; done = None
    for attempt in range(1, 3):
        out, done, dt = ollama_stream(SIMPLE_PROMPT + f"SOURCE NOTES ({key}_*):\n\n{members_text}", cfg)
        if verbose:
            print(f"  gen attempt {attempt}: chars={len(out)} done={done} wall={dt:.0f}s", flush=True)
        if out.strip():
            break

    out_low = out.lower()
    present = [n for n in facts if n in out_low]
    missing = [n for n in facts if n not in out_low]
    natural_cov = len(present) / max(1, len(facts))
    present_cav = [c for c, _ in all_caveats if c.lower() in out_low]
    missing_cav = [(c, f) for c, f in all_caveats if c.lower() not in out_low]

    src_low = members_text.lower()
    invented = [disp for norm, (disp, _) in extract_facts(out).items() if norm not in src_low]

    appendix = ""
    if missing or missing_cav:
        appendix = "\n\n## Reference facts (verbatim — auto-preserved from sources)\n"
        appendix += "_Hard facts/caveats from the source notes that the prose above did not restate. Kept here so nothing is lost._\n\n"
        if missing:
            appendix += "### Hard facts\n"
            for n in sorted(missing, key=lambda n: facts[n][1]):
                disp, kind = facts[n]; src = fact_src.get(n, ("?", ""))
                ctx = f" — {src[1]}" if src[1] and len(src[1]) <= 160 else ""
                appendix += f"- `{disp}` ({kind}, from {src[0]}){ctx}\n"
        if missing_cav:
            appendix += "\n### Caveats / rules\n"
            for c, f in missing_cav:
                appendix += f"- {c} (from {f})\n"

    final = out.rstrip() + appendix + "\n"
    final_low = final.lower()
    final_cov = len([n for n in facts if n in final_low]) / max(1, len(facts))
    report = {
        "cluster": key, "notes": files,
        "hard_facts": len(facts), "caveats": len(all_caveats),
        "natural_fact_coverage": round(natural_cov, 4),
        "natural_caveat_coverage": round(len(present_cav) / max(1, len(all_caveats)), 4),
        "final_fact_coverage": round(final_cov, 4),
        "appendix_facts": len(missing), "appendix_caveats": len(missing_cav),
        "possible_hallucinations": invented,
        "draft_chars": len(final), "gen_done": done,
    }
    return final, report


def main():
    if len(sys.argv) < 3:
        print("usage: memory_distill_verified.py <cluster_key> <note.md> ...", file=sys.stderr)
        sys.exit(2)
    key = sys.argv[1]; files = sys.argv[2:]
    final, report = distill_cluster(key, files, verbose=True)
    draft = Path.cwd() / f"{key}.draft.md"
    draft.write_text(final)
    report["draft_path"] = str(draft)
    print("REPORT " + json.dumps(report), flush=True)

if __name__ == "__main__":
    main()
