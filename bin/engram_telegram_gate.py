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


def notify_undo(undo_op, undo_params, text):
    """APPLY-NOW-BUT-OFFER-UNDO: the action already happened (reversibly); send a
    message with a one-tap UNDO button. If tapped, poll runs `undo_op`. This is the
    default path for reversible judgment ops (compress-merges) — no pre-approval,
    just a reversal window."""
    uid = _id("undo-" + undo_op, undo_params)
    (_dir("undo") / f"{uid}.json").write_text(json.dumps(
        {"id": uid, "op": undo_op, "params": undo_params, "created_at": _now()}, indent=1))
    if TOKEN and CHAT_ID:
        kb = {"inline_keyboard": [[{"text": "↩️ UNDO", "callback_data": f"u:{uid}"}]]}
        _tg("sendMessage", chat_id=CHAT_ID, text=text, reply_markup=kb, disable_web_page_preview=True)
    else:
        print(f"[notify+undo {uid}] {text}")
    return uid


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
            if action == "u":                        # one-tap UNDO of an applied reversible op
                urec = _dir("undo") / f"{pid}.json"
                if not urec.exists():
                    _tg("answerCallbackQuery", callback_query_id=cq["id"], text="already undone/expired")
                    continue
                u = json.loads(urec.read_text())
                ok, detail = (OPS.get(u["op"]) or (lambda p: (False, "no undo handler")))(u["params"])
                urec.rename(_dir("undone") / f"{pid}.json")
                _tg("answerCallbackQuery", callback_query_id=cq["id"],
                    text="↩️ undone" if ok else f"undo failed: {detail}")
                notify(f"{'↩️ undone' if ok else '⚠️ undo failed'}: {u['op']} — {detail}")
                continue
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
    # 4. housekeeping: probation sweep + weekly digest (both cheap / rate-limited)
    try:
        sweep(); maybe_digest()
    except Exception as e:
        sys.stderr.write(f"[gate] housekeeping error: {e}\n")
    return applied


# ---- Op handlers (extended as phases land) --------------------------------
def _apply_skill_install(params):
    """MATERIALIZE a skill on approval: read the inert staged file
    skills/.pending/<name>.SKILL.md and create the live package skills/auto/<name>/
    SKILL.md — the discoverable shape appears ONLY after the human tap. Verify the
    artifact hash first, and leave a back-pointer on the source memory."""
    name = params["name"]
    staged = HOME / ".claude" / "skills" / ".pending" / f"{name}.SKILL.md"
    dest = HOME / ".claude" / "skills" / "auto" / name
    if not staged.is_file():
        return False, f"staged skill {name} not found"
    # Artifact integrity: install ONLY the exact SKILL.md that was reviewed.
    want = params.get("artifact_sha")
    content = staged.read_bytes()
    if want and hashlib.sha256(content).hexdigest() != want:
        return False, "skill artifact changed since proposal (sha mismatch) — refused"
    if (dest / "SKILL.md").exists():
        staged.unlink(missing_ok=True)
        return True, f"skill {name} already installed"
    dest.mkdir(parents=True, exist_ok=True)
    (dest / "SKILL.md").write_bytes(content)      # discoverable shape created HERE, post-approval
    staged.unlink(missing_ok=True)
    src = params.get("source_memory")
    if src and (MEM / src).is_file():
        b = (MEM / src).read_text(errors="ignore")
        if "**Promoted to skill:**" not in b:
            (MEM / src).write_text(b.rstrip() + f"\n\n**Promoted to skill:** `auto/{name}` (approved via gate)\n")
    return True, f"installed skill {name}"


def _restore_from_quarantine(qf, target_name, save):
    """Re-save a quarantined memory to the live store. FAILURE-ATOMIC: only remove
    the quarantine backup if the restore succeeded AND the file is present. Returns
    True on success; on failure leaves the backup untouched so it's never the only copy."""
    import re as _re
    body = qf.read_text(errors="ignore")
    m = _re.search(r"^description:\s*(.+)$", body, _re.M)
    r = save(target_name, (m.group(1).strip() if m else target_name[:-3]), body)
    if r.returncode == 0 and (MEM / target_name).is_file():
        qf.unlink(missing_ok=True)
        return True
    return False


def _apply_merge_undo(params):
    """Reverse a compress-merge: restore the umbrella member's ORIGINAL content and
    every absorbed source from quarantine. FAILURE-ATOMIC — if any restore fails, the
    quarantine backups are kept and nothing is destroyed (never lose the only copy)."""
    import subprocess
    umbrella = params["umbrella"]; absorbed = params["absorbed"]
    txn = MEM / ".quarantine" / f"merge-{params.get('merge_id','x')}"
    if not txn.is_dir():
        return False, "merge backup gone (probation expired or already undone) — cannot undo"
    # FRESHNESS: refuse if the umbrella was legitimately edited since the merge —
    # restoring the stale original would silently erase newer facts.
    post = txn / ".post_sha"
    if post.is_file() and (MEM / umbrella).is_file():
        live = hashlib.sha256((MEM / umbrella).read_bytes()).hexdigest()
        if live != post.read_text().strip():
            return False, f"umbrella {umbrella} changed since the merge — undo refused (would overwrite newer edits)"
    save = lambda name, desc, body: subprocess.run(
        [str(HOME / ".claude" / "save_memory.sh"), name, desc], input=body, capture_output=True, text=True)
    failures, restored = [], []
    # 1. umbrella member's original content (overwrites the compressed umbrella)
    orig = txn / f"{umbrella}.orig"
    if orig.is_file():
        (restored if _restore_from_quarantine(orig, umbrella, save) else failures).append(umbrella)
    # 2. absorbed sources
    for f in absorbed:
        qf = txn / f
        if qf.is_file():
            if _restore_from_quarantine(qf, f, save):
                (txn / f"{f}.merged-into").unlink(missing_ok=True); restored.append(f)
            else:
                failures.append(f)
    if failures:
        return False, f"restored {restored}; FAILED {failures} — quarantine kept, nothing destroyed"
    try: txn.rmdir()                                   # tidy the txn dir only when fully restored
    except OSError: pass
    return True, f"restored {restored} (umbrella original + absorbed)"


def _apply_merge(params):
    """Apply a compress-merge REVERSIBLY. Backs up the EXACT pre-merge set — the
    umbrella member's ORIGINAL content AND every absorbed source — to .quarantine/
    before overwriting, so merge_undo can restore precisely what existed (even if the
    compression dropped a fact). Absorbed sources leave recall (de-indexed); the
    umbrella member keeps its name/index with the compressed content."""
    import subprocess
    umbrella = params["umbrella"]; content = params["umbrella_content"]; absorbed = params["absorbed"]
    merge_id = params.get("merge_id", "x")
    save = HOME / ".claude" / "save_memory.sh"
    quar = MEM / ".quarantine"
    # PER-MERGE transaction dir so a second merge on the same umbrella can't overwrite
    # the first merge's backup (which would make the earlier undo unrecoverable).
    txn = quar / f"merge-{merge_id}"
    # refuse if this umbrella has an unresolved backup from a prior, un-undone merge
    for d in quar.glob("merge-*"):
        if d != txn and (d / f"{umbrella}.orig").is_file():
            return False, f"umbrella {umbrella} has an outstanding merge backup ({d.name}) — undo it first"
    txn.mkdir(parents=True, exist_ok=True)
    had_orig = (MEM / umbrella).is_file()
    if had_orig:                                       # 1. back up umbrella ORIGINAL
        (txn / f"{umbrella}.orig").write_bytes((MEM / umbrella).read_bytes())
    r = subprocess.run([str(save), umbrella, params.get("desc") or umbrella[:-3]],  # 2. write compressed
                       input=content, capture_output=True, text=True)
    if r.returncode != 0:
        # save may have overwritten the local file before failing (e.g. index/push
        # step) — ROLL BACK from the backup so the original is never lost.
        if had_orig:
            (MEM / umbrella).write_bytes((txn / f"{umbrella}.orig").read_bytes())
        (txn / f"{umbrella}.orig").unlink(missing_ok=True)
        try: txn.rmdir()
        except OSError: pass
        return False, f"umbrella write failed: {r.stderr.strip()[:120]} — rolled back, nothing changed"
    # record the post-merge umbrella hash so undo can refuse if it was edited since
    if (MEM / umbrella).is_file():
        (txn / ".post_sha").write_text(hashlib.sha256((MEM / umbrella).read_bytes()).hexdigest())
    moved = []
    for f in absorbed:                                 # 3. absorbed -> txn dir + de-index
        src = MEM / f
        if src.is_file():
            src.rename(txn / f)
            (txn / f"{f}.merged-into").write_text(f"{umbrella}\t{_now()}\n")
            subprocess.run([str(HOME / ".claude" / "delete_memory.sh"), f], capture_output=True, text=True)
            moved.append(f)
    return True, f"umbrella {umbrella} written; backup in merge-{merge_id} (orig+{moved})"


def _apply_suspect_restore(params):
    """Restore a quarantined injection-suspect back into recall (the human decided
    it's legit). Failure-atomic: keeps the quarantine copy if the restore fails."""
    import subprocess
    name = params["name"]
    qf = MEM / ".quarantine" / name
    if not qf.is_file():
        return False, f"suspect {name} not in quarantine (already restored or purged)"
    # Freshness: if a memory of this name is already LIVE (re-harvested since quarantine),
    # a stale RESTORE would overwrite newer content — refuse and keep the quarantine copy.
    if (MEM / name).is_file():
        return False, f"a memory named {name} is already live — restore refused (kept in quarantine)"
    save = lambda n, d, b: subprocess.run(
        [str(HOME / ".claude" / "save_memory.sh"), n, d], input=b, capture_output=True, text=True)
    if _restore_from_quarantine(qf, name, save):
        return True, f"restored {name} to recall"
    return False, f"restore failed for {name} — kept in quarantine"


OPS = {"skill_install": _apply_skill_install, "merge_undo": _apply_merge_undo,
       "merge_apply": _apply_merge, "suspect_restore": _apply_suspect_restore}


def notify_suspect(name):
    """A memory was auto-quarantined as a possible injection suspect. Notify with a
    one-tap RESTORE — the human never MUST act; the suspect auto-purges after probation
    if ignored. Replaces the /memory-curate suspect-review step."""
    return notify_undo("suspect_restore", {"name": name},
                       f"⚠️ quarantined a possible injection suspect: {name}\n"
                       f"It's out of recall now. Tap RESTORE if it's legit; otherwise it "
                       f"auto-purges after probation.")


PROBATION_DAYS = int(os.environ.get("ENGRAM_PROBATION_DAYS", "30"))
DIGEST_STAMP = Q / ".last_digest"


def sweep():
    """Probation: quarantined merge backups older than PROBATION_DAYS graduate to
    .trash/ (the point of no return, +90 more days there) — so quarantine can't grow
    forever. Also expire undo records past the window. Cheap mtime scan."""
    import shutil
    quar = MEM / ".quarantine"
    cutoff = _now() - PROBATION_DAYS * 86400
    purged = 0
    trash = MEM / ".trash"; trash.mkdir(parents=True, exist_ok=True)
    for d in quar.glob("merge-*"):
        if d.is_dir() and d.stat().st_mtime < cutoff:
            shutil.move(str(d), str(trash / f"{d.name}-{_now()}"))
            purged += 1
    # loose quarantined SUSPECTS (injection): graduate to .trash after probation too,
    # so a suspect the human never restored eventually purges (was: sat forever).
    for f in quar.glob("*.md"):
        if f.stat().st_mtime < cutoff:
            shutil.move(str(f), str(trash / f"{f.name}-{_now()}"))
            (quar / f"{f.name}.merged-into").unlink(missing_ok=True)
            purged += 1
    for u in _dir("undo").glob("*.json"):              # undo window closes with probation
        if u.stat().st_mtime < cutoff:
            u.rename(_dir("undone") / u.name)
    return purged


def maybe_digest():
    """Weekly trust-calibration digest: what the daemon did autonomously, undo any."""
    try:
        last = int(DIGEST_STAMP.read_text())
    except Exception:
        last = 0
    if _now() - last < 7 * 86400:
        return
    applied = list(_dir("applied").glob("*.json"))
    recent = [json.loads(p.read_text()) for p in applied if p.stat().st_mtime > _now() - 7 * 86400]
    merges = sum(1 for r in recent if r["op"] in ("merge_apply",))
    skills = sum(1 for r in recent if r["op"] == "skill_install")
    pend = len(list(_dir("pending").glob("*.json")))
    exp = len([1 for p in _dir("expired").glob("*.json") if p.stat().st_mtime > _now() - 7 * 86400])
    notify(f"🧠 engram weekly digest\napplied: {len(recent)} ops ({merges} merges, {skills} skills)\n"
           f"pending your approval: {pend}\nexpired/dropped: {exp}\n"
           f"quarantine backups: {len(list((MEM/'.quarantine').glob('merge-*')))}")
    DIGEST_STAMP.write_text(str(_now()))


PIPELINE_LOG = HOME / ".claude" / "logs" / "pipeline.log"
ACTIVITY_STAMP = Q / ".last_activity"


def activity_summary():
    """Summarize the LAST unattended maintenance run (harvest + graduate) from
    pipeline.log and DM it — so every autonomous memory action is visible on Telegram,
    not just the ones needing approval. Dedups on the run's start-timestamp."""
    import re
    try:
        text = PIPELINE_LOG.read_text(errors="ignore")
    except Exception:
        return
    starts = [m for m in re.finditer(r"^\[([^\]]+)\] pipeline start", text, re.M)]
    if not starts:
        return
    run = text[starts[-1].start():]
    run_id = starts[-1].group(1)
    try:
        if ACTIVITY_STAMP.read_text().strip() == run_id:
            return                                   # already notified this run
    except Exception:
        pass
    def grab(rx, default="0"):
        m = re.search(rx, run)
        return m.group(1) if m else default
    transcripts = grab(r"transcripts processed:\s*(\d+)")
    staged = grab(r"candidates staged:\s*(\d+)")
    prov = grab(r"by provenance:\s*(\{[^}]*\})", "{}")
    decisions = grab(r"processed \d+ staged candidate\(s\):\s*(\{[^}]*\})", "{}")
    errs = len(re.findall(r"\bERROR\b", run))
    ACTIVITY_STAMP.write_text(run_id)
    # Log ACTIVITY, not idle proof-of-life: skip a run that did nothing and errored
    # nothing (avoids 4 empty pings/day). Anything harvested/graduated or any error -> report.
    if transcripts == "0" and staged == "0" and decisions in ("{}", "0") and errs == 0:
        return
    notify(f"🧠 engram run @ {run_id}\nharvested: {transcripts} transcript(s) · staged {staged} {prov}\n"
           f"graduated/held: {decisions}\nerrors: {errs}")


def main():
    a = sys.argv[1:]
    if "--notify-suspect" in a:
        notify_suspect(a[a.index("--notify-suspect") + 1]); return
    if "--activity" in a:
        activity_summary(); return
    if "--digest" in a:
        DIGEST_STAMP.write_text("0"); maybe_digest(); return
    if "--sweep" in a:
        print(f"probation purged {sweep()} merge backup(s) to .trash"); return
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
