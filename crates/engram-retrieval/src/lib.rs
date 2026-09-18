use engram_store::Memory;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub file: String,
    pub score: f64,
}

pub fn bm25(memories: &[Memory], query: &str, limit: usize) -> Vec<Hit> {
    let documents: Vec<Vec<String>> = memories.iter().map(tokens).collect();
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
                file: memories[index].file.clone(),
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

fn tokens(memory: &Memory) -> Vec<String> {
    tokenize(&format!(
        "{} {} {}",
        memory.name, memory.description, memory.body
    ))
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
            },
            Memory {
                file: "other.md".into(),
                name: "Other".into(),
                description: "unrelated".into(),
                memory_type: "reference".into(),
                body: "".into(),
            },
        ];
        assert_eq!(bm25(&memories, "IPv6 DNS", 1)[0].file, "dns.md");
    }
}
