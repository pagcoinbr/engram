#!/usr/bin/env python3
"""Verify a reformatted memory preserved every original body content-line.

Usage: memory_reformat_verify.py <original.md> <reformatted.md>

Strips YAML frontmatter from both, then checks that every non-empty,
whitespace-stripped line in the ORIGINAL body still appears (at least as many
times) in the REFORMATTED body. The reformat is only allowed to ADD lines
(## Summary prose, ## Index entries, ## N. Title headers); it must never drop
or alter an existing content line. Exit 0 = OK, 1 = missing/altered content.
"""
import sys
import re
import collections


def body(path):
    with open(path, encoding="utf-8") as f:
        t = f.read()
    m = re.match(r"^---\n.*?\n---\n", t, re.S)
    if m:
        t = t[m.end():]
    return t


def content_lines(s):
    return [ln.rstrip() for ln in s.split("\n")]


def main():
    orig, new = sys.argv[1], sys.argv[2]
    old_b = content_lines(body(orig))
    new_b = content_lines(body(new))
    avail = collections.Counter(ln.strip() for ln in new_b if ln.strip())
    missing = []
    for ln in old_b:
        s = ln.strip()
        if not s:
            continue
        if avail[s] > 0:
            avail[s] -= 1
        else:
            missing.append(ln)
    total = sum(1 for ln in old_b if ln.strip())
    if missing:
        print("FAIL: %d/%d original body content-lines missing or altered:" % (len(missing), total))
        for ln in missing[:60]:
            print("  MISSING | " + ln)
        return 1
    # Sanity: reformatted must actually have the new structure.
    nb = "\n".join(new_b)
    if "## Summary" not in nb or "## Index" not in nb:
        print("FAIL: reformatted body missing ## Summary or ## Index")
        return 1
    print("OK: all %d original body content-lines preserved; Summary+Index present" % total)
    return 0


if __name__ == "__main__":
    sys.exit(main())
