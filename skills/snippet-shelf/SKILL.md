---
name: snippet-shelf
description: Check the shelf of already-working code before writing operational code — shell/SSH/Docker pipelines, on-chain sends, deploy or recovery scripts, API probes, one-off DB queries against prod. Use when about to write or generate such code, when the user says "have we done this before", "reuse that script", "snippet shelf", "did we solve this already", or asks to save a working script for next time. Reuses a proven script (or diffs and adapts it) instead of regenerating it.
---

The shelf is the set of `type: snippet` memories: code that was **run and observed to
work** on this fleet. Regenerating a script you already own is the expensive failure —
wasted calls, and a fresh set of the same bugs the stored version already fixed.

## 1. Look before you write

Before writing operational code, call:

    memory_snippet_lookup(task="<what you're about to do, in the user's terms>")

(vector-only install: `memory_snippet_lookup_nograph`.)

It searches snippets only and **abstains unless two independent rankers agree**.
`(no snippet matched)` is a normal answer — it means write it fresh, not "search harder".
A result marked `SINGLE-INDEX GUESS` came from one ranker: treat it as a lead, not a match.

It returns **pointers, not code**. Read the returned `.md` — the body carries the gotchas
that are usually the reason the stored version works and a fresh one wouldn't.

## 2. Decide: reuse, adapt, or discard

Compare the snippet's target with yours — host, container, chain/network, asset, decimals,
tool versions.

- **Exact target match** → reuse verbatim. Change nothing you don't have to.
- **Close but different target** → adapt, and say out loud what you changed and why. Reuse
  the *logic and the gotchas*; re-derive only the parts that genuinely differ. Show the diff
  against the stored version before running it.
- **Same words, different job** → discard it and write fresh. A snippet that merely shares
  vocabulary is worse than none; retrieval ranks by relevance, and relevance is not fitness.

The trap to watch: proven code aimed at the wrong target. A sweep script pinned to a testnet
chain id is safe until someone swaps the RPC URL. The code being trusted does not make the
*invocation* trusted — the invocation is the part you just regenerated.

## 3. Run it according to its `risk:` tag

| `risk:` | What to do |
|---|---|
| `read` | Just run it. Balance checks, status queries, log greps — no side effects. |
| `write` | Show the resolved command, then run. Deploys, restarts, DB writes. |
| `money` | Preflight that **binds** the real parameters — host, chain, sender, destination, amount, fee ceiling — and get explicit confirmation of those exact values before executing. Never reduce this to "run the snippet?". |

Untagged snippet → treat as `write` until the user says otherwise.

For `money`, a preflight that only prints balances approves nothing. It must show the
transfer that is about to happen. And a timeout is **indeterminate, not failed** — resolve
the transaction by hash or sender/nonce before considering a retry.

## 4. Put it back on the shelf

When a script works — and only once you have *seen* it work — save it:

    cat <<'EOF' | ~/.claude/save_memory.sh snippet_<topic>.md "<one-line hook>"
    ---
    name: snippet_<topic>
    description: <what it does + where it was verified>
    metadata:
      type: snippet
      risk: read | write | money
      verified_on: <YYYY-MM-DD, + host/chain/versions it ran against>
    ---
    ## Summary
    <what it does, and the evidence it worked — tx hash, exit status, row count>
    ...
    EOF

Rules:
- **Secrets by name, never by value.** `ETHEREUM_MAIN_PRIVATE_KEY`, not the key.
- **Save the runner too**, not just the payload — the ssh/docker plumbing is usually the
  part that took the debugging.
- **Save the gotchas.** The wrong working directory, the flag that silently no-ops, the
  RPC that refuses connections. That's most of a snippet's value.
- **Adapted a snippet?** If the operational contract is unchanged (same job, same risk,
  same target class) update the existing file. If the chain, asset, target class, or safety
  assumptions changed, that's a **new** snippet — the old one's verification doesn't
  transfer. If the adaptation failed, save nothing.

Snippets are never auto-harvested from a transcript (a transcript can't prove code ran) and
are never auto-merged with each other (two scripts differing only in chain id embed nearly
identically). The shelf only grows when someone deliberately puts something on it.
