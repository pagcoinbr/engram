#!/usr/bin/env python3
"""engram_backup.py — encrypted off-host backups of the authoritative data.

engram's source of truth is small and local: the `.md` memory stores
(~4 MB) and the Obsidian vaults (~tens of KB). Everything else — Qdrant, Neo4j —
is a rebuildable index over that set. So a backup of the `.md` stores, the
vaults, and the two config files needed to stand the system back up is a
complete restore, and it is tiny. Until now it had no off-host copy.

This wraps **restic** (https://restic.net), chosen for one property above all:
client-side AES-256 encryption, always on. The data carries infrastructure
detail, and the restore set includes `graph/.env` (the Neo4j password), so the
backup store — Backblaze B2 or any S3-compatible bucket — must never see
plaintext. restic also gives content-addressed dedup (a daily snapshot of an
unchanged 4 MB set is nearly free), snapshot history, retention
(`forget --prune`), and integrity checking (`check`).

Operator-runnable AND daemon-called, like graph_sync.py. Subcommands:

    init       create the repository (idempotent)
    backup     snapshot + prune; optionally a periodic integrity check
    snapshots  list snapshots, newest first
    restore    restore a snapshot to a STAGING directory (never over live data)
    check      verify repository integrity
    status     is it configured, enabled, reachable; what is the latest snapshot

SECRETS come from the environment ONLY, never from engram.yaml and never
written to disk or the log:

    ENGRAM_BACKUP_PASSWORD     restic repository password (LOSE THIS = LOSE THE
                               BACKUPS; that is what client-side encryption means)
    ENGRAM_BACKUP_S3_KEY_ID    S3 / B2 application key id
    ENGRAM_BACKUP_S3_KEY       S3 / B2 application key
    ENGRAM_BACKUP_REPO         optional explicit restic repo string; overrides
                               the endpoint/bucket composed from engram.yaml
                               (also how the test points at a local /tmp repo)

The restic child process is the only thing that ever sees those values; they are
passed through a private env dict, not exported into this process's environment.
"""
import json
import os
import subprocess
import sys
import time
from pathlib import Path

HOME = Path(os.environ.get("ENGRAM_BIN") or (Path.home() / ".claude"))
CONFIG_PATH = Path(os.environ.get("ENGRAM_CONFIG") or (HOME / "engram.yaml"))
GRAPH_DIR = Path(os.environ.get("ENGRAM_GRAPH") or (HOME / "graph"))
# Our own tiny cadence state, deliberately SEPARATE from the daemon's
# daemon_state.json: the daemon rewrites that file wholesale each tick, so a key
# we wrote inside a task would be clobbered. The periodic-check timer lives here.
STATE_PATH = Path(os.environ.get("ENGRAM_LOG_DIR") or (HOME / "logs")) / "backup_state.json"

# bin/ is on sys.path for engram_secrets after install (flat ~/.claude) and in
# the repo (next to this file).
sys.path.insert(0, str(Path(__file__).resolve().parent))
if str(HOME) not in sys.path:
    sys.path.append(str(HOME))

try:
    import engram_secrets
except Exception:  # redaction is defence-in-depth; never let its absence break a backup
    engram_secrets = None


def _redact(text: str) -> str:
    if not text:
        return text
    if engram_secrets is not None:
        try:
            return engram_secrets.redact(text)[0]
        except Exception:
            pass
    return text


def _load_config() -> dict:
    """The `backup:` block from engram.yaml, plus the `tenants:` block (for vault
    paths). A missing/unreadable config yields an empty dict — `enabled` then
    defaults false and the daemon simply does nothing."""
    try:
        import yaml
    except Exception:
        raise SystemExit(
            "engram_backup: PyYAML is required to read engram.yaml "
            "(pip install pyyaml, or run inside the engram venv)"
        )
    try:
        return yaml.safe_load(CONFIG_PATH.read_text()) or {}
    except FileNotFoundError:
        return {}
    except Exception as error:
        raise SystemExit(f"engram_backup: could not parse {CONFIG_PATH}: {error}")


def _backup_cfg(cfg: dict) -> dict:
    block = cfg.get("backup") or {}
    return block if isinstance(block, dict) else {}


def _repo(cfg: dict) -> str:
    """The restic repository string.

    An explicit ENGRAM_BACKUP_REPO wins — that is how an operator points at a
    non-S3 target and how the test uses a local /tmp repo. Otherwise it is an S3
    repo composed from the configured endpoint, bucket and prefix. restic's S3
    backend speaks to B2's S3-compatible endpoint unchanged."""
    explicit = (os.environ.get("ENGRAM_BACKUP_REPO") or "").strip()
    if explicit:
        return explicit
    b = _backup_cfg(cfg)
    endpoint = str(b.get("endpoint") or "").strip().rstrip("/")
    bucket = str(b.get("bucket") or "").strip().strip("/")
    prefix = str(b.get("prefix") or "engram").strip().strip("/")
    if not endpoint or not bucket:
        raise SystemExit(
            "engram_backup: no repository configured — set backup.endpoint and "
            "backup.bucket in engram.yaml, or ENGRAM_BACKUP_REPO"
        )
    # restic form: s3:https://<endpoint>/<bucket>/<prefix>
    return f"s3:https://{endpoint}/{bucket}/{prefix}"


def _restic_env(cfg: dict) -> dict:
    """A CHILD environment carrying the secrets — never this process's own.

    Built fresh from os.environ minus anything, plus the restic variables, and
    handed to subprocess via env=. The daemon that spawns us therefore never has
    RESTIC_PASSWORD or the S3 key in its own environment, only the restic child
    does."""
    password = (os.environ.get("ENGRAM_BACKUP_PASSWORD") or "").strip()
    if not password:
        raise SystemExit(
            "engram_backup: ENGRAM_BACKUP_PASSWORD is not set — it is the restic "
            "encryption password; set it in ~/.config/engram/daemon.env"
        )
    env = dict(os.environ)
    env["RESTIC_REPOSITORY"] = _repo(cfg)
    env["RESTIC_PASSWORD"] = password
    # S3 credentials, when the repo is an S3 one. A local/other repo leaves these
    # unset, which is correct.
    key_id = (os.environ.get("ENGRAM_BACKUP_S3_KEY_ID") or "").strip()
    key = (os.environ.get("ENGRAM_BACKUP_S3_KEY") or "").strip()
    if key_id:
        env["AWS_ACCESS_KEY_ID"] = key_id
    if key:
        env["AWS_SECRET_ACCESS_KEY"] = key
    return env


def _restic(cfg: dict, args, *, timeout=1800, check=True):
    """Run restic with the child env, returning (rc, stdout, stderr), both
    streams redacted before they are ever surfaced."""
    if not _which("restic"):
        raise SystemExit(
            "engram_backup: restic is not installed — install it (apt install "
            "restic, or download the static binary) and re-run"
        )
    proc = subprocess.run(
        ["restic", *args],
        env=_restic_env(cfg),
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    out, err = _redact(proc.stdout), _redact(proc.stderr)
    if check and proc.returncode != 0:
        # Surface the reason, redacted, without leaking the env.
        sys.stderr.write(err or out or f"restic exited {proc.returncode}\n")
    return proc.returncode, out, err


def _which(name: str) -> bool:
    from shutil import which

    return which(name) is not None


def _paths(cfg: dict):
    """The include set, as existing paths only (restic errors on a missing one)."""
    paths = []
    # 1. memory stores: projects/<slug>/memory
    projects = HOME / "projects"
    if projects.is_dir():
        for store in sorted(projects.glob("*/memory")):
            if store.is_dir():
                paths.append(store)
    # 2. vaults, from the tenants block (authoritative) — not a hardcoded /vaults
    if _backup_cfg(cfg).get("include_vaults", True):
        for vault in _vault_paths(cfg):
            if vault.is_dir():
                paths.append(vault)
    # 3. the two files a clean restore needs
    for extra in (CONFIG_PATH, GRAPH_DIR / ".env"):
        if extra.is_file():
            paths.append(extra)
    return paths


def _vault_paths(cfg: dict):
    tenants = cfg.get("tenants") or {}
    out = []
    if isinstance(tenants, dict):
        for entry in tenants.values():
            vault = str((entry or {}).get("vault") or "").strip()
            if vault:
                out.append(Path(vault))
    return out


def _state() -> dict:
    try:
        return json.loads(STATE_PATH.read_text())
    except Exception:
        return {}


def _save_state(state: dict):
    STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
    STATE_PATH.write_text(json.dumps(state, indent=1))


# --- subcommands -----------------------------------------------------------

def cmd_init(cfg, _args) -> int:
    rc, out, err = _restic(cfg, ["init"], check=False)
    if rc == 0:
        print("initialized backup repository")
        return 0
    # An already-initialized repo is success, not failure — init is idempotent.
    if "already" in (err + out).lower():
        print("backup repository already initialized")
        return 0
    sys.stderr.write(err or out)
    return rc


def cmd_backup(cfg, args) -> int:
    paths = _paths(cfg)
    if not paths:
        print("nothing to back up (no memory stores or vaults found)")
        return 0
    b = _backup_cfg(cfg)
    rc, out, _ = _restic(
        cfg,
        [
            "backup",
            "--tag", "engram",
            "--exclude", ".obsidian",
            "--exclude", ".trash",
            "--exclude", ".git",
            # SilverBullet's per-space state — auth file and the Runtime API's
            # browser profile (.chrome-data). Regenerated, not user data, and no
            # reason to carry a copy off-host. Both are dotfiles so the wiki
            # walker already ignores them for indexing; this is the backup side.
            "--exclude", ".silverbullet.auth.json",
            "--exclude", ".chrome-data",
            *[str(p) for p in paths],
        ],
        timeout=3600,
    )
    if rc != 0:
        return rc
    print(out.strip()[-1000:] or "backup complete")

    # Retention: prune to the keep policy. Cheap on a 4 MB repo, so every run.
    retention = b.get("retention") or {}
    keep = [
        "--keep-daily", str(int(retention.get("daily", 7))),
        "--keep-weekly", str(int(retention.get("weekly", 4))),
        "--keep-monthly", str(int(retention.get("monthly", 6))),
    ]
    frc, fout, _ = _restic(cfg, ["forget", "--tag", "engram", *keep, "--prune"], check=False)
    if frc == 0:
        print("retention applied")
    else:
        # A failed prune does not fail the backup — the snapshot is already safe.
        print("warning: retention/prune did not complete; the snapshot is saved")

    # Periodic integrity check, on its own timer in OUR state file so the daemon's
    # wholesale state rewrite cannot clobber it.
    every = int(args.get("check_every_days", b.get("check_every_days", 7)) or 0)
    if every > 0:
        state = _state()
        last = int(state.get("last_check", 0))
        if time.time() - last >= every * 86400:
            crc = cmd_check(cfg, {})
            if crc == 0:
                state["last_check"] = int(time.time())
                _save_state(state)
    return 0


def cmd_snapshots(cfg, _args) -> int:
    rc, out, _ = _restic(cfg, ["snapshots", "--tag", "engram"], check=True)
    if rc == 0:
        print(out.strip() or "no snapshots yet")
    return rc


def cmd_check(cfg, _args) -> int:
    rc, out, _ = _restic(cfg, ["check"], timeout=1800, check=True)
    if rc == 0:
        print("repository integrity OK")
    return rc


def cmd_restore(cfg, args) -> int:
    snapshot = args.get("snapshot") or "latest"
    target = args.get("target")
    if not target:
        # Stage, never clobber live data. The operator diffs and moves
        # deliberately — restoring memories in place is destructive.
        target = str(HOME / f"restore-{time.strftime('%Y%m%d-%H%M%S')}")
    rc, out, _ = _restic(
        cfg, ["restore", snapshot, "--target", target], timeout=3600, check=True
    )
    if rc == 0:
        print(f"restored snapshot {snapshot} to {target}")
        print("review it and move files into place yourself; nothing live was touched")
    return rc


def cmd_status(cfg, _args) -> int:
    b = _backup_cfg(cfg)
    enabled = bool(b.get("enabled"))
    print(f"enabled:   {enabled}")
    have_secret = bool((os.environ.get("ENGRAM_BACKUP_PASSWORD") or "").strip())
    print(f"password:  {'set' if have_secret else 'MISSING (ENGRAM_BACKUP_PASSWORD)'}")
    try:
        # Show the repo WITHOUT secrets — the repo string has endpoint+bucket, no keys.
        print(f"repo:      {_repo(cfg)}")
    except SystemExit as error:
        print(f"repo:      {error}")
        return 0
    paths = _paths(cfg)
    print(f"includes:  {len(paths)} path(s)")
    if have_secret and _which("restic"):
        rc, out, _ = _restic(cfg, ["snapshots", "--tag", "engram", "--latest", "1"], check=False)
        print("latest:    " + (out.strip().splitlines()[-1] if rc == 0 and out.strip() else "none / unreachable"))
    return 0


COMMANDS = {
    "init": cmd_init,
    "backup": cmd_backup,
    "snapshots": cmd_snapshots,
    "restore": cmd_restore,
    "check": cmd_check,
    "status": cmd_status,
}


def _parse(argv):
    if not argv or argv[0] not in COMMANDS:
        sys.stderr.write(
            "usage: engram_backup.py {init|backup|snapshots|restore|check|status} [options]\n"
        )
        raise SystemExit(2)
    command = argv[0]
    args = {}
    rest = argv[1:]
    i = 0
    while i < len(rest):
        token = rest[i]
        if not token.startswith("--"):
            i += 1
            continue
        key = token[2:].replace("-", "_")
        # A flag takes the next token as its value unless that token is itself a
        # flag (or absent), in which case it is a bare boolean flag.
        if i + 1 < len(rest) and not rest[i + 1].startswith("--"):
            args[key] = rest[i + 1]
            i += 2
        else:
            args[key] = "true"
            i += 1
    return command, args


def main() -> int:
    command, args = _parse(sys.argv[1:])
    cfg = _load_config()
    return COMMANDS[command](cfg, args)


if __name__ == "__main__":
    sys.exit(main())
