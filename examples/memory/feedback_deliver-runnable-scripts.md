---
name: deliver runnable scripts
description: For multi-step server ops, write an idempotent script on the target box instead of pasting command blocks.
type: feedback
---

## Summary
For any multi-step or privileged operation, write an idempotent, dry-run-first script
file on the target host and have the operator run it — don't paste long multi-line
command blocks into chat (terminals mangle them).

**Why:** Pasted heredocs/indented blocks break at the continuation prompt; scripts are
re-runnable and reviewable.

**How to apply:** Write the script with `set -euo pipefail`, a `--dry-run` path for any
mutation, a syntax check (`bash -n`), and self-verifying assertions. Tell the operator
the exact command + which host. Related: [[verify before declaring done]].
