#!/usr/bin/env python3
"""engram-daemon.py — the always-on engram supervisor.

Runs the memory maintenance pipeline + graph sync + health on independent
cadences. Two ways to run:
  systemd (host/ollama):   a .timer fires `engram-daemon.py --once` periodically.
  container (claude-only): `engram-daemon.py --loop` runs forever (docker-compose.yml).

Each task has its own interval; `--once` runs only the tasks that are DUE (last-run
times in daemon_state.json), so a single frequent timer yields per-task cadences.

Safety: respects `local_enabled` and the dry-run apply-gates in engram.yaml — the
daemon nudges and prepares; a human approves mutations. Apply-gates ship OFF.

Tasks + default intervals (s), override via engram.yaml `daemon.intervals`:
  health      300     backend (ollama|claude) + Neo4j reachability -> log
  graph       1800    graph_sync.py --insert  (new memories -> graph)
  maintenance 21600   memory_fixate_cron.sh || memory_pipeline.sh  (light pass + pipeline, gated)
  export      86400   graph_sync.py --export --verify  (round-trip drift report)
  reconcile   86400   graph_sync.py --reconcile  (superseded-fact report)

Paths are resolved from env (the installer / container set these):
  ENGRAM_BIN   engine dir (default ~/.claude)        ENGRAM_GRAPH  graph dir (default $ENGRAM_BIN/graph)
  ENGRAM_LOG_DIR (default ~/.claude/logs)            NEO4J_URI     (default bolt://127.0.0.1:7687)
"""
import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import urlparse

HERE = Path(__file__).resolve().parent
HOME = Path.home()
ENGRAM_BIN = Path(os.environ.get("ENGRAM_BIN", HOME / ".claude"))
ENGRAM_GRAPH = Path(os.environ.get("ENGRAM_GRAPH", ENGRAM_BIN / "graph"))
ENGRAM_VECTOR = Path(os.environ.get("ENGRAM_VECTOR", ENGRAM_BIN / "vector"))
LOG_DIR = Path(os.environ.get("ENGRAM_LOG_DIR", HOME / ".claude" / "logs"))
STATE = Path(os.environ.get("ENGRAM_DAEMON_STATE", LOG_DIR / "daemon_state.json"))

sys.path.insert(0, str(ENGRAM_BIN))
try:
    import memory_ai
except Exception:
    memory_ai = None

# Cadences reflect each stage's real time-constant (Fable 2026-07-12):
#   harvest = ENCODE  -> frequent (watermark makes it delta-cost; idle-grace skips
#             active chats so it only pays tokens on FINISHED sessions).
#   maintenance = fixate SCORE + light_pass -> nightly (signals move over days).
#   distill (LLM) is gated WEEKLY inside memory_fixate_cron.sh.
DEFAULT_INTERVALS = {"health": 300, "approvals": 300, "harvest": 3600,
                     "graph": 1800, "vector": 1800, "maintenance": 86400,
                     "curate": 604800, "export": 86400, "reconcile": 86400}
ORDER = ["health", "approvals", "harvest", "graph", "vector", "maintenance",
         "curate", "export", "reconcile"]


def cfg():
    return memory_ai.load() if memory_ai else {}

def intervals():
    iv = dict(DEFAULT_INTERVALS)
    iv.update((cfg().get("daemon", {}) or {}).get("intervals", {}) or {})
    return iv

def log(msg: str):
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    line = f"[{time.strftime('%Y-%m-%dT%H:%M:%S')}] {msg}"
    print(line, flush=True)
    try:
        with open(LOG_DIR / "daemon.log", "a") as f:
            f.write(line + "\n")
    except Exception:
        pass

def load_state() -> dict:
    try:
        return json.loads(STATE.read_text())
    except Exception:
        return {}

def save_state(s: dict):
    STATE.parent.mkdir(parents=True, exist_ok=True)
    STATE.write_text(json.dumps(s, indent=1))

def _run(cmd, timeout=3600) -> int:
    log("run: " + " ".join(str(c) for c in cmd))
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        if r.stdout.strip():
            log(r.stdout.strip()[-2000:])
        if r.returncode:
            log(f"  exit {r.returncode}: {(r.stderr or '')[-500:]}")
        return r.returncode
    except Exception as e:
        log(f"  ERROR: {e}")
        return 1

def _neo4j_uri() -> str:
    """NEO4J_URI env override > engram.yaml graph.neo4j_uri > loopback. Mirrors
    mg_config._neo4j_uri so the daemon probes the SAME Neo4j the graph code uses."""
    return (os.environ.get("NEO4J_URI")
            or (cfg().get("graph", {}) or {}).get("neo4j_uri")
            or "bolt://127.0.0.1:7687")

def _neo4j_up() -> bool:
    p = urlparse(_neo4j_uri())
    try:
        with socket.create_connection((p.hostname or "127.0.0.1", p.port or 7687), timeout=3):
            return True
    except Exception:
        return False

def _vector_enabled() -> bool:
    return bool(memory_ai and memory_ai.vector_enabled(cfg()))

def _qdrant_up() -> bool:
    url = (cfg().get("vector_store", {}) or {}).get("url", "http://127.0.0.1:6333")
    p = urlparse(url)
    try:
        with socket.create_connection((p.hostname or "127.0.0.1", p.port or 6333), timeout=3):
            return True
    except Exception:
        return False

def _vector_python() -> str:
    """Python that can import qdrant-client: the vector venv if present, else this one."""
    cand = os.environ.get("ENGRAM_VECTOR_PYTHON") or str(ENGRAM_VECTOR / "venv" / "bin" / "python")
    return cand if Path(cand).exists() else sys.executable

def _maintenance_script():
    for c in (os.environ.get("ENGRAM_MAINTENANCE"),
              ENGRAM_BIN / "memory_fixate_cron.sh", HERE / "memory_fixate_cron.sh",
              ENGRAM_BIN / "memory_pipeline.sh", HERE / "memory_pipeline.sh"):
        if c and Path(c).exists():
            return Path(c)
    return None


# ---- tasks ----------------------------------------------------------------
def task_health():
    h = {}
    try:
        sys.path.insert(0, str(ENGRAM_BIN))
        import engram_llm
        h = engram_llm.health(cfg())
    except Exception as e:
        h = {"error": str(e)}
    h["neo4j"] = _neo4j_up()
    if _vector_enabled():
        h["qdrant"] = _qdrant_up()
    log(f"health: {json.dumps(h)}")

def task_graph():
    if not _neo4j_up():
        log("graph: Neo4j down — skipping insert")
        return False
    _run([sys.executable, str(ENGRAM_GRAPH / "graph_sync.py"), "--insert"])

def task_vector():
    if not _vector_enabled():
        return  # optional + off -> pure-markdown; nothing to do
    if not _qdrant_up():
        log("vector: Qdrant down — skipping insert")
        return False
    _run([_vector_python(), str(ENGRAM_VECTOR / "vector_sync.py"), "--insert"])

def _generate_available() -> bool:
    """Can the configured backend (or its fallback) actually generate right now?
    Gate the LLM pipeline on this so a down backend DEFERS cleanly (skip + one log
    line) instead of spraying per-transcript failures and stalling silently."""
    try:
        sys.path.insert(0, str(ENGRAM_BIN))
        import engram_llm
        return bool(engram_llm.health(cfg()).get("generate"))
    except Exception:
        return False

def task_harvest():
    """ENCODE (frequent): harvest just-finished chats -> graduate. The LLM is needed,
    so DEFER cleanly when no backend can generate (watermark preserved, nothing lost)."""
    if not _generate_available():
        log("harvest: no generate backend — deferring encode (watermark preserved)")
        return False
    pipe = None
    for c in (ENGRAM_BIN / "memory_pipeline.sh", HERE / "memory_pipeline.sh"):
        if c.exists():
            pipe = c; break
    if not pipe:
        log("harvest: memory_pipeline.sh not found"); return False
    _run(["bash", str(pipe)])
    if ((cfg().get("telegram") or {}).get("activity_log")):
        gate = ENGRAM_BIN / "engram_telegram_gate.py"
        if gate.exists():
            _run([sys.executable, str(gate), "--activity"], timeout=30)


def task_maintenance():
    # FIXATE (nightly): score + quarantine + light_pass + weekly-gated distill. Encode
    # (harvest->graduate) is now its own frequent task, NOT run here. Deterministic
    # scoring needs no LLM, but distill does — the cron gates that internally.
    sh = _maintenance_script()
    if sh:
        _run(["bash", str(sh)])   # activity notify lives in task_harvest (encode is where the news is)
    else:
        log("maintenance: no maintenance script found (memory_fixate_cron.sh / memory_pipeline.sh)")
        return False

def task_curate():
    """CONSOLIDATE (weekly): auto-merge near-duplicate memories. Same slow time-constant
    as distill — near-dups accrue as new memories graduate over days, and each merge is
    Codex-reviewed (needs generate) + reversible. No-op unless auto_curate.enabled=true.
    Uses the vector venv (qdrant-client) for the ANN clustering."""
    if not (cfg().get("auto_curate") or {}).get("enabled"):
        return
    if not _generate_available():
        log("curate: no generate backend — deferring auto-consolidation")
        return False
    ac = ENGRAM_BIN / "memory_auto_curate.py"
    if ac.exists():
        _run([_vector_python(), str(ac), "--apply"])


def task_export():
    if not _neo4j_up():
        return False
    _run([sys.executable, str(ENGRAM_GRAPH / "graph_sync.py"), "--export", "--verify"])

def task_reconcile():
    if not _neo4j_up():
        return False
    _run([sys.executable, str(ENGRAM_GRAPH / "graph_sync.py"), "--reconcile"])

def task_approvals():
    """Process the async human-approval queue: consume Telegram callbacks, expire
    stale proposals (drop), apply approved ops. No-op (cheap) when the queue is
    empty or no Telegram token is set."""
    gate = ENGRAM_BIN / "engram_telegram_gate.py"
    if not gate.exists():
        return
    _run([sys.executable, str(gate), "--poll"], timeout=60)

TASKS = {"health": task_health, "approvals": task_approvals, "harvest": task_harvest,
         "graph": task_graph, "vector": task_vector, "maintenance": task_maintenance,
         "curate": task_curate, "export": task_export, "reconcile": task_reconcile}


def tick(force=None):
    iv, st, now = intervals(), load_state(), int(time.time())
    enabled = memory_ai.local_enabled(cfg()) if memory_ai else True
    if force:
        order = [force]
    elif not enabled:
        order = ["health"]
        log("local_enabled=false — running health only (no automated memory work)")
    else:
        order = ORDER
    for t in order:
        fn = TASKS.get(t)
        if not fn:
            log(f"unknown task: {t}")
            continue
        if force or (now - st.get(t, 0)) >= iv[t]:
            # A task returns False when it DEFERRED — a dependency was down (Neo4j,
            # Qdrant, generate backend) so it did no work. Don't stamp those: the
            # stamp means "this ran", and stamping a no-op makes a transient outage
            # cost a full interval (a daily task skipped at 21:04 with Qdrant down
            # would not retry until 21:04 tomorrow). Anything else stamps as before.
            if fn() is not False:
                st[t] = now
    save_state(st)


def main():
    a = sys.argv[1:]
    if "--force" in a:
        tick(force=a[a.index("--force") + 1])
        return
    if "--loop" in a:
        base = int(a[a.index("--tick") + 1]) if "--tick" in a else 60
        log(f"engram-daemon loop start (base tick {base}s; backend={cfg().get('backend','ollama')})")
        while True:
            try:
                tick()
            except Exception as e:
                log(f"tick ERROR: {e}")
            time.sleep(base)
    tick()  # default: --once


if __name__ == "__main__":
    main()
