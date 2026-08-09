-- Supabase Auth owns the password and session lifecycle when AUTH_PROVIDER=supabase.
-- D1 keeps only the local user identity used by agents, sessions and devices.
ALTER TABLE users ADD COLUMN supabase_user_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_supabase_user_id
  ON users(supabase_user_id)
  WHERE supabase_user_id IS NOT NULL;
