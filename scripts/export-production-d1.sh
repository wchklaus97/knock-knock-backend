#!/usr/bin/env bash
set -euo pipefail

OUTPUT="${1:?usage: $0 /path/to/knock-knock-YYYYMMDD.sql}"
CONFIG="${KNOCK_KNOCK_PRODUCTION_CONFIG:-wrangler.production.toml}"

umask 077
mkdir -p "$(dirname "$OUTPUT")"
wrangler d1 export knock-knock \
  --remote \
  --config "$CONFIG" \
  --output "$OUTPUT"
chmod 600 "$OUTPUT"
echo "D1 export written to $OUTPUT"
