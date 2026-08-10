#!/usr/bin/env bash
set -euo pipefail

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

command -v sqlite3 >/dev/null
command -v gpg >/dev/null

checksum() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

source_db="$TMP_DIR/source.sqlite"
source_sql="$TMP_DIR/source.sql"
encrypted="$TMP_DIR/source.sql.gpg"
restored_sql="$TMP_DIR/restored.sql"
restored_db="$TMP_DIR/restored.sqlite"
passphrase="${BACKUP_SMOKE_PASSPHRASE:-knock-knock-local-backup-smoke-passphrase}"

sqlite3 "$source_db" <<'SQL'
CREATE TABLE backup_fixture (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO backup_fixture (value) VALUES ('restore-smoke');
SQL
sqlite3 "$source_db" .dump > "$source_sql"
test -s "$source_sql"
grep -q 'CREATE TABLE backup_fixture' "$source_sql"

printf '%s' "$passphrase" | gpg --batch --yes --pinentry-mode loopback \
  --passphrase-fd 0 --symmetric --cipher-algo AES256 \
  --output "$encrypted" "$source_sql"
test -s "$encrypted"

printf '%s' "$passphrase" | gpg --batch --yes --pinentry-mode loopback \
  --passphrase-fd 0 --decrypt --output "$restored_sql" "$encrypted"
test "$(checksum "$source_sql")" = "$(checksum "$restored_sql")"

sqlite3 "$restored_db" < "$restored_sql"
test "$(sqlite3 "$restored_db" 'PRAGMA integrity_check;')" = "ok"
test "$(sqlite3 "$restored_db" 'SELECT value FROM backup_fixture;')" = "restore-smoke"

printf '%s\n' 'backup restore smoke passed: encrypted export/decrypt, checksum, SQLite integrity, and schema/data restore'
