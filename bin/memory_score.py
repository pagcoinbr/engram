#!/usr/bin/env python3
"""memory_score.py — deterministic scoring engine for memory "fixation".

For every memory file it computes the signals that decide how much to trust /
how often to review a memory, then derives a fixation status. The LLM-side
distillation is done elsewhere (the /memory-fixate command, via the Ollama
MCP); this script is the *measurement* layer and is pure except for a small
state cache.

Signals (the variables the operator specified):
  - age_days       older memories are likelier immutable/important
  - frequency      # of distinct transcript sessions that mention the memory's
                   key terms — frequent context => likelier true/important
  - survival_count # of distillation passes the memory has survived
  - suspicion      recent + self-asserts persistence + uncorroborated  =>
                   possible injection / "memory virus" => needs a human

Derived:
  - confidence       weighted blend of age/frequency/survival (0..1)
  - fixation_status  suspect | provisional | corroborated | fixed
  - review_interval  days until next review (grows as a memory stabilises)

Usage:
  memory_score.py                      # human table
  memory_score.py --json               # machine-readable
  memory_score.py --commit-survivors a.md,b.md   # +1 survival, stamp fixated
Env tunables: CLAUDE_MEMORY_SLUG, MEM_AGE_FULL_DAYS, MEM_FREQ_CAP,
              MEM_SURV_CAP, MEM_SUSPECT_AGE_DAYS, MEM_W_AGE/W_FREQ/W_SURV.
"""
from __future__ import annotations
import json, math, os, re, subprocess, sys, glob
from datetime import datetime, timezone
from pathlib import Path

HOME = Path.home()

def slug() -> str:
    s = os.environ.get("CLAUDE_MEMORY_SLUG")
    return s if s else str(HOME).replace("/", "-")

USERNAME   = HOME.name
SLUG       = slug()
PROJ_DIR   = HOME / ".claude" / "projects" / SLUG
MEM_DIR    = PROJ_DIR / "memory"
STATE_FILE = MEM_DIR / ".fixation_state.json"   # per-store (not global)
REPO_CACHE = HOME / ".claude" / ".memory_repo_cache"
REMOTE_DIR = f"{USERNAME}/projects/{SLUG}/memory"

# ---- tunables -------------------------------------------------------------
AGE_FULL_DAYS    = float(os.environ.get("MEM_AGE_FULL_DAYS", "365"))
FREQ_CAP         = float(os.environ.get("MEM_FREQ_CAP", "40"))
SURV_CAP         = float(os.environ.get("MEM_SURV_CAP", "5"))
SUSPECT_AGE_DAYS = float(os.environ.get("MEM_SUSPECT_AGE_DAYS", "7"))
W_AGE  = float(os.environ.get("MEM_W_AGE",  "0.35"))
W_FREQ = float(os.environ.get("MEM_W_FREQ", "0.40"))
W_SURV = float(os.environ.get("MEM_W_SURV", "0.25"))
REVIEW_BASE_DAYS = 7

# Self-persistence / injection patterns. A *recent, uncorroborated* memory that
# matches these is treated as a possible "memory virus" and routed to a human.
PERSIST_PATTERNS = [
    r"\balways (remember|run|use|do|apply|execute)\b",
    r"\bnever (forget|delete|remove|change)\b",
    r"\bremember (this|that|to|always)\b",
    r"\bdo not (delete|remove|forget|change) (this|the)\b",
    r"\bpersist(ent|ence)?\b",
    r"\bimportant\b[^.]{0,40}\bremember\b",
    r"\b(ignore|disregard|override) (previous|prior|above|all)\b",
    r"\byou (must|must always|should always|are required to)\b",
    r"\bsystem prompt\b",
    r"\bfrom now on\b",
]
PERSIST_RE = re.compile("|".join(PERSIST_PATTERNS), re.IGNORECASE)

STOPWORDS = set("""the a an and or of to in on for with from this that into via per
project reference feedback memory notes setup config server fixed using used new old
overhaul status known issues deployment""".split())

def now() -> datetime:
    return datetime.now(timezone.utc)

def iso(dt: datetime) -> str:
    return dt.astimezone(timezone.utc).isoformat()

# ---- state ----------------------------------------------------------------
def load_state() -> dict:
    if STATE_FILE.exists():
        try:
            return json.loads(STATE_FILE.read_text())
        except Exception:
            pass
    return {"schema": 1, "memories": {}}

def save_state(st: dict) -> None:
    tmp = STATE_FILE.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(st, indent=1, sort_keys=True))
    tmp.replace(STATE_FILE)

# ---- frontmatter ----------------------------------------------------------
def frontmatter(text: str) -> dict:
    """Return top-level + metadata.* frontmatter keys (both conventions)."""
    fm = {}
    if not text.startswith("---"):
        return fm
    end = text.find("\n---", 3)
    if end == -1:
        return fm
    for line in text[3:end].splitlines():
        m = re.match(r"^(\w[\w-]*):\s*(.*)$", line)
        if m:
            fm[m.group(1)] = m.group(2).strip().strip('"')
        else:
            m2 = re.match(r"^\s+(\w[\w-]*):\s*(.*)$", line)  # nested (metadata:)
            if m2 and m2.group(1) not in fm:
                fm[m2.group(1)] = m2.group(2).strip().strip('"')
    return fm

# ---- creation date (layered, frozen in state) -----------------------------
def git_add_date(fname: str) -> str | None:
    if not (REPO_CACHE / ".git").exists():
        return None
    try:
        out = subprocess.run(
            ["git", "-C", str(REPO_CACHE), "log", "--diff-filter=A",
             "--format=%aI", "--", f"{REMOTE_DIR}/{fname}"],
            capture_output=True, text=True, timeout=20).stdout.strip().splitlines()
        return out[-1] if out else None
    except Exception:
        return None

def session_date(origin: str) -> str | None:
    if not origin:
        return None
    tr = PROJ_DIR / f"{origin}.jsonl"
    if not tr.exists():
        return None
    try:
        with tr.open() as fh:
            for line in fh:
                try:
                    ev = json.loads(line)
                    ts = ev.get("timestamp")
                    if ts:
                        return ts
                except Exception:
                    continue
    except Exception:
        pass
    return iso(datetime.fromtimestamp(tr.stat().st_mtime, timezone.utc))

def resolve_created(fname: str, fm: dict, path: Path, st_mem: dict) -> str:
    if st_mem.get("created_at"):
        return st_mem["created_at"]
    for cand in (fm.get("created") or fm.get("date"),
                 git_add_date(fname),
                 session_date(fm.get("originSessionId", "")),
                 iso(datetime.fromtimestamp(path.stat().st_mtime, timezone.utc))):
        if cand:
            return cand
    return iso(now())

# ---- keyword extraction + frequency ---------------------------------------
def keywords(fname: str, fm: dict) -> set[str]:
    toks = set()
    stem = re.sub(r"\.md$", "", fname)
    for t in re.split(r"[_\-]", stem):
        if len(t) >= 4 and t.lower() not in STOPWORDS:
            toks.add(t.lower())
    for field in (fm.get("name", ""), fm.get("description", "")):
        for w in re.findall(r"[A-Za-z][A-Za-z0-9_.-]{4,}", field):
            wl = w.lower()
            if wl not in STOPWORDS:
                toks.add(wl)
    # keep the most distinctive (longest) handful to limit false matches
    return set(sorted(toks, key=len, reverse=True)[:8])

SYSREMINDER_RE = re.compile(r"<system-reminder>.*?</system-reminder>", re.DOTALL)

def human_text(path: str) -> str:
    """Genuine human-typed text from a transcript: user messages only, with
    injected <system-reminder> blocks (which carry the MEMORY.md index and
    recalled memories) and tool_result blocks removed. Without this filter the
    injected memory index makes every memory look mentioned in every session."""
    parts = []
    try:
        with open(path, "r", errors="ignore") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    ev = json.loads(line)
                except Exception:
                    continue
                if ev.get("type") != "user":
                    continue
                content = (ev.get("message") or {}).get("content")
                if isinstance(content, str):
                    parts.append(content)
                elif isinstance(content, list):
                    for b in content:
                        if isinstance(b, dict) and b.get("type") == "text":
                            parts.append(b.get("text", ""))
                        # tool_result blocks are skipped (not human-authored)
    except Exception:
        return ""
    text = "\n".join(parts)
    text = SYSREMINDER_RE.sub("", text)
    return text.lower()

def build_frequency(mem_keywords: dict[str, set[str]]) -> dict[str, int]:
    """Count distinct sessions whose HUMAN-typed text mentions a memory's
    keywords (injected context excluded — see human_text)."""
    all_kw = set()
    for kws in mem_keywords.values():
        all_kw |= kws
    freq = {name: 0 for name in mem_keywords}
    for tr in glob.glob(str(PROJ_DIR / "*.jsonl")):
        text = human_text(tr)
        if not text:
            continue
        present = {kw for kw in all_kw if kw in text}
        if not present:
            continue
        for name, kws in mem_keywords.items():
            if kws & present:
                freq[name] += 1
    return freq

# ---- scoring --------------------------------------------------------------
def clamp(x, lo, hi):
    return max(lo, min(hi, x))

def score_memory(age_days, freq, survival, suspicion):
    age_s  = clamp(math.log1p(max(age_days, 0)) / math.log1p(AGE_FULL_DAYS), 0, 1)
    freq_s = clamp(freq / FREQ_CAP, 0, 1)
    surv_s = clamp(survival / SURV_CAP, 0, 1)
    conf = W_AGE * age_s + W_FREQ * freq_s + W_SURV * surv_s
    if suspicion:
        status = "suspect"
    elif conf >= 0.66 and survival >= 3:
        status = "fixed"
    elif conf >= 0.33:
        status = "corroborated"
    else:
        status = "provisional"
    review = REVIEW_BASE_DAYS * (1 + survival) * (0.5 + conf)
    review = int(clamp(round(review), 1, 180))
    return round(conf, 3), status, review, {
        "age": round(age_s, 3), "freq": round(freq_s, 3), "surv": round(surv_s, 3)}

def main():
    args = sys.argv[1:]
    if "--commit-survivors" in args:
        names = args[args.index("--commit-survivors") + 1].split(",")
        st = load_state()
        mems = st.setdefault("memories", {})
        for n in [x.strip() for x in names if x.strip()]:
            m = mems.setdefault(n, {})
            m["survival_count"] = int(m.get("survival_count", 0)) + 1
            m["last_fixated_at"] = iso(now())
        save_state(st)
        print(f"[score] recorded survival for {len([x for x in names if x.strip()])} memories")
        return

    as_json = "--json" in args
    if not MEM_DIR.is_dir():
        print(f"[score] no memory dir at {MEM_DIR}", file=sys.stderr); sys.exit(1)

    st = load_state()
    mems = st.setdefault("memories", {})
    files = [p for p in MEM_DIR.glob("*.md") if p.name != "MEMORY.md"]

    mem_fm, mem_text, mem_kw = {}, {}, {}
    for p in files:
        txt = p.read_text(errors="ignore")
        mem_text[p.name] = txt
        mem_fm[p.name] = frontmatter(txt)
        mem_kw[p.name] = keywords(p.name, mem_fm[p.name])

    freq = build_frequency(mem_kw)

    rows = []
    for p in files:
        name = p.name
        fm = mem_fm[name]
        st_mem = mems.setdefault(name, {})
        created = resolve_created(name, fm, p, st_mem)
        st_mem["created_at"] = created            # freeze
        st_mem.setdefault("first_seen", iso(now()))
        survival = int(st_mem.get("survival_count", 0))
        try:
            age_days = (now() - datetime.fromisoformat(created)).total_seconds() / 86400
        except Exception:
            age_days = 0.0
        f = freq.get(name, 0)
        body = mem_text[name]
        persist = bool(PERSIST_RE.search(body))
        suspicion = persist and survival == 0 and f <= 1 and age_days <= SUSPECT_AGE_DAYS
        conf, status, review, parts = score_memory(age_days, f, survival, suspicion)
        st_mem["last_status"] = status
        st_mem["last_score"] = conf
        st_mem["last_scored"] = iso(now())
        rows.append({
            "name": name,
            "type": fm.get("type", "?"),
            "age_days": round(age_days, 1),
            "frequency": f,
            "survival": survival,
            "suspicion": suspicion,
            "self_persist": persist,
            "confidence": conf,
            "status": status,
            "review_interval_days": review,
            "score_parts": parts,
        })
    save_state(st)

    # suspects first, then weakest confidence first (most in need of review)
    rows.sort(key=lambda r: (not r["suspicion"], r["confidence"]))

    if as_json:
        print(json.dumps({"store": str(MEM_DIR), "count": len(rows),
                          "generated": iso(now()), "memories": rows}, indent=1))
        return

    print(f"# Memory fixation scores — {MEM_DIR}  ({len(rows)} memories)")
    print(f"{'status':<13}{'conf':>5} {'age_d':>6} {'freq':>5} {'surv':>5} {'rev_d':>5}  name")
    for r in rows:
        flag = "⚠" if r["suspicion"] else " "
        print(f"{r['status']:<13}{r['confidence']:>5.2f} {r['age_days']:>6.0f} "
              f"{r['frequency']:>5} {r['survival']:>5} {r['review_interval_days']:>5}{flag} {r['name']}")
    from collections import Counter
    c = Counter(r["status"] for r in rows)
    print("\nby status:", dict(c))

if __name__ == "__main__":
    main()
