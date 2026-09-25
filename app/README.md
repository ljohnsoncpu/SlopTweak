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
(`daemon`, `dead`, `stuck`, `taken`, `provfail`, `selfdestruct`, `slowpull`, `normal`)
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
| `config.rs` | Pinned image/assets, settings (user-editable subset saved to settings.json), LoRA merge, create-call spec |
| `catalog.rs` | Catalog fetch from GitHub, validation, cache, bundled fallback |
| `civitai.rs` | CivitAI key check (`/me`) and LoRA links → verified file metadata |
| `cost.rs` | Low-balance gate and cost bar (pure) |

## Dev-only hooks (debug builds only)

| Env | Effect |
| --- | --- |
| `SLOPTWEAK_PROVIDER=mock` | Use `MockProvider` + `MockCivitai`. Mock mode has its own Credential Manager service (`SlopTweak-mock`) and `…\mock` folders |
| `SLOPTWEAK_MOCK_CREDIT` | Mock credit in dollars (default 11.55), for the low-balance gate |
| `SLOPTWEAK_DEV_RESET=1` | Mock mode only: wipe the mock profile (keys, settings, catalog cache) at startup |
| `SLOPTWEAK_CATALOG_URL` | Fetch the catalog from here instead of GitHub |
| `SLOPTWEAK_DEV_IMPORT_KEYS=1` | Vast mode only: copy `VAST_API_KEY`/`CIVITAI_TOKEN` from env or `HKCU\Environment` into Credential Manager if missing |
| `SLOPTWEAK_MAIN_DEBUG_PORT`, `SLOPTWEAK_REMOTE_DEBUG_PORT` | CDP port for the main / remote window |
| `SLOPTWEAK_DEV_REMOTE_URL` | Open the remote window at this URL on startup |

## Checks in `dev/`

- `webview-check.mjs`: $0. Remote-window isolation against a local sidecar.
- `mock-ui-check.mjs`: $0. Wizard, estimate, low-balance gate, settings, LoRAs,
  upstream catalog edit (local server), restart persistence, cost bar,
  close-to-destroy. All against the mock.
- `vast-acceptance.mjs`: **spends money.** Phase 2 acceptance on real Vast.
  Destroys everything it creates in `finally`.
- `fresh-profile-acceptance.mjs`: **spends money, and wipes this app's real
  keys and folders.** Phase 3 acceptance: a person completes the wizard and
  makes the first image; the script checks, stops, and restarts.

## Wizard screenshots

The wizard shows `src/wizard/<name>.png` when present (`vast-signup`,
`vast-billing`, `vast-keys`, `civitai-signup`, `civitai-keys`). Crop them to
the relevant buttons and blur account details: the repo is public.
