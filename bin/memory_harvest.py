#!/usr/bin/env python3
"""memory_harvest.py — local-Ollama harvester that turns conversation history
into *staged* memory candidates, with STRUCTURAL provenance.

This is stage ① of the unattended memory pipeline. It replaces the Anthropic
`claude -p` extraction in memory_agent.sh with a fully-local pass (offload pref):
it reads Claude Code session transcripts (~/.claude/projects/<slug>/*.jsonl),
extracts durable candidate facts via the LOCAL Ollama models (memory_ai), and
writes them as QUARANTINED candidates into memory/.staging/ — never directly
into recall. The downstream cron (score/quarantine -> auto-graduate) decides
what graduates.

WHY PROVENANCE IS COMPUTED FROM STRUCTURE, NOT THE LLM:
In a Claude Code transcript a *tool result* is delivered as a `user`-role
message carrying a `tool_result` content block. So "role == user" does NOT mean
"the human said it" — most user-role turns are tool/web/file output. An injected
instruction hiding in a fetched web page or a file we cat'd would therefore look
like user input. We must NOT let a model self-report where a fact came from (an
injected fact would simply lie). Instead we segment the transcript by content-
block type deterministically and only feed the model:
    U#  — genuine user-authored text   (provenance: user-direct)
    A#  — assistant's own prose         (provenance: assistant — may restate tools)
`tool_result` and `thinking` blocks are EXCLUDED from harvesting entirely — that
is the safest default and severs the injection->memory->skill path at the source.
A candidate is `user-direct` (the only skill-eligible class) iff every segment it
cites is a U# segment.

CLI:
  memory_harvest.py                    # harvest new turns in all transcripts since watermark
  memory_harvest.py --transcript F     # only this transcript (full, ignoring watermark)
  memory_harvest.py --role triage      # use a faster expert for extraction (default: distill)
  memory_harvest.py --since-all        # ignore the watermark; reprocess everything (backfill)
  memory_harvest.py --dry-run          # extract + classify but DON'T write staging files
  memory_harvest.py --max-chars N      # cap transcript text sent per model call (default 24000)
"""
from __future__ import annotations

import json
import os
import re
import sys
import datetime
from pathlib import Path

sys.path.insert(0, str(Path.home() / ".claude"))
import memory_ai  # shared Ollama routing + config (host, experts, timeouts)

HOME = Path.home()


def slug() -> str:
    return os.environ.get("CLAUDE_MEMORY_SLUG") or str(HOME).replace("/", "-")


PROJ_DIR = HOME / ".claude" / "projects" / slug()
MEM_DIR = PROJ_DIR / "memory"
STAGING = MEM_DIR / ".staging"
STATE_PATH = HOME / ".claude" / "logs" / "harvest" / "state.json"

VALID_TYPES = {"user", "feedback", "project", "reference"}

# ----------------------------------------------------------------------------
# Transcript parsing — deterministic segmentation by content-block type.
# ----------------------------------------------------------------------------

# Injected blocks that ride inside a user-role message but are NOT the human's
# words: <system-reminder> carries the MEMORY.md index + recalled memories, and
# <relevant-memory> is the auto-recall payload. If we don't strip these, a
# poisoned/recalled memory re-surfaces here, gets cited as a U# segment, and
# graduates as `user-direct` — a self-reinforcing injection loop. Mirrors the
# SYSREMINDER_RE strip in memory_score.human_text().
INJECTED_BLOCK_RE = re.compile(
    r"<(system-reminder|relevant-memory)\b[^>]*>.*?</\1\s*>", re.DOTALL | re.IGNORECASE)
# Any surviving injected tag FRAGMENT (opener or closer, with/without attributes)
# after stripping means the text was crafted to break the block match (e.g. an
# extra </system-reminder> to split a real block). Treat the segment as contaminated.
INJECTED_TAG_RESIDUE = re.compile(
    r"</?(system-reminder|relevant-memory)\b", re.IGNORECASE)


def _strip_injected(text: str) -> str:
    """Remove injected system-reminder / relevant-memory blocks from user text.
    Returns "" (segment dropped) if any injected tag fragment remains after the
    strip — fail-closed against tag-splitting bypasses rather than risk leaking
    injected text as user-direct."""
    cleaned = INJECTED_BLOCK_RE.sub("", text or "")
    if INJECTED_TAG_RESIDUE.search(cleaned):
        return ""
    return cleaned.strip()


def _blocks(content):
    """Normalise a message `content` into a list of (block_type, text) pairs."""
    out = []
    if isinstance(content, str):
        out.append(("text", content))
    elif isinstance(content, list):
        for b in content:
            if not isinstance(b, dict):
                continue
            bt = b.get("type")
            if bt == "text":
                out.append(("text", b.get("text", "")))
            elif bt == "tool_result":
                out.append(("tool_result", ""))      # text intentionally dropped
            elif bt == "thinking":
                out.append(("thinking", ""))          # never harvested
            elif bt == "tool_use":
                out.append(("tool_use", ""))
    return out


def segment_events(lines):
    """Turn raw JSONL lines into an ordered list of harvestable segments.

    Returns list of dicts: {id: 'U3'|'A7', kind: 'user-direct'|'assistant', text}.
    Only genuine user text and assistant prose survive; tool_result / thinking /
    tool_use / sidechain (subagent) events are dropped here, by design.
    """
    segs = []
    u = a = 0
    for line_idx, line in enumerate(lines):
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except Exception:
            continue
        if ev.get("isSidechain"):          # subagent chatter, not the operator
            continue
        if ev.get("type") not in ("user", "assistant"):
            continue
        msg = ev.get("message") or {}
        role = msg.get("role")
        blocks = _blocks(msg.get("content"))
        has_tool_result = any(bt == "tool_result" for bt, _ in blocks)
        texts = [t.strip() for bt, t in blocks if bt == "text" and t and t.strip()]
        if role == "user":
            # A user turn carrying ANY tool_result is tool output, not the human.
            if has_tool_result or not texts:
                continue
            for t in texts:
                # Strip injected system-reminder / relevant-memory payloads so
                # recalled memories can't be re-harvested as user-direct.
                t = _strip_injected(t)
                if not t:
                    continue
                u += 1
                segs.append({"id": f"U{u}", "kind": "user-direct", "text": t, "line_idx": line_idx})
        elif role == "assistant":
            for t in texts:
                a += 1
                segs.append({"id": f"A{a}", "kind": "assistant", "text": t, "line_idx": line_idx})
    return segs


# ----------------------------------------------------------------------------
# Extraction via the local model.
# ----------------------------------------------------------------------------

EXTRACT_PROMPT = """\
You extract DURABLE, reusable facts worth remembering long-term from a developer
conversation. You are given numbered segments. U# = the human operator's own
words. A# = the assistant's words.

Return ONLY a JSON array (no prose, no code fence). Each element:
{{
  "name": "<kebab-case slug, <=6 words>",
  "type": "user|feedback|project|reference",
  "description": "<one concrete line>",
  "body": "<1-4 sentences; the fact itself>",
  "cites": ["U3","A7"],          // the segment ids that SUPPORT this fact
  "confidence": 0.0-1.0
}}

Rules:
- Save ONLY lasting facts: durable decisions, preferences/feedback, stable project
  architecture/config, infrastructure that persists, durable references.
- HARD REJECT transient/session-state — do NOT save any of these:
  * status checks: "X is running/healthy/listening/up", "port N open", "build passed"
  * momentary actions: "rebuilt/restarted/recreated/stashed X", "container recreated"
  * progress/meta: "resume checkpoint", "status update", "final state check",
    "permission denied then fixed", one-off command outputs, "did X this session".
  Save the DECISION or the DURABLE CONFIG behind an action, never the momentary status.
  If a fact would be false next week, it is NOT durable — drop it.
- `cites` MUST list the exact segment ids the fact is grounded in. Prefer U#.
- If nothing durable is present, return [].
- Be terse and STRICT — better to return [] than to save session noise. Max 6 items.

SEGMENTS:
{segments}
"""


def _render_segments(segs, max_chars):
    """Render OLDEST-first up to a char budget. Oldest-first (was newest-first) so
    the watermark can advance monotonically to the last rendered segment and the
    UN-rendered remainder is picked up next run — instead of being silently dropped
    when a dense session exceeds the budget. Returns (text, valid_ids, kept_count):
    kept_count lets the caller advance the watermark only past what was harvested."""
    kept, total = [], 0
    for s in segs:
        line = f"[{s['id']}] {s['text']}"
        if len(line) > 2000:
            line = line[:2000] + " …"
        if kept and total + len(line) > max_chars:   # always keep at least one (progress)
            break
        kept.append(s)
        total += len(line)
    return ("\n".join(f"[{s['id']}] {s['text'][:2000]}" for s in kept),
            {s["id"] for s in kept}, len(kept))


def _parse_json_array(raw):
    """Pull a JSON array out of a model response that may wrap it in fences/prose."""
    raw = raw.strip()
    raw = re.sub(r"^```[a-zA-Z]*\n?", "", raw)
    raw = re.sub(r"\n?```$", "", raw).strip()
    i, j = raw.find("["), raw.rfind("]")
    if i == -1 or j == -1 or j < i:
        return []
    try:
        data = json.loads(raw[i:j + 1])
        return data if isinstance(data, list) else []
    except Exception:
        return []


def classify_provenance(cites, valid_ids, seg_kind):
    """Deterministic provenance from cited segment ids.

    user-direct : non-empty cites AND every cited id is a valid U# segment.
    assistant   : at least one valid A# citation, none invalid.
    unverified  : cites empty or referencing ids we didn't send (model guessed).
    """
    cites = [c for c in (cites or []) if isinstance(c, str)]
    if not cites:
        return "unverified"
    if any(c not in valid_ids for c in cites):
        return "unverified"
    kinds = {seg_kind.get(c) for c in cites}
    if kinds == {"user-direct"}:
        return "user-direct"
    if "assistant" in kinds:
        return "assistant"
    return "unverified"


# ----------------------------------------------------------------------------
# Staging writer.
# ----------------------------------------------------------------------------

def _safe_name(name):
    s = re.sub(r"[^a-z0-9-]+", "-", (name or "").lower()).strip("-")
    return s or "untitled"


def write_candidate(cand, provenance, session_id, valid_ids):
    typ = cand.get("type") if cand.get("type") in VALID_TYPES else "reference"
    base = _safe_name(cand.get("name"))
    fname = f"{typ}_{base}.md"
    path = STAGING / fname
    n = 2
    while path.exists():
        path = STAGING / f"{typ}_{base}-{n}.md"
        n += 1
    now = datetime.datetime.now().astimezone().isoformat(timespec="seconds")
    cites = [c for c in (cand.get("cites") or []) if c in valid_ids]
    desc = (cand.get("description") or "").replace("\n", " ").strip()
    body = (cand.get("body") or "").strip()
    doc = f"""---
name: {path.stem}
description: {desc}
metadata:
  type: {typ}
harvest:
  status: quarantined
  provenance: {provenance}
  skill_eligible: {str(provenance == "user-direct").lower()}
  source_session: {session_id}
  cites: {json.dumps(cites)}
  confidence: {float(cand.get('confidence') or 0.0):.2f}
  harvested_at: {now}
---

{body}

<!-- staged by memory_harvest.py — quarantined, not in recall. provenance={provenance} -->
"""
    path.write_text(doc, encoding="utf-8")
    return path


# ----------------------------------------------------------------------------
# Watermark (per-transcript byte offset; transcripts are append-only JSONL).
# ----------------------------------------------------------------------------

def load_state():
    try:
        return json.loads(STATE_PATH.read_text())
    except Exception:
        return {"schema": 1, "files": {}}


def save_state(state):
    STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
    STATE_PATH.write_text(json.dumps(state, indent=1))


# ----------------------------------------------------------------------------

def harvest_transcript(path: Path, role: str, max_chars: int, state: dict,
                       use_watermark: bool, dry_run: bool):
    fkey = path.name
    rec = state["files"].get(fkey, {}) if use_watermark else {}
    offset = int(rec.get("offset", 0)) if use_watermark else 0
    size = path.stat().st_size
    if use_watermark and offset >= size:
        return {"file": fkey, "skipped": "no new bytes", "candidates": []}

    # Read in BYTES for exact per-line offsets, so the watermark can advance to a
    # precise line boundary (never mid-line, never past un-harvested content).
    with path.open("rb") as fh:
        fh.seek(offset)
        blob = fh.read()
    blines = blob.splitlines(keepends=True)
    # 1c: a live transcript can be mid-append — a final line without a trailing
    # newline is incomplete; drop it and stop the watermark before it so both halves
    # of that event are re-read next run.
    if blines and not blines[-1].endswith((b"\n", b"\r")):
        blines = blines[:-1]
    new_lines, line_end_off, pos = [], [], offset
    for bl in blines:
        pos += len(bl)
        new_lines.append(bl.decode("utf-8", "ignore"))
        line_end_off.append(pos)          # byte offset just AFTER this line
    new_offset = pos

    segs = segment_events(new_lines)
    if not segs:
        if use_watermark:
            state["files"][fkey] = {"offset": new_offset}
        return {"file": fkey, "segments": 0, "candidates": []}

    rendered, valid_ids, kept_n = _render_segments(segs, max_chars)
    # 1a: advance the watermark only past the segments actually rendered/harvested.
    # If some were dropped (budget), the remainder is re-read next run (not lost).
    if kept_n < len(segs):
        watermark_off = line_end_off[segs[kept_n - 1]["line_idx"]]
    else:
        watermark_off = new_offset
    seg_kind = {s["id"]: s["kind"] for s in segs}
    session_id = path.stem

    prompt = EXTRACT_PROMPT.format(segments=rendered)
    raw = memory_ai.ollama_generate(prompt, role=role)
    cands = _parse_json_array(raw)
    # 1b: the LLM returned nothing usable AND didn't legitimately say "empty" — treat
    # as a transient failure and DON'T advance the watermark, so the window is retried
    # instead of silently discarded (truncated/garbage response, gateway hiccup).
    if not cands and "[]" not in (raw or ""):
        return {"file": fkey, "segments": len(segs),
                "error": "empty/garbage LLM response — watermark held", "candidates": []}

    results = []
    for c in cands:
        if not isinstance(c, dict) or not c.get("body"):
            continue
        # Don't even stage transient session-status noise.
        if memory_ai.is_transient_fact(c.get("name", ""), c.get("description", "")):
            continue
        prov = classify_provenance(c.get("cites"), valid_ids, seg_kind)
        out = {
            "name": c.get("name"),
            "type": c.get("type"),
            "provenance": prov,
            "cites": [x for x in (c.get("cites") or []) if x in valid_ids],
            "confidence": c.get("confidence"),
        }
        if not dry_run:
            out["path"] = str(write_candidate(c, prov, session_id, valid_ids))
        results.append(out)

    if use_watermark and not dry_run:
        state["files"][fkey] = {"offset": watermark_off}   # only past harvested segments (1a)
    return {"file": fkey, "segments": len(segs), "rendered": kept_n,
            "user_direct_segs": sum(1 for s in segs if s["kind"] == "user-direct"),
            "candidates": results}


def main():
    args = sys.argv[1:]
    cfg = memory_ai.load()
    if not memory_ai.local_enabled(cfg):
        print("local_enabled is false — harvest skipped.")
        return

    role = "harvest"
    if "--role" in args:
        role = args[args.index("--role") + 1]
    # Fall back to the distill expert if the harvest role isn't configured yet.
    if not memory_ai.expert_model(role, cfg):
        print(f"[harvest] expert '{role}' not configured — falling back to 'distill'", file=sys.stderr)
        role = "distill"
    max_chars = 24000
    if "--max-chars" in args:
        max_chars = int(args[args.index("--max-chars") + 1])
    # Backfill guard: each invocation harvests at most this many transcripts that
    # actually have new content (one slow LLM call each). The watermark means the
    # next run picks up where this left off, so a 102-transcript backfill spreads
    # over nights instead of pinning the 8GB host for hours in one go.
    max_files = int((cfg.get("harvest") or {}).get("max_files_per_run", 4))
    if "--max-files" in args:
        max_files = int(args[args.index("--max-files") + 1])
    dry_run = "--dry-run" in args
    use_watermark = "--since-all" not in args and "--transcript" not in args

    STAGING.mkdir(parents=True, exist_ok=True)

    if "--transcript" in args:
        targets = [Path(args[args.index("--transcript") + 1])]
        use_watermark = False
    else:
        targets = sorted(PROJ_DIR.glob("*.jsonl"), key=lambda p: p.stat().st_mtime)

    # Idle-grace: only harvest a FINISHED chat. A transcript still being written
    # (active session) keeps gaining bytes, so harvesting it on a schedule would pay
    # tokens on the growing tail of an in-progress conversation, repeatedly. Skip any
    # transcript touched within `idle_minutes` — harvest it once it has settled.
    idle_min = float((cfg.get("harvest") or {}).get("idle_minutes", 30))
    if "--idle-minutes" in args:
        idle_min = float(args[args.index("--idle-minutes") + 1])
    now_ts = datetime.datetime.now().timestamp()

    state = load_state()
    summary = []
    processed = 0
    for t in targets:
        if not t.exists():
            print(f"[harvest] not found: {t}", file=sys.stderr)
            continue
        # Cheap skip for fully-harvested transcripts (don't count against the cap).
        if use_watermark:
            if idle_min > 0 and (now_ts - t.stat().st_mtime) < idle_min * 60:
                continue                      # still active — wait until the chat finishes
            off = int(state["files"].get(t.name, {}).get("offset", 0))
            if off >= t.stat().st_size:
                continue
        if processed >= max_files:
            break
        try:
            res = harvest_transcript(t, role, max_chars, state, use_watermark, dry_run)
        except Exception as e:
            print(f"[harvest] {t.name}: ERROR {e}", file=sys.stderr)
            continue
        summary.append(res)
        if res.get("segments"):
            processed += 1

    if use_watermark and not dry_run:
        save_state(state)

    # Print a structural summary ONLY — never candidate bodies (may hold secrets).
    total = sum(len(r.get("candidates", [])) for r in summary)
    by_prov = {}
    for r in summary:
        for c in r.get("candidates", []):
            by_prov[c["provenance"]] = by_prov.get(c["provenance"], 0) + 1
    print(f"# memory_harvest — role={role} dry_run={dry_run}")
    print(f"transcripts processed: {sum(1 for r in summary if r.get('segments'))}")
    print(f"candidates staged: {total}   by provenance: {by_prov or '{}'}")
    print(f"  (user-direct = skill-eligible; assistant/unverified = memory-only)")
    for r in summary:
        cs = r.get("candidates", [])
        if not cs:
            continue
        print(f"\n{r['file']}  (segs={r.get('segments')}, user-direct={r.get('user_direct_segs')}):")
        for c in cs:
            nm = _safe_name(c.get("name"))
            print(f"  - [{c['provenance']:<11}] {c.get('type') or '?':<9} {nm}  cites={c['cites']} conf={c.get('confidence')}")
    if not dry_run:
        print(f"\nstaged into {STAGING}  (quarantined — not in recall until graduated)")


if __name__ == "__main__":
    main()
