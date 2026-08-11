#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: scripts/voice-model-release.sh \
  --artifact PATH.litertlm \
  --private-key ED25519_PRIVATE_KEY.pem \
  --model-version X.Y.Z \
  --output-dir DIRECTORY \
  [--model-id gemma-command] \
  [--minimum-capability cpu-v1]

Signs a deterministic, domain-separated manifest payload with an operator-owned
Ed25519 key. The payload binds schema, model identity/version, artifact SHA-256
and size, and minimum capability. The private key is never copied or printed.
The command refuses to overwrite an existing manifest or public-key output.
EOF
}

artifact=""
private_key=""
model_version=""
output_dir=""
model_id="gemma-command"
minimum_capability="cpu-v1"
maximum_artifact_size_bytes=2147483648

while (($# > 0)); do
  case "$1" in
    --artifact) artifact="${2:-}"; shift 2 ;;
    --private-key) private_key="${2:-}"; shift 2 ;;
    --model-version) model_version="${2:-}"; shift 2 ;;
    --output-dir) output_dir="${2:-}"; shift 2 ;;
    --model-id) model_id="${2:-}"; shift 2 ;;
    --minimum-capability) minimum_capability="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 64 ;;
  esac
done

for command_name in openssl xxd mktemp; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "required command is missing: $command_name" >&2
    exit 127
  fi
done
if [[ ! -f "$artifact" || ! -s "$artifact" || "$artifact" != *.litertlm ]]; then
  echo "--artifact must be a non-empty .litertlm file" >&2
  exit 64
fi
if [[ ! -f "$private_key" || ! -s "$private_key" ]]; then
  echo "--private-key must be a non-empty Ed25519 PEM file" >&2
  exit 64
fi
if [[ ! "$model_id" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]]; then
  echo "--model-id is invalid" >&2
  exit 64
fi
semver_pattern='^(0|[1-9][0-9]*)[.](0|[1-9][0-9]*)[.](0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)([.](0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?([+]([0-9A-Za-z-]+([.][0-9A-Za-z-]+)*))?$'
if [[ ${#model_version} -gt 128 || ! "$model_version" =~ $semver_pattern ]]; then
  echo "--model-version must be a valid SemVer value no longer than 128 characters" >&2
  exit 64
fi
if [[ ! "$minimum_capability" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]]; then
  echo "--minimum-capability is invalid" >&2
  exit 64
fi
if [[ -z "$output_dir" ]]; then
  echo "--output-dir is required" >&2
  exit 64
fi

size_bytes="$(wc -c < "$artifact" | tr -d '[:space:]')"
if [[ ! "$size_bytes" =~ ^[0-9]+$ ]] || ((size_bytes == 0 || size_bytes > maximum_artifact_size_bytes)); then
  echo "--artifact must be between 1 and $maximum_artifact_size_bytes bytes" >&2
  exit 64
fi

umask 077
mkdir -p -- "$output_dir"
manifest_path="$output_dir/manifest.json"
public_key_path="$output_dir/public-key.base64"
if [[ -e "$manifest_path" || -e "$public_key_path" ]]; then
  echo "refusing to overwrite release output in $output_dir" >&2
  exit 73
fi

release_tmp="$(mktemp -d)"
cleanup() {
  rm -f -- "$release_tmp/payload.bin" "$release_tmp/signature.bin" \
    "$release_tmp/public.der" "$release_tmp/public.pem"
  rmdir -- "$release_tmp" 2>/dev/null || true
}
trap cleanup EXIT

if command -v sha256sum >/dev/null 2>&1; then
  digest_hex="$(sha256sum "$artifact" | awk '{print $1}')"
else
  digest_hex="$(shasum -a 256 "$artifact" | awk '{print $1}')"
fi

# This byte-for-byte format is mirrored by ModelManifest.signaturePayload on
# iOS. Validated values cannot contain a newline or '=' separator.
{
  printf '%s\n' 'com.knockknock.voice-model-manifest.ed25519.v1'
  printf 'schema_version=%s\n' '1'
  printf 'model_id=%s\n' "$model_id"
  printf 'model_version=%s\n' "$model_version"
  printf 'sha256=%s\n' "$digest_hex"
  printf 'size_bytes=%s\n' "$size_bytes"
  printf 'minimum_capability=%s\n' "$minimum_capability"
} > "$release_tmp/payload.bin"

# pkeyutl -rawin is required for Ed25519's one-shot signing operation.
openssl pkeyutl -sign -rawin \
  -inkey "$private_key" \
  -in "$release_tmp/payload.bin" \
  -out "$release_tmp/signature.bin"
test "$(wc -c < "$release_tmp/signature.bin" | tr -d ' ')" -eq 64

openssl pkey -in "$private_key" -pubout -out "$release_tmp/public.pem" >/dev/null
openssl pkey -in "$private_key" -pubout -outform DER -out "$release_tmp/public.der" >/dev/null
public_der_hex="$(xxd -p "$release_tmp/public.der" | tr -d '\n')"
expected_prefix="302a300506032b6570032100"
if [[ ${#public_der_hex} -ne 88 || "${public_der_hex:0:24}" != "$expected_prefix" ]]; then
  echo "--private-key is not an Ed25519 key" >&2
  exit 64
fi
public_key_base64="$(printf '%s' "${public_der_hex:24}" | xxd -r -p | openssl base64 -A)"
signature_base64="$(openssl base64 -A -in "$release_tmp/signature.bin")"

openssl pkeyutl -verify -pubin -rawin \
  -inkey "$release_tmp/public.pem" \
  -in "$release_tmp/payload.bin" \
  -sigfile "$release_tmp/signature.bin" >/dev/null

printf '%s\n' "$public_key_base64" > "$public_key_path"
printf '{\n  "schema_version": 1,\n  "model_id": "%s",\n  "model_version": "%s",\n  "sha256": "%s",\n  "signature": "%s",\n  "size_bytes": %s,\n  "minimum_capability": "%s"\n}\n' \
  "$model_id" \
  "$model_version" \
  "$digest_hex" \
  "$signature_base64" \
  "$size_bytes" \
  "$minimum_capability" > "$manifest_path"

chmod 600 "$manifest_path" "$public_key_path"
printf 'voice model release metadata created:\n  manifest: %s\n  public key: %s\n' \
  "$manifest_path" "$public_key_path"
printf 'The signature covers the domain-separated manifest payload, not the digest alone. Keep the private key outside the repository. Upload the artifact to private R2, then configure the manifest, R2 key, and pinned iOS public key through reviewed release settings.\n'
