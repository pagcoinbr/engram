#!/usr/bin/env bash
# S2 regression: the shared writer secret-guard blocks real credential shapes,
# passes clean content, never echoes the secret value, and honours the override.
set -uo pipefail
source "$(dirname "$0")/../bin/memory_lib.sh"

fail=0
must_block() { memory_guard_secret_content "$1" t >/dev/null 2>&1 && { echo "FAIL leak: ${1:0:24}"; fail=1; }; }
must_pass()  { memory_guard_secret_content "$1" t >/dev/null 2>&1 || { echo "FAIL blocked clean: ${1:0:24}"; fail=1; }; }

must_block "Bearer abcdefghijklmnopqrstuvwxyz012345"
must_block "password = Sup3rS3cretValue123"
must_block "clientSecret: a1b2c3d4e5f6g7h8"
must_block "-----BEGIN RSA PRIVATE KEY-----"
must_block "AKIA1234567890ABCDEF"
must_block "ghp_abcdefghijklmnopqrstuvwxyz0123"
must_block "api_key=sk_live_51H8xYzAbCdEf12345"
must_block "api_key=sk-proj-abcdefghijklmnopqrstuvwxyz1234"   # hyphenated vendor token body
must_block "password=Secret123"                              # 8+ char assigned value
must_block "mnemonic: abandon abandon abandon abandon abandon ability"  # BIP39 phrase
must_block "macaroon=0201036c6e6402eb01030a1089abcdef"       # lnd macaroon
must_block "private_key=Kx1234567890abcdefghij"              # priv key assignment
must_block "L1aW4aubDFB7yfras2S1mN3bqg9nwySY8nkoLmJebSLD5BWv3ENZ"       # WIF-shaped key

must_pass "Deploy on port 9000, path /home/foo/bar.py; commit a1b2c3d4e5f6"
must_pass "The DB password lives in Vault; rotate quarterly."   # prose, no assigned value
must_pass "originSessionId: bde53012-4393-486a-8eff-a8006642aae9"  # provenance UUID, not a live cred
must_pass "the macaroon lives at ~/.lnd/data/chain/bitcoin/mainnet/admin.macaroon"  # path prose
must_pass "seed the database with fixtures before the run"  # 'seed' word, not a phrase

# override must let a secret through when explicitly set
MEMORY_ALLOW_SECRET=1 must_pass "password = definitelyASecret123"

# guard must NOT print the secret value
out=$(memory_guard_secret_content "password = TOPSECRETvalue999" t 2>&1)
grep -q "TOPSECRETvalue999" <<<"$out" && { echo "FAIL: secret value leaked into output"; fail=1; }

[[ $fail == 0 ]] && echo "ok — S2 secret scan guard" || exit 1
