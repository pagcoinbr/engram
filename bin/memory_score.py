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
import json, math, os, re, subprocess, sys, glob, gzip
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
FREQ_CAP         = float(os.environ.get("MEM_FREQ_CAP", "40"))   # legacy floor for freq normalisation
SURV_CAP         = float(os.environ.get("MEM_SURV_CAP", "5"))
SUSPECT_AGE_DAYS = float(os.environ.get("MEM_SUSPECT_AGE_DAYS", "7"))
W_AGE  = float(os.environ.get("MEM_W_AGE",  "0.35"))
W_FREQ = float(os.environ.get("MEM_W_FREQ", "0.40"))
W_SURV = float(os.environ.get("MEM_W_SURV", "0.25"))
REVIEW_BASE_DAYS = 7

# Frequency discrimination (fixes the "every memory is mentioned everywhere"
# saturation): a token appearing in more than DISTINCT_DF_FRAC of human sessions
# is "generic" and excluded; a memory's frequency = popularity of its most-
# discussed *distinctive* token. freq is then self-normalised against a corpus
# percentile (FREQ_NORM_PCTL) so the score discriminates regardless of corpus size.
DISTINCT_DF_FRAC   = float(os.environ.get("MEM_DISTINCT_DF_FRAC", "0.10"))
KW_MAX             = int(os.environ.get("MEM_KW_MAX", "16"))
FREQ_NORM_PCTL     = float(os.environ.get("MEM_FREQ_NORM_PCTL", "90"))
# A memory only earns "corroborated" once it has matured: survived >=1
# distillation pass OR aged past CORROB_MIN_AGE_DAYS. This stops a brand-new
# (possibly self-asserting / injected) memory from being auto-trusted purely
# because its topic is frequently discussed.
CORROB_MIN_AGE_DAYS = float(os.environ.get("MEM_CORROB_MIN_AGE_DAYS", "7"))
# Optional: cap how many transcripts the frequency pass scans (0 = all). A
# stop-gap for speed until the incremental index lands; sampling still yields a
# representative DF distribution because DISTINCT_DF_FRAC is relative.
FREQ_SAMPLE        = int(os.environ.get("MEM_FREQ_SAMPLE", "0"))

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
    """Candidate match terms, ordered by likely distinctiveness. File-stem tokens
    (e.g. tron / lnbits / depix / rebalancer) are the strongest identifiers, so
    they come FIRST; name/description terms follow. We deliberately do NOT rank by
    length (that prefers generic long words like 'infrastructure' and drops
    distinctive short ones). Final per-token generic-ness is decided corpus-wide
    by document-frequency in build_frequency()."""
    ordered = []
    seen = set()
    def add(tok):
        tl = tok.lower()
        if len(tl) >= 4 and tl not in STOPWORDS and tl not in seen:
            seen.add(tl); ordered.append(tl)
    stem = re.sub(r"\.md$", "", fname)
    for t in re.split(r"[_\-]", stem):
        add(t)
    for field in (fm.get("name", ""), fm.get("description", "")):
        for w in re.findall(r"[A-Za-z][A-Za-z0-9_.-]{4,}", field):
            add(w)
    return set(ordered[:KW_MAX])

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

# ---- incremental transcript-token cache -----------------------------------
# JSON-parsing all ~150k transcripts every run is what made scoring take ~6 min
# and time the weekly hook out at 180 s. We cache, per transcript (keyed by
# mtime+size), the SET of candidate tokens in its human text. The cache is
# vocabulary-INDEPENDENT (it stores the transcript's own tokens, not the
# intersection with current memory keywords), so editing a memory's
# name/description does NOT invalidate it — document-frequency is recomputed by
# in-memory set intersection. Only new/changed transcripts are parsed.
FREQ_CACHE = MEM_DIR / ".freq_cache.json.gz"
CACHE_SCHEMA = 2
TOKEN_RE = re.compile(r"[a-z][a-z0-9_.-]{3,}")

def _candidate_tokens(text: str) -> list[str]:
    return sorted({w for w in TOKEN_RE.findall(text)
                   if len(w) >= 4 and w not in STOPWORDS})

def transcript_tokens(path: str) -> list[str]:
    text = human_text(path)
    return _candidate_tokens(text) if text else []

def _load_freq_cache() -> dict:
    try:
        with gzip.open(FREQ_CACHE, "rt", encoding="utf-8") as fh:
            c = json.load(fh)
        if c.get("schema") == CACHE_SCHEMA and isinstance(c.get("transcripts"), dict):
            return c
    except Exception:
        pass
    return {"schema": CACHE_SCHEMA, "transcripts": {}}

def _save_freq_cache(cache: dict) -> None:
    try:
        tmp = FREQ_CACHE.with_suffix(".gz.tmp")
        with gzip.open(tmp, "wt", encoding="utf-8") as fh:
            json.dump(cache, fh, separators=(",", ":"))
        tmp.replace(FREQ_CACHE)
    except Exception as e:
        print(f"[score] WARN: could not write freq cache: {e}", file=sys.stderr)

def build_frequency(mem_keywords: dict[str, set[str]]) -> tuple[dict[str, int], int]:
    """Per-memory conversation frequency that discriminates, computed incrementally.

    Each candidate token's session document-frequency is the number of distinct
    HUMAN-typed sessions containing it (injected context excluded — see
    human_text). A token in more than DISTINCT_DF_FRAC of sessions is "generic"
    (e.g. 'service', 'gateway') and dropped. A memory's frequency = the
    document-frequency of its most-discussed *distinctive* token (0 if it has
    none) — replacing the old "any keyword matches → +1" union, which saturated.

    Transcript tokens are read from / written to a per-transcript cache so only
    new or changed transcripts are JSON-parsed. Returns (freq_by_name, n_sessions)."""
    all_kw = set()
    for kws in mem_keywords.values():
        all_kw |= kws

    cache = _load_freq_cache()
    tdict = cache["transcripts"]
    trs = sorted(glob.glob(str(PROJ_DIR / "*.jsonl")))
    if FREQ_SAMPLE and len(trs) > FREQ_SAMPLE:
        step = len(trs) / FREQ_SAMPLE
        trs = [trs[int(i * step)] for i in range(FREQ_SAMPLE)]

    df = {kw: 0 for kw in all_kw}
    n_sessions = 0
    live = set()
    dirty = False
    parsed = 0
    for tr in trs:
        name = os.path.basename(tr)
        live.add(name)
        try:
            sttr = os.stat(tr)
        except OSError:
            continue
        ent = tdict.get(name)
        if ent and ent.get("m") == int(sttr.st_mtime) and ent.get("s") == sttr.st_size:
            toks = ent["t"]
        else:
            toks = transcript_tokens(tr)
            tdict[name] = {"m": int(sttr.st_mtime), "s": sttr.st_size, "t": toks}
            dirty = True
            parsed += 1
            # Periodically flush so a cold build that gets killed (e.g. the 180s
            # weekly-hook cap) still persists progress and self-heals next run
            # instead of looping forever on a never-written cache.
            if parsed % 20000 == 0:
                _save_freq_cache(cache)
        if not toks:
            continue
        n_sessions += 1
        for kw in (set(toks) & all_kw):   # C-level set intersection (fast)
            df[kw] += 1

    # Archived corpus: cache entries whose raw .jsonl is no longer on disk (moved
    # or compressed by transcript retention) STILL count — the cache is the
    # authoritative corpus for frequency; the raw file is only needed to (re)derive
    # tokens. So retention can shrink disk without distorting the frequency signal.
    # We therefore do NOT prune here; intentional cleanup is via `--vacuum`.
    if not FREQ_SAMPLE:
        for name, ent in tdict.items():
            if name in live:
                continue
            toks = ent.get("t")
            if toks:
                n_sessions += 1
                for kw in (set(toks) & all_kw):
                    df[kw] += 1
    if dirty:
        _save_freq_cache(cache)
    print(f"[score] freq: {len(trs)} transcripts ({parsed} parsed, "
          f"{len(trs) - parsed} from cache), {n_sessions} with human text",
          file=sys.stderr)

    ceil_df = max(1, int(DISTINCT_DF_FRAC * n_sessions))
    freq = {}
    for name, kws in mem_keywords.items():
        distinctive = [df[kw] for kw in kws if df.get(kw, 0) <= ceil_df]
        freq[name] = max(distinctive) if distinctive else 0
    return freq, n_sessions

# ---- scoring --------------------------------------------------------------
def clamp(x, lo, hi):
    return max(lo, min(hi, x))

def score_memory(age_days, freq, survival, suspicion, freq_norm):
    age_s  = clamp(math.log1p(max(age_days, 0)) / math.log1p(AGE_FULL_DAYS), 0, 1)
    # log-scaled and self-normalised against a corpus percentile (freq_norm), so
    # freq spreads across [0,1] instead of saturating to 1.0 for everything.
    freq_s = clamp(math.log1p(max(freq, 0)) / math.log1p(max(freq_norm, 1.0)), 0, 1)
    surv_s = clamp(survival / SURV_CAP, 0, 1)
    conf = W_AGE * age_s + W_FREQ * freq_s + W_SURV * surv_s
    # "earns trust over time": a memory must have matured before it can be
    # corroborated — survived a distillation pass, or aged past the threshold.
    matured = survival >= 1 or age_days >= CORROB_MIN_AGE_DAYS
    if suspicion:
        status = "suspect"
    elif conf >= 0.66 and survival >= 3:
        status = "fixed"
    elif conf >= 0.33 and matured:
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

    if "--vacuum" in args:
        # Drop freq-cache entries whose transcript is GENUINELY gone — not present
        # as a raw .jsonl, a compressed .jsonl.gz, or under an archive/ subdir.
        # (Archived transcripts are kept so they keep contributing to frequency.)
        cache = _load_freq_cache()
        tdict = cache.get("transcripts", {})
        archive = PROJ_DIR / "archive"
        removed = 0
        for name in list(tdict):
            if (PROJ_DIR / name).exists() or (PROJ_DIR / (name + ".gz")).exists() \
               or (archive / name).exists() or (archive / (name + ".gz")).exists():
                continue
            del tdict[name]; removed += 1
        _save_freq_cache(cache)
        print(f"[score] vacuum: removed {removed} dead cache entries, "
              f"{len(tdict)} remain")
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

    freq, n_sessions = build_frequency(mem_kw)
    # self-normalising scale: the FREQ_NORM_PCTL-th percentile of non-zero
    # frequencies (floored at the legacy FREQ_CAP) is treated as "fully frequent".
    nz = sorted(v for v in freq.values() if v > 0)
    if nz:
        idx = min(len(nz) - 1, int(round((FREQ_NORM_PCTL / 100.0) * (len(nz) - 1))))
        freq_norm = max(float(nz[idx]), FREQ_CAP)
    else:
        freq_norm = FREQ_CAP

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
        # Exempt freshly AUTO-GRADUATED memories: they already cleared the graduation
        # injection denylist + LLM verdict, so a legit "Persistent=true" (systemd) or
        # "persistent volume" mention must not trip a re-quarantine → re-harvest loop.
        auto_graduated = "auto-graduated" in body.lower()
        suspicion = (persist and not auto_graduated
                     and survival == 0 and f <= 1 and age_days <= SUSPECT_AGE_DAYS)
        conf, status, review, parts = score_memory(age_days, f, survival, suspicion, freq_norm)
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
