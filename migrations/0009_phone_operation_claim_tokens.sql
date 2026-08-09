-- Bind compatibility-operation completion and release to the worker that
-- claimed the lease. This prevents an expired request from overwriting or
-- deleting a newer claimant's idempotency record.

ALTER TABLE phone_operations ADD COLUMN claim_token TEXT;

CREATE INDEX IF NOT EXISTS idx_phone_operations_claim
  ON phone_operations(user_id, idempotency_key, operation, claim_token);
