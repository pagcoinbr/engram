#!/usr/bin/env python3
"""engram_secrets.py — ONE shared secret detector/redactor for the memory system.

Used by everything on the Python side that could send memory text somewhere it
shouldn't: distillation (redact before the LLM prompt, which may be a REMOTE
backend), vector embedding (redact before the embedding provider, which may be an
off-box Ollama/LM Studio host), index generation (redact descriptions before they
land in MEMORY.md), and stage-apply dedup. The shell writers use the mirrored
high-precision block ERE in memory_lib.sh.

This pattern is intentionally AGGRESSIVE — it is used for REDACTION (masking a
stray hash/UUID costs nothing) and for HOLDING content off remote backends. The
shell *block* guard is separately tuned high-precision so it rarely false-blocks
a save. Both must cover the crypto material this fleet actually handles:
macaroons, BIP39 seed phrases, WIF/xprv keys.
"""
import re

SECRET_RE = re.compile(
    r"(?i)("
    # BIP39 / recovery phrases: keyword then 6+ space/comma-separated words —
    # listed FIRST so it masks the whole phrase, not just the first word.
    r"(?:mnemonic|seed[_-]?phrase|recovery[_-]?phrase)\s*[:=]\s*(?:[a-z]+[\s,]+){5,}[a-z]+"
    # named credential = VALUE (incl. crypto key material)
    r"|(?:client[_-]?secret|webhook[_-]?secret|api[_-]?key|apikey|password|passwd|secret|token|access[_-]?token|mnemonic|seed[_-]?phrase|recovery[_-]?phrase|private[_-]?key|priv[_-]?key|macaroon)\s*[:=]\s*['\"]?[^\s'\"]{6,}"
    r"|originSessionId\s*[:=]?\s*[0-9a-fA-F-]{8,}"
    r"|-----BEGIN[ A-Z]*PRIVATE KEY"
    r"|Bearer\s+[A-Za-z0-9._\-]{20,}"
    r"|xprv[a-zA-Z0-9]{20,}"                 # BIP32 extended private key
    r"|\b[5KL][1-9A-HJ-NP-Za-km-z]{50,51}\b" # WIF private key
    r"|AKIA[0-9A-Z]{16}"
    r"|gh[pousr]_[A-Za-z0-9]{20,}"
    r"|sk-[A-Za-z0-9_-]{20,}"
    r"|[0-9a-f]{32,}"                        # long hex (hashes, raw macaroons)
    r"|[A-Za-z0-9+/]{40,}={0,2}"             # long base64 (tokens, blobs)
    r")")


def redact(text: str):
    """Mask secret-looking substrings. Returns (masked_text, n_masked)."""
    return SECRET_RE.subn("«redacted-secret»", text or "")


def looks_secret(line: str) -> bool:
    return bool(SECRET_RE.search(line or ""))


def _corpus():
    """The shared corpus, also used by the engram-secrets Rust crate. One file, two
    implementations — that is what keeps the Python and Rust detectors in lockstep
    instead of each growing its own idea of what a credential looks like."""
    import json
    from pathlib import Path
    p = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "secret_samples.json"
    d = json.loads(p.read_text())
    return d["secret"], d["clean"]


if __name__ == "__main__":  # tiny self-check
    samples_secret, samples_clean = _corpus()
    for s in samples_secret:
        assert looks_secret(s), f"missed secret: {s}"
    for s in samples_clean:
        assert not looks_secret(s), f"false positive: {s}"
    assert redact("api_key=sk-proj-abcdefghijklmnopqrstuvwxyz1234")[1] == 1
    print(f"ok — engram_secrets self-check ({len(samples_secret)} secret, "
          f"{len(samples_clean)} clean samples)")
