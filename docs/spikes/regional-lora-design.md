# Design: staged regional-LoRA layouts in SlopTweak (MOSS/Kromoss panel)

Status: design only (2026-10-28). No app code written, nothing rented. Evidence:
`docs/findings.md`, the three "Regional-LoRA spike" sections, and
`docs/spikes/regional-lora-moss.md`. Prototype scripts: `spikes/regional-lora/run.py`.
Per CLAUDE.md, this is a proposal; PLAN.md is unchanged. A proposed "Phase R" entry is at
the end, for the user to accept or edit.

## 1. What we are building

In the MOSS-Image panel, a **Layout** mode: the user draws labelled boxes on a canvas
(background, characters, text), optionally gives a box its own LoRA, and gets one picture
where each LoRA only affects its own box.

How, in one ComfyUI graph (one queued prompt, one result image):

1. **Layout pass.** MOSS's trained-in JSON layout prompt (`summary` + `items` with `bbox`,
   `yxyx`, `abs_centralized`) makes the base picture. All boxes take part, LoRA or not.
2. **Region passes.** For each box that has a LoRA, in draw order: VAE-encode the current
   image, denoise only inside that box's (feathered) mask with the same layout prompt and
   that LoRA, decode, and paste back through the same mask.

Why not hook LoRAs / masked conditioning: rejected by the spike (int8 crashes; bf16 leaks
the style across the canvas, breaks the layout, and is 6-40x slower). Staged passes were
contained exactly (pixels outside a box bit-identical), showed no seams, kept the layout at
denoise 0.5-0.7, and cost about 16 s per box on a 12 GB laptop.

## 2. Scope

**v1**
- Layout mode in the existing MOSS panel, Fast mode only (Kromoss/MOSS + the 8-step
  distillation, CFG 1). Fast is what the spike tested.
- Box kinds: `object`, `character`, `text` (as the ComfyUI node defines them).
- Per-box LoRA (one), strength, and "how much to change" (denoise).
- LoRAs: the user's local files (e.g. their trained Kromoss LoRAs) and the model's own.
  See section 6 for what delivery needs.
- Up to 12 boxes, up to 6 LoRA passes per picture.

**Not in v1** (and why)
- Full-quality mode with layout passes: CFG 3 plus skip-layer guidance untested with masked
  passes.
- Silhouette/segmentation masks: the spike showed the rectangle also restyles the box's
  background (obvious at denoise 0.9). Needs a segmentation custom node or mask upload;
  v2.
- Two LoRAs blended in one box, overlapping-box blending rules beyond "later box wins".
- Saving/loading layouts to disk; the draft lives in the window for the session.
- Hook LoRAs, attention-level regional LoRA, custom ComfyUI nodes.
- Invoke backend. This is `backend: comfyui` only.

## 3. User experience

The MOSS panel (`app/index.html` `#moss`, `app/src/main.ts` `renderMoss`) gets a
**Prompt / Layout** switch above the prompt box. Prompt mode is unchanged.

**Layout mode**
- A canvas the shape of the chosen width x height (the existing size controls), with the
  boxes drawn over a neutral grid. Plain DOM + SVG overlay, no library; strict TS.
- Drag on empty canvas to add a box; drag to move; eight handles to resize; Delete removes
  the selected box. Everything also works from the keyboard (arrow = nudge 1 unit, Shift =
  10, Tab = next box), since the list below is the accessible path.
- Boxes are numbered in draw order, which is also pass order. The list has up/down to
  reorder, with the hint "Background first, foreground last". A button adds a full-canvas
  background box.
- Per-box fields: kind (object / character / text), caption (what is in the box), the text
  to render (text kind only), and, for non-text boxes, LoRA (None or one of the available
  LoRAs; the LoRA's trigger words are offered as a one-click insert into the caption),
  strength (0 to 1.5, default 1.0), and **Change amount** (denoise, 0.4 to 0.9, default 0.7;
  labels "gentle / balanced / redraw" at about 0.5 / 0.7 / 0.9, from the spike).
- "Scene description" replaces the prompt field: the summary caption for the whole picture.
- The Make button is disabled with a plain reason when: no scene description, no boxes, a
  box smaller than 64 px on a side, a text box without text, or the LoRA is not on the
  instance yet.
- Progress: the app polls `/history` only, so it can say "Making your picture: layout +
  N regions, about X s" but not which pass it is on. Time estimate comes from section 8.
  (A live per-pass indicator would need ComfyUI's websocket; not in v1.)
- Result: the picture appears in the normal results grid with a caption listing the boxes
  and LoRAs. The layout-only base picture is not saved separately in v1 (open question 6).

Box coordinates in the UI are normalized 0-1000 on each axis, `[y0, x0, y1, x1]`, the same
as ComfyUI's `Create Bounding Boxes`, so what the user sees maps 1:1 to the node's output.

## 4. Data model (Rust)

Extend `moss.rs` `Request`:

```text
layout: Option<LayoutRequest>      // None = today's behaviour, untouched

LayoutRequest { scene: String, boxes: Vec<BoxReq> }
BoxReq {
  kind: object | character | text,
  caption: String,
  text: Option<String>,            // text kind only
  rect: [u16; 4],                  // y0, x0, y1, x1, normalized 0..=1000
  lora: Option<LoraRef>,           // not on text boxes
}
LoraRef { file: String, strength: f64, denoise: f64 }   // file = a name the instance has
```

`Params::check` validates (all errors are plain sentences, like the existing ones):
- `scene` trimmed, non-empty, within `MAX_PROMPT_CHARS`; the final layout JSON string is
  also within a limit (proposed 8000 chars), since it is the prompt MOSS reads.
- 1..=12 boxes; each rect has `y0 < y1`, `x0 < x1`, within 0..=1000, and at least 64 px on
  each side at the chosen size.
- `text` boxes require text and carry no LoRA; non-text boxes carry no `text`.
- at most 6 boxes with a LoRA; `strength` in 0.0..=1.5; `denoise` in 0.4..=0.9.
- `lora.file` must be a LoRA the GPU has (the panel's uploads, named `user/<file>.safetensors`
  by the sidecar, checked by `moss::valid_lora_name`); never a free path. `begin()` also
  checks the name against the session's uploaded list.
- `fast` must be true when `layout` is set. Size checks reuse `valid_size`.

The seed is shared by the layout pass and every region pass (this is what the spike did;
it keeps a re-run with the same inputs identical, which acceptance relies on).

## 5. Graph builder

As built (R1): `moss/layout.rs` holds the types, checks, JSON and `extend_graph`, which
`build_graph` calls last when `Params.layout` is set. In the text below, "`build_layout_graph`"
means that path. Same `serde_json::Value`
API-format output, same `run()`/`MossService` machinery. Everything below is core ComfyUI at
the pinned master SHA `a639f187` (every node was exercised in the spike).

**Layout JSON** is built in Rust, mirroring `MOSSImageLayoutPrompt.execute` exactly
(`comfy_extras/nodes_moss_image.py`): item order = draw order; `label` is `text` for text
boxes, `character` for character boxes, else `object`; a `text` item carries `text`;
bbox = `[floor(y0*H/1000) - H/2, floor(x0*W/1000) - W/2, ceil(y1*H/1000) - H/2,
ceil(x1*W/1000) - W/2]` after rounding the product to 9 places; top-level
`bbox_coord_space: "abs_centralized"`, `bbox_format: "yxyx"`; compact separators, no ASCII
escaping. Doing this in Rust avoids `CreateBoundingBoxes` (a frontend editor node) and keeps
the graph free of UI-only nodes. Golden test: normalized `[200, 40, 1000, 500]` on 1024x1024
gives `[-308, -472, 512, 0]`.

**Nodes**
```text
Loaders (existing build_graph's 1-3), distillation LoRA (+ time delta when the catalog
entry needs it), exactly as Fast mode today  -> base_model
Positive = CLIPTextEncode(layout JSON); negative = CLIPTextEncode("") (unused at CFG 1)
Stage 0:  EmptyFlux2LatentImage -> KSampler(denoise 1.0, base_model) -> VAEDecode  = img_0
For each LoRA box i, in draw order:
  model_i = LoraLoaderModelOnly(base_model, lora_i, strength_i)
  mask_i  = SolidMask(1,w_i,h_i) -> FeatherMask(f px, only on sides facing the picture's interior)
            -> MaskComposite(add onto SolidMask(0,W,H) at x_i,y_i)
  lat_i   = VAEEncode(img_{i-1}) -> SetLatentNoiseMask(mask_i)
  dec_i   = VAEDecode(KSampler(same seed/steps/sampler, denoise_i, model_i, lat_i))
  img_i   = ImageCompositeMasked(destination = img_{i-1}, source = dec_i, mask_i)
SaveImage(img_n)
```
Defaults: feather 48 px (clamped to a quarter of the smaller box side), shared seed, steps and
sampler from the request. **Correction (R4, 2026-10-28):** ComfyUI's `FeatherMask` softens the
borders of the mask image it is given, so the first version (feather applied after compositing onto
the canvas) never touched a box edge and every edge was hard; the spike's "feathered and hard edges
are the same" was that bug. The box's own mask is now feathered before it is composited, and not on
sides that lie on the picture's border. See `docs/findings.md`, "R4 local acceptance".

**Must not appear** (asserted in tests): any `*Hook*` node, `ConditioningSetProperties`,
`ModelSamplingSD3` (it turns MOSS output to noise; findings), per-region prompts other
than the one layout JSON.

Time/shape: nodes per region about 9; 6 regions about 60 nodes plus the base. All in one
prompt; cancel works as today (`/interrupt`).

**Pixel guarantee.** `ImageCompositeMasked` with `destination = img_{i-1}` keeps everything
outside box `i` identical to the previous image, so a later pass cannot change an earlier
box except where boxes overlap (later wins). Inside a box the pixels take one VAE round
trip per pass.

`RUN_TIMEOUT` in `identity.rs` is 15 minutes, shared with Identity Edit. A warm 6-region
picture is about 2 minutes on the laptop, but a cold model load plus a slow card could
approach it, so layout runs should use a scaled timeout (proposed 15 min + 90 s per region)
rather than raising the shared constant.

## 6. LoRA delivery

Today: user LoRAs are CivitAI links resolved to a pinned file (`civitai.rs::Lora`), added to
the model's file list by `config.rs::with_loras`, and downloaded by `provision.sh` at start.
`with_loras` deliberately returns early for `Backend::Comfyui`, and `civitai::family`
has no MOSS entry, so neither path works for the MOSS panel yet. The instance cannot take a
file from the PC for this backend either: `/__train/upload` exists only for `BACKEND=train`.
Also, the user's own trained LoRAs are local files (Phase K output), not CivitAI.

### Sources

| Source | Needs | Session timing |
| --- | --- | --- |
| **Local file** (trained Kromoss LoRA, `.safetensors` on the PC) | new sidecar upload endpoint + app-side probe | after Ready, before first Make; adding another later is fine |
| **CivitAI link** | `family()` mapping for MOSS/Kromoss, `with_loras` allowed for MOSS comfy models | at Start (provision), like Invoke LoRAs |
| **Model's own** (distillation) | nothing; already provisioned | n/a |

Recommendation: **local upload first.** It is the main use (own Kromoss LoRAs), it needs no
CivitAI naming facts, and it can add a LoRA to a running session. CivitAI-at-provision
follows once the base-model string is verified (open question 2).

### Local-file path

1. **Probe on the PC** (new `lora_probe` in Rust, no network): read the safetensors header
   only (8-byte length, JSON; cap header 8 MB, file 1 GB); reject anything else (a pickle
   `.ckpt` is never accepted). Accept the two key styles seen in the real files:
   `lora_unet_layers_N_proj_{qkv|o|ffn_down|ffn_up_gate}.{lora_down,lora_up}.weight` (+ `.alpha`),
   from the Kohaku trainer (504 tensors, fused qkv, rank 32; `km-mix-800`), and
   `diffusion_model.layers.N.proj.*.lora_{down,up}.weight` (756 tensors, the distillation
   LoRA). Require at least 90 % of keys to match and layer indexes < 42. Result is
   `compatible | looks_wrong(reason)`. **Why:** ComfyUI loads a mismatched LoRA with only a log
   line and renders as if no LoRA were set, a silent failure for the user. An SDXL LoRA
   dropped in would otherwise "work" and do nothing. Warn, with an override.
2. **Upload** (new sidecar endpoint, section 6.1) with the launch secret, streamed, with
   `X-Sha256` computed by the app; progress shown in the panel. The instance is billing
   during the upload; show the size and a rough time first (about 200 MB for a rank-32
   LoRA).
3. The app records `name -> sha256` for the session so Make can only name LoRAs the
   instance has; the dropdown lists exactly those.

### 6.1 Sidecar endpoint (instance/, comfyui backend only)

`PUT /__lora/upload/{name}`; mirrors `handle_train_upload`'s auth and streaming:
- Launch-secret auth; 404 when `backend != comfyui`.
- `name` must match `^[A-Za-z0-9][A-Za-z0-9._-]{0,78}\.safetensors$`; joined onto the
  fixed ComfyUI `models/loras` directory, never an arbitrary path; refuse overwrite of an
  existing file with a different hash (same hash = idempotent 200).
- Size cap 1 GB per file, 8 files per session, disk-free check.
- Stream to a temp name; verify `X-Sha256` equals the computed hash; re-run the header check
  (valid safetensors, no pickle, key style as above); then atomic rename.
- Never executes anything; the file is only data for ComfyUI's loader.
- ComfyUI must see the new file without a restart. Whether `/object_info/LoraLoaderModelOnly`
  refreshes its file list on a new file is **unverified**; test locally first (section 9).
  Fallback: the sidecar calls ComfyUI's refresh, or the graph validation error is retried
  once after the upload.
- Tests (`instance/tests/`, pytest, injectable clock): bad name, traversal, wrong hash,
  not safetensors, truncated header, over cap, wrong key style, wrong auth, disabled on
  the invoke backend, idempotent repeat.

Any change under `instance/` changes the bundle hash: rebuild with `build_assets.py`,
update `ASSETS_SHA256`; publishing a pre-release needs the user's OK (CLAUDE.md).

## 7. Catalog dependency

The spike used the Kromoss repo's own `8-steps-dmd2-lora` and no time-embedding delta. The
shipped `moss-image-dmd` entry is base MOSS plus lodestones' DMD LoRA and a time delta. So
the model the user actually wants to lay out on, Kromoss, needs its catalog entry first
(Phase K3 already lists "a `kromoss` catalog entry for the MOSS panel"): the
`kromoss_bf16.safetensors` file (10,164,655,808 B, sha256 `54c863b9…adaba`, revision
`a4fe9744…`, verified in the spike), the shared encoder/VAE, and the DMD2 LoRA (sha256 to be
pinned). `ModelFiles::of` currently requires a `time_embedding` file; the Kromoss entry has
none, so that check must become "required only when the entry says so".

The spike used int8 ConvRot on a 12 GB laptop. Rentals are 24 GB, where bf16 fits
(bf16 plus ordinary LoRA loading was exercised in the spike's E1 global case; the staged
passes themselves were run on int8 only). Acceptance covers both.

## 8. Cost and time

Measured, 12 GB laptop, int8, warm, 1024x1024, 8 steps: layout pass about 19 s, each region
pass about 15-16 s; a 2-box picture about 50 s. Cold start adds model load. These are laptop
numbers; a rented 24 GB card should be no slower, not measured. The panel shows an estimate of
`layout + passes x per-pass` using a conservative per-pass figure and never a promise.
Time on the instance is billed as usual and covered by the existing cost bar, idle timer and
session limits; uploading a LoRA is billable time too.

## 9. Acceptance tests

**Unit (Rust, no network)**
- Layout JSON golden values (above), a non-square size, a text item, character vs object
  labels, unicode text not escaped, draw order kept.
- `Params::check`: every limit in section 4, both sides of each boundary; text box with a
  LoRA rejected; bad LoRA names (traversal, absolute, wrong extension); `fast=false` with a
  layout rejected.
- Graph: every link points at an existing node (extend the existing test); node count per
  region; region order = draw order; mask `SolidMask` sizes equal the pixel box; masks
  inside the canvas; same seed on every sampler; `denoise` passed through; LoRA-less boxes
  add no pass; no node whose class contains `Hook`; no `ModelSamplingSD3`; with no LoRA
  boxes the graph is the layout pass only.
- `lora_probe`: the real `km-mix-800` header and the DMD2 header accepted; an SDXL-style
  header, a truncated header, a pickle, an oversize header all rejected.
- Timeout scaling and the estimate function.

**UI (mock, `app/dev`, like `identity-ui-check.mjs`, on `MockProvider`)**: draw, move,
resize, reorder, delete; keyboard nudging; disabled-Make reasons; LoRA dropdown shows only
uploaded LoRAs; the request sent to the stub matches the boxes; Prompt mode unaffected.

**Instance (pytest)**: the upload endpoint cases in 6.1.

**Local ComfyUI (manual script, free, on the user's PC at port 8198)** using a graph dumped
from the Rust builder (`cargo test -- --ignored` writes JSON into `spikes/`):
1. Same graph twice, same seed: byte-identical images.
2. Layout-only graph vs the same graph with a region pass: pixels outside the box identical
   (diff 0.0); inside the box mean abs diff at least 15 at denoise 0.7 (spike: 31).
3. A new LoRA file placed in ComfyUI's `loras` folder while it runs becomes usable without
   a restart (decides the refresh question).
4. At least 6 seeds x 2 scenes (3 and 6 boxes), recorded in findings: layout kept (the
   expected count of characters), no extra characters, no seam visible at 100 % on the
   box edge (including a busy-background scene), and which denoise default holds up.
5. A mismatched LoRA (SDXL file) produces the probe warning, and a forced run shows no change
   (documents why the probe exists).

**Live Vast (manual trigger only; needs the user's explicit approval per action)**:
one on-demand (never interruptible) 24 GB instance with the Kromoss entry. Same budget
class as the MOSS panel acceptance in `moss-image-comfyui-plan.md` step 7; GPU, $/hr, duration
and cap are quoted again right before asking. Script follows `vast-acceptance.mjs`: print the
instance id first, destroy on every exit path. Checks: upload a real trained LoRA, make a
2-box and a 4-box picture, the passes finish within the timeout, outside-box pixels match
a layout-only run, Cancel mid-pass works, Stop destroys, cost bar matches.

**Quality bar** (to call v1 done): in the 12-picture local set, the layout holds in at
least 10; no seam the user can see at 100 % on 10; the LoRA's look is clear inside the box at
the default denoise in 10; nothing outside the box changes in any of them (guaranteed by
construction, checked by diff).

## 10. Security and invariants

- No new network exposure: ComfyUI still on 127.0.0.1:8188 behind the sidecar; the app talks
  to it with the launch secret and no `Origin` header. The upload endpoint is on the
  sidecar port, same auth as the other `__*` endpoints, and exists only for `comfyui`.
- Files are data only: safetensors headers parsed, never pickle; names sanitised and rooted;
  size/count/disk caps; hash verified end to end.
- No keys in logs, graphs, captions or fixtures. Graph JSON contains file names, never paths
  or tokens. Captions list LoRA file names only.
- The remote webview is unchanged. No new Tauri capability beyond the commands the local
  window already uses.
- Spending: nothing here creates an instance; the live test follows CLAUDE.md (approval,
  trap/finally destroy, id printed first, on-demand only).

## 11. Risks and unknowns

1. **Layout fidelity with many boxes** is measured on one 3-box scene only. Acceptance item 4
   is the real test.
2. **Rectangle restyles the background** inside the box (spike, denoise 0.9). Mitigation:
   default 0.7, tighter boxes, hint text; real fix is silhouette masks (v2).
3. **LoRA refresh without restart** unverified.
4. **CivitAI base-model string for MOSS/Kromoss** unknown (the site returned 403 to fetch);
   needed only for the CivitAI source.
5. **bf16 staged passes** untested; **second distinct MOSS LoRA** never tested (the spike's
   two-box test used two checkpoints of the same character).
6. **ComfyUI pin** is an unreleased master SHA (`a639f187`), same as the existing MOSS panel;
   all nodes used are core and were exercised there.
7. **Coarse progress**: no per-pass feedback without websocket support.
8. **VAE round trip** per pass inside the box slightly softens detail; not measured beyond
   eyeballing.

## 12. Phasing (proposed "Phase R", needs the user's OK to add to PLAN.md)

- **R0, Kromoss catalog entry** (shared with K3): file list with SHA-256, `ModelFiles::of`
  made time-embedding-optional, catalog tests. Useful on its own.
- **R1, graph + validation** (Rust): `LayoutRequest`, `Params::check`, JSON builder,
  `build_layout_graph`, timeouts, unit tests, local graph-dump script. No UI.
- **R2, box editor UI**: Layout mode, list, keyboard, mock-UI check.
- **R3, LoRA delivery**: `lora_probe`, sidecar upload endpoint + tests, app upload with
  progress, session LoRA list; bundle rebuild and pin (needs OK to publish a pre-release);
  CivitAI source if question 2 is resolved.
- **R4, acceptance + docs**: local set, live run (approval), `docs/findings.md`,
  `docs/user-guide.md`, PLAN backends/phase notes.

Rough effort in PLAN's unit: R0 1 session, R1 1, R2 2, R3 1-2, R4 1 plus the live rental.
R1 and R2 do not depend on each other after the request type is fixed.

## 13. Decisions needed (PLAN section 9 style)

1. **Delivery for v1:** local upload only, CivitAI-at-provision only, or both? Recommend local
   upload first (section 6).
2. **CivitAI base name** for MOSS/Kromoss LoRAs: can you confirm what CivitAI calls it, or
   paste one LoRA's model-version page?
3. **Mode:** Layout on Fast only for v1 (recommended), or also Full quality?
4. **Where:** a Layout mode inside the MOSS panel (recommended) or its own panel.
5. **Limits:** 12 boxes / 6 LoRA passes / 1 GB per LoRA / 8 LoRAs per session, fine?
6. **Keep the layout-only base picture** as a second result for comparison, or only the final
   picture?
7. **Phase R:** add to PLAN.md as proposed, and start with R0?

## 14. Status notes

**R1 (2026-10-28, built, unit-tested, not yet run on an instance).** `app/src-tauri/src/moss/layout.rs`:
`LayoutRequest`/`BoxReq`/`LoraRef`, `layout::check` (all limits in section 4, boundaries tested),
the layout JSON (byte-identical to the spike's Python for the spike scene; golden test),
`extend_graph`, `run_timeout` (15 min + 90 s per pass), `estimate_secs` (25 s + 20 s per pass),
`Layout::check_known_loras`. In `moss.rs`: `Request.layout`, `Params.layout`, the layout branch
of `Params::check` (Fast only; no global `lora`/`control` with it), `run()` takes a timeout,
`begin()` refuses LoRAs the GPU doesn't have and shows the estimate. Caps: 12 boxes, 6 LoRA passes,
caption 500 characters, text 200, layout JSON 8000. The local script for the dumped graphs is
`spikes/regional-lora/check_graphs.py` (`cargo test --lib dump_layout_graphs -- --ignored` writes them).

**Overlap with R3.** Another session landed a single-LoRA upload path in parallel: `/__lora/`
sidecar endpoint, `SidecarApi::lora_upload`, `MossService::add_lora`, `MossLora` list in the view,
`user/<file>.safetensors` names, a safetensors header check, a "base control" run. Layout mode builds
on those names and that list instead of the endpoint specified in section 6.1, so R3 shrinks to
what is missing: the key-style probe (section 6, step 1), which that session's header check does
not do, per-box LoRA pickers, and the 8-LoRAs-per-session cap if it isn't there. Re-read that code
before starting R3.

**R2 (2026-10-28, built).** The editor is `app/src/layout.ts` (+ `layout.css`), mounted by
`main.ts` into `#moss-layout`; the MOSS panel gets a Prompt / Layout switch. Differences from
section 3 as written: the scene description reuses the panel's prompt field (relabelled), the
single-LoRA block hides in Layout mode, and a LoRA added from the editor goes to the box being
edited if it has none. Trigger words aren't offered (uploaded LoRAs carry none in the panel's
data). The editor mirrors the Rust limits so Make can say why it is off; Rust stays the
authority. Run `node dev/layout-ui-check.mjs` (debug build + mock GPU); `dev/layout-preview.html`
shows the editor alone on the vite server. Needs the debug exe rebuilt after Rust changes.

**R3 (2026-10-28, built).** `app/src-tauri/src/moss/probe.rs` replaces the design's `lora_probe`: it
reads only the safetensors header and checks tensor names against ComfyUI's MOSS LoRA key map at the
pinned commit (`comfy/lora.py`: `lora_unet_<module>` and `diffusion_model.<module>`, plus the split of
fused `qkv`). The module list comes from the real Kromoss header (a guarded `#[ignore]` test re-checks
it). Verdicts: fits (>= 90 % of modules), partial (added, with a note in the panel), foreign (refused,
naming an example tensor), empty (refused). No override: a foreign file would load as a no-op in ComfyUI
anyway. The instance sidecar keeps its header-only check; a key-style check there would reject
files the app has already vetted and break its fixtures. Caps: 8 per GPU in the app before the upload
and in the sidecar; 1 GB per file. Section 6.1's own upload endpoint was not built: the Kromoss LoRA
session's `/__lora/{name}` is the one in use.

**R4 local (2026-10-28).** Passed the local part of the quality bar in section 9 (layout 12/12, nothing
outside a box changed, LoRA look clear, no visible seam), after fixing the feather (section 5's
correction). A LoRA added to a running ComfyUI is usable on the next request. Details and numbers:
`docs/findings.md`, "R4 local acceptance". Open: the live run on a rented card, and the user guide.
