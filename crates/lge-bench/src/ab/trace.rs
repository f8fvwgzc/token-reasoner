//! The daemon's trace of one session: one record per model request, kept in
//! its memory (`GET /lge/api/sessions/<thread-id>/requests`). The runner
//! saves it beside the results and the report reads the context per request
//! and the largest tool outputs from it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The folder of the work dir the traces are written to.
pub const TRACES_DIR: &str = "traces";

/// Largest tool outputs of a session kept in its summary.
pub const TOP_OUTPUTS: usize = 3;

/// Characters of a command shown with an output.
const COMMAND_CHARS: usize = 120;

/// What the report shows of one session's trace.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceSummary {
    /// Requests the daemon traced.
    pub requests: usize,
    /// Of those, the ones whose usage the daemon read from the response.
    pub measured: usize,
    /// Mean and largest input tokens (cached included) of a request.
    pub mean_context: Option<f64>,
    pub max_context: Option<i64>,
    /// Tokens lge added (packs) and removed (shortened outputs) over the session.
    pub lge_added_tokens: i64,
    pub lge_removed_tokens: i64,
    /// The largest tool outputs new in any request, largest first.
    pub largest_outputs: Vec<TraceOutput>,
}

/// One tool output as the trace names it: no output text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceOutput {
    /// The request (1-based) it was new in.
    pub request: u64,
    /// What produced it, at most 120 characters.
    pub command: String,
    /// Its tokens as the client sent it.
    pub tokens: u64,
    /// lge sent something shorter in its place.
    pub shortened: bool,
}

/// The summary of the daemon's answer; `None` when it is not a trace (no
/// `requests` list).
pub fn summarize_trace(trace: &Value) -> Option<TraceSummary> {
    let requests = trace.get("requests")?.as_array()?;
    let int = |v: &Value, key: &str| v.get(key).and_then(Value::as_i64).unwrap_or(0);
    let contexts: Vec<i64> = requests
        .iter()
        .filter(|r| r.get("usage_found").and_then(Value::as_bool).unwrap_or(false))
        .map(|r| int(r, "input_tokens"))
        .collect();
    let mut outputs: Vec<TraceOutput> = requests
        .iter()
        .flat_map(|r| {
            let request = r.get("request").and_then(Value::as_u64).unwrap_or(0);
            r.get("largest_outputs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(move |o| TraceOutput {
                    request,
                    command: o
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .chars()
                        .take(COMMAND_CHARS)
                        .collect(),
                    tokens: o.get("tokens").and_then(Value::as_u64).unwrap_or(0),
                    shortened: o.get("shortened").and_then(Value::as_bool).unwrap_or(false),
                })
        })
        .collect();
    outputs.sort_by(|a, b| {
        b.tokens
            .cmp(&a.tokens)
            .then(a.request.cmp(&b.request))
            .then(a.command.cmp(&b.command))
    });
    outputs.truncate(TOP_OUTPUTS);
    Some(TraceSummary {
        requests: requests.len(),
        measured: contexts.len(),
        mean_context: (!contexts.is_empty()).then(|| contexts.iter().sum::<i64>() as f64 / contexts.len() as f64),
        max_context: contexts.iter().copied().max(),
        lge_added_tokens: requests.iter().map(|r| int(r, "lge_added_tokens")).sum(),
        lge_removed_tokens: requests.iter().map(|r| int(r, "lge_removed_tokens")).sum(),
        largest_outputs: outputs,
    })
}

/// True when `thread` can go in the trace's URL path as it is.
pub fn safe_thread_id(thread: &str) -> bool {
    thread.len() <= 200
        && thread.chars().any(|c| c.is_ascii_alphanumeric())
        && thread
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_trace_gives_context_per_request_and_the_largest_outputs() {
        let trace = json!({ "thread": "t", "known": true, "requests": [
            { "request": 1, "usage_found": true, "input_tokens": 10_000, "lge_added_tokens": 300, "lge_removed_tokens": 0,
              "largest_outputs": [{ "command": "rg -n foo", "tokens": 900, "shortened": false }] },
            { "request": 2, "usage_found": true, "input_tokens": 30_000, "lge_added_tokens": 0, "lge_removed_tokens": 4_000,
              "largest_outputs": [
                  { "command": format!("cargo test {}", "x".repeat(200)), "tokens": 5_000, "shortened": true },
                  { "command": "cat src/a.rs", "tokens": 700, "shortened": false },
                  { "command": "ls", "tokens": 10, "shortened": false }] },
            { "request": 3, "usage_found": false, "input_tokens": 0, "largest_outputs": [] }
        ]});
        let s = summarize_trace(&trace).unwrap();
        assert_eq!((s.requests, s.measured), (3, 2));
        assert_eq!(s.mean_context, Some(20_000.0));
        assert_eq!(s.max_context, Some(30_000));
        assert_eq!((s.lge_added_tokens, s.lge_removed_tokens), (300, 4_000));
        let tokens: Vec<u64> = s.largest_outputs.iter().map(|o| o.tokens).collect();
        assert_eq!(tokens, [5_000, 900, 700]);
        assert_eq!(s.largest_outputs[0].request, 2);
        assert!(s.largest_outputs[0].shortened);
        assert_eq!(s.largest_outputs[0].command.chars().count(), 120);

        let empty = summarize_trace(&json!({ "known": false, "requests": [] })).unwrap();
        assert_eq!((empty.requests, empty.mean_context, empty.max_context), (0, None, None));
        assert_eq!(summarize_trace(&json!({ "error": "x" })), None);
    }

    #[test]
    fn only_plain_thread_ids_go_in_the_url() {
        assert!(safe_thread_id("0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"));
        for bad in ["", "..", "a/b", "a b", "../x", "a?b", "%2e"] {
            assert!(!safe_thread_id(bad), "{bad}");
        }
    }
}
