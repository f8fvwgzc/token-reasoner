//! The usage log and the handoff notes. They are the only data in the
//! database that cannot be rebuilt from the source tree.

use crate::values::{col_b, col_i, col_list, col_s, int, rows, st, strings};
use anyhow::Result;
use lbug::{Connection, Value};
use lge_core::{HandoffNote, UsageRecord};

const USAGE_COLS: &str = "u.id, u.task_id, u.task, u.attempt, u.level, u.reason, u.runner, u.model, u.session_id, \
     u.started_at, u.finished_at, u.input_tokens, u.cached_input_tokens, u.reasoning_tokens, u.output_tokens, \
     u.pack_tokens, u.outcome, u.escalated, u.files, u.services";

/// Insert or replace a usage record by id.
pub(crate) fn put_usage(conn: &Connection, rec: &UsageRecord) -> Result<()> {
    rows(
        conn,
        "MERGE (u:UsageRecord {id:$id}) SET u.task_id = $task_id, u.task = $task, u.attempt = $attempt, \
         u.level = $level, u.reason = $reason, u.runner = $runner, u.model = $model, u.session_id = $session_id, \
         u.started_at = $started_at, u.finished_at = $finished_at, u.input_tokens = $input_tokens, \
         u.cached_input_tokens = $cached_input_tokens, u.reasoning_tokens = $reasoning_tokens, \
         u.output_tokens = $output_tokens, u.pack_tokens = $pack_tokens, u.outcome = $outcome, \
         u.escalated = $escalated, u.files = $files, u.services = $services;",
        vec![
            ("id", st(&rec.id)),
            ("task_id", st(&rec.task_id)),
            ("task", st(&rec.task)),
            ("attempt", int(rec.attempt)),
            ("level", st(&rec.level)),
            ("reason", st(&rec.reason)),
            ("runner", st(&rec.runner)),
            ("model", st(&rec.model)),
            ("session_id", st(&rec.session_id)),
            ("started_at", int(rec.started_at)),
            ("finished_at", int(rec.finished_at)),
            ("input_tokens", int(rec.input_tokens)),
            ("cached_input_tokens", int(rec.cached_input_tokens)),
            ("reasoning_tokens", int(rec.reasoning_tokens)),
            ("output_tokens", int(rec.output_tokens)),
            ("pack_tokens", int(rec.pack_tokens)),
            ("outcome", st(&rec.outcome)),
            ("escalated", Value::Bool(rec.escalated)),
            ("files", strings(&rec.files)),
            ("services", strings(&rec.services)),
        ],
    )?;
    Ok(())
}

/// Usage records with `started_at >= since`, oldest first. The attempts of
/// one task that start within the same second stay in attempt order.
pub(crate) fn list_usage(conn: &Connection, since: i64) -> Result<Vec<UsageRecord>> {
    let q = format!(
        "MATCH (u:UsageRecord) WHERE u.started_at >= $since \
         RETURN {USAGE_COLS} ORDER BY u.started_at, u.task_id, u.attempt, u.id;"
    );
    let found = rows(conn, &q, vec![("since", int(since))])?;
    Ok(found
        .iter()
        .map(|r| UsageRecord {
            id: col_s(r, 0),
            task_id: col_s(r, 1),
            task: col_s(r, 2),
            attempt: col_i(r, 3),
            level: col_s(r, 4),
            reason: col_s(r, 5),
            runner: col_s(r, 6),
            model: col_s(r, 7),
            session_id: col_s(r, 8),
            started_at: col_i(r, 9),
            finished_at: col_i(r, 10),
            input_tokens: col_i(r, 11),
            cached_input_tokens: col_i(r, 12),
            reasoning_tokens: col_i(r, 13),
            output_tokens: col_i(r, 14),
            pack_tokens: col_i(r, 15),
            outcome: col_s(r, 16),
            escalated: col_b(r, 17),
            files: col_list(r, 18),
            services: col_list(r, 19),
        })
        .collect())
}

/// Insert or replace a handoff note by id.
pub(crate) fn put_handoff(conn: &Connection, note: &HandoffNote) -> Result<()> {
    rows(
        conn,
        "MERGE (h:HandoffNote {id:$id}) SET h.task_id = $task_id, h.task = $task, h.created_at = $created_at, \
         h.text = $text;",
        vec![
            ("id", st(&note.id)),
            ("task_id", st(&note.task_id)),
            ("task", st(&note.task)),
            ("created_at", int(note.created_at)),
            ("text", st(&note.text)),
        ],
    )?;
    Ok(())
}

/// Most recent handoff notes, newest first.
pub(crate) fn latest_handoffs(conn: &Connection, limit: usize) -> Result<Vec<HandoffNote>> {
    let q = format!(
        "MATCH (h:HandoffNote) RETURN h.id, h.task_id, h.task, h.created_at, h.text \
         ORDER BY h.created_at DESC, h.id DESC LIMIT {limit};"
    );
    let found = rows(conn, &q, vec![])?;
    Ok(found
        .iter()
        .map(|r| HandoffNote {
            id: col_s(r, 0),
            task_id: col_s(r, 1),
            task: col_s(r, 2),
            created_at: col_i(r, 3),
            text: col_s(r, 4),
        })
        .collect())
}
