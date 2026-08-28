-- Durable APNs delivery for session events reuses outbox_events and
-- action_attempts. The domain batch writes the parent notification intent;
-- the scheduled outbox drain fans it out into one stable delivery per device.

CREATE INDEX IF NOT EXISTS idx_outbox_session_event_notification_due
  ON outbox_events (topic, state, next_attempt_at, created_at)
  WHERE topic IN ('session.event.notification', 'session.event.apns');

CREATE INDEX IF NOT EXISTS idx_action_attempts_session_event_apns_state
  ON action_attempts (provider, state, next_attempt_at, updated_at)
  WHERE provider = 'apns.session_event';
