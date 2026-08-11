#!/usr/bin/env python3
"""UserPromptSubmit hook — auto-recall: put the memories relevant to THIS prompt in
front of Claude, without waiting for it to think of calling the recall tool.

Design constraints, in order of importance:
  1. Never block the prompt. Any error, timeout, or empty result prints nothing and
     exits 0. A broken memory system must not break the session.
  2. Never balloon the context. Names + one-line descriptions only — never memory
     bodies — and each memory is injected AT MOST ONCE PER SESSION (state keyed on
     session_id). Without that, a long session re-injects the same few memories on
     every prompt and the "help" becomes the problem.
  3. No daemon. It talks to Qdrant/Ollama/Neo4j over HTTP on localhost via
     memory_recall.py (stdlib only, ~0.3s on system python3). The previous
     incarnation of this hook called a FastAPI server that nobody was running, so it
     silently injected nothing for weeks — fail-open plus a daemon means invisible
     death.

Config (engram.yaml):
  recall.inject.{enabled,k,max_facts,timeout_ms}
"""
import hashlib
import json
import os
import re
import sys
import time
from pathlib import Path

ENGRAM_BIN = Path(os.environ.get("ENGRAM_BIN", Path.home() / ".claude"))
STATE_DIR = ENGRAM_BIN / "logs" / "recall-inject"
STATE_TTL = 7 * 86400          # forget session state after a week
SAFE_ID = re.compile(r"[^A-Za-z0-9._-]")


def _state_path(session_id: str) -> Path:
    return STATE_DIR / f"{SAFE_ID.sub('_', session_id or 'default')[:120]}.json"


def _fact_id(fact: str) -> str:
    """Facts are free text, so key them by hash — keeps the state file bounded."""
    return hashlib.sha1(" ".join(fact.split()).encode()).hexdigest()[:16]


def _seen(session_id: str) -> tuple[set, set]:
    """(memory files, fact ids) already injected in this session."""
    try:
        d = json.loads(_state_path(session_id).read_text())
        return set(d.get("files", [])), set(d.get("facts", []))
    except Exception:
        return set(), set()


def _remember(session_id: str, files: list, facts: list) -> None:
    """Record what we injected, and sweep state files older than STATE_TTL."""
    try:
        old_files, old_facts = _seen(session_id)
        STATE_DIR.mkdir(parents=True, exist_ok=True)
        _state_path(session_id).write_text(json.dumps({
            "files": sorted(old_files | set(files)),
            "facts": sorted(old_facts | {_fact_id(f) for f in facts})}))
        cutoff = time.time() - STATE_TTL
        for old in STATE_DIR.glob("*.json"):
            if old.stat().st_mtime < cutoff:
                old.unlink(missing_ok=True)
    except Exception:
        pass                    # state is an optimisation, not a requirement


def _quiet(why: str) -> None:
    """Explain a no-op on stderr under ENGRAM_HOOK_DEBUG=1. A fail-open hook that
    can't say why it produced nothing is how the last one stayed dead for weeks:
      echo '{"prompt":"...","session_id":"dbg"}' | ENGRAM_HOOK_DEBUG=1 <this hook>
    """
    if os.environ.get("ENGRAM_HOOK_DEBUG"):
        print(f"[recall-inject] {why}", file=sys.stderr)


def main() -> None:
    try:
        payload = json.load(sys.stdin)
    except Exception as e:
        return _quiet(f"unreadable hook payload: {e}")
    prompt = (payload.get("prompt") or "").strip()
    session_id = payload.get("session_id") or "default"
    # Cheap gates first: one-word follow-ups and slash/bang commands carry no
    # retrieval signal, and running on them is how you turn 0.3s into an annoyance.
    if len(prompt) < 25 or prompt[0] in "/!":
        return _quiet("gated: prompt too short or a slash/bang command")

    sys.path.insert(0, str(ENGRAM_BIN))
    try:
        import memory_ai
        import memory_recall
    except Exception as e:
        return _quiet(f"engine not importable from {ENGRAM_BIN}: {e}")

    try:
        inject = (memory_ai.recall_cfg(memory_ai.load()).get("inject") or {})
    except Exception as e:
        _quiet(f"config unreadable, using defaults: {e}")
        inject = {}
    if not inject.get("enabled", True):
        return _quiet("disabled (recall.inject.enabled: false)")
    k = int(inject.get("k", 4))
    max_facts = int(inject.get("max_facts", 6))
    timeout = max(0.2, float(inject.get("timeout_ms", 2500)) / 1000.0)

    try:
        # ponytail: per-HTTP-call timeouts, not a wall-clock kill — the keyword leg is
        # local BM25 and unbounded in store size. Move to a subprocess + hard timeout
        # if anyone's store grows big enough for that to matter.
        out = memory_recall.recall(prompt, k=k, fast=True,
                                   cwd=payload.get("cwd") or "", timeout=timeout)
    except Exception as e:
        return _quiet(f"recall failed: {e}")
    _quiet(f"store={os.environ.get('CLAUDE_MEMORY_SLUG')} "
           f"hits={len(out.get('results', []))} facts={len(out.get('facts', []))}")

    seen_files, seen_facts = _seen(session_id)
    fresh = [r for r in out.get("results", []) if r["file"] not in seen_files]
    facts = [f for f in out.get("facts", []) if _fact_id(f) not in seen_facts][:max_facts]
    if not fresh and not facts:
        return _quiet("everything relevant was already injected this session")

    lines = ['<relevant-memory note="engram auto-recall: vector + keyword + graph. '
             'Read the .md file for the full memory.">']
    for r in fresh:
        lines.append(f"- {r['name']} [{'+'.join(r['sources'])}]: {r['description']}")
    if facts:
        lines.append("Graph facts (1-hop):")
        lines += [f"- {f}" for f in facts]
    lines.append("</relevant-memory>")
    print("\n".join(lines))
    _remember(session_id, [r["file"] for r in fresh], facts)


if __name__ == "__main__":
    main()
