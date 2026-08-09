use serde::Deserialize;
use serde_json::json;
use worker::{D1Database, Env};

use crate::auth::new_id;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::push::{self, PushRequest};

const BATCH_SIZE: i64 = 20;
const LEASE_SECONDS: i64 = 300;
const MAX_ATTEMPTS: i64 = 3;

#[derive(Debug, Clone, Deserialize)]
struct DueReminderRow {
    id: String,
    user_id: String,
    session_id: Option<String>,
    title: String,
    due_at: String,
    timezone: String,
    notification_attempts: i64,
}

/// Deliver reminders that reached their due time. The claim/update fence is
/// separate from command execution because a reminder is a durable scheduled
/// effect, not a second command invocation. A dedupe key on the push record
/// makes a retry after a Worker interruption safe.
pub async fn drain_due(db: &D1Database, env: &Env) -> ApiResult<usize> {
    cancel_deleted_session_reminders(db).await?;
    recover_stale_claims(db).await?;
    let now = db::now_iso();
    let rows: Vec<DueReminderRow> = db::all(
        db,
        "SELECT id, user_id, session_id, title, due_at, timezone, notification_attempts FROM reminders WHERE status = 'scheduled' AND provider = 'local.reminder' AND notification_state IN ('pending', 'retrying') AND notification_attempts < ? AND due_at <= ? AND (session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE id = reminders.session_id AND user_id = reminders.user_id AND deleted_at IS NULL)) ORDER BY due_at ASC, id ASC LIMIT ?",
        vec![
            db::number(MAX_ATTEMPTS),
            db::text(&now),
            db::number(BATCH_SIZE),
        ],
    )
    .await?;

    let mut processed = 0;
    for row in rows {
        if !claim(db, &row, &now).await? {
            continue;
        }
        processed += 1;
        match deliver(db, env, &row).await {
            Ok(()) => finish_sent(db, &row).await?,
            Err(error) => finish_error(db, &row, &error).await?,
        }
    }
    Ok(processed)
}

async fn cancel_deleted_session_reminders(db: &D1Database) -> ApiResult<()> {
    let now = db::now_iso();
    db::run(
        db,
        "UPDATE reminders SET status = 'cancelled', notification_state = 'cancelled', last_notification_error = 'session_deleted', updated_at = ? WHERE status = 'scheduled' AND provider = 'local.reminder' AND session_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM sessions WHERE id = reminders.session_id AND user_id = reminders.user_id AND deleted_at IS NULL)",
        vec![db::text(&now)],
    )
    .await?;
    Ok(())
}

async fn recover_stale_claims(db: &D1Database) -> ApiResult<()> {
    let cutoff = db::add_seconds_iso(-LEASE_SECONDS);
    db::run(
        db,
        "UPDATE reminders SET notification_state = CASE WHEN notification_attempts >= ? THEN 'failed' ELSE 'retrying' END, last_notification_error = 'worker_lease_expired', updated_at = ? WHERE status = 'scheduled' AND provider = 'local.reminder' AND notification_state = 'processing' AND updated_at <= ?",
        vec![
            db::number(MAX_ATTEMPTS),
            db::text(&db::now_iso()),
            db::text(&cutoff),
        ],
    )
    .await?;
    Ok(())
}

async fn claim(db: &D1Database, row: &DueReminderRow, now: &str) -> ApiResult<bool> {
    let result = db::run(
        db,
        "UPDATE reminders SET notification_state = 'processing', notification_attempts = notification_attempts + 1, last_notification_error = NULL, updated_at = ? WHERE id = ? AND status = 'scheduled' AND provider = 'local.reminder' AND notification_state IN ('pending', 'retrying') AND notification_attempts < ? AND due_at <= ? AND (session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE id = reminders.session_id AND user_id = reminders.user_id AND deleted_at IS NULL))",
        vec![
            db::text(now),
            db::text(&row.id),
            db::number(MAX_ATTEMPTS),
            db::text(now),
        ],
    )
    .await?;
    Ok(db::changes(&result) == 1)
}

async fn deliver(db: &D1Database, env: &Env, row: &DueReminderRow) -> ApiResult<()> {
    let dedupe_key = format!("reminder-due:{}", row.id);
    push::notify_user(
        db,
        env,
        PushRequest {
            user_id: &row.user_id,
            session_id: row.session_id.as_deref(),
            title: "Reminder",
            body: &row.title,
            voice_script: Some(&row.title),
            dedupe_key: Some(&dedupe_key),
            payload: json!({
                "type": "reminder.due",
                "reminder_id": row.id,
                "due_at": row.due_at,
                "timezone": row.timezone,
            }),
        },
    )
    .await
    .map(|_| ())
}

async fn finish_sent(db: &D1Database, row: &DueReminderRow) -> ApiResult<()> {
    let now = db::now_iso();
    let statements = vec![
        db::prepare(
            db,
            "UPDATE reminders SET status = 'completed', notification_state = 'sent', notified_at = ?, updated_at = ? WHERE id = ? AND status = 'scheduled' AND notification_state = 'processing'",
            vec![
                db::text(&now),
                db::text(&now),
                db::text(&row.id),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'reminder.delivered', ?, ? WHERE EXISTS (SELECT 1 FROM reminders WHERE id = ? AND status = 'completed' AND notification_state = 'sent' AND notified_at = ?)",
            vec![
                db::text(&new_id("aud")?),
                db::text(&row.user_id),
                db::optional_text(row.session_id.as_deref()),
                db::text(&json!({"reminder_id": row.id, "due_at": row.due_at}).to_string()),
                db::text(&now),
                db::text(&row.id),
                db::text(&now),
            ],
        )?,
    ];
    db.batch(statements).await?;
    Ok(())
}

async fn finish_error(db: &D1Database, row: &DueReminderRow, error: &ApiError) -> ApiResult<()> {
    let now = db::now_iso();
    let exhausted = attempts_exhausted(row.notification_attempts);
    let state = if exhausted { "failed" } else { "retrying" };
    db::run(
        db,
        "UPDATE reminders SET notification_state = ?, last_notification_error = ?, updated_at = ? WHERE id = ? AND status = 'scheduled' AND notification_state = 'processing'",
        vec![
            db::text(state),
            db::text(&error.code),
            db::text(&now),
            db::text(&row.id),
        ],
    )
    .await?;
    Ok(())
}

fn attempts_exhausted(attempts: i64) -> bool {
    attempts + 1 >= MAX_ATTEMPTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reminder_attempts_exhaust_after_three_claims() {
        assert!(!attempts_exhausted(1));
        assert!(attempts_exhausted(2));
    }
}
