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
- Node-graph UIs. (Amended 2026-10-02, user decision: ComfyUI may run as a
  hidden second backend for the Identity Edit panel; the user never sees a
  node graph. See "Backends" under Key decisions.)
- Persistent Vast volumes (outputs sync locally instead).
- LoRA management beyond "paste a CivitAI link". (Training: amended
  2026-10-26, user decision; see Phase T.)

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
| Backends | `backend: invoke` (default, stock InvokeAI image) or `backend: comfyui` (Vast ComfyUI image, ComfyUI on 127.0.0.1:8188 behind the same sidecar) per catalog entry | Identity Edit (Wulver) needs ComfyUI; Invoke instances are untouched. The ComfyUI backend has a SlopTweak panel in the local window, no remote ComfyUI UI. |

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

### Phase 6 — Tutorial v2 (user decisions 2026-09-26)
Replaces the vase tour. One character carries through every stage, and
each stage teaches one idea. Stages are resumable: the user can leave
between them and pick up where they left off. Skip and minimize stay.

The fixed prompt (user-supplied), 832×1216:

- Positive: `a woman standing in front of a white background,` then on
  its own line `solo, female, human, white background, black tank top,
  black jeans, red eyes, red hair, simple background, front view,
  forehead, standing, medium shot, smile`
- Negative: `nsfw`

1. **Prompting.** In the Generate tab, generate the character above. Say
   briefly what the two parts of the prompt are: the first line is a
   natural-language sentence and the second is tags. Each model was
   trained on its own caption style, so check the model page (CivitAI
   example images, the description) for what it expects. The negative
   prompt teaches positive vs. negative, and `nsfw` there keeps a first
   generation safe on an NSFW-capable model (Banana Splitz).
2. **Prompt Templates.** From the model page, take the tags that appear
   in every example (quality/style tags) and put them in a Prompt
   Template (**Create Prompt Template**, with `{prompt}` where the user's
   words go). This is how users set up their first templates. Point out
   that a template isn't tied to a model. Which tags to suggest for each
   catalog model gets decided during tuning.
3. **Generated image → canvas.** Right-click the result in the gallery →
   **New Canvas from Image → As Raster Layer (Resize)**. A **Use ours
   instead** button uploads the bundled fallback portrait
   (`app/src-tauri/assets/tutorial-portrait.webp`, user-supplied, made from
   this prompt) if the user's generation is unusable.
4. **Small edit: fix and recolor the eyes** (red → green; not blue, so it
   doesn't mix with the visor). The fallback's eyes have visible artifacts;
   this step fixes them too. Mask the eyes, press **Shift+B** (Fit Bbox To
   Masks), then drag the bbox out to include the whole face. The mask is
   what gets redrawn; the bbox is what the model sees. In a medium shot the
   face is small, so this shows off Scale Before Processing (Auto):
   generating the small bbox at full model resolution is what fixes the
   detail. Denoising Strength: try about 0.3 (barely changes), then about
   0.55.
5. **Big edit: transparent cyberpunk visor.** Add a Raster Layer, paint a
   rough blue band across the eyes, and **lower that layer's opacity** so
   the eyes show through. That teaches layer opacity and gives the model a
   see-through tint to work from. Mask over the band, add the visor to the
   prompt, and use a higher denoise, about 0.6–0.75. Contrast with step 4:
   small edits need no paint, big ones do. Tuning decides between that and
   the alternative (lower the opacity of the accepted result layer), which
   risks ghosting the whole bbox area. Finish with Accept / Save To
   Gallery (see findings: Accept alone doesn't save).

Invoke facts this relies on are in `docs/findings.md` → "Tutorial v2
groundwork".

**Before building copy that depends on numbers:** tune the suggested
seed, denoise values, visor opacity, and template tags on real GPUs with
each catalog base (Illustrious, Anima). This needs a paid rental; get
approval first (GPU, $/hr, duration, cap). The overlay structure,
stage persistence, fallback upload, and mock tests can be built before
that.

**Accept:** a clean install goes through all five stages on each catalog
model using the given values; the fallback path works; every label named
in the copy matches the pinned Invoke version (checked live).

---

### Phase T — LoRA trainer (user decision 2026-10-26)
Goal: replace the hand-driven Codex pipeline (Thalia run, 2026-10-25) with a
Train tab. Reference run: SDXL U-Net LoRA on Banana Splitz XXL V2.0, 23
images, rank/alpha 16, AdamW 1e-4, 920 steps, 24 GB RTX 3090, about 15 min.
Pinned trainer: kohya sd-scripts (commit pinned in the instance bundle).
Scope v1: SDXL/Illustrious character LoRAs only; other bases become presets.
- **T1 (done):** `app/src-tauri/src/train/`: settings and kohya config
  rendering (`settings.rs`), dataset audit and bucket prediction
  (`dataset.rs`), and the approve, persist, watch, sync, destroy lifecycle
  against `GpuProvider` with `MockProvider` tests (`run.rs`).
- **T2 (done; untested on a real instance):** `instance/train_runner.py`
  (bootstrap, prepare, train, evaluate, hash manifest), `/__train/*` sidecar
  endpoints, a `BACKEND=train` branch in `provision.sh`, the asset bundle and
  its pin, and in Rust `train/job.rs` (job.json, dataset tar, launch spec),
  `train/client.rs` (upload, state, verified result download) and
  `train/commands.rs` (`train_defaults`, `train_audit`). The run manager that
  starts, watches and cancels a run ships with the UI in T3.
- **T3 (built; real run pending):** `train/manager.rs` (quote, approve, run,
  status, cancel; the Start button is the approval of one exact quote that
  expires in 10 minutes), `train/sim.rs` (mock-mode stand-in for the GPU),
  CivitAI checkpoint resolution, Tauri commands, and the Train tab
  (`app/src/train.ts`). The UI was checked in the browser pane against a
  stubbed backend (`app/dev/train-preview.html`); the Rust side was tested with
  `MockProvider`. Acceptance: the third real run (2026-10-26) reproduced Thalia end
  to end (loss 0.0775 vs 0.0773, 208 test images, about $0.18, GPU destroyed).
Invariants: nothing is rented without an approval matching the exact offer and
limits; the record is saved before the create call; results are synced and
hash-verified before destroy unless under two minutes remain; destroy runs on
every exit path.

### Phase K — Kromoss (MOSS-Image) LoRA trainer (user decision 2026-10-27)
Goal: a second trainer in the Train tab for character LoRAs on Kromoss, the
MOSS-Image fine-tune. Spike and run logs: `docs/spikes/kromoss-lora-training.md`;
verified numbers: `docs/findings.md`. Trainer: KohakuMossTrainer, fetched at run
time at a pinned commit and tree digest, never vendored.
- **K0 (done):** desk research, a local smoke test and three Vast runs (3090,
  4090 benchmark, 4090 `mix` run, about $0.42). Settled: on-demand rentals only
  (a `price` field makes an interruptible bid), host CUDA >= 12.8, an output
  watchdog, checkpoints synced as they appear.
- **K1 (done in mock mode, commit 8a5cd30):** `train/moss.rs` (pins, settings,
  config, jsonl, `collect`), `moss_launch_spec` and `moss_offer_query`, the
  `Trainer` enum in `manager.rs`, `train_moss_*` commands, the trainer picker in
  `train.ts`, and `MossRunner` in `train_runner.py`. Never run on a real instance.
- **K2 (pending):** one small character set on a 24 GB card, with the user's
  approval of GPU, $/hr, duration and cap. The MOSS venv is hash-locked (`instance/moss_requirements.lock`, installed on a real instance 2026-10-27); 
  next is the run itself, recorded in `findings.md`.
- **K3 (pending):** evaluation images (MOSS ComfyUI graph with the exported LoRA
  against a base control, or the trainer's sampler), the saved per-card
  kernel-tuning cache, and a `kromoss` catalog entry for the MOSS panel.
- **K3 progress (2026-10-28):** the `kromoss` catalog entry is R0's; a trained LoRA can be tried in the
  MOSS panel (upload, strength, base control; committed) and picked from the latest runs' results
  without a file dialog (`train/recent.rs`); the bundle carries the 4090 kernel-tuning tables
  (`moss_kernel_cache.tgz`, restored before training) and every run hands its own tables back as
  `kernel-cache.tgz` for `kernel_cache.py merge`. Both are untried on an instance.
Invariants are Phase T's.

### Phase R — Regional-LoRA layouts in the MOSS panel (user decision 2026-10-28)
Goal: a **Layout** mode in the MOSS panel. The user draws labelled boxes, gives a box
its own LoRA, and gets one picture where each LoRA only changes its own box. Mechanism
(settled by the spike): MOSS's trained-in JSON layout prompt makes the base picture, then
one masked img2img pass per LoRA box, pasted back through the mask, all in one ComfyUI
graph. ComfyUI hook LoRAs were tried and rejected (int8 crash; bf16 bleed, broken layout,
6-40x slower). Evidence: `docs/findings.md` ("Regional-LoRA spike" sections); design:
`docs/spikes/regional-lora-design.md`.
Settled 2026-10-28: v1 is **Fast mode only**; **Layout is a mode inside the MOSS panel**;
LoRAs come from **local files uploaded to the instance** (CivitAI-at-provision later);
limits 12 boxes, 6 LoRA passes, 1 GB per LoRA, 8 LoRAs per session; the result is the
final picture only.
- **R0 (code done, unit-tested; never run on an instance):** the `kromoss` catalog entry
  (all files pinned: Kromoss at HF revision `a4fe9744`, the shared int8 encoder, the
  small FLUX.2 VAE, the DMD2 LoRA), `moss::ModelFiles.time_delta` (the time-embedding
  patch is optional), `provision.sh` recognising a MOSS instance by either main file and
  building the delta only when a time-embedding file exists, and `moss_workflow.py`
  without a patch node. The bundle changed, so `ASSETS_SHA256` is re-pinned; the entry
  works on a real instance only after a release publishes that bundle (needs the user's OK).
  This is also Phase K3's catalog entry.
- **R1 (done; unit-tested and run on the local ComfyUI, never on an instance):**
  `app/src-tauri/src/moss/layout.rs` (`LayoutRequest`, `layout::check`, layout JSON, `extend_graph`
  called by `build_graph`, scaled timeout, estimate), wired into `moss.rs`. Local acceptance
  (`spikes/regional-lora/check_graphs.py`): the Rust graph reproduces the spike's layout picture
  byte for byte, outside-box pixels are identical, inside the box changes. No UI.
- **R2 (done; 43/43 UI checks against the mock GPU, never on a real instance):** the Layout tab in the MOSS
  panel: `app/src/layout.ts` + `layout.css` (boxes as divs over an SVG grid; draw, move, resize
  by mouse; arrow / Alt+arrow / `[` `]` / Delete by keyboard; box list with reorder; position
  fields; kind, description, words, LoRA, strength, change amount), small hooks in `main.ts` and
  `index.html`, `app/dev/layout-preview.html` (stub page) and `app/dev/layout-ui-check.mjs`
  (real window, mock GPU, real mouse and key events). Per-box LoRAs come from the panel's own
  uploads; the editor's "Add a LoRA file…" button clicks that flow.
- **R3 (done, unit-tested; never on a real instance):** the upload path itself (`/__lora/` endpoint,
  `user/` names, header check) came from the Kromoss LoRA session. R3 added `moss/probe.rs`, which
  reads a LoRA's tensor names before the upload and checks them against ComfyUI's MOSS key map
  (kohya `lora_unet_layers_N_proj_*` and generic `diffusion_model.layers.N.proj.*`): a LoRA for
  another model or an empty file is refused (ComfyUI would load it as a silent no-op), a partial fit
  is added with a note. Checked against every real Kohaku export on the PC and lodestones' DMD2. Also
  the 8-LoRAs-per-session cap, in the app before the upload (`MAX_LORAS_PER_GPU`) and in the sidecar
  (`LORA_MAX_FILES`, was 20). The instance bundle changed again and is re-pinned. Still unverified:
  whether ComfyUI lists a newly added LoRA without a restart (R4's local test decides).
- **R4 (local part done 2026-10-28; live part and user-guide pending):** 12 pictures on the laptop (3-box,
  6-box with two LoRAs, busy-background seam scene): layout 12/12, nothing outside a box changes (diff 0.0),
  LoRA look clear, no visible seam at feather 48; a LoRA added while ComfyUI runs is usable at once
  (`docs/findings.md`, "R4 local acceptance"). It found and fixed a bug in R1's graph: the feather never
  reached the box edges (ComfyUI's FeatherMask feathers the mask image's border). Still to do: one live
  run on a rented 24 GB card with the user's approval of GPU, $/hr, duration and cap (on-demand only),
  and `docs/user-guide.md`.
Invariants: nothing here rents anything; uploads are data only (safetensors, never pickle),
named and rooted by the sidecar, size-capped and hash-verified; ComfyUI stays on 127.0.0.1.

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
| 6 Tutorial v2 | 2 sessions + one tuning rental |

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
7. **KohakuMoss licence naming.** It asks for "Kohaku" in the name of works
   built on it. The spike reads SlopTweak as outside this (the trainer is
   fetched at run time, never vendored); the user decides.
8. ~~Kromoss caption default and scope~~ Settled 2026-10-28: **`mix` is the default**
   caption mode, and the trainer is **Kromoss-only** for now (no Base/SFT preset).