-- Migration 0022: distinguish an explicit listener release from natural expiry.
-- Acquisition clears this marker when it advances the per-agent lease fence.

ALTER TABLE agent_listener_leases ADD COLUMN released_at TEXT;
