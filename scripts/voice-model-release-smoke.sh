#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
smoke_tmp="$(mktemp -d)"
cleanup() {
  rm -rf -- "$smoke_tmp"
}
trap cleanup EXIT

artifact="$smoke_tmp/gemma-command.litertlm"
private_key="$smoke_tmp/signing-private.pem"
output_dir="$smoke_tmp/release"
printf 'deterministic voice model release smoke artifact\n' > "$artifact"
openssl genpkey -algorithm ED25519 -out "$private_key" >/dev/null 2>&1

"$ROOT_DIR/scripts/voice-model-release.sh" \
  --artifact "$artifact" \
  --private-key "$private_key" \
  --model-version 1.2.3-rc.1+smoke.7 \
  --output-dir "$output_dir" \
  --model-id gemma-command \
  --minimum-capability cpu-v1 >/dev/null

python3 - "$artifact" "$private_key" "$output_dir" <<'PY'
import base64
import hashlib
import json
import pathlib
import subprocess
import sys
import tempfile

artifact = pathlib.Path(sys.argv[1])
private_key = pathlib.Path(sys.argv[2])
output_dir = pathlib.Path(sys.argv[3])
manifest = json.loads((output_dir / "manifest.json").read_text())
public_key = base64.b64decode((output_dir / "public-key.base64").read_text().strip(), validate=True)
signature = base64.b64decode(manifest["signature"], validate=True)
assert manifest["schema_version"] == 1
assert manifest["model_id"] == "gemma-command"
assert manifest["model_version"] == "1.2.3-rc.1+smoke.7"
assert manifest["sha256"] == hashlib.sha256(artifact.read_bytes()).hexdigest()
assert manifest["size_bytes"] == artifact.stat().st_size
assert manifest["minimum_capability"] == "cpu-v1"
assert len(public_key) == 32
assert len(signature) == 64

with tempfile.TemporaryDirectory() as directory:
    directory = pathlib.Path(directory)
    payload = directory / "payload.bin"
    signature_file = directory / "signature.bin"
    public_pem = directory / "public.pem"
    signature_file.write_bytes(signature)

    def signature_payload(document):
        return (
            "com.knockknock.voice-model-manifest.ed25519.v1\n"
            f"schema_version={document['schema_version']}\n"
            f"model_id={document['model_id']}\n"
            f"model_version={document['model_version']}\n"
            f"sha256={document['sha256'].lower()}\n"
            f"size_bytes={document['size_bytes']}\n"
            f"minimum_capability={document['minimum_capability']}\n"
        ).encode("ascii")

    payload.write_bytes(signature_payload(manifest))
    subprocess.run(
        ["openssl", "pkey", "-in", str(private_key), "-pubout", "-out", str(public_pem)],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    subprocess.run(
        [
            "openssl", "pkeyutl", "-verify", "-pubin", "-rawin",
            "-inkey", str(public_pem), "-in", str(payload), "-sigfile", str(signature_file),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
    )

    mutations = {
        "schema_version": 2,
        "model_id": "gemma-command-mutated",
        "model_version": "1.2.4",
        "sha256": ("0" if manifest["sha256"][0] != "0" else "1") + manifest["sha256"][1:],
        "size_bytes": manifest["size_bytes"] + 1,
        "minimum_capability": "gpu-v1",
    }
    for field, value in mutations.items():
        mutated = dict(manifest)
        mutated[field] = value
        payload.write_bytes(signature_payload(mutated))
        verification = subprocess.run(
            [
                "openssl", "pkeyutl", "-verify", "-pubin", "-rawin",
                "-inkey", str(public_pem), "-in", str(payload),
                "-sigfile", str(signature_file),
            ],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        assert verification.returncode != 0, f"signature accepted mutated {field}"
PY

if "$ROOT_DIR/scripts/voice-model-release.sh" \
  --artifact "$artifact" \
  --private-key "$private_key" \
  --model-version 1.2.3-rc.1+smoke.7 \
  --output-dir "$output_dir" >/dev/null 2>&1; then
  echo "voice model release unexpectedly overwrote existing output" >&2
  exit 1
fi

invalid_index=0
for invalid_version in 01.2.3 1.02.3 1.2.03 1.2.3-01 1.2.3-alpha..1 1.2.3+build+again; do
  invalid_index=$((invalid_index + 1))
  if "$ROOT_DIR/scripts/voice-model-release.sh" \
    --artifact "$artifact" \
    --private-key "$private_key" \
    --model-version "$invalid_version" \
    --output-dir "$smoke_tmp/invalid-$invalid_index" >/dev/null 2>&1; then
    echo "voice model release accepted invalid SemVer: $invalid_version" >&2
    exit 1
  fi
done

echo "voice model release smoke passed: domain-separated Ed25519 metadata signature, mutation rejection, SemVer validation, raw public key, and overwrite fence"
