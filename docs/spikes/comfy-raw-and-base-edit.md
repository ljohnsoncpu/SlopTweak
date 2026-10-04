# Session prompt: raw ComfyUI view + base-image edit mode

Paste everything below the line into a new session in this repo.

---

You are continuing SlopTweak's **Identity Edit** work (ComfyUI as a hidden second
backend next to InvokeAI). Two features, in this order. Plan first (plan mode),
ask me the open questions, then build step by step, stopping to report between
steps.

Read first: `CLAUDE.md`, `PLAN.md` (§1 non-goals, §2 incl. the "Backends" row,
§9), and in `docs/findings.md` the sections "Krea 2 edit spike", "Identity Edit
on ComfyUI: Step 0, the real image", and "Identity Edit: live acceptance in the
real app". The code: `app/src-tauri/src/identity.rs` (graph + run loop),
`comfy.rs`, `sync.rs` (`comfy_pass`), `session.rs` (`backend_of`, `ready_loop`,
`comfy_endpoint`), `remote.rs` (the remote window), `instance/provision.sh`,
`instance/sidecar.py`, `app/src/main.ts` + `app/index.html` (the panel). Verify
against the code before relying on any of this.

## State you start from
The work is on branch `claude/identity-edit-comfyui`, **uncommitted** (ask me to
commit it / open a PR first, or branch from it). Everything passed: `cargo fmt`,
`clippy -D warnings`, 184 Rust tests, 36 pytest, `node dev/identity-ui-check.mjs`
(mock, 18/18), and the live run `dev/vast-acceptance.mjs r4` (11/11, about
$0.20). Dev pre-release `instance-v0.1.3` holds the current instance bundle;
**any change under `instance/` changes the bundle hash** (`python
instance/build_assets.py`, update `ASSETS_SHA256` in `config.rs`, publish a new
dev pre-release, point `SLOPTWEAK_DEV_ASSETS_URL` at it; publishing is
outward-facing: ask me first).

## Feature 1: "Open ComfyUI" (the raw ComfyUI interface)
Goal: an advanced button so the user can open ComfyUI's own node-graph UI on the
running Identity Edit GPU, in the same kind of separate window Invoke uses.

Known constraints (all verified, see findings):
- ComfyUI answers **403 to any request with an `Origin` header**, even one equal
  to the tunnel host. The sidecar forwards `Origin`, so a browser through the
  tunnel is blocked today. The app's own calls (Rust, no `Origin`) are fine.
  Decide how to fix it safely, probably in `sidecar.py` for `BACKEND=comfyui`
  (strip or rewrite `Origin`/`Referer` on proxied HTTP and the `/ws` websocket),
  and say what that does to ComfyUI's CSRF protection. The sidecar's session
  cookie is `SameSite=Lax`; check state-changing requests from a foreign site
  can't ride it.
- The remote window has **no Tauri capabilities** and its own data dir
  (`remote.rs`, `build.rs`). Keep that. `tutorial.js`/`defaults.js` are
  Invoke-only: skip them for ComfyUI.
- ComfyUI-Manager is moved aside by `provision.sh` on purpose (it installs code).
  The raw UI has no extra node packs; say so in the UI. Think about what the user
  can now do on the GPU (load any workflow, queue heavy jobs, burn time) and
  whether the idle/session limits still behave (`/queue` busy probe; webview
  non-GET requests count as activity).
- Output sync (`comfy_pass`) already saves anything in `/history`, so raw-UI
  results are saved too. Check it handles other `SaveImage` prefixes and
  subfolders, and `PreviewImage` (type `temp`) is correctly ignored.
- `session.rs::open_invoke` refuses for ComfyUI GPUs; add an
  `open_comfyui`-style path (new command: `build.rs`, `capabilities/main.json`,
  `lib.rs`), a button in `index.html`/`main.ts`, and window-close behaviour like
  Invoke's (ask to stop?).
- Nice to have: preload the **Identity Edit workflow** so the user starts from
  something working (the graph in `identity.rs::build_graph`; ComfyUI also takes
  the UI-format workflow from the `comfyui-krea2edit` repo's
  `workflows/krea2_identity_edit.json`).
Tests: pytest for the sidecar change (fake ComfyUI with an Origin check), Rust
tests with the existing `MockProvider`/`MockComfy`, extend
`app/dev/identity-ui-check.mjs`. Live: ask me before any rental.

## Feature 2: edit mode (base image + references + prompt)
What I want: the user gives a **base image** and optionally **reference
materials** (character sheets, style/detail refs), writes a prompt, and the
program **edits the base image** per the prompt, optionally pulling details from
the references. This is NOT what the panel does today: the panel makes a new image
from 1-2 character sheets (`EmptySD3LatentImage` is the starting canvas, there is
no base image, no strength, no mask).

Start with a **spike**, not code. Things to find out (use the real graph/nodes;
cheap rental with my approval, state GPU, $/hr, duration, cap first; destroy on
every exit path, print the instance id first; scratch outputs outside the repo):
1. **Does the Identity Edit pack already do this?** The `Krea2EditModelPatch`
   node's tooltips describe two-reference LoRAs as "scene first, subject second"
   (`source_latent`/`source_image` = scene, `source_latent_b`/`source_image_b` =
   subject), and `identity.rs` already sends slot 0 as the first and slot 1 as the
   second. So "base image = first reference, character sheet = second" may work
   with no new nodes. Test it: does the output keep the base's composition and
   change only what the prompt says? How well does it keep the scene?
2. **True img2img:** start the sampler from the base image's latent (`VAEEncode` ->
   `KSampler` with `denoise` < 1) while the Identity Edit patch also sees the
   references. Which denoise range keeps the base yet follows the prompt?
3. **Masked edit (inpaint):** `SetLatentNoiseMask`-style, if I want it (ask me).
4. The Ostris-format edit LoRAs from the first spike (detail enhancer, style
   reference; `ComfyUI-Krea2-Ostris-Edit` nodes) are other routes; compare only if
   1-3 fall short. Note LoRAs and node packs don't mix across the two node packs.
5. Resolution: base image sizing/aspect (output size follows the base?), 1 MP
   reference cap, VRAM, seconds per image (current: 36-64 s on a 4080S 32 GB).
Report what works, what doesn't, and show me images. Then propose the UI and
API (an "Edit" mode next to "New image" in the panel: base slot, 0-2 reference
slots, prompt, strength slider if img2img wins) and wait for my decision before
building. Use SFW, adult test characters; the user judges content themselves.

## Rules (from `CLAUDE.md`, restated)
- Anything that spends money needs my explicit per-action approval in chat.
  Real keys stay in Credential Manager/env, never in logs or commits.
- Content policy (PLAN §9.3) and the Krea license's content-filter clause (§4.2)
  are still open; don't decide them, flag where a feature touches them.
- Don't commit unless asked. Update `docs/findings.md` (verified, differs,
  untested, spend before/after) and the user guide for anything user-visible.
- Keep checks green: `cargo fmt`, `cargo clippy --locked --all-targets -- -D warnings`,
  `cargo test --locked`, `npx tsc --noEmit`, in `instance/`: `uv run ruff
  check/format --check`, `uv run mypy`, `uv run python -m pytest`, shellcheck
  (`uvx --from shellcheck-py shellcheck`), `python instance/build_assets.py
  --check-pin`.
