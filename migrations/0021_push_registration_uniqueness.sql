-- Each deployed D1 database is one Worker/APNs environment. Within that
-- environment, one canonical user/device registration owns a non-null key and
-- one normalized APNs token has exactly one active owner.
--
-- Commands reference devices.id. Duplicate legacy registrations must therefore
-- be retired in place rather than deleted so command and history references
-- remain valid.

-- Canonicalize only the registration key first. APNs token text is deliberately
-- left untouched until case-equivalent conflicts have been retired; the legacy
-- UNIQUE(user_id, push_token) constraint is case-sensitive and can otherwise
-- fail while two same-user case variants are normalized by one UPDATE.
UPDATE devices
SET platform = LOWER(TRIM(platform)),
    device_id = COALESCE(NULLIF(TRIM(device_id), ''), '__legacy_device__');

-- A malformed or non-iOS legacy token must not remain an active owner. APNs
-- tokens are opaque variable-length bytes represented as bounded even-length
-- hexadecimal strings; do not freeze the historical 32-byte size.
UPDATE devices
SET push_token = NULL
WHERE push_token IS NOT NULL
  AND (
    platform <> 'ios'
    OR LENGTH(TRIM(push_token)) < 32
    OR LENGTH(TRIM(push_token)) > 512
    OR LENGTH(TRIM(push_token)) % 2 <> 0
    OR LOWER(TRIM(push_token)) GLOB '*[^0-9a-f]*'
  );

-- Keep the newest valid registration for each canonical owner key. Losers
-- remain addressable by devices.id, but clearing both ownership fields makes
-- them inactive and lets the full unique registration index coexist with
-- preserved historical rows (SQLite unique indexes permit multiple NULLs).
WITH ranked_registrations AS (
  SELECT
    id,
    ROW_NUMBER() OVER (
      PARTITION BY user_id, platform, device_id
      ORDER BY
        CASE WHEN push_token IS NOT NULL THEN 1 ELSE 0 END DESC,
        updated_at DESC,
        created_at DESC,
        id DESC
    ) AS registration_rank
  FROM devices
)
UPDATE devices
SET push_token = NULL,
    device_id = NULL
WHERE id IN (
  SELECT id
  FROM ranked_registrations
  WHERE registration_rank > 1
);

-- Resolve global ownership by the value the token will have after
-- normalization. This runs while case-distinct legacy values are still
-- distinct under the old per-user constraint, so no intermediate collision is
-- possible. Reinstall/account-change losers remain as inactive device rows.
WITH ranked_token_owners AS (
  SELECT
    id,
    ROW_NUMBER() OVER (
      PARTITION BY LOWER(TRIM(push_token))
      ORDER BY updated_at DESC, created_at DESC, id DESC
    ) AS owner_rank
  FROM devices
  WHERE push_token IS NOT NULL
)
UPDATE devices
SET push_token = NULL
WHERE id IN (
  SELECT id
  FROM ranked_token_owners
  WHERE owner_rank > 1
);

-- Only conflict-free active tokens are normalized.
UPDATE devices
SET push_token = LOWER(TRIM(push_token))
WHERE push_token IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_registration_owner
  ON devices(user_id, platform, device_id);

CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_active_push_token
  ON devices(push_token)
  WHERE push_token IS NOT NULL;

-- Preserve the invariant for future writes instead of relying on every caller
-- to remember normalization before the global unique index is evaluated.
CREATE TRIGGER IF NOT EXISTS trg_devices_push_token_normalized_insert
BEFORE INSERT ON devices
FOR EACH ROW
WHEN NEW.push_token IS NOT NULL
  AND NEW.push_token <> LOWER(TRIM(NEW.push_token))
BEGIN
  SELECT RAISE(ABORT, 'devices.push_token must be normalized');
END;

CREATE TRIGGER IF NOT EXISTS trg_devices_push_token_normalized_update
BEFORE UPDATE OF push_token ON devices
FOR EACH ROW
WHEN NEW.push_token IS NOT NULL
  AND NEW.push_token <> LOWER(TRIM(NEW.push_token))
BEGIN
  SELECT RAISE(ABORT, 'devices.push_token must be normalized');
END;
