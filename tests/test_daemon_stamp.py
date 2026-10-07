#!/usr/bin/env python3
"""Deferred tasks must NOT stamp daemon_state.json. A task whose dependency is down
(Neo4j/Qdrant/generate backend) returns False and did no work — stamping it would
make a transient outage cost a full interval before the next retry."""
import sys, types, importlib.util, json, tempfile, os
from pathlib import Path


def _load(state_path, intervals):
    sys.modules["memory_ai"] = types.ModuleType("memory_ai")
    sys.modules["memory_ai"].load = lambda: {"daemon": {"intervals": intervals}}
    sys.modules["memory_ai"].local_enabled = lambda c: True
    sys.modules["memory_ai"].vector_enabled = lambda c: True
    os.environ["ENGRAM_DAEMON_STATE"] = str(state_path)
    sp = importlib.util.spec_from_file_location(
        "engd", Path(__file__).resolve().parent.parent / "daemon" / "engram-daemon.py")
    m = importlib.util.module_from_spec(sp); sp.loader.exec_module(m)
    return m


def test_deferred_task_is_not_stamped():
    d = tempfile.mkdtemp()
    state = Path(d) / "daemon_state.json"
    m = _load(state, {"vector": 86400})
    m.STATE = state
    m.ORDER = ["vector"]
    m.TASKS = {"vector": lambda: False}          # dependency down -> deferred
    m.tick()
    assert "vector" not in json.loads(state.read_text()), \
        "deferred task stamped — a down dependency would cost a full interval"

    m.TASKS = {"vector": lambda: None}           # ran normally
    m.tick()
    assert "vector" in json.loads(state.read_text()), "completed task was not stamped"

    import shutil; shutil.rmtree(d)
    print("ok — deferred tasks retry next tick, completed tasks wait out the interval")


def test_real_tasks_defer_when_deps_down():
    """The availability gates in the real task functions must return False, not None."""
    d = tempfile.mkdtemp()
    m = _load(Path(d) / "s.json", {})
    m._neo4j_up = lambda: False
    m._qdrant_up = lambda: False
    m._vector_enabled = lambda: True
    m._generate_available = lambda: False
    for name in ("graph", "vector", "export", "reconcile", "harvest"):
        assert m.TASKS[name]() is False, f"task_{name} must return False when its dependency is down"
    import shutil; shutil.rmtree(d)
    print("ok — graph/vector/export/reconcile/harvest all defer explicitly")


def test_graphiti_compat_uses_legacy_jobs():
    """The selected reader and writer must address the same Graphiti index.

    Parameterised over whether the `engram-graph-sync` wrapper is installed,
    because it is only a wrapper: it resolves ENGRAM_CONFIG/ENGRAM_GRAPH and hands
    them to graph_sync.py, so both routes drive the same Graphiti writer. An
    earlier version asserted that `graph_sync.py` appeared in the command, which
    made the result depend on which binaries happened to be installed on the host
    running the tests — it passed until a real install added the wrapper.
    """
    import shutil
    for wrapper in (None, "/opt/engram/rust/engram-graph-sync"):
        d = tempfile.mkdtemp()
        m = _load(Path(d) / "s.json", {})
        m.cfg = lambda: {"graph": {"backend": "graphiti_compat"}}
        m._neo4j_up = lambda: True
        m._rust = lambda name, _w=wrapper: _w if name == "engram-graph-sync" else None
        seen = []
        m._run = lambda command, **_: seen.append(command) or 0
        m.task_graph(); m.task_export(); m.task_reconcile()
        assert len(seen) == 3, f"wrapper={wrapper}: expected three jobs, got {seen}"
        for command in seen:
            target = " ".join(str(c) for c in command)
            assert ("graph_sync.py" in target) or ("engram-graph-sync" in target), \
                f"wrapper={wrapper}: not the Graphiti path: {command}"
            # The native writer must never run while Graphiti is the reader.
            assert "native-graph-sync" not in target, \
                f"wrapper={wrapper}: native writer under a Graphiti reader: {command}"
        shutil.rmtree(d)
    print("ok — graphiti compatibility keeps the legacy writer/export/reconcile path")


if __name__ == "__main__":
    test_deferred_task_is_not_stamped(); test_real_tasks_defer_when_deps_down(); test_graphiti_compat_uses_legacy_jobs()
