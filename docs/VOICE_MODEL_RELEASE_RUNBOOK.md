# Voice Model Release Runbook

> **中文摘要：** 代码已经支持 LiteRT-LM、签名验证、私有 R2 下载和回滚；真正的 Gemma 模型文件不能由 CI 自动取得。发布负责人必须先在 Hugging Face 接受 Gemma 许可，下载官方 `.litertlm`，在仓库外用 Ed25519 私钥签名，再通过受保护的 staging 配置发布。私钥、Hugging Face token 和模型文件都不得提交到 Git。

## Scope

This runbook turns an operator-approved Gemma LiteRT-LM artifact into the
signed descriptor consumed by Knock Knock. It does not authorize a production
rollout. The application is currently pinned to the LiteRT-LM `v0.12.0` C
framework so the main target can retain its iOS 15 deployment floor.

The recommended first artifact is the 4-bit Gemma 3 1B `.litertlm` published
by the LiteRT community. Its Hugging Face repository is publicly listed but
gated by the Gemma license. A human must accept that license and create their
own access token; neither the repository nor CI may bypass that step.

## Security boundaries

- Keep the Ed25519 private key, Hugging Face token, `.litertlm` artifact, APNs
  key, `.env` files, and production values outside both repositories.
- Upload the model under `models/{model_id}/...litertlm` in a private R2
  bucket. The API exposes an authenticated same-origin download route and
  never returns the R2 object key.
- Pin the raw 32-byte Ed25519 public key into the reviewed iOS build setting
  `KNOCK_MODEL_PUBLIC_KEY_BASE64`.
- The app accepts artifacts from 1 byte through 2 GiB and activates one only
  after manifest shape, artifact size, SHA-256, Ed25519 signature, capability,
  and rollback checks pass. Both active and rollback artifacts are reverified
  before their persisted selection is restored after relaunch.
- An authenticated model download may follow redirects only within the exact
  HTTPS API origin. Credential-free downloads may redirect across origins only
  to another HTTPS URL; ambient cookies and URL credentials are disabled.
- Model output is untrusted. iOS canonicalizes the draft, and the backend
  independently owns action allowlisting, argument validation, risk,
  confirmation, ownership, and idempotency.

## Prepare a candidate

1. Sign in to Hugging Face and accept the Gemma license for
   `litert-community/Gemma3-1B-IT`.
2. Download the pinned `gemma3-1b-it-int4.litertlm` revision using a personal
   token. Record the source repository, revision, filename, and license review
   in the release ticket. Do not place the token or artifact in a worktree.
3. Generate or select an operator-controlled Ed25519 key outside the
   repositories. For a new test key:

   ```bash
   umask 077
   openssl genpkey -algorithm ED25519 -out /secure/path/voice-model-ed25519.pem
   ```

4. Create release metadata in a new, private output directory:

   ```bash
   ./scripts/voice-model-release.sh \
     --artifact /secure/path/gemma3-1b-it-int4.litertlm \
     --private-key /secure/path/voice-model-ed25519.pem \
     --model-id gemma-command \
     --model-version 1.0.0 \
     --minimum-capability cpu-v1 \
     --output-dir /secure/path/gemma-command-1.0.0
   ```

   `--model-version` must be valid SemVer (including correct prerelease/build
   syntax and precedence rules) and no longer than 128 characters. The script
   rejects artifacts larger than 2,147,483,648 bytes before hashing them.

5. Independently verify the emitted `manifest.json` and
   `public-key.base64`. The script refuses to overwrite an existing release
   directory.

## Signed payload format

The Ed25519 signature does not cover the digest alone. It covers the following
UTF-8/ASCII payload exactly, including the final newline. The field values are
validated so they cannot contain separators, and `sha256` is lowercase:

```text
com.knockknock.voice-model-manifest.ed25519.v1
schema_version=1
model_id={model_id}
model_version={model_version}
sha256={64-character lowercase SHA-256}
size_bytes={base-10 artifact byte count}
minimum_capability={minimum_capability}
```

Any change to schema, model ID, version, hash, size, or capability invalidates
the signature. The descriptor's outer `model_id` must also equal the signed
manifest's `model_id`.

## Staging rollout

1. Upload the exact artifact to a private staging R2 key such as
   `models/gemma-command/1.0.0/gemma3-1b-it-int4.litertlm`.
2. Configure staging with `VOICE_MODEL_ENABLED=true`, the private
   `VOICE_MODEL_R2_KEY`, and the exact one-line `VOICE_MODEL_MANIFEST_JSON`.
   Do not configure an external public URL when private R2 is used.
3. Build the Staging iOS configuration with the emitted public key in
   `KNOCK_MODEL_PUBLIC_KEY_BASE64`.
4. Run `scripts/voice-model-r2-smoke.sh`, the staging contract gate, the iOS
   signed-download/rollback tests, and the real-model golden suite.
5. Attach per-locale accuracy, zero high-risk false-execution evidence, p50 and
   p95 intent latency, peak memory, thermal state, crash count, and iPhone 13
   screenshots/logs to the release ticket.

The real-model test is opt-in and requires only local paths and a public key:

```bash
KNOCK_VOICE_MODEL_PATH=/secure/path/model.litertlm \
KNOCK_VOICE_MODEL_MANIFEST_PATH=/secure/path/manifest.json \
KNOCK_MODEL_PUBLIC_KEY_BASE64="$(tr -d '\n' </secure/path/public-key.base64)" \
xcodebuild test ...
```

Do not export the Hugging Face token or private signing key to the test
process.

## Acceptance and rollback

The model gate passes only when all 32 checked-in locale-tagged examples run
against the signed artifact with at least 95% intent accuracy, zero high-risk
false executions, and the device performance limits approved in the release
ticket. Structural fixture tests without the artifact are useful but do not
pass the real-model gate.

Rollback is configuration-only: disable `VOICE_MODEL_ENABLED` or restore the
previous signed manifest/R2 key and iOS trust configuration. Keep the previous
artifact until all supported clients have reported successful rollback. Never
delete the active or previous R2 object before that evidence exists.
