---
name: verify before declaring done
description: Never claim a change works until it's been run/tested; show the actual output.
type: feedback
---

## Summary
Don't report a task as complete until the change has actually been exercised — run the
code, run the tests, observe the behavior — and show the real output. A claim of "done"
without evidence has burned us before.

**Why:** Untested "done" claims led to a broken deploy that passed code review but failed
at runtime; the assertion of completeness hid the gap.

**How to apply:** Before saying a fix works: run it (or the test), paste the output. If a
step was skipped, say so. If tests fail, report the failure plainly. Related:
[[acme-api deploy procedure]], [[developer profile]].
