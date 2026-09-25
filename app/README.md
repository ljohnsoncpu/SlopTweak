# SlopTweak launcher (Tauri 2)

Rust core in `src-tauri/`, plain-TS UI in `src/`.

## Build and test

```bash
npm install
npm run typecheck
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Run against the mock provider (no Vast calls, no money):

```bash
SLOPTWEAK_PROVIDER=mock npx tauri dev
```

`SLOPTWEAK_MOCK_SCRIPT=daemon,dead,normal` scripts per-create failures
(`daemon`, `dead`, `stuck`, `taken`, `provfail`, `selfdestruct`, `normal`)
so the retry and failure paths can be seen in the UI.

## Module map

| File | What |
| --- | --- |
| `provider/mod.rs` | `GpuProvider` trait, instance health rules, tunnel-label validation |
| `provider/vast.rs` | Vast REST client (all Vast calls) |
| `provider/mock.rs` | `MockProvider` + `MockSidecar` with scripted stages |
| `provider/offers.rs` | Offer filtering and session-cost ranking (incl. bandwidth) |
| `session.rs` | Pure state machine (`next`) and the `SessionManager` driver |
| `sidecar.rs` | Client for `instance/sidecar.py` (heartbeat, status, ticket) |
| `remote.rs` | The isolated Invoke window |
| `secrets.rs` | Credential Manager via `keyring` |
| `persist.rs` | Active-instance record for crash recovery |
| `redact.rs` | Secret redaction for logs |
| `config.rs` | Pinned image/assets, settings, create-call spec |

## Dev-only hooks (debug builds only)

| Env | Effect |
| --- | --- |
| `SLOPTWEAK_PROVIDER=mock` | Use `MockProvider` |
| `SLOPTWEAK_DEV_IMPORT_KEYS=1` | Copy `VAST_API_KEY`/`CIVITAI_TOKEN` from env or `HKCU\Environment` into Credential Manager if missing |
| `SLOPTWEAK_MAIN_DEBUG_PORT`, `SLOPTWEAK_REMOTE_DEBUG_PORT` | CDP port for the main / remote window |
| `SLOPTWEAK_DEV_REMOTE_URL` | Open the remote window at this URL on startup |

## Checks in `dev/`

- `webview-check.mjs`: $0. Remote-window isolation against a local sidecar.
- `mock-ui-check.mjs`: $0. UI flow against the mock, including close-to-destroy.
- `vast-acceptance.mjs`: **spends money.** Phase 2 acceptance on real Vast.
  Destroys everything it creates in `finally`.
