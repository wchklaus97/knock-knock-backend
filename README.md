# knock-knock-backend

Rust API for Knock Knock, deployed as a Cloudflare Worker with D1 SQLite storage.

The route contract intentionally stays compatible with the existing iPhone and
MCP clients:

- `/v1/auth/*` — user registration, login, refresh and logout
- `/v1/agents/*` and `/v1/pairing/*` — agent keys and one-time pairing
- `/v1/skills` — skill definitions
- `/v1/sessions/*` and `/v1/actions/*` — agent session/action loop
- `/v1/phone/*` — phone inbox, replies, confirmations and devices
- `/v1/dev/pushes` — development push inbox

## Local development

Requirements: Rust, the `wasm32-unknown-unknown` target, `worker-build`, and
Wrangler.

```sh
cargo check --target wasm32-unknown-unknown
worker-build --release
wrangler d1 migrations apply DB --local
wrangler dev
```

Run the repeatable contract loop while `wrangler dev` is running:

```sh
./scripts/contract-smoke.sh
```

The local endpoint is `http://127.0.0.1:8787`. For the iPhone, use the Mac's
LAN address instead of `127.0.0.1`.

Set a development secret in `.dev.vars`:

```text
JWT_SECRET=replace-this-for-local-development
```

For Cloudflare deployment:

```sh
wrangler d1 create knock-knock
# Put the returned UUID into wrangler.toml as database_id.
wrangler d1 migrations apply DB --remote
wrangler secret put JWT_SECRET
# Required for real APNs delivery (use the .p8 contents as the key value).
wrangler secret put APNS_KEY
wrangler secret put APNS_KEY_ID
wrangler secret put APNS_TEAM_ID
wrangler deploy
```

Before the first production deploy, change the remote vars from their local
defaults to `NODE_ENV=production`, `PUSH_MODE=apns` (or `both`), a specific
`CORS_ORIGIN`, and the correct APNs bundle/environment. The Worker returns a
generic message for HTTP 5xx responses so database or signing details are not
leaked to clients.

`PUSH_MODE=dev` stores push events in D1 so the current iPhone development
inbox continues to work. For a production phone build, set `PUSH_MODE=apns`
or `PUSH_MODE=both`, configure `APNS_BUNDLE_ID` and
`APNS_PRODUCTION=true`, and register the iOS device token through
`POST /v1/phone/devices`. The Worker signs APNs provider tokens with the
configured Apple `.p8` key; missing or failed APNs delivery falls back to the
development inbox when appropriate instead of being reported as a false
success.

The old Node API remains in the original `voice-agent-bridge` tree as a
migration reference. The Rust Worker is now the backend in this repository and
uses the same HTTP paths and auth headers, so existing iPhone, Codex, Cursor,
and Paperclip clients can switch by pointing `BRIDGE_API_URL` at this Worker.
