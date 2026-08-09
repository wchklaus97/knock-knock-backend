# API Contracts

`openapi.yaml` is the canonical REST, SSE, error, pagination, and
`CommandEnvelope v1` contract for Knock Knock.

Rules:

- Additive changes land first; breaking changes require a new schema version
  and an ADR entry.
- Existing `/v1/phone/sessions/{id}/reply` and `/confirm` paths are compatibility
  adapters while the command endpoints are rolled out.
- Generated examples and iOS decoding fixtures must be derived from this file.
- Do not put access tokens, provider secrets, raw audio, or full sensitive
  transcripts into examples or fixtures.

Run the lightweight structural check from the backend repository root:

```bash
./scripts/contract-schema-smoke.sh
```

