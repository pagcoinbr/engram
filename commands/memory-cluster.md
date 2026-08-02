Consolidate a *topic cluster* of memories — many files describing ONE system — into a single distilled memory, resolving contradictions as you go.

This is **not** de-duplication. `auto_curate` already merges near-duplicates (cosine ≥ 0.92, same `type`, ≤2 per run). This command handles the opposite case: files that are **complementary facets of one system** — a deploy log, a threat model, a backup pipeline — whose similarity is far below any dedup threshold but which a reader should meet as one document.

Because the merge requires judgment (which fact is current, which contradiction is real), the reading is **not** delegated to a summarizer. Read the files.

## Step 0 — bind to the memory directory (resolve it the way the helpers do)

⚠️ **Do NOT derive the store from `$PWD`.** `save_memory.sh` and `delete_memory.sh` resolve it through `memory_lib.sh`, which honours `CLAUDE_MEMORY_SLUG` and otherwise falls back to `$HOME` with slashes turned to dashes. A box commonly has **many** stores under `~/.claude/projects/`. If you compute a different directory than the helpers use, you will verify one store while `delete_memory.sh` removes same-named files from **another** — destroying unrelated operational records.

Resolve it once, from the same source of truth:

    MEM="$(bash -c 'source ~/.claude/memory_lib.sh; memory_dir')"

The write path is concurrency-safe on its own: `save_memory.sh`, `save_memory_content_only.sh` and `delete_memory.sh` each hold that memory's **per-file lock** for their whole mutation, `MEMORY_NOCLOBBER=1` makes creation atomic, and `MEMORY_EXPECT_SHA` makes retirement conditional. You still get a cleaner run — and clearer diagnostics — if nothing else is mutating the store, so check and tell the user what is active:

    systemctl --user is-active memory-nightly-apply.timer engram.timer 2>/dev/null
    grep -A2 '^session_curate:' ~/.claude/engram.yaml | grep -q 'enabled: true' && \
      echo "WARNING: session_curate may launch a background curator on session stop"
    pgrep -af 'memory_auto_curate|memory_session_curate|memory_pipeline' || echo "no curator running"

If one is active, say so — a cluster run mutates many files over several minutes, so a concurrent curator will show up as `changed since it was read` aborts in Step 5c. That is the safety net working, not a failure; re-run the merge once the store is quiet.

Then **assert the helpers agree** before mutating anything:

    test -d "$MEM" || { echo "no store at $MEM"; exit 1; }
    echo "operating on store: $MEM"      # show the user, and confirm it is the one they mean

If the user intended a different store, they must set `CLAUDE_MEMORY_SLUG` — do not "fix" it by hand-building a path.

Every read, write, glob, redirect and verification in this command binds to `$MEM`. A bare `os.listdir('.')` or `*.md` would operate on the caller's own repository. In Python, `os.chdir(MEM)` first or join every path against it.

## Step 1 — define the cluster

Parse `$ARGUMENTS`:
- `<prefix>` or `<glob>` → files matching it (e.g. `project_cipher_*`, `alfred`)
- empty → run `~/.claude/memory_audit.sh` and offer its "Cluster candidates" list, letting the user pick one

⚠️ **Validate the resolved source set before it is used for anything.** A broad argument (`*`, `memory`, an empty prefix) can otherwise sweep in the store's own control files, and Step 5 would hand `MEMORY.md` to `delete_memory.sh` — destroying the index, or the whole store. Filter to regular direct-child memory files and hard-reject everything else:

```bash
# keep: regular files, direct children of $MEM, *.md, not control/hidden
for f in "${CANDIDATES[@]}"; do
  case "$f" in
    MEMORY.md|MEMORY_FULL.md|.*)      echo "REJECT control/hidden: $f"; exit 1 ;;
    */*)                              echo "REJECT not a direct child: $f"; exit 1 ;;
    *.md)                             [ -f "$MEM/$f" ] || { echo "REJECT not a regular file: $f"; exit 1; } ;;
    *)                                echo "REJECT not a memory file: $f"; exit 1 ;;
  esac
done
[ "${#CANDIDATES[@]}" -ge 2 ] || { echo "REFUSING: a cluster needs >= 2 members"; exit 1; }
```

Print the validated list back to the user for confirmation. List each member with size and mtime, plus the total. Then apply the gates below **before** reading anything.

### Gate A — is this one system, or just a shared name?

A prefix is not a topic. Ask what a reader searching for one member would want back.

- **Merge**: files that describe one system from different angles (`cypher-bringup` + `cypher_signer_custody` + `cipher_disaster_recovery` — one platform).
- **Do NOT merge**: files that share a company/product name but span unrelated subsystems (a `pagcoin_*` prefix covering an EVM RPC service, a TUI, a website, POS terminals and tax filing). Merging those makes a query about tax filing return a document about RPC internals — worse recall, not better.

When a cluster fails Gate A, propose splitting it into 2-4 coherent sub-clusters and merge those separately. Say so plainly rather than merging anyway.

### Gate B — exclude by type

Never fold these in; they are reachable through machinery a project file is not:
- **`type: feedback`** — standing behavioural rules; they belong in the hot index and load every session.
- **`type: snippet`** — `memory_snippet_lookup` searches `type=snippet` **only**. Merging a snippet silently removes it from the snippet shelf.
- **`type: user`** — identity facts, always-load.

Keep them standalone and link to them from the merged file.

### Gate C — size arithmetic

A memory is read on demand, so it may exceed the index's budget — but a file the reader must page through is a worse document. Compute `sum(bytes)` first:

- Expect a good distillation to land at **20–35 %** of the input (tonight's runs: 146 KB→32 KB, 54 KB→23 KB, 34 KB→20 KB).
- If the projected result exceeds ~35 KB, the cluster is probably two topics. Re-check Gate A.
- Never simply concatenate. The compression comes from dropping chronology, not from dropping facts.

Report the projection to the user and get agreement before proceeding.

## Step 2 — read every member in full, and record its hash

⚠️ **This store has other writers.** `session_curate` fires a background curator on session stop, the nightly job runs unattended, and a remote may be synced by another seat. Between reading a source here and retiring it in Step 5c, a concurrent writer can update a payout address, custody rule or threshold — and Step 5c would delete that unseen update while the merge preserves the stale value. Record a hash per source now so that is detectable:

    sha256sum "$MEM"/<each source>.md > "$TMPDIR_HASHES"   # keep alongside $TMP

Use `Read` on each file. Do **not** substitute a summarizer: the value of this command is catching what a per-file summary cannot see — a value that drifted, a host that was decommissioned, a decision superseded in a *different* file.

While reading, keep a running list of:
1. **Contradictions** — two files asserting different values/states for the same thing.
2. **Superseded facts** — an older file describing infrastructure a newer one says was deleted, wiped or migrated.
3. **Open items** — unfixed defects, blocked PRs, unverified claims.
4. **Credentials / secrets** — note where they are; do not restate fund-critical key *values*.

## Step 3 — resolve contradictions explicitly

🚨 **Never resolve a money, custody, or authorization contradiction by file recency.** Payout addresses, key locations, spend caps, thresholds, allowlists and permission flags must be settled by **authoritative live verification** (read the running config, query the DB, resolve the DNS name, check the chain) or, failing that, by **explicit user decision**. `mtime` is not evidence of semantic validity — a checkout, restore, rsync or migration rewrites it without touching meaning, so "newest" can hand canonical status to a stale payout address and then delete the correct one. If neither verification nor a user answer is available, keep **both** values in the merged file, labelled and dated, and mark the conflict unresolved. Do not pick.

For everything else, default to **newest wins**, but never silently. In the merged file, state the correction and why, e.g.

> ⚠️ Older notes claim `manager_reserve_depix=5000`; live config reads `0.0` (verified <date>). The top-up step no longer gates a run.

Two patterns worth checking for specifically, both of which have caused real wasted work:
- **An "outage" that was a migration** — a host/domain recorded as unreachable, while a later file shows it simply moved.
- **A live-looking record of dead infrastructure** — ports, paths and restart procedures for a machine that was since wiped. Keep only what stops someone acting on it, and label it dead.

Where a claim can be cheaply verified against the live system (a config file, a DB table, a DNS lookup), **verify it** rather than picking by date, and stamp the result with the date.

## Step 4 — write the merged memory

One file, canonical shape (`feedback_memory_file_format`): frontmatter → `## Summary` → `## Index` → one `## <n>. <Title>` per index entry.

- Frontmatter `description:` must say it is a merge and name the count.
- The Summary states what the system is and **any correction a reader must not miss** (e.g. "the host moved", "these are two different systems").
- List the source filenames in the Summary so the merge is auditable.
- Preserve: concrete values, addresses, ports, thresholds, commands, and every ⚠️ gotcha.
- Drop: phase-by-phase chronology, "DONE + verified" run logs, and machine-generated appendices.
- Keep `[[wiki-links]]` to memories **outside** the cluster.

⚠️ **Build the merge OUTSIDE the store.** Do not `Write` into `$MEM`. `save_memory.sh` is the guarded writer: it secret-scans the content and only then creates the file. Writing the canonical file yourself and *then* calling the scanner inverts that — when the scan rejects a key, the secret is already sitting in the store and will be recalled in later sessions. (This exact inversion has happened.)

Compose into a temporary file instead, mode 600, outside `$MEM`:

    TMP="$(mktemp -t memcluster.XXXXXX)"; chmod 600 "$TMP"

Write the merged content to `$TMP`. Confirm its size against the Step 1 projection.

⚠️ **The destination must not already exist, and the check must be fatal.** Unlike a retired source, an overwritten file gets **no `.trash` snapshot** — on a local-only install its previous contents are gone permanently. A warning that does not stop execution is not a guard:

    [ -e "$MEM/<merged>.md" ] && { echo "REFUSING: destination exists — choose another name"; exit 1; }

**Choosing a different destination name is the only supported resolution.** Do not "snapshot then replace": retiring the old destination before the merge is durable opens a window where an interrupted or failed write leaves the canonical store with **neither**. If the existing file genuinely belongs in the merge, read it in Step 2 like any other member, write the merge under a **new** name, and let Step 5c retire the old one *after* the replacement is verified — exactly the ordering Step 5 already enforces for every other source.

Re-check existence immediately before the write in Step 5a as well; between the check and the write, a concurrent curator or the nightly job may have created it.

## Step 5 — PERSIST AND VERIFY, THEN delete (order is load-bearing)

⚠️ **Persist the replacement before removing anything.** `save_memory.sh` can *refuse* — its secret guard rejects a file that merely looks like it contains a credential, and with a remote configured the push can fail independently. If sources were already deleted, the store is left with **neither** the sources nor the replacement. This is not hypothetical: it has happened.

**5a — create the canonical file, create-only, under the per-memory lock:**

    MEMORY_NOCLOBBER=1 ~/.claude/save_memory.sh <merged>.md "<description>" < "$TMP"

`MEMORY_NOCLOBBER=1` makes `save_memory.sh` refuse if the destination already exists, and the check happens **inside** that memory's own lock — so it is atomic against a concurrent writer creating the same name, with no TOCTOU window and no placeholder to clean up. A refusal (existing file, or the secret scan) leaves the store completely untouched, which is why the merge was composed in `$TMP`.

`save_memory.sh` secret-scans stdin and writes `$MEM/<merged>.md` itself, so a rejection leaves the store **untouched** — which is the whole point of composing in `$TMP`. Shred the temp file once Step 5b passes: `shred -u "$TMP"` (or `rm -f`).

If it refuses with a secret warning, **look at the flagged line in `$TMP` before overriding** — confirm it is a false positive (a code snippet mentioning `password:`, not a key) and not a real secret. If it is real, edit `$TMP` to replace the value with a pointer to where the secret lives, and re-run. Only override with `MEMORY_ALLOW_SECRET=1` for test/sandbox credentials the user has explicitly approved keeping, and never for a fund-critical key.

**5b — verify it landed before deleting anything:**

    test -s "$MEM/<merged>.md"                        # non-empty on disk
    grep -q "](<merged>.md)" "$MEM/MEMORY.md"         # indexed
    # if CLAUDE_MEMORY_REPO is set, confirm the remote copy exists too

**Do not proceed while any check fails.** If the remote is configured and the push failed, stop and report — deleting now would destroy the only copies.

**5c — only now, delete the sources**, one call per file:

    ~/.claude/delete_memory.sh <filename.md>

⚠️ **Retire each source conditionally on the hash you recorded in Step 2.** A concurrent curator or a remote sync may have updated one after you read it; deleting that now discards an unseen change while the merge carries the stale value. Pass the expected hash so `delete_memory.sh` re-checks it **under that memory's own lock**, immediately before removing it — no window between check and delete:

    SHA="$(awk -v n="$f" '$2==n {print $1}' "$TMPDIR_HASHES")"   # exact field match, NOT a regex
    [[ "$SHA" =~ ^[0-9a-f]{64}$ ]] || { echo "REFUSING: no valid hash recorded for $f"; exit 1; }
    MEMORY_EXPECT_SHA="$SHA" ~/.claude/delete_memory.sh "$f"

⚠️ Look up the hash by **exact field match**, never `grep "$f"` — a filename is not a regex, and one containing `[`, `.` or `*` would match nothing, yielding an empty `MEMORY_EXPECT_SHA`. An empty value must never be passed: `delete_memory.sh` rejects a malformed one rather than silently falling back to an unconditional delete, but the caller should catch it first.

It refuses with `changed since it was read` on any mismatch. **Abort the whole retirement** at that point and re-run the merge — a partial merge is recoverable, a lost update is not.

If a remote is configured, also record each file's **remote blob sha** in Step 2 and pass `MEMORY_EXPECT_REMOTE_SHA` alongside — the local hash only proves *your* copy is unchanged, and another seat may have published an update you never read.

⚠️ **The merged file's own name must never appear in this list** — deleting it here destroys the merge after the sources are gone:

    printf '%s\n' "${SOURCES[@]}" | grep -qx "<merged>.md" && { echo "REFUSING: merged file is in the delete list"; exit 1; }

Each source is backed up to `$MEM/.trash/<ts>-<name>.md` and restorable. If any call fails, **stop** and report — do not continue deleting.

## Step 6 — repoint wiki-links (read this carefully)

Surviving memories still link to the deleted files. Rewrite those links to the merged file.

⚠️ **The regex trap.** `'\[\[' + '|'.join(names) + '\]\]'` is **wrong**: alternation binds loosest, so `\[\[a|b|c\]\]` parses as `(\[\[a)` OR `(b)` OR `(c\]\])`. Middle names then match as **bare text anywhere in the file**, corrupting prose, and first/last names leave dangling brackets — producing `[[[[merged]]]]`. This has already shipped damage across 25 files. **Always use a capture group:**

Export the path resolved in Step 0 and consume it **verbatim** — never rebuild it from a slug, and never use `'.'`. A mistaken slug rewrites unrelated memories in another store, with no snapshot on a local-only install.

    export MEM   # the value resolved in Step 0

```python
import os, re, subprocess, hashlib
MEM = os.environ['MEM']                       # verbatim from Step 0 — do NOT reconstruct
assert os.path.isdir(MEM), MEM
names = [...]            # deleted filenames, without .md
merged = 'project_foo'
pat = re.compile(r'\[\[(' + '|'.join(map(re.escape, names)) + r')\]\]')
touched = []
for f in sorted(os.listdir(MEM)):
    if not f.endswith('.md') or f in ('MEMORY.md', 'MEMORY_FULL.md', merged + '.md'):
        continue
    p = os.path.join(MEM, f)
    t = open(p, encoding='utf-8').read()
    n = pat.sub(f'[[{merged}]]', t)
    n = re.sub(r'(\[\[' + merged + r'\]\])(,?\s*\[\[' + merged + r'\]\])+', r'\1', n)  # collapse runs
    if n == t:
        continue
    # Compute only — do NOT write here. `open(p,'w')` would truncate the original
    # before the replacement is durable, and a direct write takes no per-memory lock,
    # so a concurrent curator's update between the read above and the write is lost.
    # Hand the new content to the locked, secret-guarded writer instead.
    # CAS: the read above happened BEFORE the writer takes its lock, so pass the
    # hash of what we read. If a concurrent writer changed the file in between,
    # the rewrite is refused instead of overwriting their update with stale text.
    env = {**os.environ, 'MEMORY_EXPECT_SHA': hashlib.sha256(t.encode()).hexdigest()}
    r = subprocess.run(['bash', os.path.expanduser('~/.claude/save_memory_content_only.sh'), f],
                       input=n, text=True, capture_output=True, env=env)
    if r.returncode != 0:
        raise SystemExit(f'rewrite refused for {f}: {r.stderr.strip()}')
    touched.append(f)
print('\n'.join(touched))
```

`save_memory_content_only.sh` holds that memory's per-file lock for the whole mutation, secret-scans the content, writes it, and pushes to the remote if one is configured — so the rewrite is serialized against every other writer and can never leave a truncated file or a local/remote divergence. It deliberately does not touch `MEMORY.md` (the description is unchanged), which also avoids the index race.

⚠️ **Persist every rewritten file through the canonical writer.** Writing them in place only updates the local copy; with `CLAUDE_MEMORY_REPO` configured the deleted sources are already gone from the remote while the survivors still carry dangling links there — deterministic local/remote divergence. For each file in `touched` (the description is unchanged, so this is the content-only writer, which leaves `MEMORY.md` alone and avoids the index race):

    ~/.claude/save_memory_content_only.sh <file>.md < "$MEM/<file>.md"

Check each result; if a push fails, report which files are locally-ahead rather than declaring the cluster done.

Then **verify**, because a silent corruption here is worse than a broken link:

    grep -rlE '\[\[\[|\]\]\]' "$MEM"/*.md                          # must be empty
    grep -rnE '`\[\[[a-z_]+\]\]`|\[\[[a-z_]+\]\]\.md' "$MEM"/*.md  # backticked / .md-suffixed = corrupted prose

A link that pointed at a *section* of a deleted file (`[[old]] section 7`) no longer resolves — rewrite the sentence rather than leaving a dangling reference.

## Step 7 — rebuild indexes and audit

The merged file was already saved and indexed in Step 5a; this only refreshes the derived index and confirms the store improved:

    ~/.claude/memory_full_index.sh                     # rebuild the full (derived) index
    ~/.claude/memory_audit.sh | grep -E '^## |^Total'  # flags should DROP, not rise

If the audit's flag count **rose**, something in Step 6 went wrong — check for the corruption patterns before reporting success.

## Step 8 — report

```
Clustered <N> memories into <merged>.md
  before:  <N> files, <X> KB
  after:   1 file, <Y> KB  (-<Z>%)
  kept out: <files excluded by Gate B, and why>
  corrections resolved:
    - <contradiction> → <resolution>
  links repointed in <M> files, 0 corruptions
```

Then state any **open items** the merge surfaced (unfixed defects, blocked PRs, unverified claims) — those are the highest-value output and are easy to lose in a consolidation.

## Hard rules

- Never delete a source before the merged file is **saved, indexed and verified** (Step 5b) — a refused secret-scan or failed push after deletion destroys both copies.
- Never derive the store from `$PWD`; resolve `$MEM` through `memory_lib.sh` so it is byte-identical to what `save_memory.sh`/`delete_memory.sh` use. A mismatch deletes from a *different* store.
- Never `Write` the merged file into `$MEM`. Compose in `$TMP` and let `save_memory.sh` create it, or the secret scan is decorative.
- Never overwrite an existing destination — it gets no `.trash` snapshot. The existence check is **fatal**; rename instead.
- Never let `MEMORY.md`, `MEMORY_FULL.md`, a hidden/control file, or anything outside the validated filename pattern enter the source set.
- Never resolve a money / custody / authorization contradiction by `mtime` — verify live, ask, or record both.
- Never retire a source without passing `MEMORY_EXPECT_SHA`; this store has concurrent writers (`session_curate`, the nightly job, remote syncs).
- Never create the destination without `MEMORY_NOCLOBBER=1` — a plain existence test is a TOCTOU race against another writer.
- Never include the merged file's own name in the Step 5c delete list — that destroys the merge after the sources are already gone.
- Never leave a rewritten memory unpersisted — with a remote configured that is deterministic local/remote divergence.
- Never merge `feedback`, `snippet`, or `user` type memories into a project file.
- Never merge a cluster that fails Gate A — propose sub-clusters instead.
- Never resolve a contradiction silently; state the correction and its date.
- Never use the ungrouped alternation regex in Step 6.
- Never restate fund-critical key values; point at where they live.
