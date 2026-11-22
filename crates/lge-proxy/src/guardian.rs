//! A measurement of the approval reviewer's (guardian) requests: how much of
//! each request's start is shared with the previous request of the same
//! parent thread, against what the provider actually served from its cache.
//!
//! Nothing is changed by it. For each guardian request, after lge's own
//! rewrite, the table here keeps only per-item hashes and token estimates of
//! the last request of each parent (at most [`MAX_PARENTS`] parents, each
//! dropped an hour after its last request): never a byte of text. The longest
//! common prefix is read at item granularity (the instructions, the tools,
//! then each input item), so it is an estimate, and `cached_tokens` from the
//! response's usage is the only ground truth. A sum "shareable but uncached"
//! is an upper bound: a sliding window of the transcript diverges for good,
//! and no key or routing change could recover that part.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};

/// Parents kept in memory.
pub const MAX_PARENTS: usize = 64;
/// Seconds after its last request that a parent is forgotten.
pub const EVICT_SECONDS: i64 = 3600;
/// A common prefix of at least this many tokens is "long" (the provider caches
/// in blocks from 1,024 tokens).
pub const LONG_PREFIX_TOKENS: i64 = 1024;
/// A gap above this many seconds may have outlived the cache.
pub const GAP_SECONDS: i64 = 300;

/// The elements of a request in the order the provider reads them: the
/// instructions, the tool definitions, then each input item. Hashes and token
/// estimates only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signature {
    elements: Vec<(u64, i64)>,
}

fn hashed(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// The signature of a request body as it is sent.
pub fn signature(body: &Value) -> Signature {
    let element = |text: String| (hashed(&text), lge_retrieve::budget::count(&text) as i64);
    let mut elements = vec![
        element(body.get("instructions").map(Value::to_string).unwrap_or_default()),
        element(body.get("tools").map(Value::to_string).unwrap_or_default()),
    ];
    match body.get("input") {
        Some(Value::Array(items)) => elements.extend(items.iter().map(|i| element(i.to_string()))),
        Some(other) => elements.push(element(other.to_string())),
        None => {}
    }
    Signature { elements }
}

impl Signature {
    /// Tokens (an estimate) of the leading elements `self` shares with `other`.
    pub fn common_prefix_tokens(&self, other: &Signature) -> i64 {
        self.elements
            .iter()
            .zip(&other.elements)
            .take_while(|(a, b)| a.0 == b.0)
            .map(|(a, _)| a.1)
            .sum()
    }
}

/// What a request carries besides its items.
#[derive(Debug, Clone, Default)]
pub struct Current {
    pub thread: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub session_id: Option<String>,
    /// lge changed the request's bytes.
    pub lge_changed_bytes: bool,
}

/// The previous request of a parent.
#[derive(Debug, Clone)]
struct Previous {
    signature: Signature,
    thread: Option<String>,
    key: Option<String>,
    session_id: Option<String>,
    arrived: i64,
    /// When its response ended; `None` while it is still being relayed.
    ended: Option<i64>,
    seq: u64,
}

/// What a request has in common with the previous request of its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub lcp_tokens: i64,
    pub same_thread: bool,
    pub key_same: bool,
    pub session_id_same: bool,
    pub gap_seconds: i64,
    /// The previous response was still being relayed when this request arrived.
    pub overlapped: bool,
    pub lge_changed_bytes: bool,
}

/// The last request of each parent, in memory only.
#[derive(Debug, Default)]
pub struct Table {
    parents: HashMap<String, Previous>,
    seq: u64,
}

impl Table {
    /// Parents held.
    pub fn len(&self) -> usize {
        self.parents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    /// Compare a request of `parent` with its predecessor and keep it as the
    /// new predecessor. Returns the comparison (`None` for a parent's first
    /// request) and a number to hand to [`Table::ended`].
    pub fn observe(
        &mut self,
        parent: &str,
        now: i64,
        signature: Signature,
        current: Current,
    ) -> (Option<Observation>, u64) {
        self.parents
            .retain(|_, p| now - p.ended.unwrap_or(p.arrived).max(p.arrived) <= EVICT_SECONDS);
        if !self.parents.contains_key(parent) && self.parents.len() >= MAX_PARENTS {
            let oldest = self
                .parents
                .iter()
                .min_by_key(|(_, p)| p.arrived)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                self.parents.remove(&oldest);
            }
        }
        self.seq += 1;
        let seq = self.seq;
        let observation = self.parents.get(parent).map(|previous| Observation {
            lcp_tokens: signature.common_prefix_tokens(&previous.signature),
            same_thread: previous.thread == current.thread,
            key_same: previous.key == current.prompt_cache_key,
            session_id_same: previous.session_id == current.session_id,
            gap_seconds: (now - previous.ended.unwrap_or(previous.arrived)).max(0),
            overlapped: previous.ended.is_none(),
            lge_changed_bytes: current.lge_changed_bytes,
        });
        self.parents.insert(
            parent.to_string(),
            Previous {
                signature,
                thread: current.thread,
                key: current.prompt_cache_key,
                session_id: current.session_id,
                arrived: now,
                ended: None,
                seq,
            },
        );
        (observation, seq)
    }

    /// The response of request `seq` of `parent` ended at `now`.
    pub fn ended(&mut self, parent: &str, seq: u64, now: i64) {
        if let Some(p) = self.parents.get_mut(parent).filter(|p| p.seq == seq) {
            p.ended = Some(now);
        }
    }
}

/// Requests and their shareable-but-uncached tokens in one split of the stats.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bucket {
    pub requests: u64,
    pub shareable_uncached: i64,
}

/// Counts of the measurement over the requests of one session; plain sums, so
/// sessions add up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// Requests with a previous request of the same parent and a usage.
    pub with_previous: u64,
    /// Of those, with a common prefix of at least [`LONG_PREFIX_TOKENS`].
    pub long_prefix: u64,
    pub lcp_tokens: i64,
    pub input_tokens: i64,
    pub cached_tokens: i64,
    /// Sum of max(0, min(lcp, input) - cached): an upper bound.
    pub shareable_uncached: i64,
    /// By overlapped, gap, key, lge change, thread and session id.
    pub splits: BTreeMap<String, Bucket>,
}

impl Stats {
    /// Add one request's comparison with the usage of its response.
    pub fn add(&mut self, o: &Observation, input: i64, cached: i64) {
        let shareable = (o.lcp_tokens.min(input) - cached).max(0);
        self.with_previous += 1;
        if o.lcp_tokens >= LONG_PREFIX_TOKENS {
            self.long_prefix += 1;
        }
        self.lcp_tokens += o.lcp_tokens;
        self.input_tokens += input;
        self.cached_tokens += cached;
        self.shareable_uncached += shareable;
        let keys = [
            if o.overlapped { "overlapped" } else { "not_overlapped" },
            if o.gap_seconds > GAP_SECONDS {
                "gap_over_300s"
            } else {
                "gap_within_300s"
            },
            if o.key_same { "key_same" } else { "key_different" },
            if o.lge_changed_bytes {
                "lge_changed_bytes"
            } else {
                "lge_unchanged_bytes"
            },
            if o.same_thread { "same_thread" } else { "new_thread" },
            if o.session_id_same {
                "session_id_same"
            } else {
                "session_id_different"
            },
        ];
        for key in keys {
            let b = self.splits.entry(key.to_string()).or_default();
            b.requests += 1;
            b.shareable_uncached += shareable;
        }
    }

    /// The sum of two sessions' stats.
    pub fn plus(&self, other: &Stats) -> Stats {
        let mut out = self.clone();
        out.with_previous += other.with_previous;
        out.long_prefix += other.long_prefix;
        out.lcp_tokens += other.lcp_tokens;
        out.input_tokens += other.input_tokens;
        out.cached_tokens += other.cached_tokens;
        out.shareable_uncached += other.shareable_uncached;
        for (k, b) in &other.splits {
            let slot = out.splits.entry(k.clone()).or_default();
            slot.requests += b.requests;
            slot.shareable_uncached += b.shareable_uncached;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(items: &[&str]) -> Value {
        json!({ "instructions": "policy", "tools": [], "input": items.iter().map(|t| json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": t.repeat(400) }] })).collect::<Vec<_>>() })
    }

    fn current(thread: &str) -> Current {
        Current {
            thread: Some(thread.into()),
            prompt_cache_key: Some("guardian:p1".into()),
            session_id: Some(thread.into()),
            lge_changed_bytes: false,
        }
    }

    #[test]
    fn the_common_prefix_is_the_leading_items_both_requests_have() {
        let a = signature(&request(&["a", "b", "c"]));
        let b = signature(&request(&["a", "b", "x", "y"]));
        let each = a.elements[2].1;
        assert!(each > 10, "{each}");
        // instructions, tools and two items.
        assert_eq!(
            b.common_prefix_tokens(&a),
            a.elements[..4].iter().map(|e| e.1).sum::<i64>()
        );
        assert_eq!(a.common_prefix_tokens(&a), a.elements.iter().map(|e| e.1).sum::<i64>());
        let other = signature(&json!({ "instructions": "another", "tools": [], "input": [] }));
        assert_eq!(other.common_prefix_tokens(&a), 0, "instructions differ");
    }

    #[test]
    fn a_request_is_compared_with_the_previous_one_of_its_parent_only() {
        let mut table = Table::default();
        let (first, seq) = table.observe("p1", 100, signature(&request(&["a", "b"])), current("t1"));
        assert_eq!(first, None, "no previous sibling");
        table.ended("p1", seq, 130);
        let mut next = current("t2");
        next.lge_changed_bytes = true;
        let (second, _) = table.observe("p1", 500, signature(&request(&["a", "b", "c"])), next);
        let second = second.unwrap();
        assert!(!second.same_thread && second.key_same && !second.session_id_same);
        assert_eq!(
            (second.gap_seconds, second.overlapped, second.lge_changed_bytes),
            (370, false, true)
        );
        let (other, _) = table.observe("p2", 501, signature(&request(&["a", "b"])), current("t3"));
        assert_eq!(other, None, "another parent");
        // A sibling that arrives before the previous response ended overlaps it.
        let (third, _) = table.observe("p1", 502, signature(&request(&["a"])), current("t4"));
        assert!(third.unwrap().overlapped);
    }

    #[test]
    fn stats_sum_what_could_have_been_shared_but_was_not_cached() {
        let mut stats = Stats::default();
        let o = |lcp, overlapped| Observation {
            lcp_tokens: lcp,
            same_thread: false,
            key_same: true,
            session_id_same: false,
            gap_seconds: 10,
            overlapped,
            lge_changed_bytes: false,
        };
        stats.add(&o(30_000, false), 33_000, 0);
        stats.add(&o(500, true), 25_000, 0);
        stats.add(&o(60_000, false), 50_000, 49_000);
        assert_eq!((stats.with_previous, stats.long_prefix), (3, 2));
        // min(lcp, input) - cached: 30,000 + 500 + 1,000.
        assert_eq!(stats.shareable_uncached, 31_500);
        assert_eq!(
            stats.splits["overlapped"],
            Bucket {
                requests: 1,
                shareable_uncached: 500
            }
        );
        assert_eq!(stats.splits["not_overlapped"].shareable_uncached, 31_000);
        assert_eq!(stats.plus(&stats).shareable_uncached, 63_000);
    }

    #[test]
    fn parents_are_bounded_and_forgotten_after_an_hour() {
        let mut table = Table::default();
        for n in 0..(MAX_PARENTS + 10) {
            table.observe(&format!("p{n}"), n as i64, signature(&request(&["a"])), current("t"));
        }
        assert_eq!(table.len(), MAX_PARENTS);
        table.observe("late", EVICT_SECONDS + 1000, signature(&request(&["a"])), current("t"));
        assert_eq!(table.len(), 1, "the others are gone, with their hashes");
    }
}
