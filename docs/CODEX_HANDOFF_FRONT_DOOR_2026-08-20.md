# Codex handoff pointer — front door / Staging daily (2026-08-20)

Full handoff lives in the iOS worktree (copy that file into Codex):

`/Users/klaus_mac/Projects/01-Active/voice-agent-bridge/.worktrees/structured-memory-ios-v2/docs/CODEX_HANDOFF_FRONT_DOOR_2026-08-20.md`

This backend worktree: `structured-memory-backend-v2` on `gauntlet/staging-confirm-drain-20260817`.

Hard locks: no Production deploy; Staging send stays off; Ask listening window is exclusive 90s.

Key files here: `src/asks.rs`, `src/db.rs`, `scripts/asks-listening-window-tests.sh`, `scripts/staging-ask-front-door-uat.sh`.
