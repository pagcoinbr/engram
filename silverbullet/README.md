# SilverBullet — the browser editor over engram's vaults

engram's vaults live on this headless server at `/vaults/<tenant>`, and engram
reads/indexes them and writes agent findings into `_agent/`. [SilverBullet](https://silverbullet.md)
is a **server-first** markdown editor: it serves a server-side directory over the
browser (a PWA, so phones work too). Pointed at a vault, it edits the *same files
engram indexes* — so there is no sync layer, no Obsidian, no local copy. The human
edits in the browser; engram indexes those edits; agents recall them and file
findings back; the human sees those in the browser. One shared directory.

## The loop

```
human writes a runbook in SilverBullet ─▶ /vaults/<tenant>/Runbooks/foo.md
                                              │
                        engram wiki index (daemon, 30 min, sha-based)
                                              ▼
                        agent: wiki_search / wiki_fetch
                                              │
                        agent: wiki_write ─▶ /vaults/<tenant>/_agent/finding.md
                                              ▼
                        human reviews it in SilverBullet
```

## Isolation

One instance per tenant, each bind-mounting **only** its own vault
(`/vaults/<tenant>:/data`) and on its **own Docker network**. A compromised or
mistaken instance can reach neither another identity's files (never mounted) nor
another instance (separate network) — the per-tenant boundary engram enforces in
Qdrant and Neo4j, here at the container-mount and network layers. Each instance
keeps outbound internet egress (SilverBullet's Library sync needs it).

## Posture

- **Tailnet-only.** Each instance binds `127.0.0.1:<port>`; the Traefik gateway
  gives it a `wiki-<tenant>.home.arpa` hostname with TLS over the LAN/tailnet.
  No public entrypoint, no Tailscale funnel. SilverBullet runs space-script/Lua,
  which is a second reason to keep it off the open internet.
- **Work vaults read-only by default.** `mjsv`/`bbhost`/`dseclab` run with
  `SB_READ_ONLY=true` — browse client-infra notes, but don't edit them from a
  browser. Remove that line for a tenant to make it read-write.
- **Auth is SilverBullet's own** (`SB_USER`, with lockout), credentials in
  `sb.env` (mode 600). Traefik adds TLS, not a second password; a commented
  basicAuth middleware is there if you want defense-in-depth on the work vaults.

## Ports

| tenant | instance | loopback port | hostname |
|---|---|---|---|
| homelab | wiki-homelab | 3011 | wiki-homelab.home.arpa |
| mjsv | wiki-mjsv | 3012 | wiki-mjsv.home.arpa |
| bbhost | wiki-bbhost | 3013 | wiki-bbhost.home.arpa |
| dseclab | wiki-dseclab | 3014 | wiki-dseclab.home.arpa |

## Running it

`./install.sh --silverbullet` provisions everything (image, `sb.env` with
per-tenant passwords, the Traefik routes, and brings up the instances). By hand:

```sh
# secrets (mode 600), one per tenant you expose:
cat > silverbullet/sb.env <<'ENV'
SB_USER_HOMELAB=you:a-strong-password
SB_USER_MJSV=you:another
SB_USER_BBHOST=you:another
SB_USER_DSECLAB=you:another
ENV
chmod 600 silverbullet/sb.env

docker compose --env-file silverbullet/sb.env -f silverbullet/docker-compose.yml up -d wiki-homelab
```

## Notes for engram

- SilverBullet writes one internal file into each space, `.silverbullet.auth.json`
  (auth state). It is a **dotfile**, so the wiki walker already skips it from
  indexing and the backup excludes it — nothing to configure.
- Files engram or an agent writes appear in SilverBullet immediately; files
  SilverBullet writes are indexed on the daemon's next `wiki` pass.
