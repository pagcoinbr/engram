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


if __name__ == "__main__":
    test_deferred_task_is_not_stamped(); test_real_tasks_defer_when_deps_down()
