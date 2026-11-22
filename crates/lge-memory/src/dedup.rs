//! Deciding whether a new fact repeats one already stored, without a model.
//!
//! The ladder follows graphiti's entity dedup: an exact match after
//! normalization, then a gate that keeps short or low-information texts out
//! of fuzzy matching, then character-shingle similarity at a high threshold.
//!
//! Before any of these, a guard compares the numbers, identifiers, acronym
//! case and negations of the two raw texts: when one of them differs, the
//! texts state different facts however alike they read. This keeps a changed
//! value from being folded into the old fact, so supersede-by-key can close
//! the old fact and the one recalled is the current one.

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

/// The values of a text that decide what it claims, each list sorted so that
/// two lists are equal exactly when they hold the same values the same number
/// of times.
#[derive(Debug, Default, PartialEq, Eq)]
struct Guard {
    numbers: Vec<String>,
    idents: Vec<String>,
    negations: Vec<String>,
}

const NEGATIONS: [&str; 8] = ["not", "no", "never", "none", "without", "cannot", "disabled", "removed"];

/// An identifier: a name with `_`, `/` or an inner dot, camelCase, or an
/// acronym of at least two capitals. Its exact spelling matters.
fn is_ident(t: &str) -> bool {
    let chars: Vec<char> = t.chars().collect();
    let inner_dot = chars.windows(3).any(|w| w[1] == '.' && w[0] != '.' && w[2] != '.');
    let camel = chars.windows(2).any(|w| w[0].is_lowercase() && w[1].is_uppercase());
    let acronym = chars.len() >= 2 && chars.iter().all(char::is_ascii_uppercase);
    t.contains(['_', '/']) || inner_dot || camel || acronym
}

/// Collect the numbers, identifiers and negations of a raw text. The raw text
/// is used, not the normalized one, because normalizing lower-cases it.
fn guard_tokens(raw: &str) -> Guard {
    let mut guard = Guard::default();
    for piece in raw.split_whitespace() {
        let t = piece
            .trim_start_matches(['"', '\'', '`', '(', '[', '{', '<'])
            .trim_end_matches(['"', '\'', '`', ')', ']', '}', '>', '.', ',', ';', ':', '!', '?']);
        if t.is_empty() {
            continue;
        }
        if t.chars().any(|c| c.is_ascii_digit()) {
            guard.numbers.push(t.to_lowercase());
        } else if is_ident(t) {
            guard.idents.push(t.to_string());
        } else {
            let lower = t.to_lowercase();
            if NEGATIONS.contains(&lower.as_str()) {
                guard.negations.push(lower);
            } else if lower.ends_with("n't") {
                guard.negations.push("not".to_string());
            }
        }
    }
    guard.numbers.sort();
    guard.idents.sort();
    guard.negations.sort();
    guard
}

/// True when two raw texts carry the same numbers, identifiers (with their
/// case) and negations. A text with none of them agrees with any other such
/// text, so prose without values is judged by similarity alone.
pub fn guard_agrees(a: &str, b: &str) -> bool {
    guard_tokens(a) == guard_tokens(b)
}

/// True when `candidate` states the same fact as `new`. Both are raw texts.
/// A differing number, identifier, acronym case or negation says no first.
pub fn same_fact(new: &str, candidate: &str) -> bool {
    if !guard_agrees(new, candidate) {
        return false;
    }
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

    fn facts_105(tail: &str) -> String {
        format!(
            "The nightly export job writes its summary to the shared bucket and then notifies the team channel {tail}"
        )
    }

    #[test]
    fn a_changed_number_is_a_different_fact_though_the_texts_are_close() {
        let pairs = [
            (
                "The gateway drops idle upstream connections and returns a clear error to the client after 15 seconds of silence",
                "The gateway drops idle upstream connections and returns a clear error to the client after 45 seconds of silence",
            ),
            (
                "The gateway drops idle upstream connections and returns a clear error to the client after 900 seconds of silence",
                "The gateway drops idle upstream connections and returns a clear error to the client after 600 seconds of silence",
            ),
            (&facts_105("memo 1"), &facts_105("memo 2")),
        ];
        for (a, b) in pairs {
            assert!(similarity(&normalize(a), &normalize(b)) >= SAME_FACT, "{a}");
            assert!(!same_fact(a, b), "{a} / {b}");
        }
    }

    #[test]
    fn repeats_that_only_differ_in_form_still_match() {
        let base = facts_105("memo 1");
        assert!(same_fact(&base, &base));
        assert!(same_fact(&base, &format!("  {}.  ", base.replace(' ', "  "))));
        assert!(same_fact(
            "Tokens expire after 15 minutes.",
            "tokens expire after 15 minutes"
        ));
        assert!(same_fact("v1", "V1"));
        assert!(same_fact("0xAB", "0xab"));
    }

    #[test]
    fn acronym_case_and_negation_change_the_fact() {
        assert!(!same_fact(
            "TLS 1.3 is required for the gateway",
            "tls 1.3 is required for the gateway"
        ));
        assert!(!same_fact(
            "Use pnpm in web_gateway",
            "Use pnpm, not npm, in web_gateway"
        ));
        assert!(!same_fact(
            "The worker doesn't retry a failed upload to the bucket",
            "The worker retries a failed upload to the bucket"
        ));
        assert!(!same_fact("Publish order.created", "Publish order_created"));
    }

    #[test]
    fn matching_identifiers_and_typos_without_values_still_match() {
        assert!(same_fact(
            "The orders service publishes to RabbitMQ after the row is committed",
            "The orders service publishes to RabbitMQ after the row is commited"
        ));
    }

    #[test]
    fn an_inserted_negation_alone_makes_a_different_fact() {
        let a = "The gateway should retry the failed request to the payments upstream after a short delay";
        let b = "The gateway should not retry the failed request to the payments upstream after a short delay";
        assert!(
            similarity(&normalize(a), &normalize(b)) >= SAME_FACT,
            "close enough to merge"
        );
        assert!(!same_fact(a, b));
        assert!(!same_fact(b, a));
        let c = "The gateway shouldn't retry the failed request to the payments upstream after a short delay";
        assert!(!same_fact(a, c));
    }

    #[test]
    fn guard_tokens_are_sorted_and_trimmed_of_sentence_marks() {
        let g = guard_tokens("(See src/a.rs.) Then order.created, v2 and 15s; TLS is not off. Doesn't Zed_x");
        assert_eq!(g.numbers, ["15s", "v2"]);
        assert_eq!(g.idents, ["TLS", "Zed_x", "order.created", "src/a.rs"]);
        assert_eq!(g.negations, ["not", "not"]);
        assert_eq!(
            guard_tokens("Done. ... a.b"),
            Guard {
                idents: vec!["a.b".into()],
                ..Guard::default()
            }
        );
        assert!(guard_agrees("", "plain words only"));
    }
}
