#!/usr/bin/env python3
"""engram_telegram_gate.py — async human-approval queue for the risky autonomous
memory ops (skill installs, purges, permanent deletes, Codex-deferred merges).

Design (Fable 2026-07-11): the daemon PROPOSES; a human APPROVES with one tap on
Telegram; a dumb dispatcher APPLIES. The daemon never blocks — propose is
fire-and-forget, approvals apply on the poll callback or the next tick.

Security / reliability properties:
  * Long-poll getUpdates only — outbound, no webhook, no public endpoint (works
    behind NAT). Token + chat id from daemon.env (mode 600), never the repo.
  * callback_data carries an OPAQUE proposal id only; every action parameter lives
    server-side in the queue file -> a forged/replayed callback can't do anything new.
  * chat_id allowlist = the operator only. Idempotent consume via atomic rename
    (pending -> approved/rejected/applied); a second callback on a consumed id is a
    no-op. Replay is structurally impossible.
  * 72h TTL -> DROP (never default-apply). source_hashes re-checked at apply time;
    a changed source drops+re-proposes.
  * Works with NO token: proposals still queue to disk; approve via the local CLI
    (--approve <id>). Telegram send/poll simply no-op until the token appears.

Queue: <store>/.approvals/{pending,approved,applied,rejected,expired}/<id>.json
CLI:
  engram_telegram_gate.py --poll         # handle callbacks + expire + apply approved (one pass)
  engram_telegram_gate.py --list         # show pending proposals
  engram_telegram_gate.py --approve <id> # local approval (no phone needed)
  engram_telegram_gate.py --reject <id>
  engram_telegram_gate.py --notify "msg" # fire a plain Telegram message (undo notices, digest)
"""
from __future__ import annotations
import hashlib
import json
import os
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

HOME = Path.home()
SLUG = os.environ.get("CLAUDE_MEMORY_SLUG") or str(HOME).replace("/", "-")
MEM = HOME / ".claude" / "projects" / SLUG / "memory"
Q = MEM / ".approvals"
STATES = ("pending", "approved", "applied", "rejected", "expired")
OFFSET_FILE = Q / ".tg_offset"
DEFAULT_TTL_H = int(os.environ.get("ENGRAM_APPROVAL_TTL_H", "72"))

TOKEN = os.environ.get("TELEGRAM_BOT_TOKEN", "")
CHAT_ID = os.environ.get("TELEGRAM_CHAT_ID", "")


def _dir(state):
    d = Q / state; d.mkdir(parents=True, exist_ok=True); return d


def _now(): return int(time.time())


def _id(op, params):
    h = hashlib.sha256(f"{op}:{json.dumps(params, sort_keys=True)}".encode()).hexdigest()[:12]
    return f"{op}-{h}"


def _find(pid):
    for st in STATES:
        p = _dir(st) / f"{pid}.json"
        if p.exists():
            return st, p
    return None, None


def source_hashes(files):
    """Hash the affected source files so a stale proposal (files changed between
    propose and approve) can be detected and dropped."""
    out = {}
    for f in files or []:
        p = MEM / f
        out[f] = hashlib.sha256(p.read_bytes()).hexdigest()[:16] if p.is_file() else None
    return out


# ---- Telegram I/O (no-op without a token) ---------------------------------
def _tg(method, **params):
    if not TOKEN:
        return None
    try:
        data = urllib.parse.urlencode({k: (json.dumps(v) if isinstance(v, (dict, list)) else v)
                                       for k, v in params.items()}).encode()
        req = urllib.request.Request(f"https://api.telegram.org/bot{TOKEN}/{method}", data=data)
        with urllib.request.urlopen(req, timeout=35) as r:
            return json.loads(r.read())
    except Exception as e:
        sys.stderr.write(f"[tg] {method} failed: {e}\n"); return None


def notify(text):
    """Plain message (undo notices, weekly digest). No-op without a token."""
    if TOKEN and CHAT_ID:
        _tg("sendMessage", chat_id=CHAT_ID, text=text, disable_web_page_preview=True)
    else:
        print(f"[notify] {text}")


def _send_proposal(pid, prop):
    kb = {"inline_keyboard": [[{"text": "✅ Approve", "callback_data": f"a:{pid}"},
                               {"text": "❌ Reject", "callback_data": f"r:{pid}"}]]}
    txt = (f"🧠 engram proposal — {prop['op']}\n\n{prop['preview']}\n\n"
           f"Codex: {prop.get('codex_verdict', 'n/a')} · expires in {DEFAULT_TTL_H}h")
    res = _tg("sendMessage", chat_id=CHAT_ID, text=txt, reply_markup=kb)
    if res and res.get("ok"):
        return res["result"]["message_id"]
    return None


# ---- Proposal lifecycle ----------------------------------------------------
def propose(op, params, preview, files=None, codex_verdict=None, ttl_h=DEFAULT_TTL_H):
    """Queue an op for human approval. Returns the proposal id (idempotent: the same
    op+params re-proposes at most once; an already-queued/decided id is left as-is)."""
    pid = _id(op, params)
    st, _ = _find(pid)
    if st in ("pending", "approved", "applied"):
        return pid                       # already in flight / done — don't spam
    prop = {"id": pid, "op": op, "params": params, "preview": preview,
            "source_hashes": source_hashes(files), "codex_verdict": codex_verdict,
            "created_at": _now(), "expires_at": _now() + ttl_h * 3600}
    prop["telegram_msg_id"] = _send_proposal(pid, prop)
    (_dir("pending") / f"{pid}.json").write_text(json.dumps(prop, indent=1))
    return pid


def _move(pid, to_state):
    st, p = _find(pid)
    if not p:
        return None
    dst = _dir(to_state) / f"{pid}.json"
    p.rename(dst)
    return dst


def _apply_one(prop):
    """Dispatch an approved op to its handler. Re-check source freshness first."""
    cur = source_hashes(list((prop.get("source_hashes") or {}).keys()))
    if cur != prop.get("source_hashes"):
        return False, "sources changed since proposal — dropped"
    handler = OPS.get(prop["op"])
    if not handler:
        return False, f"no handler for op {prop['op']}"
    try:
        return handler(prop["params"])
    except Exception as e:
        return False, f"apply error: {e}"


def poll_once():
    """One maintenance pass: consume Telegram callbacks, expire stale, apply approved."""
    # 1. Telegram callbacks -> approved/rejected
    if TOKEN:
        off = 0
        try:
            off = int(OFFSET_FILE.read_text())
        except Exception:
            pass
        res = _tg("getUpdates", offset=off + 1, timeout=25, allowed_updates=["callback_query"])
        for up in (res or {}).get("result", []):
            OFFSET_FILE.write_text(str(up["update_id"]))
            cq = up.get("callback_query") or {}
            if str((cq.get("message") or {}).get("chat", {}).get("id")) != str(CHAT_ID):
                continue                 # allowlist: only the operator
            action, _, pid = (cq.get("data") or "").partition(":")
            st, _ = _find(pid)
            if st != "pending":
                _tg("answerCallbackQuery", callback_query_id=cq["id"], text="already handled")
                continue
            _move(pid, "approved" if action == "a" else "rejected")
            _tg("answerCallbackQuery", callback_query_id=cq["id"],
                text="✅ approved" if action == "a" else "❌ rejected")
    # 2. expire stale pendings -> DROP
    for p in _dir("pending").glob("*.json"):
        prop = json.loads(p.read_text())
        if _now() > prop.get("expires_at", 0):
            _move(prop["id"], "expired")
            notify(f"⏰ proposal expired (dropped): {prop['op']} — {prop['id']}")
    # 3. apply approved
    applied = []
    for p in sorted(_dir("approved").glob("*.json")):
        prop = json.loads(p.read_text())
        ok, detail = _apply_one(prop)
        prop["result"] = {"ok": ok, "detail": detail, "at": _now()}
        (_dir("applied") / f"{prop['id']}.json").write_text(json.dumps(prop, indent=1))
        p.unlink(missing_ok=True)
        applied.append((prop["id"], ok, detail))
        notify(f"{'✅ applied' if ok else '⚠️ apply failed'}: {prop['op']} — {detail}")
    return applied


# ---- Op handlers (extended as phases land) --------------------------------
def _apply_skill_install(params):
    """Move a staged skill from skills/auto/.pending/<name> into skills/auto/<name>."""
    name = params["name"]
    pend = HOME / ".claude" / "skills" / "auto" / ".pending" / name
    dest = HOME / ".claude" / "skills" / "auto" / name
    if not pend.is_dir():
        return False, f"pending skill {name} not found"
    dest.parent.mkdir(parents=True, exist_ok=True)
    pend.rename(dest)
    return True, f"installed skill {name}"


OPS = {"skill_install": _apply_skill_install}


def main():
    a = sys.argv[1:]
    if "--notify" in a:
        notify(a[a.index("--notify") + 1]); return
    if "--approve" in a:
        pid = a[a.index("--approve") + 1]
        print("approved" if _move(pid, "approved") else "not found"); return
    if "--reject" in a:
        pid = a[a.index("--reject") + 1]
        print("rejected" if _move(pid, "rejected") else "not found"); return
    if "--list" in a:
        ps = sorted(_dir("pending").glob("*.json"))
        print(f"{len(ps)} pending proposal(s):")
        for p in ps:
            d = json.loads(p.read_text())
            print(f"  {d['id']}  {d['op']}  exp={d['expires_at']-_now()}s\n    {d['preview'][:100]}")
        return
    if "--poll" in a:
        for pid, ok, detail in poll_once():
            print(f"{pid}: {'ok' if ok else 'FAIL'} — {detail}")
        return
    print(__doc__)


if __name__ == "__main__":
    main()
