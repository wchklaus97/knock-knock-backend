# Voice Model Release Runbook

> **中文摘要：** 代码已经支持 LiteRT-LM、签名验证、私有 R2 下载和回滚。Gemma 3 1B 是当前唯一通过准确率门禁的设备模型；270M 最佳准确率只有 50%，已被明确拒绝，不能下载或发布。发布负责人必须在仓库外处理模型和 Ed25519 私钥，再通过受保护的 staging 配置发布。私钥、Hugging Face token 和模型文件都不得提交到 Git。

## Scope

This runbook turns an operator-approved Gemma LiteRT-LM artifact into the
signed descriptor consumed by Knock Knock. It does not authorize a production
rollout. The application is currently pinned to the LiteRT-LM `v0.12.0` C
framework so the main target can retain its iOS 15 deployment floor.

The approved default artifact is pinned to the official LiteRT community
source below. The Hugging Face repository is gated by the Gemma license. A
human must accept its license grant and authenticate the `hf` CLI; neither the
repository nor CI may bypass that step.

| Tier | Repository | Revision | Filename | Size | SHA-256 |
|---|---|---|---|---:|---|
| `default-1b` | `litert-community/Gemma3-1B-IT` | `6d54daa71cfbffba6b2843c08eeb1a27e7430bf0` | `gemma3-1b-it-int4.litertlm` | 584417280 | `1325ae366d31950f137c9c357b9fa89448b176d76998180c08ceaca78bba98be` |

### Rejected 270M tier

The pinned 304,005,120-byte Gemma 3 270M q8 artifact was evaluated against the
same checked-in 32-example synthetic command set. The original prompt scored
0.125 with command p95 2.469 seconds. JSON-schema constrained decoding scored
0.000 because the model vocabulary is unsupported by the LiteRT FST constraint
provider. A shortened unconstrained prompt improved accuracy to 0.500 with
command p95 1.533 seconds. Every run recorded zero high-risk false executions,
but none met the 0.950 accuracy threshold.

The `iphone13-270m` tier is therefore rejected for iPhone 13, staging, and
production. `voice-model-candidate.sh` exits 78 before authentication,
preflight, or download when that tier is requested. iPhone 13 remains on the
deterministic parser plus clarification path until a future model passes a new
reviewed RFC and all release gates.

## Security boundaries

- Keep the Ed25519 private key, Hugging Face token, `.litertlm` artifact, APNs
  key, `.env` files, and production values outside both repositories.
- Upload the model under `models/{model_id}/...litertlm` in a private R2
  bucket. The API exposes an authenticated same-origin download route and
  never returns the R2 object key.
- Keep the raw 32-byte Ed25519 public key in a private file outside Git. The
  reviewed iOS archive/UAT scripts receive only that file path and a matching
  private Info.plist path. Release builds fail when either file is missing,
  invalid, symlinked, or mismatched; key contents are never passed as an
  `xcodebuild` command-line argument.
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

## Gemma 4 remains an isolated experiment

The repository
[`wchklaus97/swift-gemma4-sample`](https://github.com/wchklaus97/swift-gemma4-sample)
was reviewed at commit `5dcc5f060ebac8c5e1d1ebed09c8b706a5394fac`.
It is useful as an experimental adapter reference, but it must not be linked
into the current release target:

- it uses MLX and the third-party `Swift-gemma4-core` package rather than the
  pinned LiteRT-LM C runtime;
- it raises the deployment floor from iOS 15 to iOS 17;
- its local `mlx-community/gemma-4-e2b-it-4bit` directory is roughly 3.4 GB in
  the current workspace and does not fit the signed `.litertlm` delivery path;
- its sample runtime currently calls `applyChatTemplate`, while the dependency's
  own verified Gemma 4 path requires `Gemma4PromptFormatter`; that mismatch must
  be corrected before treating the sample as a trustworthy device baseline.

Keep Gemma 3 1B INT4 as the verified default tier on supported newer devices.
Do not install a local LLM on iPhone 13 in this release; use deterministic
parsing and clarification because 1B misses its latency target and 270M misses
the accuracy target. A future
Gemma 4 RFC may introduce a separately feature-flagged adapter for newer
devices after artifact size, prompt formatting, cancellation, memory, thermal,
and golden-set gates pass. Every tier must continue to emit only
`CommandEnvelope v1`; no model gains authority to execute commands locally.

## Prepare a candidate

1. Sign in to Hugging Face, accept the Gemma license for the exact repository
   selected above, and authenticate locally using the
   interactive `hf auth login` flow. Never put the token in this runbook's
   commands, a shell history argument, CI, or Git.
2. Run the pinned access preflight. It uses only the existing `hf` login and
   performs an authenticated dry run; by default it downloads nothing:

   ```bash
   ./scripts/voice-model-candidate.sh
   ```

   Exit 77 with an “Accept the Gemma license” message means the logged-in
   account has not been granted access. Accept the license in the browser and
   rerun the same preflight. Do not add `--token`.
3. Create a private destination outside every Git worktree, then explicitly
   download the pinned candidate:

   ```bash
   umask 077
   mkdir -p /secure/path/gemma3-1b-it-candidate
   ./scripts/voice-model-candidate.sh \
     --download \
     --output /secure/path/gemma3-1b-it-candidate/gemma3-1b-it-int4.litertlm
   ```

   The script pins repository, revision, and filename; refuses an output in a
   Git worktree or an existing output; suppresses all `hf` output; and publishes
   the artifact only after the selected tier's exact byte size and pinned
   SHA-256 both match. Record the tier, pinned values, and license review in
   the release ticket.
4. Generate or select an operator-controlled Ed25519 key outside the
   repositories. For a new test key:

   ```bash
   umask 077
   openssl genpkey -algorithm ED25519 -out /secure/path/voice-model-ed25519.pem
   ```

5. Create release metadata in a new, private output directory:

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

6. Independently verify the emitted `manifest.json` and
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
3. Build the Staging iOS configuration through the reviewed frontend
   archive/UAT script, supplying the emitted public-key file and a matching
   private Info.plist by path. Do not put key contents in shell arguments or
   commit either file.
4. Run `scripts/voice-model-r2-smoke.sh`, the staging contract gate, the iOS
   signed-download/rollback tests, and the real-model golden suite.
5. Attach per-locale accuracy, zero high-risk false-execution evidence, p50 and
   p95 intent latency, peak memory, thermal state, crash count, and iPhone 13
   screenshots/logs to the release ticket.

The real-model test is opt-in and uses only private local paths. Use the
frontend repository's `scripts/ios-voice-model-uat.sh`; it validates and
injects the public key through the protected path-based build flow:

```bash
./scripts/ios-voice-model-uat.sh \
  --model /secure/path/model.litertlm \
  --manifest /secure/path/manifest.json \
  --public-key /secure/path/public-key.base64
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
