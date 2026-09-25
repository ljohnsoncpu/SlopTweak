# Invoke Launcher — Implementation Plan

A Windows desktop app that lets a non-technical person generate and inpaint
images with open-weight models on a rented Vast.ai GPU, without touching a
terminal, Docker, or the Vast console after signup.

Each user brings **their own Vast.ai account and their own CivitAI key**. The
author of this app never holds user credentials or pays for user compute.

---

## 1. Product requirements

The user experience, end to end:

1. Download a signed `.exe` installer, run it.
2. First-run wizard: create Vast account → add credit → create Vast API key →
   create CivitAI API key. Each key is validated the moment it is pasted.
3. Home screen: pick a model (friendly names, not filenames), press **Start**.
4. Plain-language progress ("Renting a GPU… Downloading the model… Almost
   ready"), typically under 5 minutes.
5. InvokeAI opens inside the app window. A running cost + balance bar stays
   visible ("$0.42/hr · $8.10 left · 38 min").
6. Every generated image is copied to `Pictures\Invoke Launcher\` automatically.
7. **Stop** (or closing the app) destroys the instance. Nothing is lost,
   nothing keeps billing.
8. Optional first-run tutorial: guided inpainting on a sample image.

### Non-goals (v1)
- macOS/Linux builds (keep the code portable; don't ship them yet).
- Shared/pooled accounts, billing, or any server run by the app author.
- ComfyUI or node-graph UIs.
- Persistent Vast volumes (outputs sync locally instead).
- Training, LoRA management beyond "paste a CivitAI link".

### Hard requirements
- **No surprise bills.** An instance must self-destruct if the launcher
  disappears (crash, sleep, network loss), and on idle.
- **Never publicly exposed.** The remote UI requires a per-instance secret the
  user never sees.
- **Secrets at rest in Windows Credential Manager**, never plaintext on disk,
  never in logs or diagnostics bundles.
- **Model catalog updatable without shipping a new exe.**

---

## 2. Architecture

```
┌──────────────── User's PC ─────────────────┐        ┌────────── Vast.ai instance ──────────┐
│ Tauri app (Rust core + TS UI)              │        │ InvokeAI container (official image)  │
│                                            │        │                                      │
│  • Wizard / home / status screens (local)  │  HTTPS │  sidecar (Python, port 8080)         │
│  • Vast REST client ───────────────────────┼──────► │   • one-time token → session cookie  │
│  • Keychain (Credential Manager)           │        │   • /__heartbeat                     │
│  • Catalog fetcher (catalog.json)          │        │   • reverse proxy → Invoke :9090     │
│  • Heartbeat loop (every 60 s)             │        │     (HTTP + websockets/socket.io)    │
│  • Output sync (poll Invoke images API)    │        │   • watchdog: no heartbeat 10 min    │
│  • Remote webview window → sidecar URL     │        │     or idle N min → self-destroy     │
└────────────────────────────────────────────┘        │  provision.sh: fetch models w/ token │
                                                      │  cloudflared quick tunnel (HTTPS)    │
                                                      └──────────────────────────────────────┘
```

### Key decisions
| Decision | Choice | Why |
| --- | --- | --- |
| Desktop framework | **Tauri 2** | Small signed installer, uses WebView2, Rust core for keychain + HTTP. PyInstaller exes get flagged by AV; Electron is ~10× larger. |
| Launcher UI | TypeScript + Svelte (or plain TS) | Only a few screens; keep it light. |
| Instance image | **Official InvokeAI image, unmodified** + onstart script | No custom image to build, host, or license-check. Models are downloaded at startup with the user's own CivitAI key. |
| Provisioning | `onstart` pulls a pinned `provision.sh` + `sidecar.py` from this repo's GitHub Release, verifies SHA-256, runs them | Update instance-side logic without rebuilding images. |
| Auth | Sidecar in front of Invoke; launcher opens `/__auth?t=<one-time token>` → HttpOnly session cookie → redirect to `/` | Webviews can't easily inject headers on navigation; cookie exchange is simple and robust. Token is single-use and expires in 60 s. |
| Transport | `cloudflared` quick tunnel from the instance (HTTPS URL, no account) | Avoids sending cookies over plain HTTP to a raw IP. Fallback: direct port + token (see §9). |
| Self-destruct | Sidecar watchdog calls Vast API to destroy its own instance | Must work when the PC is gone. Credential choice is an open question (§9). |
| Outputs | Launcher polls Invoke's images API via the sidecar and downloads new full-res files | Destroy is always safe; no volumes. |
| Model catalog | `catalog.json` hosted in this repo (raw GitHub / release asset), cached locally | Add models by editing JSON. |
| Provider abstraction | All Vast calls behind a `GpuProvider` trait | RunPod can be added later without touching the UI. |

---

## 3. Repo layout

```
invoke-launcher/
├── PLAN.md                  # this file
├── CLAUDE.md                # agent conventions (create in Phase 0)
├── app/                     # Tauri app
│   ├── src-tauri/           # Rust: provider, keychain, heartbeat, sync, commands
│   │   ├── src/provider/    # GpuProvider trait, vast.rs, mock.rs
│   │   ├── src/secrets.rs   # keyring crate wrapper
│   │   ├── src/session.rs   # start/stop state machine
│   │   ├── src/sync.rs      # output sync
│   │   └── src/catalog.rs
│   └── src/                 # TS UI: wizard, home, status, tutorial
├── instance/
│   ├── provision.sh         # runs from Vast onstart
│   ├── sidecar.py           # auth proxy + heartbeat + watchdog
│   └── tests/
├── catalog/catalog.json
├── docs/                    # user-facing guide with screenshots
└── .github/workflows/       # build, sign, release, publish instance assets
```

---

## 4. Phases

Each phase ends with a working, demoable state. Don't start a phase until the
previous one's acceptance criteria pass.

### Phase 0 — Spikes (verify assumptions before writing product code)
Everything below is an assumption from memory; confirm against current docs
and a real $1 rental, and record findings in `docs/findings.md`.

1. **Vast REST API**: base URL, auth header, endpoints for: offer search,
   create instance from offer (with image, env, onstart, disk, ports),
   instance status, destroy, current user balance, per-instance $/hr.
2. **Instance self-identity**: does the container get `CONTAINER_ID` and an
   instance-scoped API key (e.g. `CONTAINER_API_KEY`)? Can that key destroy
   its own instance? Does Vast support restricted/scoped user API keys?
3. **Official InvokeAI image** on Vast: which tag, cold-pull time on a
   fast-internet host, where `INVOKEAI_ROOT` lives, how to register a model
   file at startup (model install API vs. scan-on-startup in the pinned
   version).
4. **Invoke API**: endpoints to list images newest-first and fetch full-res
   files; whether socket.io works through a simple reverse proxy.
5. **cloudflared quick tunnel** from inside a Vast container: works, supports
   websockets, how to read the assigned URL, rate limits/ToS fit.
6. **Tauri remote webview**: open a second window at a remote HTTPS URL with
   **no** Tauri IPC exposed to that origin; cookies persist for the session.

**Accept:** `findings.md` answers every item; open questions in §9 are
resolved or escalated to the user.

### Phase 1 — Instance side, driven by hand
- `provision.sh`: read env (`CIVITAI_TOKEN`, `MODEL_URLS`, `LAUNCH_TOKEN_HASH`,
  `IDLE_MINUTES`, `HEARTBEAT_MINUTES`), download models with retry + size
  check, register them with Invoke, start Invoke, start sidecar, start tunnel,
  write the tunnel URL somewhere the launcher can read (instance label via
  API, or a status endpoint on a known port — decide in Phase 0).
- `sidecar.py`: one-time token exchange → signed cookie; reject everything
  else with 401; proxy HTTP + websockets to `127.0.0.1:9090`;
  `/__heartbeat`; `/__status` (stage: downloading / starting / ready, with
  %); watchdog that destroys the instance on heartbeat loss or idle
  (idle = no Invoke queue activity and no proxied requests).
- pytest coverage for token exchange, cookie validation, heartbeat/idle
  timers (injectable clock), destroy call (mocked).

**Accept:** create an instance manually with the Vast API (curl/script),
reach Invoke through the tunnel only after token exchange, generate + inpaint,
kill the heartbeat and watch the instance destroy itself within the window.

### Phase 2 — Launcher skeleton + provider
- Tauri app scaffold, `GpuProvider` trait, `VastProvider`, `MockProvider`
  (fake stages with delays, for UI work without spending money).
- `secrets.rs` over the `keyring` crate.
- Session state machine: `Idle → Renting → Provisioning → Ready → Stopping →
  Idle`, plus `Failed(reason)`. Persist the active instance ID to disk so a
  relaunch after a crash can find and destroy (or reattach to) it.
- Offer selection: filter verified, reliability ≥ threshold, min download
  speed, min VRAM from catalog entry, max $/hr from settings; pick cheapest.
- Auto-retry: if not `Ready` within N minutes, destroy and try the next offer
  (max 3 attempts), then fail with a plain message.
- Heartbeat loop while `Ready`.
- On app close: confirm → destroy.

**Accept:** Start → Invoke visible in app window → Stop destroys; crash the
app mid-session and confirm (a) watchdog destroys, (b) relaunch detects the
orphan and cleans up.

### Phase 3 — Wizard, catalog, cost bar
- Wizard screens with deep links and screenshots; validate Vast key (balance
  call) and CivitAI key (authenticated metadata call) on paste.
- Low-balance gate: refuse to start below a configurable floor; warn when
  balance < 1 hour of runtime.
- `catalog.json` schema: `id, name, description, base (sdxl/flux/…),
  files[{url, sha256?, kind}], min_vram_gb, license_note, invoke_min_version`.
  Fetch on launch, fall back to cached copy.
- "Add a LoRA from a CivitAI link" (validated, stored in local settings,
  downloaded at provision time).
- Cost bar: $/hr, elapsed, balance (refresh every few minutes).
- Settings: model, max $/hr, idle minutes, max session hours, output folder.

**Accept:** a fresh Windows user profile goes from install to first image
using only the wizard; keys survive app restart; editing `catalog.json`
upstream changes the model list without a rebuild.

### Phase 4 — Output sync + tutorial
- Poll images endpoint every ~10 s via the sidecar session; download new
  full-res images to the output folder; track synced IDs locally; final
  sync pass before destroy (Stop waits for it, with a timeout).
- "Open output folder" button.
- Tutorial overlay: loads a bundled sample image into Canvas and walks
  through mask → prompt → generate. Keep it skippable and re-openable.

**Accept:** generate 10 images, press Stop, all 10 are on disk; tutorial
completes on a clean install.

### Phase 5 — Ship
- Code signing (Azure Trusted Signing or an OV cert — confirm individual
  eligibility), in CI.
- `tauri-plugin-updater` with signed GitHub Releases; instance assets
  published to the same release with SHA-256s that the app pins.
- "Copy diagnostics" button: app version, state machine log, instance
  stage history, **redacted** of all keys/tokens (unit-test the redaction).
- User guide in `docs/` with screenshots; a one-page "what this costs and
  how billing works" explainer.
- Beta with 1–2 non-technical users; watch them without helping; fix what
  they trip on.

**Accept:** signed installer installs without SmartScreen block; auto-update
works from vN to vN+1; beta users complete a session unassisted.

---

## 5. Security checklist
- Vast key and CivitAI key only in Credential Manager; never logged, never in
  diagnostics, never in URLs.
- CivitAI token is passed to the instance as an env var — visible to the
  host operator. Acceptable (user's own, revocable key); state this in the
  wizard.
- Launch token: random 256-bit, single-use, 60 s expiry; only its hash goes
  in instance env.
- Remote webview window has no Tauri IPC/capabilities; only local windows do.
- Sidecar binds the only exposed port; Invoke listens on localhost only.
- Instance-side scripts are fetched by pinned version + SHA-256.
- The credential the watchdog uses to self-destroy must be the least
  privileged option available (see §9).

## 6. Cost safety checklist
- Heartbeat loss → destroy (default 10 min).
- Idle → destroy (default 20 min, user-adjustable).
- Max session length → destroy (default 4 h, warn at −10 min).
- App close → destroy (with confirm).
- Relaunch → find orphaned instances tagged by this app and offer to destroy.
- Low-balance gate before start.

## 7. Testing
- Rust: unit tests for offer filtering, state machine transitions, orphan
  recovery, redaction; `MockProvider` for everything UI-side.
- Python: pytest for sidecar with injectable clock and mocked Vast API.
- One scripted end-to-end run against real Vast per release (cheap GPU,
  small model), gated behind a manual CI trigger.

## 8. Rough effort
| Phase | Effort |
| --- | --- |
| 0 Spikes | 1–2 sessions |
| 1 Instance side | 2–3 sessions |
| 2 Launcher + provider | 3–4 sessions |
| 3 Wizard/catalog/cost | 2–3 sessions |
| 4 Sync + tutorial | 2 sessions |
| 5 Ship | 2 sessions + beta time |

## 9. Open questions — ask the user, don't assume
1. **Watchdog credential.** If Vast has no instance-scoped or restricted key
   that can destroy only its own instance, the fallback options are: (a) pass
   the user's full Vast key to the instance (bad: host can read it), (b) have
   the sidecar exit and rely on stopped-instance behaviour (still bills
   storage), (c) launcher-only cleanup (fails when the PC is off). Needs a
   decision after Phase 0.
2. **Transport.** cloudflared quick tunnel vs. direct port with token over
   HTTP. Confirm quick-tunnel ToS/rate limits are acceptable for this use.
3. **Content policy.** Vast hosts and CivitAI have their own content rules;
   how much should the app say about this, and where?
4. **Starting model set** for `catalog.json`, and their licenses.
5. **App name** and GitHub org/repo for releases and the catalog.
6. **Signing identity** (individual vs. business) and budget.
