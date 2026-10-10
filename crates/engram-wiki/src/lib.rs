//! Reading an Obsidian vault: walk, parse, chunk.
//!
//! This is the shape engram did not previously have. A memory is one fact in one
//! file, embedded whole — `engram-index` builds its input from
//! `name + description + truncate(body, 1500)`, one vector per file. That is
//! right for a one-fact memory and useless for a 4,000-word runbook: the body is
//! cut at 1,500 bytes and the rest is simply not indexed. A wiki page is the
//! opposite shape, so it needs chunking, and chunking needs three things the
//! memory path never did:
//!
//! - **Recursion.** `engram_store::load` is a flat `read_dir`; a vault is nested
//!   folders.
//! - **Headings.** A chunk cut at a fixed character count lands mid-sentence and
//!   mid-table. Cutting at headings keeps a chunk about one thing.
//! - **Context.** A chunk retrieved alone has lost the page it came from, so
//!   each one carries its own breadcrumb into the embedding. That matters more
//!   for retrieval quality than the chunk size does.
//!
//! All of it is pure: `walk` reads a directory, everything else transforms
//! strings, so the whole module is unit-testable without a vault, a Qdrant or an
//! embedding endpoint.

use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

/// Bumped whenever chunk boundaries change.
///
/// Folded into the freshness hash by the indexer, so changing the chunker
/// invalidates every stored chunk. The exact analogue of
/// `Config::embedding_space_id`, and for the same reason: without it, a re-chunk
/// leaves documents looking current while their stored chunks no longer
/// correspond to any boundary the chunker would now produce.
pub const CHUNKER_VERSION: u32 = 1;

/// Directory names never indexed. `.obsidian` is the app's own config and
/// workspace state; `.trash` is Obsidian's soft-delete, and indexing it would
/// resurrect deleted pages in recall.
const SKIP_DIRS: [&str; 4] = [".obsidian", ".trash", ".git", "node_modules"];

#[derive(Debug, Error)]
pub enum WikiError {
    #[error("could not read the vault at {path}: {detail}")]
    Read { path: String, detail: String },
}

/// One wiki page.
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    /// Vault-relative path with `/` separators — the record's identity.
    pub path: String,
    pub title: String,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
    pub links: Vec<Link>,
    /// The body with frontmatter removed.
    pub body: String,
    pub source_mtime: i64,
}

/// An Obsidian `[[wikilink]]`, in any of its four forms.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    /// The page being linked to, as written.
    pub target: String,
    /// `[[Page#Heading]]`
    pub heading: Option<String>,
    /// `[[Page|shown text]]`
    pub alias: Option<String>,
    /// `![[Page]]` — a transclusion rather than a reference.
    pub embed: bool,
}

/// One embeddable unit of a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    pub index: usize,
    /// Title and enclosing headings, outermost first.
    pub heading_path: Vec<String>,
    /// The chunk's own text, without the breadcrumb.
    pub text: String,
    /// Byte range within [`Document::body`], so a hit can be located exactly.
    pub start: usize,
    pub end: usize,
}

impl Chunk {
    /// What actually gets embedded: the breadcrumb, then the text.
    ///
    /// A chunk is retrieved on its own, so without this it is a paragraph with
    /// no subject — "restart the broker and clear the queue" is indistinguishable
    /// between four runbooks. Prepending `Runbooks > RabbitMQ > Failover` costs a
    /// dozen tokens and is the difference between a hit and a near-miss.
    pub fn embedding_text(&self) -> String {
        if self.heading_path.is_empty() {
            return self.text.clone();
        }
        format!("{}\n\n{}", self.heading_path.join(" > "), self.text)
    }

    /// A one-line breadcrumb for display.
    pub fn breadcrumb(&self) -> String {
        self.heading_path.join(" > ")
    }
}

/// Chunk sizing, in characters.
///
/// Characters rather than tokens because tokenizing here would mean shipping a
/// tokenizer and pinning it to whichever model is configured — and the figures
/// only need to be approximately right. At roughly 4 characters per token the
/// defaults are ~400 tokens with ~60 of overlap, comfortably inside every
/// embedding model engram supports.
#[derive(Clone, Copy, Debug)]
pub struct ChunkParams {
    /// Preferred size. A section at or under this is never split.
    pub target: usize,
    /// Hard ceiling before a paragraph is broken mid-way.
    pub max: usize,
    /// Carried from the end of one chunk into the next, so a fact spanning a
    /// boundary is still findable from either side.
    pub overlap: usize,
    /// Sections shorter than this are merged forward instead of becoming their
    /// own chunk: a lone `## Notes` heading with one line under it is noise as a
    /// separate record.
    pub min: usize,
}

impl Default for ChunkParams {
    fn default() -> Self {
        Self {
            target: 1600,
            max: 2400,
            overlap: 240,
            min: 120,
        }
    }
}

/// Every indexable file in the vault, as vault-relative paths, sorted.
///
/// Refuses to leave `root`. A symlink inside one tenant's vault pointing at
/// another's would otherwise hand an agent the other identity's documents while
/// every database filter stayed perfectly correct — a leak below the level any
/// query can see. The check is against the CANONICAL root, so a symlinked vault
/// root is handled too.
pub fn walk(root: &Path) -> Result<Vec<String>, WikiError> {
    let canonical = root.canonicalize().map_err(|error| WikiError::Read {
        path: root.display().to_string(),
        detail: error.to_string(),
    })?;
    let mut found = Vec::new();
    let mut stack = vec![canonical.clone()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A directory that vanished or is unreadable is skipped, not fatal:
            // a vault is a live directory an app and a sync client are both
            // writing to, so a mid-walk change must not abort the pass.
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            // Resolve before deciding, so a symlink is judged by where it
            // actually goes rather than by where it sits.
            let Ok(real) = path.canonicalize() else {
                continue;
            };
            if !real.starts_with(&canonical) {
                continue; // escapes the vault
            }
            if real.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                    stack.push(real);
                }
                continue;
            }
            if !name.to_ascii_lowercase().ends_with(".md") || name.starts_with('.') {
                continue;
            }
            if let Ok(relative) = real.strip_prefix(&canonical) {
                found.push(to_slash(relative));
            }
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

fn to_slash(path: &Path) -> String {
    path.components()
        .filter_map(|part| match part {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Parse a page: frontmatter, tags, wikilinks, body.
pub fn parse(path: &str, raw: &str, source_mtime: i64) -> Document {
    let normalized = raw.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let (front, body) = split_frontmatter(&normalized);
    let meta = parse_frontmatter(front);
    let stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".md")
        .trim_end_matches(".MD");
    // Obsidian's own convention is that the FILENAME is the page's identity and
    // its title; a `title:` key is an override some templates add.
    let title = meta
        .get("title")
        .or_else(|| meta.get("name"))
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| stem.to_string());
    let mut tags = list_value(&meta, "tags");
    tags.extend(inline_tags(body));
    tags.sort();
    tags.dedup();
    Document {
        path: path.to_string(),
        title,
        aliases: list_value(&meta, "aliases"),
        tags,
        links: links(body),
        body: body.to_string(),
        source_mtime,
    }
}

fn split_frontmatter(raw: &str) -> (&str, &str) {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return ("", raw);
    };
    match rest.split_once("\n---\n") {
        Some((front, body)) => (front, body),
        // An unterminated fence is prose that happens to start with `---`.
        None => ("", raw),
    }
}

/// Frontmatter as a flat map. Values may be scalars or inline/block lists; both
/// are kept as the raw right-hand side and split by [`list_value`].
fn parse_frontmatter(front: &str) -> BTreeMap<String, String> {
    let mut meta = BTreeMap::new();
    let mut key: Option<String> = None;
    let mut collected: Vec<String> = Vec::new();
    /// Commit a pending block list under the key that introduced it.
    fn flush(
        meta: &mut BTreeMap<String, String>,
        key: &mut Option<String>,
        collected: &mut Vec<String>,
    ) {
        if let (Some(name), false) = (key.take(), collected.is_empty()) {
            meta.insert(name, collected.join(","));
            collected.clear();
        }
    }
    for line in front.lines() {
        let trimmed = line.trim();
        // A block-list item belongs to the key above it.
        if let (Some(item), true) = (trimmed.strip_prefix("- "), key.is_some()) {
            collected.push(unquote(item).to_string());
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.starts_with([' ', '\t']) {
            continue; // nested; not addressable by a flat map
        }
        flush(&mut meta, &mut key, &mut collected);
        let name = name.trim().to_string();
        let value = unquote(value.trim()).to_string();
        if value.is_empty() {
            key = Some(name); // a block list follows
        } else {
            meta.insert(name, value);
        }
    }
    flush(&mut meta, &mut key, &mut collected);
    meta
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn list_value(meta: &BTreeMap<String, String>, key: &str) -> Vec<String> {
    let Some(raw) = meta.get(key) else {
        return Vec::new();
    };
    let raw = raw.trim().trim_start_matches('[').trim_end_matches(']');
    let mut values: Vec<String> = raw
        .split(',')
        .map(|value| unquote(value).trim().trim_start_matches('#').to_string())
        .filter(|value| !value.is_empty())
        .collect();
    values.sort();
    values.dedup();
    values
}

/// Inline `#tags`, skipping headings, code fences and URL fragments.
fn inline_tags(body: &str) -> Vec<String> {
    let mut tags = Vec::new();
    for line in CodeAware::new(body) {
        // An ATX heading is `# Heading`, not a tag; the difference is the space.
        for (index, _) in line.match_indices('#') {
            let before = line[..index].chars().next_back();
            // A tag starts a word, and `#` immediately after a non-space is
            // either a URL fragment or a heading continuation.
            if before.is_some_and(|ch| !ch.is_whitespace()) {
                continue;
            }
            let rest = &line[index + 1..];
            let value: String = rest
                .chars()
                .take_while(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '/'))
                .collect();
            // `# ` is a heading: the character after # must be tag-legal.
            if value.is_empty() || value.chars().next().is_some_and(|c| c.is_numeric()) {
                continue;
            }
            tags.push(value);
        }
    }
    tags
}

/// Wikilinks in all four Obsidian forms.
fn links(body: &str) -> Vec<Link> {
    let mut found = Vec::new();
    let bytes = body.as_bytes();
    let mut index = 0;
    while let Some(open) = body[index..].find("[[") {
        let open = index + open;
        let Some(close) = body[open..].find("]]") else {
            break;
        };
        let close = open + close;
        let inner = &body[open + 2..close];
        index = close + 2;
        if inner.is_empty() || inner.contains('\n') {
            continue;
        }
        let embed = open >= 1 && bytes[open - 1] == b'!';
        let (target, alias) = match inner.split_once('|') {
            Some((target, alias)) => (target, Some(alias.trim().to_string())),
            None => (inner, None),
        };
        let (target, heading) = match target.split_once('#') {
            Some((target, heading)) => (target, Some(heading.trim().to_string())),
            None => (target, None),
        };
        let target = target.trim();
        // `[[#Heading]]` is a link within the same page, not to another page.
        if target.is_empty() {
            continue;
        }
        found.push(Link {
            target: target.to_string(),
            heading,
            alias,
            embed,
        });
    }
    found
}

/// Resolve link targets to vault paths, Obsidian's way.
///
/// Obsidian matches a link by FILENAME anywhere in the vault, preferring the
/// shortest path when a name is ambiguous. So `[[DNS]]` finds
/// `Runbooks/Network/DNS.md` without being told where it is — which is why
/// link resolution needs an index of the whole vault rather than a path join.
/// A target may also be written as a full relative path, with or without `.md`.
pub fn link_index(paths: &[String]) -> BTreeMap<String, String> {
    let mut index: BTreeMap<String, String> = BTreeMap::new();
    for path in paths {
        let stem = path
            .rsplit('/')
            .next()
            .unwrap_or(path)
            .trim_end_matches(".md");
        for key in [
            stem.to_string(),
            path.clone(),
            path.trim_end_matches(".md").to_string(),
        ] {
            let key = key.to_ascii_lowercase();
            match index.get(&key) {
                // Shortest path wins, then lexicographic, so resolution is
                // stable rather than dependent on directory read order.
                Some(existing)
                    if (existing.matches('/').count(), existing.as_str())
                        <= (path.matches('/').count(), path.as_str()) => {}
                _ => {
                    index.insert(key, path.clone());
                }
            }
        }
    }
    index
}

/// Iterate a body's lines, skipping fenced code blocks.
///
/// Needed because a technical wiki is full of `# comment` lines inside shell
/// blocks. Treating those as headings splits a page at every comment in every
/// snippet, which is both wrong and unstable — the chunk boundaries would move
/// whenever someone edited a code sample.
struct CodeAware<'a> {
    lines: std::str::Lines<'a>,
    fence: Option<String>,
}

impl<'a> CodeAware<'a> {
    fn new(body: &'a str) -> Self {
        Self {
            lines: body.lines(),
            fence: None,
        }
    }
}

impl<'a> Iterator for CodeAware<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        for line in self.lines.by_ref() {
            let trimmed = line.trim_start();
            let marker = if trimmed.starts_with("```") {
                Some("```")
            } else if trimmed.starts_with("~~~") {
                Some("~~~")
            } else {
                None
            };
            match (&self.fence, marker) {
                (None, Some(marker)) => {
                    self.fence = Some(marker.to_string());
                    continue;
                }
                (Some(open), Some(marker)) if open == marker => {
                    self.fence = None;
                    continue;
                }
                (Some(_), _) => continue,
                (None, None) => return Some(line),
            }
        }
        None
    }
}

/// A heading and the body beneath it, down to the next heading of the same or
/// higher level.
struct Section {
    /// Enclosing headings, outermost first, including this one.
    path: Vec<String>,
    start: usize,
    end: usize,
}

/// Split a body into heading-delimited sections, in document order.
fn sections(document: &Document) -> Vec<Section> {
    let body = &document.body;
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut found: Vec<Section> = Vec::new();
    let mut offset = 0usize;
    let mut open: Option<Section> = None;
    let mut fence: Option<&str> = None;

    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let marker = if trimmed.starts_with("```") {
            Some("```")
        } else if trimmed.starts_with("~~~") {
            Some("~~~")
        } else {
            None
        };
        match (fence, marker) {
            (None, Some(m)) => fence = Some(m),
            (Some(f), Some(m)) if f == m => fence = None,
            _ => {}
        }
        let heading = (fence.is_none()).then(|| atx(trimmed)).flatten();
        if let Some((level, text)) = heading {
            if let Some(mut previous) = open.take() {
                previous.end = offset;
                found.push(previous);
            }
            stack.retain(|(existing, _)| *existing < level);
            stack.push((level, text));
            let mut path = vec![document.title.clone()];
            // Obsidian pages very often repeat the page name as their H1, so
            // `RabbitMQ.md` starting with `# RabbitMQ` would give every chunk a
            // breadcrumb of "RabbitMQ > RabbitMQ > ...". The duplicate costs
            // tokens in every embedding and tells a reader nothing.
            path.extend(
                stack
                    .iter()
                    .map(|(_, text)| text.clone())
                    .filter(|text| !text.eq_ignore_ascii_case(&document.title)),
            );
            open = Some(Section {
                path,
                start: offset + line.len(),
                end: body.len(),
            });
        } else if open.is_none() && !trimmed.is_empty() {
            // Prose before the first heading still belongs to the page.
            open = Some(Section {
                path: vec![document.title.clone()],
                start: offset,
                end: body.len(),
            });
        }
        offset += line.len();
    }
    if let Some(mut last) = open {
        last.end = body.len();
        found.push(last);
    }
    found
}

/// `## Heading` -> `(2, "Heading")`. Only H1–H6, and only with a space, so a
/// `#tag` at line start is not mistaken for a heading.
fn atx(line: &str) -> Option<(usize, String)> {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    let text = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('\t'))?;
    let text = text.trim().trim_end_matches('#').trim();
    (!text.is_empty()).then(|| (hashes, text.to_string()))
}

/// Split a page into embeddable chunks.
pub fn chunk(document: &Document, params: ChunkParams) -> Vec<Chunk> {
    let body = &document.body;
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut pending: Option<Chunk> = None;

    for section in sections(document) {
        let text = body[section.start..section.end].trim();
        if text.is_empty() {
            continue;
        }
        let start = section.start + leading_ws(&body[section.start..section.end]);
        // A heading with almost nothing under it is noise as its own record, so
        // it merges forward into the next chunk. The merged section's own
        // heading stays in the chunk TEXT (the range spans it), so it is still
        // searchable; the breadcrumb is the earlier section's, because that is
        // where the chunk starts.
        if let Some(previous) = pending.take() {
            if previous.text.len() < params.min
                && previous.text.len() + text.len() <= params.max
                && previous.end <= start
            {
                let merged = body[previous.start..section.end].trim();
                pending = Some(Chunk {
                    index: 0,
                    heading_path: previous.heading_path,
                    text: merged.to_string(),
                    start: previous.start,
                    end: previous.start + merged.len(),
                });
                continue;
            }
            chunks.push(previous);
        }
        if text.len() <= params.target {
            pending = Some(Chunk {
                index: 0,
                heading_path: section.path.clone(),
                text: text.to_string(),
                start,
                end: start + text.len(),
            });
            continue;
        }
        for piece in split_long(text, start, &section.path, params) {
            chunks.push(piece);
        }
    }
    if let Some(last) = pending {
        chunks.push(last);
    }
    for (index, chunk) in chunks.iter_mut().enumerate() {
        chunk.index = index;
    }
    chunks
}

fn leading_ws(value: &str) -> usize {
    value.len() - value.trim_start().len()
}

/// Break an over-long section on paragraph boundaries, with overlap.
///
/// Paragraphs first, because a mid-sentence cut costs more retrieval quality
/// than an uneven chunk size does. A single paragraph longer than `max` is cut
/// at a whitespace boundary rather than mid-word.
fn split_long(text: &str, base: usize, path: &[String], params: ChunkParams) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        let remaining = &text[cursor..];
        if remaining.trim().is_empty() {
            break;
        }
        let mut take = params.target.min(remaining.len());
        if take < remaining.len() {
            // Prefer a paragraph break inside the window, then any whitespace.
            take = remaining[..take]
                .rfind("\n\n")
                .map(|at| at + 2)
                .or_else(|| remaining[..take.min(params.max)].rfind(char::is_whitespace))
                .filter(|at| *at > params.overlap)
                .unwrap_or(take);
            take = ceil_boundary(remaining, take);
        }
        let piece = remaining[..take].trim();
        if !piece.is_empty() {
            let offset = leading_ws(&remaining[..take]);
            chunks.push(Chunk {
                index: 0,
                heading_path: path.to_vec(),
                text: piece.to_string(),
                start: base + cursor + offset,
                end: base + cursor + offset + piece.len(),
            });
        }
        if take >= remaining.len() {
            break;
        }
        // Step forward by the chunk minus the overlap, never by zero.
        let step = take.saturating_sub(params.overlap).max(1);
        cursor += ceil_boundary(remaining, step);
    }
    chunks
}

/// Round an index up to the next char boundary, so slicing never panics on
/// multi-byte text.
fn ceil_boundary(value: &str, mut index: usize) -> usize {
    if index >= value.len() {
        return value.len();
    }
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// `<vault-relative path>` -> an absolute path under `root`, for reading.
pub fn absolute(root: &Path, relative: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        path.push(part);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_tags_aliases_and_title() {
        let doc = parse(
            "Runbooks/DNS.md",
            "---\ntitle: DNS Failover\naliases:\n  - dns\n  - \"name service\"\ntags: [network, ops]\n---\n\
             Body about #bind9 and #infra/dns.\n",
            7,
        );
        assert_eq!(doc.title, "DNS Failover");
        assert_eq!(doc.aliases, ["dns", "name service"]);
        // frontmatter and inline tags merge, deduped and sorted
        assert_eq!(doc.tags, ["bind9", "infra/dns", "network", "ops"]);
        assert_eq!(doc.source_mtime, 7);
        assert!(doc.body.starts_with("Body about"));
    }

    /// The filename is the page's identity in Obsidian, so it is the title
    /// unless a template overrode it.
    #[test]
    fn the_filename_is_the_default_title() {
        let doc = parse("Clients/Acme Corp.md", "no frontmatter here\n", 0);
        assert_eq!(doc.title, "Acme Corp");
        assert_eq!(doc.body, "no frontmatter here\n");

        // an unterminated fence is prose, not frontmatter
        let doc = parse("x.md", "---\nnot really frontmatter\n", 0);
        assert!(doc.body.starts_with("---"));
    }

    #[test]
    fn all_four_wikilink_forms_parse() {
        let doc = parse(
            "a.md",
            "See [[DNS]], [[DNS|the DNS page]], [[DNS#Failover]] and ![[Diagram]].\n\
             Not a link: [single] or [[unclosed\n",
            0,
        );
        assert_eq!(doc.links.len(), 4);
        assert_eq!(doc.links[0].target, "DNS");
        assert_eq!(doc.links[1].alias.as_deref(), Some("the DNS page"));
        assert_eq!(doc.links[2].heading.as_deref(), Some("Failover"));
        assert!(doc.links[3].embed, "![[...]] is a transclusion");
        assert!(!doc.links[0].embed);
    }

    /// `[[#Heading]]` is an intra-page jump and names no document.
    #[test]
    fn a_same_page_anchor_is_not_a_document_link() {
        let doc = parse("a.md", "jump to [[#Later]] and [[Other#Bit]]\n", 0);
        assert_eq!(doc.links.len(), 1);
        assert_eq!(doc.links[0].target, "Other");
    }

    /// Obsidian resolves a link by filename anywhere in the vault, shortest
    /// path winning — which is why this needs a vault-wide index rather than a
    /// path join.
    #[test]
    fn links_resolve_by_filename_anywhere_in_the_vault() {
        let paths = vec![
            "Runbooks/Network/DNS.md".to_string(),
            "Archive/Old/Deep/DNS.md".to_string(),
            "Clients/Acme.md".to_string(),
        ];
        let index = link_index(&paths);
        // the shallower path wins the bare name
        assert_eq!(index["dns"], "Runbooks/Network/DNS.md");
        // a full path still resolves, with or without the extension
        assert_eq!(index["archive/old/deep/dns.md"], "Archive/Old/Deep/DNS.md");
        assert_eq!(index["archive/old/deep/dns"], "Archive/Old/Deep/DNS.md");
        assert_eq!(index["acme"], "Clients/Acme.md");
    }

    /// A technical wiki is full of `# comment` inside shell blocks. Treating
    /// those as headings would split a page at every comment in every snippet,
    /// and move the boundaries whenever a sample was edited.
    #[test]
    fn headings_and_tags_inside_code_fences_are_ignored() {
        let doc = parse(
            "a.md",
            "# Real Heading\n\nprose\n\n```bash\n# not a heading\n#nottag\nsudo x\n```\n\n## Second\n\nmore\n",
            0,
        );
        assert_eq!(doc.tags, Vec::<String>::new(), "no tags inside the fence");
        let chunks = chunk(&doc, ChunkParams::default());
        let crumbs: Vec<String> = chunks.iter().map(|c| c.breadcrumb()).collect();
        assert!(
            crumbs.iter().all(|c| !c.contains("not a heading")),
            "a fenced comment became a heading: {crumbs:?}"
        );
        // the fenced content is still indexed, as part of its section
        assert!(
            chunks.iter().any(|c| c.text.contains("sudo x")),
            "fenced code must still be searchable"
        );
    }

    #[test]
    fn every_chunk_carries_its_breadcrumb_into_the_embedding() {
        let doc = parse(
            "Runbooks/RabbitMQ.md",
            "# Operations\n\n## Failover\n\nRestart the broker and clear the queue.\n",
            0,
        );
        let chunks = chunk(&doc, ChunkParams::default());
        let last = chunks.last().expect("a chunk");
        assert_eq!(
            last.heading_path,
            ["RabbitMQ", "Operations", "Failover"],
            "the page title leads the breadcrumb"
        );
        let embedded = last.embedding_text();
        assert!(
            embedded.starts_with("RabbitMQ > Operations > Failover"),
            "a chunk retrieved alone must say what it is about: {embedded:?}"
        );
        assert!(embedded.contains("Restart the broker"));
    }

    #[test]
    fn a_long_section_splits_on_paragraphs_with_overlap() {
        let paragraph = "Each paragraph here is long enough to matter and is repeated. ".repeat(12);
        let body = format!("# Big\n\n{paragraph}\n\n{paragraph}\n\n{paragraph}\n");
        let doc = parse("Big.md", &body, 0);
        let params = ChunkParams::default();
        let chunks = chunk(&doc, params);

        assert!(chunks.len() > 1, "an over-long section must split");
        for piece in &chunks {
            assert!(
                piece.text.len() <= params.max,
                "chunk of {} exceeds max {}",
                piece.text.len(),
                params.max
            );
            assert!(!piece.text.is_empty());
            // Not ["Big", "Big"]: an H1 repeating the page name is collapsed,
            // which is the common Obsidian shape.
            assert_eq!(piece.heading_path, ["Big"]);
        }
        // indexes are contiguous from zero, which the indexer relies on to prune
        // the tail of a shrunk document
        let indexes: Vec<usize> = chunks.iter().map(|c| c.index).collect();
        assert_eq!(indexes, (0..chunks.len()).collect::<Vec<_>>());
        // byte ranges point back into the body
        for piece in &chunks {
            assert_eq!(&doc.body[piece.start..piece.end], piece.text);
        }
    }

    /// A heading with one line under it is noise as its own record.
    #[test]
    fn a_tiny_section_merges_forward_instead_of_becoming_a_chunk() {
        let doc = parse(
            "a.md",
            "## Notes\n\nshort\n\n## Detail\n\nThis section has considerably more substance to it.\n",
            0,
        );
        let chunks = chunk(&doc, ChunkParams::default());
        assert_eq!(chunks.len(), 1, "expected a merge, got {chunks:?}");
        assert!(chunks[0].text.contains("short"));
        assert!(chunks[0].text.contains("more substance"));
    }

    /// Obsidian pages routinely repeat the page name as their H1. Without
    /// collapsing it, every chunk's breadcrumb reads "RabbitMQ > RabbitMQ > ..."
    /// — tokens spent in every embedding to say nothing.
    #[test]
    fn an_h1_repeating_the_page_name_is_not_doubled() {
        let doc = parse(
            "RabbitMQ.md",
            "# RabbitMQ\n\n## Queues\n\nDurable queues and the dead-letter exchange, mirrored.\n",
            0,
        );
        let chunks = chunk(&doc, ChunkParams::default());
        let last = chunks.last().expect("a chunk");
        assert_eq!(last.heading_path, ["RabbitMQ", "Queues"]);

        // a heading that merely CONTAINS the title is not collapsed
        let doc = parse(
            "DNS.md",
            "# DNS Failover\n\nSome substantial content here for the section.\n",
            0,
        );
        let chunks = chunk(&doc, ChunkParams::default());
        assert_eq!(chunks.last().unwrap().heading_path, ["DNS", "DNS Failover"]);
    }

    #[test]
    fn prose_before_the_first_heading_is_not_lost() {
        let doc = parse("a.md", "An intro paragraph with no heading at all.\n", 0);
        let chunks = chunk(&doc, ChunkParams::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading_path, ["a"]);
        assert!(chunks[0].text.contains("An intro paragraph"));
    }

    #[test]
    fn multibyte_text_never_splits_mid_character() {
        let unit = "função de configuração — açúcar, ação, coração. ".repeat(40);
        let doc = parse("pt.md", &format!("# Título\n\n{unit}\n"), 0);
        let chunks = chunk(&doc, ChunkParams::default());
        assert!(chunks.len() > 1);
        for piece in &chunks {
            // slicing the body by the recorded range must be valid UTF-8
            assert_eq!(&doc.body[piece.start..piece.end], piece.text);
        }
    }

    #[test]
    fn an_empty_or_whitespace_page_yields_no_chunks() {
        for raw in ["", "\n\n   \n", "---\ntitle: x\n---\n\n"] {
            assert!(
                chunk(&parse("e.md", raw, 0), ChunkParams::default()).is_empty(),
                "expected no chunks for {raw:?}"
            );
        }
    }

    // ---- walking ---------------------------------------------------------

    struct Vault(PathBuf);

    impl Vault {
        fn new(name: &str, files: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "engram-wiki-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            for (path, body) in files {
                let full = dir.join(path);
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(full, body).unwrap();
            }
            Self(dir)
        }
    }

    impl Drop for Vault {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn the_walk_recurses_and_skips_what_the_editors_own() {
        let vault = Vault::new(
            "walk",
            &[
                ("Runbooks/Network/DNS.md", "# DNS\n"),
                ("Clients/Acme.md", "# Acme\n"),
                ("README.md", "# Top\n"),
                (".obsidian/workspace.json", "{}"),
                (".obsidian/notes.md", "# config note\n"),
                (".trash/Deleted.md", "# deleted\n"),
                // SilverBullet (the browser editor over the same vault) writes
                // exactly this one internal file — a dotfile, verified against a
                // live instance. It must never be indexed; if it were, its auth
                // state would surface in wiki_search. It is caught by the dotfile
                // rule, not SKIP_DIRS, so this pins that the rule covers it.
                (".silverbullet.auth.json", "{\"token\":\"secret\"}"),
                ("assets/diagram.png", "notpng"),
                ("Notes/draft.txt", "not markdown"),
            ],
        );
        let found = walk(&vault.0).unwrap();
        assert_eq!(
            found,
            ["Clients/Acme.md", "README.md", "Runbooks/Network/DNS.md"],
            "unexpected walk result: {found:?}"
        );
    }

    /// The filesystem-level half of tenant isolation. A symlink out of the vault
    /// would hand an agent another identity's documents while every database
    /// filter stayed correct.
    #[test]
    #[cfg(unix)]
    fn the_walk_refuses_to_leave_the_vault() {
        let inside = Vault::new("walk-inside", &[("Own.md", "# own\n")]);
        let outside = Vault::new("walk-outside", &[("Secret.md", "# secret\n")]);
        std::os::unix::fs::symlink(&outside.0, inside.0.join("leak")).unwrap();
        std::os::unix::fs::symlink(outside.0.join("Secret.md"), inside.0.join("AlsoSecret.md"))
            .unwrap();

        let found = walk(&inside.0).unwrap();
        assert_eq!(found, ["Own.md"], "the walk escaped the vault: {found:?}");
    }

    #[test]
    fn a_missing_vault_is_an_error_not_an_empty_vault() {
        // An empty result would look exactly like "the vault has no pages",
        // which is how a misconfigured path becomes silence.
        let absent = std::env::temp_dir().join("engram-wiki-definitely-not-here");
        assert!(walk(&absent).is_err());
    }
}
