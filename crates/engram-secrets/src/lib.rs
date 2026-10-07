//! The secret boundary, ported from `bin/engram_secrets.py`.
//!
//! Anything that can carry memory text off the box has to pass through here first:
//! an embedding provider may be a remote host, the native graph extractor sends
//! memory bodies to a reasoning endpoint, and Neo4j keeps whatever it is handed.
//!
//! The Rust side previously used its own one-line regex —
//! `(api[_-]?key|token|password|secret)\s*[:=]\s*\S+` — which recognised none of
//! the credential classes this fleet actually handles: bearer tokens, PEM private
//! keys, BIP39 mnemonics, macaroons, WIF/xprv keys, vendor-prefixed tokens. It was
//! also recompiled on every call. The pattern below is the Python one, verbatim, so
//! the two implementations cannot drift; `tests/fixtures/secret_samples.json` is
//! the shared corpus both are tested against.
//!
//! It is deliberately AGGRESSIVE. It drives redaction (masking a stray hash costs
//! nothing) and hold-off-remote decisions, not save-blocking — the high-precision
//! block guard lives in `memory_lib.sh`.

use regex::Regex;
use std::sync::OnceLock;

pub const REDACTION: &str = "«redacted-secret»";

/// Mirrors `SECRET_RE` in `bin/engram_secrets.py`. Keep the two in lockstep; the
/// shared fixture test fails if they diverge on any known class.
const PATTERN: &str = concat!(
    r"(?i)(",
    // BIP39 / recovery phrases: keyword then 6+ separated words — FIRST so the
    // whole phrase masks, not just its first word.
    r"(?:mnemonic|seed[_-]?phrase|recovery[_-]?phrase)\s*[:=]\s*(?:[a-z]+[\s,]+){5,}[a-z]+",
    // named credential = VALUE (including crypto key material)
    r"|(?:client[_-]?secret|webhook[_-]?secret|api[_-]?key|apikey|password|passwd|secret|token|access[_-]?token|mnemonic|seed[_-]?phrase|recovery[_-]?phrase|private[_-]?key|priv[_-]?key|macaroon)\s*[:=]\s*['\x22]?[^\s'\x22]{6,}",
    r"|originSessionId\s*[:=]?\s*[0-9a-fA-F-]{8,}",
    r"|-----BEGIN[ A-Z]*PRIVATE KEY",
    r"|Bearer\s+[A-Za-z0-9._\-]{20,}",
    r"|xprv[a-zA-Z0-9]{20,}",                 // BIP32 extended private key
    r"|\b[5KL][1-9A-HJ-NP-Za-km-z]{50,51}\b", // WIF private key
    r"|AKIA[0-9A-Z]{16}",
    r"|gh[pousr]_[A-Za-z0-9]{20,}",
    r"|sk-[A-Za-z0-9_-]{20,}",
    r"|[0-9a-f]{32,}",            // long hex (hashes, raw macaroons)
    r"|[A-Za-z0-9+/]{40,}={0,2}", // long base64 (tokens, blobs)
    r")",
);

fn secret_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Compiled once per process. The previous implementation rebuilt its regex
    // inside the per-memory loop.
    RE.get_or_init(|| Regex::new(PATTERN).expect("secret pattern compiles"))
}

/// Mask secret-looking substrings. Returns the masked text and how many were hit.
pub fn redact(text: &str) -> (String, usize) {
    let re = secret_re();
    let count = re.find_iter(text).count();
    (re.replace_all(text, REDACTION).into_owned(), count)
}

/// True when the text contains something that must not leave the box as-is.
pub fn looks_secret(text: &str) -> bool {
    secret_re().is_match(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixtures() -> serde_json::Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/secret_samples.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("shared secret corpus missing at {}: {e}", path.display()));
        serde_json::from_str(&text).expect("corpus is valid JSON")
    }

    /// The Python detector and this one are tested against the SAME corpus. If the
    /// two ever diverge on a known credential class, one of these fails.
    #[test]
    fn matches_the_shared_corpus() {
        let data = fixtures();
        for sample in data["secret"].as_array().expect("secret samples") {
            let s = sample.as_str().unwrap();
            assert!(looks_secret(s), "missed secret: {s}");
        }
        for sample in data["clean"].as_array().expect("clean samples") {
            let s = sample.as_str().unwrap();
            assert!(!looks_secret(s), "false positive: {s}");
        }
    }

    #[test]
    fn redacts_and_counts() {
        let (masked, n) = redact("api_key=sk-proj-abcdefghijklmnopqrstuvwxyz1234");
        assert_eq!(n, 1, "{masked}");
        assert!(!masked.contains("sk-proj"), "{masked}");
        assert!(masked.contains(REDACTION), "{masked}");

        let (clean, n) = redact("deploy on port 9000 at /home/x/y.py");
        assert_eq!(n, 0);
        assert_eq!(clean, "deploy on port 9000 at /home/x/y.py");
    }

    /// The classes the replaced one-line regex could not see. These are the reason
    /// the old detector was a release blocker, so they are pinned explicitly.
    #[test]
    fn covers_classes_the_old_regex_missed() {
        for sample in [
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123",
            "-----BEGIN RSA PRIVATE KEY-----",
            "mnemonic: abandon abandon abandon abandon abandon ability",
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_abcdefghijklmnopqrstuvwxyz0123",
            "macaroon=0201036c6e6402eb01030a10",
        ] {
            assert!(looks_secret(sample), "missed: {sample}");
        }
    }
}
