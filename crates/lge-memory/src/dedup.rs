//! Deciding whether a new fact repeats one already stored, without a model.
//!
//! The ladder follows graphiti's entity dedup: an exact match after
//! normalization, then a gate that keeps short or low-information texts out
//! of fuzzy matching, then character-shingle similarity at a high threshold.

use std::collections::BTreeSet;

/// Two facts at or above this shingle similarity say the same thing.
pub const SAME_FACT: f64 = 0.9;

/// Lower-case, with every run of whitespace as one space and without
/// trailing punctuation. Two texts that normalize equally are the same fact.
pub fn normalize(text: &str) -> String {
    let joined: String = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    joined.trim_end_matches(['.', '!', ';', ',']).to_string()
}

/// Shannon entropy of the characters, in bits per character.
fn entropy(text: &str) -> f64 {
    let mut counts = std::collections::BTreeMap::new();
    let mut total = 0.0;
    for c in text.chars() {
        *counts.entry(c).or_insert(0.0) += 1.0;
        total += 1.0;
    }
    counts.values().map(|n: &f64| -(n / total) * (n / total).log2()).sum()
}

/// A text too short or too repetitive to compare by similarity: one changed
/// character could change what it means. Such a text only matches exactly.
pub fn too_small_for_fuzzy(normalized: &str) -> bool {
    let words = normalized.split(' ').filter(|w| !w.is_empty()).count();
    (normalized.chars().count() < 6 && words < 2) || entropy(normalized) < 1.5
}

fn shingles(normalized: &str) -> BTreeSet<[char; 3]> {
    let chars: Vec<char> = normalized.chars().collect();
    chars.windows(3).map(|w| [w[0], w[1], w[2]]).collect()
}

/// Jaccard similarity of the 3-character shingles of two normalized texts.
pub fn similarity(a: &str, b: &str) -> f64 {
    let (sa, sb) = (shingles(a), shingles(b));
    let inter = sa.intersection(&sb).count();
    let union = sa.len() + sb.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// True when `candidate` states the same fact as `new`. Both are raw texts.
pub fn same_fact(new: &str, candidate: &str) -> bool {
    let (a, b) = (normalize(new), normalize(candidate));
    if a == b {
        return true;
    }
    if too_small_for_fuzzy(&a) || too_small_for_fuzzy(&b) {
        return false;
    }
    similarity(&a, &b) >= SAME_FACT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_after_normalization() {
        assert!(same_fact(
            "Tokens expire  after 15 minutes.",
            "tokens expire after 15 minutes"
        ));
        assert_eq!(normalize("  A\tB\n c;  "), "a b c");
    }

    #[test]
    fn near_duplicates_match_and_different_facts_do_not() {
        let base = "The orders service publishes order.created to RabbitMQ after the row is committed";
        assert!(same_fact(
            base,
            "The orders service publishes order.created to RabbitMQ after the row is committed."
        ));
        assert!(same_fact(
            base,
            "The orders service publishes order.created to RabbitMQ after the row is commited"
        ));
        assert!(!same_fact(
            base,
            "The orders service consumes order.created from RabbitMQ before the row is committed"
        ));
        assert!(
            !same_fact("Tokens expire after 15 minutes", "Tokens expire after 45 minutes"),
            "one changed number is a different fact"
        );
        assert!(!same_fact(base, "Use pnpm, not npm, in web_gateway"));
    }

    #[test]
    fn short_or_repetitive_texts_only_match_exactly() {
        assert!(too_small_for_fuzzy("abc"));
        assert!(too_small_for_fuzzy("aaaaaaaaaaaa"));
        assert!(!too_small_for_fuzzy("use pnpm in the gateway"));
        assert!(!same_fact("v1", "v2"));
        assert!(same_fact("v1", "V1"));
        assert_eq!(similarity("", ""), 0.0);
    }
}
