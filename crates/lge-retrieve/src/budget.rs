//! Token counting and budgeted packing.
//!
//! Tokens are counted with the `o200k_base` encoding, the one current OpenAI
//! models use, so a budget here is close to what the session is charged.

use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

/// Default size of an evidence pack.
pub const DEFAULT_PACK_BUDGET: usize = 6000;
/// Default size of one MCP tool answer.
pub const DEFAULT_TOOL_BUDGET: usize = 1500;

fn encoder() -> Option<&'static CoreBPE> {
    static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::o200k_base().ok()).as_ref()
}

/// Number of tokens in `text`. If the encoder cannot be built, one token per
/// four bytes, rounded up: an overestimate for code, so a budget is still kept.
pub fn count(text: &str) -> usize {
    match encoder() {
        Some(bpe) => bpe.encode_ordinary(text).len(),
        None => text.len().div_ceil(4),
    }
}

/// The longest prefix of whole lines of `text` that fits in `max_tokens`, with
/// a marker line when something was cut. Returns the text and its token count.
pub fn truncate_lines(text: &str, max_tokens: usize) -> (String, usize) {
    let total = count(text);
    if total <= max_tokens {
        return (text.to_string(), total);
    }
    const MARKER: &str = "    ... (cut)";
    let marker_cost = count(MARKER) + 1;
    let mut out = String::new();
    let mut used = 0;
    for line in text.lines() {
        // +1 for the newline that joins it.
        let cost = count(line) + 1;
        if used + cost + marker_cost > max_tokens {
            break;
        }
        out.push_str(line);
        out.push('\n');
        used += cost;
    }
    out.push_str(MARKER);
    let used = count(&out);
    (out, used)
}

/// A running token budget.
#[derive(Debug, Clone)]
pub struct Budget {
    pub limit: usize,
    pub used: usize,
}

impl Budget {
    pub fn new(limit: usize) -> Self {
        Budget { limit, used: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.used)
    }

    /// Take `tokens` if they fit. An item that does not fit is skipped and the
    /// caller goes on to the next one (smaller items may still fit).
    pub fn take(&mut self, tokens: usize) -> bool {
        if tokens <= self.remaining() {
            self.used += tokens;
            true
        } else {
            false
        }
    }

    /// Take `tokens` even when over the limit. Only for what must never be cut.
    pub fn force(&mut self, tokens: usize) {
        self.used += tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_truncates_on_line_boundaries() {
        assert_eq!(count(""), 0);
        let one = count("fn create_order(payload: Order) -> Result<Id> {");
        assert!((8..=20).contains(&one), "{one}");
        let text = (0..200)
            .map(|i| format!("    let value_{i} = compute({i});"))
            .collect::<Vec<_>>()
            .join("\n");
        let (cut, tokens) = truncate_lines(&text, 100);
        assert!(tokens <= 100, "{tokens}");
        assert!(cut.ends_with("... (cut)"));
        assert!(cut.lines().count() > 3);
        assert!(text.starts_with(cut.trim_end_matches("    ... (cut)")));
        let (same, t) = truncate_lines("short", 100);
        assert_eq!((same.as_str(), t), ("short", count("short")));
    }

    #[test]
    fn budget_skips_what_does_not_fit_and_continues() {
        let mut b = Budget::new(10);
        assert!(b.take(6));
        assert!(!b.take(5), "too big: skipped");
        assert!(b.take(4), "a smaller one still fits");
        assert_eq!(b.remaining(), 0);
        b.force(3);
        assert_eq!((b.used, b.remaining()), (13, 0));
    }
}
