//! Building the usage record of one session attempt.

use crate::codex::ParsedSession;
use lge_core::{outcome, stable_id, Level, UsageRecord};

/// Id that groups the attempts of one task. The start time is part of it, so the
/// same task text run again later is a new task, not a retry of the old one.
pub fn task_id(task: &str, started_at: i64) -> String {
    stable_id(&["task", task, &started_at.to_string()])
}

/// The record of an attempt that is about to start: no tokens yet, outcome
/// `unknown`, `finished_at` 0.
///
/// The record id is derived from the task id and the attempt number, so writing
/// the same attempt twice replaces the first write instead of counting it twice.
/// `escalated` follows from `attempt`: every attempt after the first exists
/// because the one before it failed the local checks.
pub fn new_record(
    task: &str,
    task_id: &str,
    attempt: i64,
    level: Level,
    reason: &str,
    runner: &str,
    started_at: i64,
) -> UsageRecord {
    UsageRecord {
        id: stable_id(&["usage", task_id, &attempt.to_string()]),
        task_id: task_id.to_string(),
        task: task.to_string(),
        attempt,
        level: level.as_str().to_string(),
        reason: reason.to_string(),
        runner: runner.to_string(),
        started_at,
        outcome: outcome::UNKNOWN.to_string(),
        escalated: attempt > 1,
        ..UsageRecord::default()
    }
}

/// Fills in what the finished session reported.
///
/// The token counts always come from the session. The model and session id are
/// only replaced when the session names them: the `exec --json` stream carries no
/// model, and the caller may have set the one it asked for.
pub fn apply_session(rec: &mut UsageRecord, s: &ParsedSession, finished_at: i64, outcome: &str) {
    rec.input_tokens = s.usage.input_tokens;
    rec.cached_input_tokens = s.usage.cached_input_tokens;
    rec.reasoning_tokens = s.usage.reasoning_tokens;
    rec.output_tokens = s.usage.output_tokens;
    if !s.session_id.is_empty() {
        rec.session_id = s.session_id.clone();
    }
    if !s.model.is_empty() {
        rec.model = s.model.clone();
    }
    rec.finished_at = finished_at;
    rec.outcome = known_outcome(outcome).to_string();
}

/// One of the three values in `lge_core::outcome`. Anything else is `unknown`: a
/// misspelt outcome must not be stored as if it were a result.
pub(crate) fn known_outcome(outcome: &str) -> &'static str {
    match outcome.trim().to_ascii_lowercase().as_str() {
        outcome::SUCCESS => outcome::SUCCESS,
        outcome::FAILURE => outcome::FAILURE,
        _ => outcome::UNKNOWN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::TokenUsage;

    fn session() -> ParsedSession {
        ParsedSession {
            session_id: "thread-1".to_string(),
            model: "gpt-test".to_string(),
            reasoning_effort: "low".to_string(),
            usage: TokenUsage {
                input_tokens: 1000,
                cached_input_tokens: 400,
                reasoning_tokens: 30,
                output_tokens: 100,
            },
            final_message: "done".to_string(),
            turns: 1,
        }
    }

    #[test]
    fn task_id_is_stable_and_depends_on_text_and_time() {
        assert_eq!(task_id("fix the parser", 100), task_id("fix the parser", 100));
        assert_ne!(task_id("fix the parser", 100), task_id("fix the parser", 101));
        assert_ne!(task_id("fix the parser", 100), task_id("fix the lexer", 100));
        assert_eq!(
            task_id("fix the parser", 100),
            stable_id(&["task", "fix the parser", "100"])
        );
    }

    #[test]
    fn new_record_starts_empty_and_unknown() {
        let tid = task_id("fix the parser", 100);
        let rec = new_record("fix the parser", &tid, 1, Level::Low, "one file", "codex", 100);
        assert_eq!(rec.id, stable_id(&["usage", &tid, "1"]));
        assert_eq!(rec.task_id, tid);
        assert_eq!(rec.task, "fix the parser");
        assert_eq!(rec.attempt, 1);
        assert_eq!(rec.level, "low");
        assert_eq!(rec.reason, "one file");
        assert_eq!(rec.runner, "codex");
        assert_eq!(rec.started_at, 100);
        assert_eq!(rec.finished_at, 0);
        assert_eq!(rec.outcome, outcome::UNKNOWN);
        assert!(!rec.escalated);
        assert_eq!(TokenUsage::of(&rec), TokenUsage::default());
        assert_eq!(
            (rec.model.as_str(), rec.session_id.as_str(), rec.pack_tokens),
            ("", "", 0)
        );
        assert!(rec.files.is_empty() && rec.services.is_empty());
    }

    #[test]
    fn later_attempts_are_escalations_with_their_own_id() {
        let tid = task_id("fix the parser", 100);
        let first = new_record("fix the parser", &tid, 1, Level::Low, "one file", "codex", 100);
        let second = new_record("fix the parser", &tid, 2, Level::Medium, "checks failed", "codex", 160);
        assert!(second.escalated);
        assert_eq!(second.level, "medium");
        assert_eq!(second.task_id, first.task_id);
        assert_ne!(second.id, first.id);
        assert_eq!(
            second.id,
            new_record("fix the parser", &tid, 2, Level::Top, "", "dry-run", 999).id
        );
    }

    #[test]
    fn apply_session_fills_tokens_ids_time_and_outcome() {
        let mut rec = new_record("t", "task-1", 1, Level::Top, "cross-service", "codex", 100);
        rec.pack_tokens = 5900;
        apply_session(&mut rec, &session(), 175, outcome::SUCCESS);
        assert_eq!(TokenUsage::of(&rec), session().usage);
        assert_eq!(rec.session_id, "thread-1");
        assert_eq!(rec.model, "gpt-test");
        assert_eq!(rec.finished_at, 175);
        assert_eq!(rec.outcome, outcome::SUCCESS);
        assert_eq!(rec.pack_tokens, 5900, "fields the session does not know are left alone");
        assert_eq!((rec.level.as_str(), rec.attempt, rec.started_at), ("top", 1, 100));
    }

    #[test]
    fn apply_session_keeps_what_the_stream_does_not_name() {
        let mut rec = new_record("t", "task-1", 1, Level::Low, "", "codex", 100);
        rec.model = "requested-model".to_string();
        rec.session_id = "known-before".to_string();
        let stream_only = ParsedSession {
            usage: session().usage,
            ..ParsedSession::default()
        };
        apply_session(&mut rec, &stream_only, 130, outcome::FAILURE);
        assert_eq!(rec.model, "requested-model");
        assert_eq!(rec.session_id, "known-before");
        assert_eq!(rec.outcome, outcome::FAILURE);
        assert_eq!(rec.input_tokens, 1000);
    }

    #[test]
    fn apply_session_replaces_tokens_instead_of_adding() {
        let mut rec = new_record("t", "task-1", 1, Level::Low, "", "codex", 100);
        apply_session(&mut rec, &session(), 130, outcome::UNKNOWN);
        apply_session(&mut rec, &session(), 131, outcome::SUCCESS);
        assert_eq!(
            TokenUsage::of(&rec),
            session().usage,
            "applying twice must not double the count"
        );
    }

    #[test]
    fn outcomes_outside_the_vocabulary_become_unknown() {
        assert_eq!(known_outcome("success"), outcome::SUCCESS);
        assert_eq!(known_outcome(" Success\n"), outcome::SUCCESS);
        assert_eq!(known_outcome("FAILURE"), outcome::FAILURE);
        assert_eq!(known_outcome("unknown"), outcome::UNKNOWN);
        assert_eq!(known_outcome("failed"), outcome::UNKNOWN);
        assert_eq!(known_outcome(""), outcome::UNKNOWN);

        let mut rec = new_record("t", "task-1", 1, Level::Low, "", "codex", 100);
        apply_session(&mut rec, &session(), 130, "passed");
        assert_eq!(rec.outcome, outcome::UNKNOWN);
    }
}
