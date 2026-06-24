#!/usr/bin/env python3
"""memory_transcript_retention.py — bound the transcript store's disk growth.

The session transcript dir grows unbounded (≈156k files / 5.2 GB observed). Since
memory_score.py now caches each transcript's token-set and treats that cache as the
authoritative corpus for frequency (counting entries even when the raw .jsonl is
gone), old raw transcripts can be archived/compressed WITHOUT distorting the
frequency signal.

Safety:
  * Dry-run by default (prints the plan); pass --apply to act.
  * Only archives transcripts ALREADY represented in the freq cache — so an
    un-scored transcript is never moved out from under a future first scan.
  * MOVES (never deletes) to <store>/archive/ and gzips there → ~10x smaller,
    fully reversible (gunzip + mv back). archive/ is a subdir, so the
    non-recursive *.jsonl glob (scorer + harvest) ignores it.

Usage:
  memory_transcript_retention.py                 # dry-run, default 120-day cutoff
  memory_transcript_retention.py --days 90 --apply
  memory_transcript_retention.py --apply --no-gzip
Env: CLAUDE_MEMORY_SLUG, MEM_RETENTION_DAYS
"""
from __future__ import annotations
import gzip, os, shutil, sys, time, json
from pathlib import Path

HOME = Path.home()
SLUG = os.environ.get("CLAUDE_MEMORY_SLUG") or str(HOME).replace("/", "-")
PROJ_DIR = HOME / ".claude" / "projects" / SLUG
MEM_DIR  = PROJ_DIR / "memory"
CACHE    = MEM_DIR / ".freq_cache.json.gz"
ARCHIVE  = PROJ_DIR / "archive"

def cached_names() -> set[str]:
    try:
        with gzip.open(CACHE, "rt", encoding="utf-8") as fh:
            return set(json.load(fh).get("transcripts", {}))
    except Exception:
        return set()

def main():
    args = sys.argv[1:]
    apply = "--apply" in args
    gz = "--no-gzip" not in args
    days = int(os.environ.get("MEM_RETENTION_DAYS", "120"))
    if "--days" in args:
        days = int(args[args.index("--days") + 1])

    if not PROJ_DIR.is_dir():
        print(f"[retention] no project dir at {PROJ_DIR}", file=sys.stderr); sys.exit(1)

    cached = cached_names()
    if not cached:
        print("[retention] freq cache empty/missing — run memory_score.py first so "
              "transcripts are represented before archiving. Aborting.", file=sys.stderr)
        sys.exit(1)

    cutoff = time.time() - days * 86400
    candidates, skipped_uncached, bytes_freed = [], 0, 0
    for p in PROJ_DIR.glob("*.jsonl"):
        try:
            st = p.stat()
        except OSError:
            continue
        if st.st_mtime >= cutoff:
            continue
        if p.name not in cached:              # not yet scored → never archive
            skipped_uncached += 1
            continue
        candidates.append((p, st.st_size))
        bytes_freed += st.st_size

    mb = bytes_freed / 1e6
    mode = "APPLY" if apply else "DRY-RUN"
    print(f"[retention] {mode}: {len(candidates)} transcripts older than {days}d "
          f"({mb:.0f} MB raw){' → gzip' if gz else ''} → {ARCHIVE}")
    if skipped_uncached:
        print(f"[retention] skipped {skipped_uncached} old-but-uncached transcripts "
              f"(run memory_score.py to cache them first)")
    if not candidates:
        return
    if not apply:
        print("[retention] dry-run; re-run with --apply to move+compress. "
              f"After applying, run: python3 ~/.claude/memory_score.py  (rebuilds nothing — "
              f"archived tokens already cached).")
        return

    ARCHIVE.mkdir(exist_ok=True)
    moved = 0
    for p, _sz in candidates:
        try:
            if gz:
                dest = ARCHIVE / (p.name + ".gz")
                with open(p, "rb") as fi, gzip.open(dest, "wb") as fo:
                    shutil.copyfileobj(fi, fo)
                p.unlink()
            else:
                shutil.move(str(p), str(ARCHIVE / p.name))
            moved += 1
        except Exception as e:
            print(f"[retention] WARN: {p.name}: {e}", file=sys.stderr)
    print(f"[retention] archived {moved} transcripts (~{mb:.0f} MB reclaimed once "
          f"compressed). Frequency is unaffected (cache is authoritative).")

if __name__ == "__main__":
    main()
