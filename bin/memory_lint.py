#!/usr/bin/env python3
"""memory_lint.py — structural linter for the memory store (report-only).

Memories drift structurally over many edits/distillations: an Index entry left
pointing at a section that was renamed/removed, a section with no Index entry, a
duplicated header (an un-numbered "## Title" left beside a numbered "## N. Title"),
a dangling [[wikilink]], or missing frontmatter. None of these corrupt data, so
the scoring/grading passes don't catch them — but they make a file harder to grasp
and the Index untrustworthy. This linter surfaces them in the weekly report.

It NEVER mutates anything. Exit code is always 0 (advisory); use --strict to exit
1 when issues are found (e.g. for a pre-commit gate).

Usage:
  memory_lint.py            # markdown report
  memory_lint.py --json     # machine-readable
  memory_lint.py --strict   # exit 1 if any issue
Env: CLAUDE_MEMORY_SLUG
"""
from __future__ import annotations
import json, os, re, sys
from pathlib import Path

HOME = Path.home()
SLUG = os.environ.get("CLAUDE_MEMORY_SLUG") or str(HOME).replace("/", "-")
MEM_DIR = HOME / ".claude" / "projects" / SLUG / "memory"

FM_RE      = re.compile(r"^---\n(.*?)\n---", re.DOTALL)
NAME_RE    = re.compile(r"(?m)^\s*name:\s*(.+?)\s*$")
DESC_RE    = re.compile(r"(?m)^\s*description:\s*(.+?)\s*$")
HEADER_RE  = re.compile(r"(?m)^##\s+(.+?)\s*$")
NUM_HDR_RE = re.compile(r"^(\d+[a-z]?)\.\s+(.*)$")          # "2. Title" / "0b. Title"
INDEX_ITEM = re.compile(r"^\s*(\d+[a-z]?)[.)]\s+(.+?)\s*$")  # "1. Title" inside Index
WIKILINK   = re.compile(r"\[\[([^\]]+)\]\]")

def frontmatter(text: str) -> dict:
    m = FM_RE.match(text)
    if not m:
        return {}
    fm = {}
    nm = NAME_RE.search(m.group(1));  fm["name"] = nm.group(1).strip().strip('"') if nm else ""
    dm = DESC_RE.search(m.group(1));  fm["description"] = dm.group(1).strip().strip('"') if dm else ""
    return fm

def index_numbers(text: str) -> set[str]:
    """The numbers listed under a '## Index' block (until the next '## ')."""
    nums = set()
    lines = text.splitlines()
    in_index = False
    for ln in lines:
        if re.match(r"^##\s+Index\b", ln, re.IGNORECASE):
            in_index = True; continue
        if in_index:
            if ln.startswith("## "):
                break
            mi = INDEX_ITEM.match(ln)
            if mi:
                nums.add(mi.group(1).lower())
    return nums

# Real memory slugs are kebab/snake identifiers. Wikilink targets that aren't
# slug-shaped (contain spaces, dots/ellipsis, slashes) are prose placeholders like
# [[...path]], not references — don't flag them.
SLUG_RE = re.compile(r"^[a-z0-9][a-z0-9_-]+$")

def lint_file(path: Path, known: set[str]) -> tuple[list[str], list[str]]:
    """Return (structural_issues, dangling_links). Structural issues are
    actionable; dangling links are advisory (the format allows forward-refs to
    memories not written yet), so they're reported separately."""
    text = path.read_text(errors="ignore")
    issues = []
    fm = frontmatter(text)

    if not fm:
        issues.append("missing frontmatter block")
    else:
        if not fm.get("name"):
            issues.append("frontmatter: empty/missing `name`")
        if not fm.get("description"):
            issues.append("frontmatter: empty/missing `description`")

    # headers
    headers = HEADER_RE.findall(text)
    sec_nums, sec_titles, plain_titles = set(), {}, []
    seen_headers = {}
    for h in headers:
        seen_headers[h] = seen_headers.get(h, 0) + 1
        mh = NUM_HDR_RE.match(h)
        if mh:
            sec_nums.add(mh.group(1).lower())
            sec_titles[mh.group(1).lower()] = mh.group(2).strip().lower()
        elif h.strip().lower() not in ("summary", "index"):
            plain_titles.append(h.strip().lower())

    # duplicate headers (verbatim)
    for h, n in seen_headers.items():
        if n > 1:
            issues.append(f"duplicate header `## {h}` (×{n})")
    # un-numbered header duplicating a numbered section's title (the classic drift)
    for pt in plain_titles:
        if pt in sec_titles.values():
            issues.append(f"un-numbered header `## {pt}` duplicates a numbered section")

    # index <-> section coverage (only when both an Index and numbered sections exist)
    idx = index_numbers(text)
    if idx and sec_nums:
        missing_sec = sorted(idx - sec_nums)
        orphan_sec  = sorted(sec_nums - idx)
        if missing_sec:
            issues.append(f"Index lists {missing_sec} with no matching section")
        if orphan_sec:
            issues.append(f"sections {orphan_sec} missing from the Index")
    elif idx and not sec_nums:
        issues.append("has an Index but no numbered `## N.` sections")

    # wikilinks resolve to a known memory (by name slug or filename stem)
    dangling = []
    for target in set(WIKILINK.findall(text)):
        t = target.strip().lower()
        if not SLUG_RE.match(t):      # prose placeholder, not a reference
            continue
        if t not in known:
            dangling.append(target.strip())

    return issues, sorted(dangling)

def main():
    args = sys.argv[1:]
    as_json = "--json" in args
    strict  = "--strict" in args
    if not MEM_DIR.is_dir():
        print(f"[lint] no memory dir at {MEM_DIR}", file=sys.stderr); sys.exit(0)

    files = sorted(p for p in MEM_DIR.glob("*.md") if p.name != "MEMORY.md")
    # known link targets: frontmatter name slug AND filename stem (both conventions)
    known = set()
    fm_by_file = {}
    for p in files:
        fm = frontmatter(p.read_text(errors="ignore"))
        fm_by_file[p] = fm
        known.add(p.stem.lower())
        if fm.get("name"):
            known.add(fm["name"].strip().lower())

    report = {}      # structural issues (actionable)
    links = {}       # dangling wikilinks (advisory)
    for p in files:
        iss, dang = lint_file(p, known)
        if iss:
            report[p.name] = iss
        if dang:
            links[p.name] = dang

    if as_json:
        print(json.dumps({"store": str(MEM_DIR), "files_checked": len(files),
                          "files_with_structural_issues": len(report),
                          "structural_issues": report,
                          "dangling_wikilinks": links}, indent=1))
    else:
        total = sum(len(v) for v in report.values())
        if not report:
            print(f"_Structural lint: {len(files)} memories, no structural issues._")
        else:
            print(f"### Structural lint — {len(report)}/{len(files)} files, {total} structural issue(s)\n")
            for name in sorted(report):
                print(f"- **{name}**")
                for i in report[name]:
                    print(f"  - {i}")
        # dangling links: advisory, compact (unique targets + where), since the
        # format permits forward-refs to not-yet-written memories.
        if links:
            uniq = sorted({t for v in links.values() for t in v})
            print(f"\n_Advisory: {len(uniq)} unique dangling wikilink target(s) across "
                  f"{len(links)} file(s) — may be forward-refs or stale (merged-away) links._")
    sys.exit(1 if (strict and report) else 0)

if __name__ == "__main__":
    main()
