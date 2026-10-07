//! A string that can be read from config but never written back out.
//!
//! `/api/v1/config` and `/api/v1/config/editor` both serialize the config straight
//! to the browser, under a heading that claims secrets are redacted. Before this
//! type that claim held only because no secret-bearing field was modelled at all —
//! adding `api_key` to the struct would have shipped it to the client. Rather than
//! rely on remembering to strip fields at every call site, the leak is closed in
//! the type: `Secret` deserializes normally and serializes as an empty string.
//!
//! The editor payload (`LlamaCppEdit`/`EmbedEdit`) deliberately has no secret
//! fields, so a save merges the surrounding keys and leaves credentials in the file
//! untouched.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The real value, for building an outbound request. Named so that any use is
    /// obvious in review.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    /// `Some(value)` only when a credential was actually configured, so callers can
    /// write `if let Some(key) = secret.present()` instead of testing for "".
    pub fn present(&self) -> Option<&str> {
        let trimmed = self.0.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl std::fmt::Debug for Secret {
    /// Debug output lands in logs and error strings; neither may carry the value.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            f.write_str("Secret(unset)")
        } else {
            f.write_str("Secret(set)")
        }
    }
}

impl Serialize for Secret {
    /// Always empty. A secret must not reach an HTTP response, a status payload, or
    /// a re-serialized config file.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("")
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(
            Option::<String>::deserialize(deserializer)?.unwrap_or_default(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_serializes_the_value() {
        let secret = Secret::new("sk-live-do-not-leak");
        assert_eq!(serde_json::to_string(&secret).unwrap(), "\"\"");
        assert_eq!(serde_yaml::to_string(&secret).unwrap().trim(), "''");
        assert_eq!(format!("{secret:?}"), "Secret(set)");
        assert_eq!(secret.expose(), "sk-live-do-not-leak");
    }

    #[test]
    fn reads_from_yaml_and_tolerates_null() {
        let secret: Secret = serde_yaml::from_str("'sk-abc'").unwrap();
        assert_eq!(secret.present(), Some("sk-abc"));
        let empty: Secret = serde_yaml::from_str("~").unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.present(), None);
        // whitespace is not a credential
        let blank: Secret = serde_yaml::from_str("'   '").unwrap();
        assert_eq!(blank.present(), None);
    }
}
