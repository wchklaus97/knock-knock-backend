-- Reminder delivery is a scheduled side effect, not just a persisted row.
-- These fields make the due-time worker leaseable and retryable without
-- emitting duplicate phone pushes after a Worker invocation is interrupted.
ALTER TABLE reminders ADD COLUMN notification_state TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE reminders ADD COLUMN notification_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE reminders ADD COLUMN notified_at TEXT;
ALTER TABLE reminders ADD COLUMN last_notification_error TEXT;
ALTER TABLE reminders ADD COLUMN provider TEXT NOT NULL DEFAULT 'local.reminder';
ALTER TABLE reminders ADD COLUMN provider_reminder_id TEXT;

ALTER TABLE pushes ADD COLUMN dedupe_key TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_pushes_dedupe_key
  ON pushes(dedupe_key)
  WHERE dedupe_key IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_reminders_notification_due
  ON reminders(status, notification_state, due_at);
