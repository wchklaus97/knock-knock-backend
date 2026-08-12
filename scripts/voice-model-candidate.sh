#!/usr/bin/env bash
set -euo pipefail

readonly DEFAULT_MODEL_TIER="default-1b"

usage() {
  cat >&2 <<'EOF'
usage: scripts/voice-model-candidate.sh [--tier default-1b|iphone13-270m] [--preflight]
       scripts/voice-model-candidate.sh [--tier default-1b|iphone13-270m] --download --output /absolute/outside-git/model.litertlm

Checks access to the pinned official LiteRT Gemma candidate using the existing
`hf auth login` state. With no arguments, only an authenticated Hugging Face
dry run is performed. A real download requires both --download and --output.

The script never accepts or forwards a token, refuses output in any Git
worktree, refuses overwrite, and verifies the exact expected byte size before
publishing the file at the requested path.
EOF
}

download_requested=0
preflight_requested=0
output=""
model_tier="$DEFAULT_MODEL_TIER"

while (($# > 0)); do
  case "$1" in
    --tier)
      if (($# < 2)) || [[ -z "${2:-}" ]]; then
        echo "--tier requires default-1b or iphone13-270m" >&2
        exit 64
      fi
      model_tier="$2"
      shift 2
      ;;
    --preflight)
      preflight_requested=1
      shift
      ;;
    --download)
      download_requested=1
      shift
      ;;
    --output)
      if (($# < 2)) || [[ -z "${2:-}" ]]; then
        echo "--output requires an absolute artifact path" >&2
        exit 64
      fi
      output="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unsupported argument; no token or repository override is accepted" >&2
      usage
      exit 64
      ;;
  esac
done

case "$model_tier" in
  default-1b)
    readonly MODEL_REPOSITORY="litert-community/Gemma3-1B-IT"
    readonly MODEL_REVISION="6d54daa71cfbffba6b2843c08eeb1a27e7430bf0"
    readonly MODEL_FILENAME="gemma3-1b-it-int4.litertlm"
    readonly MODEL_EXPECTED_SIZE_BYTES="584417280"
    readonly MODEL_EXPECTED_SHA256="1325ae366d31950f137c9c357b9fa89448b176d76998180c08ceaca78bba98be"
    ;;
  iphone13-270m)
    readonly MODEL_REPOSITORY="litert-community/gemma-3-270m-it"
    readonly MODEL_REVISION="9d2093270fb5aa49a986b49b5779d763dde7b630"
    readonly MODEL_FILENAME="gemma3-270m-it-q8.litertlm"
    readonly MODEL_EXPECTED_SIZE_BYTES="304005120"
    readonly MODEL_EXPECTED_SHA256="757e9119fa5bd667a2774fb470ac4afcd3190a21c677f8e69a5d6bc908abdd63"
    ;;
  *)
    echo "--tier must be default-1b or iphone13-270m" >&2
    exit 64
    ;;
esac

if ((preflight_requested == 1 && download_requested == 1)); then
  echo "--preflight and --download are mutually exclusive" >&2
  exit 64
fi
if ((download_requested == 0)) && [[ -n "$output" ]]; then
  echo "--output is valid only with --download" >&2
  exit 64
fi
if ((download_requested == 1)) && [[ -z "$output" ]]; then
  echo "--download requires --output outside every Git worktree" >&2
  exit 64
fi

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "required command is missing: $1" >&2
    exit 127
  fi
}

require_command hf
require_command grep
require_command mktemp

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
    return
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
    return
  fi
  echo "required SHA-256 command is missing: sha256sum or shasum" >&2
  exit 127
}

require_command awk

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
output_path=""
output_dir=""

if ((download_requested == 1)); then
  for command_name in basename chmod dirname git ln tr wc; do
    require_command "$command_name"
  done
  if [[ "$output" != /* ]]; then
    echo "--output must be an absolute path outside every Git worktree" >&2
    exit 64
  fi
  if [[ "$(basename "$output")" != "$MODEL_FILENAME" ]]; then
    echo "--output must end with the pinned filename: $MODEL_FILENAME" >&2
    exit 64
  fi
  output_dir="$(dirname "$output")"
  if [[ ! -d "$output_dir" ]]; then
    echo "--output parent directory must already exist" >&2
    exit 73
  fi
  output_dir="$(cd "$output_dir" && pwd -P)"
  output_path="$output_dir/$MODEL_FILENAME"

  case "$output_path" in
    "$root_dir"|"$root_dir"/*)
      echo "refusing model output inside the backend Git worktree" >&2
      exit 73
      ;;
  esac
  if [[ "$(git -C "$output_dir" rev-parse --is-inside-work-tree 2>/dev/null || true)" == "true" ]]; then
    echo "refusing model output inside a Git worktree" >&2
    exit 73
  fi
  if [[ -e "$output_path" || -L "$output_path" ]]; then
    echo "refusing to overwrite existing model output" >&2
    exit 73
  fi
fi

umask 077
control_tmp="$(mktemp -d)"
download_tmp=""
cleanup() {
  if [[ -n "$download_tmp" && -d "$download_tmp" ]]; then
    rm -rf -- "$download_tmp"
  fi
  if [[ -n "$control_tmp" && -d "$control_tmp" ]]; then
    rm -rf -- "$control_tmp"
  fi
}
trap cleanup EXIT

is_access_denied() {
  grep -Eiq '(^|[^0-9])403([^0-9]|$)|gated|forbidden|access[[:space:]_-]*(denied|restricted)|not[[:space:]_-]*authorized|accept.*(license|conditions)|license.*accept' "$1"
}

license_error() {
  cat >&2 <<EOF
Hugging Face access to the pinned Gemma repository was denied.
Accept the Gemma license at https://huggingface.co/$MODEL_REPOSITORY while
signed in to the same account used by 'hf auth login', then rerun preflight.
No model output was created.
EOF
  exit 77
}

if ! HF_HUB_DISABLE_UPDATE_CHECK=1 hf auth whoami \
  > /dev/null 2> "$control_tmp/auth.err"; then
  cat >&2 <<'EOF'
No usable existing Hugging Face login was found. Run 'hf auth login'
interactively, accept the Gemma license in the browser, and rerun preflight.
Do not pass a token on this command line.
EOF
  exit 77
fi

if ! HF_HUB_DISABLE_UPDATE_CHECK=1 hf download \
  "$MODEL_REPOSITORY" \
  "$MODEL_FILENAME" \
  --revision "$MODEL_REVISION" \
  --dry-run \
  > /dev/null 2> "$control_tmp/preflight.err"; then
  if is_access_denied "$control_tmp/preflight.err"; then
    license_error
  fi
  cat >&2 <<EOF
Hugging Face preflight failed for the pinned model candidate.
No model output was created. Check network availability and the installed 'hf'
CLI, then retry without adding a token argument.
EOF
  exit 69
fi

printf 'voice model candidate preflight passed:\n  source: %s\n  revision: %s\n  filename: %s\n  expected size: %s bytes\n  expected sha256: %s\n' \
  "$MODEL_REPOSITORY" \
  "$MODEL_REVISION" \
  "$MODEL_FILENAME" \
  "$MODEL_EXPECTED_SIZE_BYTES" \
  "$MODEL_EXPECTED_SHA256"

if ((download_requested == 0)); then
  echo "Preflight only; no model was downloaded."
  exit 0
fi

download_tmp="$(mktemp -d "$output_dir/.voice-model-candidate.XXXXXX")"
if ! HF_HUB_DISABLE_UPDATE_CHECK=1 hf download \
  "$MODEL_REPOSITORY" \
  "$MODEL_FILENAME" \
  --revision "$MODEL_REVISION" \
  --local-dir "$download_tmp" \
  --quiet \
  > /dev/null 2> "$control_tmp/download.err"; then
  if is_access_denied "$control_tmp/download.err"; then
    license_error
  fi
  echo "Hugging Face candidate download failed; no model output was created" >&2
  exit 69
fi

candidate_path="$download_tmp/$MODEL_FILENAME"
if [[ ! -f "$candidate_path" || -L "$candidate_path" ]]; then
  echo "Hugging Face did not produce the pinned regular model file; no output was created" >&2
  exit 65
fi
actual_size_bytes="$(wc -c < "$candidate_path" | tr -d '[:space:]')"
if [[ "$actual_size_bytes" != "$MODEL_EXPECTED_SIZE_BYTES" ]]; then
  printf 'downloaded model size mismatch: expected %s bytes, received %s; no output was created\n' \
    "$MODEL_EXPECTED_SIZE_BYTES" "${actual_size_bytes:-unknown}" >&2
  exit 65
fi
actual_sha256="$(sha256_file "$candidate_path")"
if [[ "$actual_sha256" != "$MODEL_EXPECTED_SHA256" ]]; then
  printf 'downloaded model SHA-256 mismatch: expected %s, received %s; no output was created\n' \
    "$MODEL_EXPECTED_SHA256" "${actual_sha256:-unknown}" >&2
  exit 65
fi

chmod 600 "$candidate_path"
if [[ -e "$output_path" || -L "$output_path" ]] || \
  ! ln "$candidate_path" "$output_path" 2>/dev/null; then
  echo "refusing to overwrite existing model output" >&2
  exit 73
fi

published_size_bytes="$(wc -c < "$output_path" | tr -d '[:space:]')"
published_sha256="$(sha256_file "$output_path")"
if [[ ! -f "$output_path" || -L "$output_path" \
  || "$published_size_bytes" != "$MODEL_EXPECTED_SIZE_BYTES" \
  || "$published_sha256" != "$MODEL_EXPECTED_SHA256" ]]; then
  rm -f -- "$output_path"
  echo "published model failed final size verification; output was removed" >&2
  exit 74
fi

printf 'verified voice model candidate created outside Git:\n  artifact: %s\n  size: %s bytes\n  sha256: %s\n' \
  "$output_path" "$published_size_bytes" "$published_sha256"
