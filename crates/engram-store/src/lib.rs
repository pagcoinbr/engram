use std::{fs, path::Path};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub struct Memory {
    pub file: String,
    pub name: String,
    pub description: String,
    pub memory_type: String,
    pub body: String,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("could not read memory store: {0}")]
    Read(#[from] std::io::Error),
}

pub fn load(dir: impl AsRef<Path>) -> Result<Vec<Memory>, StoreError> {
    let mut memories = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("md")
            || path.file_name().and_then(|value| value.to_str()) == Some("MEMORY.md")
        {
            continue;
        }
        let raw = fs::read_to_string(&path)?;
        let (meta, body) = parse(&raw);
        let file = path.file_name().unwrap().to_string_lossy().to_string();
        let fallback = file.trim_end_matches(".md").replace('_', " ");
        memories.push(Memory {
            file,
            name: meta
                .get("name")
                .cloned()
                .unwrap_or_else(|| fallback.clone()),
            description: meta.get("description").cloned().unwrap_or_default(),
            memory_type: meta
                .get("type")
                .cloned()
                .unwrap_or_else(|| "reference".into()),
            body,
        });
    }
    memories.sort_by(|left, right| left.file.cmp(&right.file));
    Ok(memories)
}

pub fn parse(raw: &str) -> (std::collections::BTreeMap<String, String>, String) {
    let mut meta = std::collections::BTreeMap::new();
    if let Some(rest) = raw.strip_prefix("---\n")
        && let Some((frontmatter, body)) = rest.split_once("\n---\n")
    {
        for line in frontmatter.lines() {
            if let Some((key, value)) = line.split_once(':')
                && !line.starts_with(' ')
            {
                meta.insert(key.trim().into(), value.trim().trim_matches('"').into());
            }
        }
        return (meta, body.to_string());
    }
    (meta, raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_canonical_frontmatter() {
        let (meta, body) =
            parse("---\nname: Gateway\ndescription: DNS\nmetadata:\n  type: reference\n---\nBody");
        assert_eq!(meta["name"], "Gateway");
        assert_eq!(body, "Body");
    }
}
