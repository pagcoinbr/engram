#!/usr/bin/env python3
"""engram_llm_audit.py — append-only audit of generation calls engram sends to the
model, so a *loop* (the same prompt re-sent over and over) is visible.

Every generation attempt — one llama.cpp / ollama / claude(-ccg) request, or one
Graphiti `chat.completions.create` — appends ONE JSON line to

    ${ENGRAM_LOG_DIR:-~/.claude/logs}/llm_events.jsonl

Design constraints (this runs on the hot path, in many processes at once):
  * NEVER raise into the caller. Auditing must not break a generation; every public
    function is wrapped so a full disk / missing key / bad fd is swallowed.
  * Multi-writer safe. Writers are the daemon, ~dozens of long-lived MCP servers, and
    one-shot CLIs — all appending concurrently. O_APPEND + a single write() is atomic
    for ordinary appends on local Linux; the sidecar flock exists ONLY to make the
    size-based rotation race-free (rotate must happen before anyone opens the file).
  * Content-free. We store a KEYED digest of the prompt (HMAC with a host-local key),
    not the prompt text: the log is global across tenants and the atlas UI has no
    content redaction, so a repeat must be detectable WITHOUT exposing or allowing a
    dictionary attack on any tenant's prompt. `detail` is a sanitized error class,
    never raw backend stderr.

Public API: SCHEMA_VERSION, new_call_id(), digest(text), record(**fields),
tail(n), detect_loops(events, window_s, threshold).
"""
from __future__ import annotations

import fcntl
import hashlib
import hmac
import json
import os
import sys
import time
import uuid
from pathlib import Path

SCHEMA_VERSION = 1

# Rotate the live file once it passes this; keep exactly one previous generation
# (.jsonl.1). 16 MB ~= 80k events — plenty of recent history, bounded on disk.
_MAX_BYTES = 16 * 1024 * 1024
# Reading backward: never load the whole file. The last slice of this many bytes
# holds far more than any sane `tail(n)` asks for (events are ~250 B).
_TAIL_BYTES = 768 * 1024


def _home() -> Path:
    return Path(os.environ.get("HOME", str(Path.home())))


def _log_dir() -> Path:
    return Path(os.environ.get("ENGRAM_LOG_DIR", _home() / ".claude" / "logs"))


def _log_path() -> Path:
    return _log_dir() / "llm_events.jsonl"


def _lock_path() -> Path:
    return _log_dir() / "llm_events.jsonl.lock"


# ---------------------------------------------------------------------------
# Keyed digest — HMAC so an identical prompt yields an identical digest (loop
# signal) while the digest is useless for guessing the prompt. Falls back to a
# plain sha256 if the key cannot be read/created, so auditing still works.
# ---------------------------------------------------------------------------
_KEY: bytes | None = None
_KEY_LOADED = False


def _key() -> bytes | None:
    global _KEY, _KEY_LOADED
    if _KEY_LOADED:
        return _KEY
    _KEY_LOADED = True
    try:
        path = Path(os.environ.get("ENGRAM_CONFIG_HOME", _home() / ".config" / "engram")) / "llm_audit.key"
        if path.is_file():
            _KEY = path.read_bytes().strip() or None
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            material = os.urandom(32).hex().encode()
            # Create private; if it already appeared (another process raced us),
            # read whatever is there so every writer agrees on one key.
            fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
            try:
                os.write(fd, material)
                _KEY = material
            finally:
                os.close(fd)
    except FileExistsError:
        try:
            _KEY = (Path(os.environ.get("ENGRAM_CONFIG_HOME", _home() / ".config" / "engram")) / "llm_audit.key").read_bytes().strip() or None
        except OSError:
            _KEY = None
    except OSError:
        _KEY = None
    return _KEY


def digest(text: str) -> str:
    """12-hex keyed HMAC-SHA256 of `text` (sha256 fallback if no key)."""
    data = (text or "").encode("utf-8", "replace")
    key = _key()
    if key:
        return hmac.new(key, data, hashlib.sha256).hexdigest()[:12]
    return hashlib.sha256(data).hexdigest()[:12]


def new_call_id() -> str:
    """A short id correlating the attempts (retries / fallbacks) of one logical call."""
    return uuid.uuid4().hex[:12]


def _proc() -> str:
    override = os.environ.get("ENGRAM_AUDIT_PROC")
    if override:
        return override
    try:
        name = Path(sys.argv[0]).name
    except Exception:
        name = ""
    return name or "python"


# ---------------------------------------------------------------------------
# Write
# ---------------------------------------------------------------------------
def record(**fields) -> None:
    """Append one audit event. Best-effort — never raises.

    Expected fields (all optional; sensible defaults filled): kind, src, call_id,
    attempt, backend, endpoint, model, role, chars, digest, ms, outcome, detail.
    """
    try:
        event = {
            "v": SCHEMA_VERSION,
            "ts": round(time.time(), 3),
            "pid": os.getpid(),
            "proc": _proc(),
        }
        event.update(fields)
        line = (json.dumps(event, separators=(",", ":"), ensure_ascii=False) + "\n").encode("utf-8", "replace")
    except Exception:
        return  # could not even serialize — give up silently

    try:
        log_dir = _log_dir()
        log_dir.mkdir(parents=True, exist_ok=True)
        lock_fd = os.open(str(_lock_path()), os.O_CREAT | os.O_RDWR, 0o600)
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_EX)
            path = _log_path()
            # Rotate BEFORE anyone opens the file: opening first would let a waiter
            # append into the about-to-be-renamed file and lose those lines.
            try:
                if path.exists() and path.stat().st_size + len(line) > _MAX_BYTES:
                    os.replace(str(path), str(path) + ".1")
            except OSError:
                pass
            out_fd = os.open(str(path), os.O_CREAT | os.O_WRONLY | os.O_APPEND, 0o600)
            try:
                view = memoryview(line)
                while view:
                    written = os.write(out_fd, view)
                    if written <= 0:
                        break
                    view = view[written:]
            finally:
                os.close(out_fd)
        finally:
            fcntl.flock(lock_fd, fcntl.LOCK_UN)
            os.close(lock_fd)
    except Exception:
        return  # disk full, permissions, etc. — auditing is never fatal


# ---------------------------------------------------------------------------
# Read
# ---------------------------------------------------------------------------
def _read_tail_bytes(path: Path, limit: int) -> str:
    """Last `limit` bytes of `path` as text (whole file if smaller). Empty on error."""
    try:
        size = path.stat().st_size
    except OSError:
        return ""
    try:
        with open(path, "rb") as handle:
            if size > limit:
                handle.seek(size - limit)
                handle.readline()  # drop the partial first line
            return handle.read().decode("utf-8", "replace")
    except OSError:
        return ""


def _parse(lines: list[str]) -> list[dict]:
    out = []
    for raw in lines:
        raw = raw.strip()
        if not raw:
            continue
        try:
            event = json.loads(raw)
        except (ValueError, TypeError):
            continue  # partial / corrupt line
        if not isinstance(event, dict) or event.get("v") != SCHEMA_VERSION:
            continue  # a line from a different schema version
        out.append(event)
    return out


def tail(n: int = 300) -> list[dict]:
    """The most recent `n` events, newest first. Best-effort — never raises."""
    try:
        n = max(1, int(n))
    except (TypeError, ValueError):
        n = 300
    try:
        lock_fd = os.open(str(_lock_path()), os.O_CREAT | os.O_RDWR, 0o600)
    except OSError:
        lock_fd = None
    try:
        if lock_fd is not None:
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_SH)
            except OSError:
                pass
        path = _log_path()
        events = _parse(_read_tail_bytes(path, _TAIL_BYTES).splitlines())
        if len(events) < n:
            older = _parse(_read_tail_bytes(Path(str(path) + ".1"), _TAIL_BYTES).splitlines())
            events = older + events
        return list(reversed(events[-n:]))
    except Exception:
        return []
    finally:
        if lock_fd is not None:
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_UN)
            finally:
                os.close(lock_fd)


def detect_loops(events: list[dict], window_s: int = 300, threshold: int = 3) -> list[dict]:
    """Group recent GENERATION events by identical (digest, model, role, proc) and
    flag any group seen >= `threshold` times within the last `window_s` seconds.

    This is an EXACT-repeat detector: prompts differing only by a timestamp or
    whitespace have different digests and will not be grouped. Health probes
    (kind=='health') are excluded — every process fires an identical probe, which
    would otherwise look like a loop.
    """
    try:
        now = time.time()
        groups: dict[tuple, dict] = {}
        for event in events or []:
            if event.get("kind") == "health":
                continue
            ts = event.get("ts")
            if not isinstance(ts, (int, float)) or now - ts > window_s:
                continue
            key = (event.get("digest"), event.get("model"), event.get("role"), event.get("proc"))
            group = groups.get(key)
            if group is None:
                groups[key] = group = {
                    "digest": event.get("digest"), "model": event.get("model"),
                    "role": event.get("role"), "proc": event.get("proc"),
                    "count": 0, "first_ts": ts, "last_ts": ts,
                }
            group["count"] += 1
            group["first_ts"] = min(group["first_ts"], ts)
            group["last_ts"] = max(group["last_ts"], ts)
        flagged = [g for g in groups.values() if g["count"] >= threshold]
        flagged.sort(key=lambda g: g["count"], reverse=True)
        return flagged
    except Exception:
        return []
