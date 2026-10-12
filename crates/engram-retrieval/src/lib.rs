use engram_store::Memory;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub file: String,
    pub score: f64,
}

pub fn rrf(rankings: &[Vec<String>], limit: usize, k: f64) -> Vec<Hit> {
    let mut scores: HashMap<String, f64> = HashMap::new();
    for ranking in rankings {
        for (position, file) in ranking.iter().enumerate() {
            *scores.entry(file.clone()).or_default() += 1.0 / (k + position as f64 + 1.0);
        }
    }
    let mut hits: Vec<Hit> = scores
        .into_iter()
        .map(|(file, score)| Hit { file, score })
        .collect();
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.file.cmp(&right.file))
    });
    hits.truncate(limit);
    hits
}

/// Anything BM25 can rank: an id to return, and the text to score.
///
/// Introduced so wiki chunks reuse the ranking rather than getting a second,
/// subtly different implementation. `Memory` implements it and the memory path
/// is unchanged — `bm25` is still the entry point for it.
pub trait Indexable {
    /// What the hit identifies. A memory filename, or `path#chunk` for a chunk.
    fn id(&self) -> String;
    /// Everything searchable about the record, concatenated.
    fn text(&self) -> String;
}

impl Indexable for Memory {
    fn id(&self) -> String {
        self.file.clone()
    }
    fn text(&self) -> String {
        format!("{} {} {}", self.name, self.description, self.body)
    }
}

pub fn bm25(memories: &[Memory], query: &str, limit: usize) -> Vec<Hit> {
    rank(memories, query, limit)
}

/// BM25 over any [`Indexable`] collection.
pub fn rank<T: Indexable>(records: &[T], query: &str, limit: usize) -> Vec<Hit> {
    let documents: Vec<Vec<String>> = records.iter().map(|r| tokenize(&r.text())).collect();
    let terms = tokenize(query);
    if terms.is_empty() || documents.is_empty() {
        return Vec::new();
    }
    let average = documents.iter().map(Vec::len).sum::<usize>() as f64 / documents.len() as f64;
    let mut frequency = HashMap::new();
    for document in &documents {
        for term in document.iter().collect::<HashSet<_>>() {
            *frequency.entry(term.as_str()).or_insert(0usize) += 1;
        }
    }
    let mut hits: Vec<Hit> = documents
        .iter()
        .enumerate()
        .map(|(index, document)| {
            let mut score = 0.0;
            for term in &terms {
                let count = document.iter().filter(|word| *word == term).count() as f64;
                if count == 0.0 {
                    continue;
                }
                let docs = *frequency.get(term.as_str()).unwrap_or(&0) as f64;
                let idf = ((documents.len() as f64 - docs + 0.5) / (docs + 0.5) + 1.0).ln();
                score += idf * count * 2.2
                    / (count + 1.2 * (1.0 - 0.75 + 0.75 * document.len() as f64 / average));
            }
            Hit {
                file: records[index].id(),
                score,
            }
        })
        .filter(|hit| hit.score > 0.0)
        .collect();
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.file.cmp(&right.file))
    });
    hits.truncate(limit);
    hits
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .filter(|part| part.len() > 1)
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_exact_infrastructure_terms() {
        let memories = vec![
            Memory {
                file: "dns.md".into(),
                name: "DNS".into(),
                description: "AdGuard ULA".into(),
                memory_type: "reference".into(),
                body: "IPv6 DNS server".into(),
                source_mtime: 0,
            },
            Memory {
                file: "other.md".into(),
                name: "Other".into(),
                description: "unrelated".into(),
                memory_type: "reference".into(),
                body: "".into(),
                source_mtime: 0,
            },
        ];
        assert_eq!(bm25(&memories, "IPv6 DNS", 1)[0].file, "dns.md");
    }
    #[test]
    fn rrf_rewards_consensus() {
        assert_eq!(
            rrf(&[vec!["a".into(), "b".into()], vec!["b".into()]], 2, 60.0)[0].file,
            "b"
        );
    }
}
