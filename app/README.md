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
| `sidecar.rs` | Client for `instance/sidecar.py` (heartbeat, status, ticket, Invoke image list + full-res download) |
| `sync.rs` | Output sync: gallery → output folder, Canvas tries → `Canvas\`, per-instance ledger |
| `remote.rs` | The isolated Invoke window, plus the injected tutorial overlay (`tutorial.js`, `assets/tutorial-sample.jpg`) |
| `secrets.rs` | Credential Manager via `keyring` |
| `persist.rs` | Active-instance record for crash recovery |
| `redact.rs` | Secret redaction for logs and diagnostics |
| `diagnostics.rs` | "Copy diagnostics" report (version, state history, log, settings), redacted as a whole |
| `updater.rs` | Update rules (no install while a GPU runs); the plugin is driven from `lib.rs` commands only |
| `config.rs` | Pinned image and instance assets (this version's release URL + SHA-256), settings (user-editable subset saved to settings.json), LoRA merge, create-call spec |
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
| `SLOPTWEAK_MOCK_IMAGES=N` | Mock mode: after Ready, the fake sidecar makes N gallery images (one per 3 s) plus Canvas tries and scratch intermediates |
| `SLOPTWEAK_MOCK_UPDATE=<version>` | Mock mode: pretend that version is released (update banner, install is a no-op) |
| `SLOPTWEAK_DEV_ASSETS_URL` | Where instances fetch the asset bundle (default: the `instance-v0.1.1` pre-release, same bytes as the pin). Release builds always use `releases/download/v<version>/` |
| `SLOPTWEAK_MOCK_SIDECAR=http` + `SLOPTWEAK_DEV_LAUNCH_SECRET` | Mock mode: talk to a real `sidecar.py` at `SLOPTWEAK_MOCK_REMOTE` with this fixed launch secret (`dev/sync-check.mjs`) |

Mock mode saves its fake images to `%LOCALAPPDATA%\com.sloptweak.launcher\mock\output`,
not to Pictures.

## Checks in `dev/`

- `webview-check.mjs`: $0. Remote-window isolation against a local sidecar.
- `sync-check.mjs`: $0. Phase 4 end to end: the real app + real `sidecar.py` in
  front of `fake_invoke.py`. Tutorial (auto-show, sample upload, steps,
  auto-advance, Done recorded, re-open), sync while running, and images made
  right before Stop on disk after it.
- `mock-ui-check.mjs`: $0. Wizard, estimate, low-balance gate, settings, LoRAs,
  upstream catalog edit (local server), restart persistence, cost bar, update
  offer, Copy diagnostics (redaction, clipboard), close-to-destroy. All against
  the mock. `GUIDE_SHOTS=1` hides the MOCK badge for `docs/images/`. It
  overwrites the clipboard.
- `vast-acceptance.mjs`: **spends money.** Phase 2 acceptance on real Vast.
  Destroys everything it creates in `finally`.
- `fresh-profile-acceptance.mjs`: **spends money, and wipes this app's real
  keys and folders.** Phase 3 acceptance: a person completes the wizard and
  makes the first image; the script checks, stops, and restarts.
- `phase4-acceptance.mjs`: **spends money.** Phase 4 acceptance: the tutorial
  on a clean tutorial state (driven in the real Invoke UI; falls back to a
  person), then 10 images, Stop, and all 10 on disk.
- `fake_invoke.py`: the stand-in Invoke API those checks use.
  `make-tutorial-sample.py` redraws the tutorial picture.

All scripts refuse to run if :1420 serves another checkout's UI (a vite left
running elsewhere keeps the port despite `--strictPort`).

## Updates and releases

`tauri-plugin-updater` checks
`https://github.com/ljohnsoncpu/SlopTweak/releases/latest/download/latest.json`
3 s after launch (Vast mode). The installer's minisign signature is checked
against `plugins.updater.pubkey`. No window has an updater or clipboard
capability; `check_update`, `install_update`, and `copy_diagnostics` are app
commands. Release process: [docs/releasing.md](../docs/releasing.md).

## Wizard screenshots

The wizard shows `src/wizard/<name>.png` when present (`vast-signup`,
`vast-billing`, `vast-keys`, `civitai-signup`, `civitai-keys`). Crop them to
the relevant buttons and blur account details: the repo is public.
