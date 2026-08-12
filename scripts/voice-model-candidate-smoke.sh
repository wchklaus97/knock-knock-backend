#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CANDIDATE_SCRIPT="$ROOT_DIR/scripts/voice-model-candidate.sh"
MODEL_FILENAME="gemma3-1b-it-int4.litertlm"
EXPECTED_SIZE_BYTES="584417280"
EXPECTED_SHA256="1325ae366d31950f137c9c357b9fa89448b176d76998180c08ceaca78bba98be"
IPHONE13_MODEL_FILENAME="gemma3-270m-it-q8.litertlm"
IPHONE13_EXPECTED_SIZE_BYTES="304005120"
IPHONE13_EXPECTED_SHA256="757e9119fa5bd667a2774fb470ac4afcd3190a21c677f8e69a5d6bc908abdd63"

smoke_tmp="$(mktemp -d)"
cleanup() {
  rm -rf -- "$smoke_tmp"
}
trap cleanup EXIT

fake_bin="$smoke_tmp/bin"
fake_log="$smoke_tmp/hf-calls.log"
mkdir -p "$fake_bin"

cat > "$fake_bin/hf" <<'FAKE_HF'
#!/usr/bin/env bash
set -euo pipefail

: "${FAKE_HF_LOG:?}"
mode="${FAKE_HF_MODE:-approved}"

for argument in "$@"; do
  case "$argument" in
    --token|--token=*)
      echo "candidate script forwarded a forbidden token argument" >&2
      exit 90
      ;;
  esac
done
printf '%s\n' "$*" >> "$FAKE_HF_LOG"

case "${1:-}" in
  auth)
    [[ "${2:-}" == "whoami" ]]
    printf '%s\n' "fake-approved-user"
    ;;
  download)
    repository="${2:-}"
    filename="${3:-}"
    shift 3
    case "$repository" in
      litert-community/Gemma3-1B-IT)
        expected_revision="6d54daa71cfbffba6b2843c08eeb1a27e7430bf0"
        expected_filename="gemma3-1b-it-int4.litertlm"
        expected_size_bytes="584417280"
        ;;
      litert-community/gemma-3-270m-it)
        expected_revision="9d2093270fb5aa49a986b49b5779d763dde7b630"
        expected_filename="gemma3-270m-it-q8.litertlm"
        expected_size_bytes="304005120"
        ;;
      *)
        echo "fake hf received an unpinned repository" >&2
        exit 64
        ;;
    esac
    revision=""
    local_dir=""
    dry_run=0
    while (($# > 0)); do
      case "$1" in
        --revision) revision="${2:-}"; shift 2 ;;
        --local-dir) local_dir="${2:-}"; shift 2 ;;
        --dry-run) dry_run=1; shift ;;
        --quiet) shift ;;
        *) echo "fake hf received an unexpected argument" >&2; exit 64 ;;
      esac
    done
    [[ "$filename" == "$expected_filename" ]]
    [[ "$revision" == "$expected_revision" ]]

    if [[ "$mode" == "denied" ]]; then
      echo "403 Forbidden: gated repository access denied; accept the license" >&2
      exit 1
    fi
    if ((dry_run == 1)); then
      echo "dry-run approved"
      exit 0
    fi
    [[ -n "$local_dir" ]]
    mkdir -p "$local_dir"
    size_bytes="$expected_size_bytes"
    if [[ "$mode" == "size-mismatch" ]]; then
      size_bytes=$((expected_size_bytes - 1))
    fi
    dd if=/dev/zero of="$local_dir/$expected_filename" bs=1 count=0 seek="$size_bytes" 2>/dev/null
    printf '%s\n' "$local_dir/$expected_filename"
    ;;
  *)
    echo "fake hf received an unexpected command" >&2
    exit 64
    ;;
esac
FAKE_HF
chmod +x "$fake_bin/hf"

cat > "$fake_bin/sha256sum" <<'FAKE_SHA256SUM'
#!/usr/bin/env bash
set -euo pipefail

case "$(basename "$1")" in
  gemma3-1b-it-int4.litertlm)
    expected="1325ae366d31950f137c9c357b9fa89448b176d76998180c08ceaca78bba98be"
    ;;
  gemma3-270m-it-q8.litertlm)
    expected="757e9119fa5bd667a2774fb470ac4afcd3190a21c677f8e69a5d6bc908abdd63"
    ;;
  *)
    echo "fake sha256sum received an unexpected model" >&2
    exit 64
    ;;
esac
if [[ "${FAKE_HF_MODE:-approved}" == "hash-mismatch" ]]; then
  expected="0325ae366d31950f137c9c357b9fa89448b176d76998180c08ceaca78bba98be"
fi
printf '%s  %s\n' "$expected" "$1"
FAKE_SHA256SUM
chmod +x "$fake_bin/sha256sum"

fake_secret="hf_smoke_secret_must_not_appear"
export PATH="$fake_bin:$PATH"
export FAKE_HF_LOG="$fake_log"
export HF_TOKEN="$fake_secret"

: > "$fake_log"
FAKE_HF_MODE=approved "$CANDIDATE_SCRIPT" > "$smoke_tmp/preflight.out" 2>&1
grep -Fq "voice model candidate preflight passed" "$smoke_tmp/preflight.out"
grep -Fq "Preflight only; no model was downloaded" "$smoke_tmp/preflight.out"
grep -Fq "auth whoami" "$fake_log"
grep -Fq -- "--dry-run" "$fake_log"
if grep -Fq -- "--local-dir" "$fake_log"; then
  echo "default candidate preflight performed a download" >&2
  exit 1
fi

: > "$fake_log"
FAKE_HF_MODE=approved "$CANDIDATE_SCRIPT" \
  --tier iphone13-270m \
  --preflight > "$smoke_tmp/iphone13-preflight.out" 2>&1
grep -Fq "litert-community/gemma-3-270m-it" "$smoke_tmp/iphone13-preflight.out"
grep -Fq "$IPHONE13_MODEL_FILENAME" "$smoke_tmp/iphone13-preflight.out"
grep -Fq "$IPHONE13_EXPECTED_SIZE_BYTES" "$smoke_tmp/iphone13-preflight.out"
grep -Fq "$IPHONE13_EXPECTED_SHA256" "$smoke_tmp/iphone13-preflight.out"
grep -Fq -- "--dry-run" "$fake_log"
if grep -Fq -- "--local-dir" "$fake_log"; then
  echo "iPhone 13 candidate preflight performed a download" >&2
  exit 1
fi

: > "$fake_log"
set +e
FAKE_HF_MODE=denied "$CANDIDATE_SCRIPT" > "$smoke_tmp/denied.out" 2>&1
denied_status=$?
set -e
if [[ "$denied_status" -ne 77 ]]; then
  echo "license-denied preflight returned $denied_status instead of 77" >&2
  exit 1
fi
grep -Fq "Accept the Gemma license" "$smoke_tmp/denied.out"
grep -Fq "No model output was created" "$smoke_tmp/denied.out"

approved_dir="$smoke_tmp/approved"
approved_output="$approved_dir/$MODEL_FILENAME"
mkdir -p "$approved_dir"
: > "$fake_log"
FAKE_HF_MODE=approved "$CANDIDATE_SCRIPT" \
  --download \
  --output "$approved_output" > "$smoke_tmp/approved.out" 2>&1
test -f "$approved_output"
test ! -L "$approved_output"
test "$(wc -c < "$approved_output" | tr -d '[:space:]')" = "$EXPECTED_SIZE_BYTES"
grep -Fq -- "--local-dir" "$fake_log"
grep -Fq "verified voice model candidate created outside Git" "$smoke_tmp/approved.out"
grep -Fq "$EXPECTED_SHA256" "$smoke_tmp/approved.out"

iphone13_dir="$smoke_tmp/iphone13-approved"
iphone13_output="$iphone13_dir/$IPHONE13_MODEL_FILENAME"
mkdir -p "$iphone13_dir"
: > "$fake_log"
FAKE_HF_MODE=approved "$CANDIDATE_SCRIPT" \
  --tier iphone13-270m \
  --download \
  --output "$iphone13_output" > "$smoke_tmp/iphone13-approved.out" 2>&1
test -f "$iphone13_output"
test ! -L "$iphone13_output"
test "$(wc -c < "$iphone13_output" | tr -d '[:space:]')" = "$IPHONE13_EXPECTED_SIZE_BYTES"
grep -Fq -- "--local-dir" "$fake_log"
grep -Fq "$IPHONE13_EXPECTED_SHA256" "$smoke_tmp/iphone13-approved.out"

set +e
"$CANDIDATE_SCRIPT" --tier unknown > "$smoke_tmp/invalid-tier.out" 2>&1
invalid_tier_status=$?
set -e
if [[ "$invalid_tier_status" -ne 64 ]]; then
  echo "invalid tier returned $invalid_tier_status instead of 64" >&2
  exit 1
fi
grep -Fq -- "--tier must be default-1b or iphone13-270m" "$smoke_tmp/invalid-tier.out"

hash_mismatch_dir="$smoke_tmp/hash-mismatch"
hash_mismatch_output="$hash_mismatch_dir/$MODEL_FILENAME"
mkdir -p "$hash_mismatch_dir"
: > "$fake_log"
set +e
FAKE_HF_MODE=hash-mismatch "$CANDIDATE_SCRIPT" \
  --download \
  --output "$hash_mismatch_output" > "$smoke_tmp/hash-mismatch.out" 2>&1
hash_mismatch_status=$?
set -e
if [[ "$hash_mismatch_status" -ne 65 ]]; then
  echo "hash mismatch returned $hash_mismatch_status instead of 65" >&2
  exit 1
fi
test ! -e "$hash_mismatch_output"
grep -Fq "SHA-256 mismatch" "$smoke_tmp/hash-mismatch.out"
grep -Fq "$EXPECTED_SHA256" "$smoke_tmp/hash-mismatch.out"

mismatch_dir="$smoke_tmp/size-mismatch"
mismatch_output="$mismatch_dir/$MODEL_FILENAME"
mkdir -p "$mismatch_dir"
: > "$fake_log"
set +e
FAKE_HF_MODE=size-mismatch "$CANDIDATE_SCRIPT" \
  --download \
  --output "$mismatch_output" > "$smoke_tmp/size-mismatch.out" 2>&1
mismatch_status=$?
set -e
if [[ "$mismatch_status" -ne 65 ]]; then
  echo "size mismatch returned $mismatch_status instead of 65" >&2
  exit 1
fi
test ! -e "$mismatch_output"
grep -Fq "size mismatch" "$smoke_tmp/size-mismatch.out"
grep -Fq "$EXPECTED_SIZE_BYTES" "$smoke_tmp/size-mismatch.out"

overwrite_dir="$smoke_tmp/overwrite"
overwrite_output="$overwrite_dir/$MODEL_FILENAME"
overwrite_expected="$smoke_tmp/overwrite-expected"
mkdir -p "$overwrite_dir"
printf '%s\n' "do not replace" > "$overwrite_output"
cp "$overwrite_output" "$overwrite_expected"
: > "$fake_log"
set +e
FAKE_HF_MODE=approved "$CANDIDATE_SCRIPT" \
  --download \
  --output "$overwrite_output" > "$smoke_tmp/overwrite.out" 2>&1
overwrite_status=$?
set -e
if [[ "$overwrite_status" -ne 73 ]]; then
  echo "overwrite fence returned $overwrite_status instead of 73" >&2
  exit 1
fi
cmp "$overwrite_expected" "$overwrite_output"
test ! -s "$fake_log"
grep -Fq "refusing to overwrite" "$smoke_tmp/overwrite.out"

worktree_output="$ROOT_DIR/scripts/$MODEL_FILENAME"
test ! -e "$worktree_output"
: > "$fake_log"
set +e
FAKE_HF_MODE=approved "$CANDIDATE_SCRIPT" \
  --download \
  --output "$worktree_output" > "$smoke_tmp/worktree.out" 2>&1
worktree_status=$?
set -e
if [[ "$worktree_status" -ne 73 ]]; then
  echo "worktree output fence returned $worktree_status instead of 73" >&2
  exit 1
fi
test ! -e "$worktree_output"
test ! -s "$fake_log"
grep -Fq "inside the backend Git worktree" "$smoke_tmp/worktree.out"

if grep -Fq "$fake_secret" "$smoke_tmp"/*.out; then
  echo "candidate output exposed an authentication secret" >&2
  exit 1
fi
if grep -Eq '(^|[[:space:]])--token([=[:space:]]|$)' "$fake_log"; then
  echo "candidate invoked hf with a token argument" >&2
  exit 1
fi

echo "voice model candidate smoke passed: approved preflight/download, license denial, exact-size and SHA-256 verification, overwrite fence, worktree fence, and no token output"
