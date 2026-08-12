#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: scripts/ci-prerequisites.sh <static|dynamic|staging|storage|health|backup|ios>

Profiles describe the tools required by each release-gate entry point. The
check is intentionally based on command -v so a caller cannot rely on an
undeclared runner PATH.
EOF
}

profile="${1:-}"
case "${profile}" in
  static|dynamic|staging|storage|health|backup|ios) ;;
  *) usage; exit 64 ;;
esac

if [[ -z "${PATH:-}" ]]; then
  echo "ci prerequisites: PATH is empty; configure the runner PATH first" >&2
  exit 127
fi

missing=()

require_commands() {
  local command_name
  for command_name in "$@"; do
    if ! command -v "${command_name}" >/dev/null 2>&1; then
      missing+=("${command_name}")
    fi
  done
}

require_wasm_target() {
  if ! rustc --print target-libdir --target wasm32-unknown-unknown >/dev/null 2>&1; then
    missing+=("rust target wasm32-unknown-unknown")
  fi
}

require_ruby_yaml() {
  if ! ruby -e 'require "yaml"' >/dev/null 2>&1; then
    missing+=("ruby yaml/psych support")
  fi
}

case "${profile}" in
  static)
    require_commands bash cargo rustc git grep sed awk ruby python3 sqlite3 gpg cmp mktemp seq sleep openssl xxd
    require_wasm_target
    require_ruby_yaml
    if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
      missing+=("sha256sum or shasum")
    fi
    ;;
  dynamic)
    require_commands bash cargo rustc curl jq python3 npm grep sed awk sqlite3 wrangler worker-build cmp mktemp seq sleep kill openssl xxd
    require_wasm_target
    ;;
  staging)
    require_commands bash curl jq npm wrangler grep sed awk mktemp seq sleep
    ;;
  storage)
    require_commands bash curl jq wrangler grep sed awk cmp mktemp seq sleep openssl xxd
    ;;
  health)
    require_commands bash curl jq grep sed awk mktemp seq sleep
    ;;
  backup)
    require_commands bash npm sed grep gpg shred wrangler
    ;;
  ios)
    require_commands bash xcodebuild xcrun
    ;;
esac

if ((${#missing[@]} > 0)); then
  echo "ci prerequisites missing for profile '${profile}': ${missing[*]}" >&2
  cat >&2 <<'EOF'
Install the missing tools using the repository/CI-pinned setup for this gate,
then rerun the same entry point. Do not replace a missing command with an
unbounded package-manager install inside a production job.
EOF
  exit 127
fi

echo "ci prerequisites passed: ${profile}"
