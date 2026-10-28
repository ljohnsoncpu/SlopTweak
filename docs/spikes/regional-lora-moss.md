# Spike plan: per-region LoRA on MOSS/Kromoss with box layouts

Status: plan only (2026-10-27). Nothing run, nothing rented, no app code
changed. Background and sources: `docs/findings.md`, "MOSS/Kromoss
bounding-box layout + per-region LoRA".

## Question

Can one image be laid out with MOSS bounding boxes (the trained-in JSON layout)
while a different LoRA applies inside each box, using ComfyUI's stock hook
LoRAs and masked conditioning? If yes, SlopTweak can build it on its existing
`backend: comfyui` MOSS panel. If not, which fallback is cheapest.

## Why this can be tested for free, locally

The existing local install at `C:\ai\ComfyUI` (commit `a639f187`, the same
pin `provision.sh` uses) already has every node needed, checked by grep:
`CreateBoundingBoxes`, `MOSSImageLayoutPrompt`, `CreateHookLora`,
`CreateHookModelAsLora`, `SetClipHooks`, `ConditioningSetProperties`,
`PairConditioningSetProperties`, `ConditioningSetMask`. Models: Kromoss int8
ConvRot, `8-steps-dmd2-lora`, and the user's own test LoRAs in
`models\loras` (`km-mix-*`, `km-nl-*`, `km-smoke-comfy`). RTX 4080 Laptop,
12 GB; 8-step Kromoss is about 14 s/image there. No Vast instance is needed
for the first answer.

Known fact about the layout node: `MOSSImageLayoutPrompt` returns a plain
**string**. Layout is just text, so the same JSON string can be encoded once
and reused by every region's conditioning, or each region can get its own
variant.

## Setup rules

- Run a **second** ComfyUI server on another port (e.g. 8198) from the same
  install so Krita's server on 8199 is not disturbed. Same
  `extra_model_paths.yaml`.
- Work in `spikes/regional-lora/` (the `spikes/` folder is already untracked):
  API-format graph JSON files plus a small Python runner that posts a graph,
  polls `/history`, saves the PNG and logs wall time and peak VRAM. No repo
  code, no keys, no network beyond localhost.
- Fixed seed, 1024x1024 (and one 832x1216) so runs are comparable. Layout
  coordinates must match the generation size.
- Don't touch the Krita plugin, its backups, or the ComfyUI install itself.

## Experiments (stop at the first failure and report)

**E0. Baseline layout.** Kromoss int8 + DMD2 (8 steps, CFG 1, shift 5), three
boxes, no regional LoRA. Confirms boxes are honoured on Kromoss and gives the
reference images and timing. Also run the same layout on MOSS-SFT weights if
they are on disk, to separate "Kromoss ignores boxes" from "layout needs
fixing". (The eval repo only covers 8 layouts.)

**E1. Hook parity.** Apply one test LoRA two ways: the normal `LoraLoader`
patch on the model, and `CreateHookLora` attached to the single conditioning
with no mask. Pass: images match closely at the same seed (small numeric
differences are fine). This proves hook patching works with MOSS's LoRA key
map and with int8 ConvRot weights. Most likely place for a hard failure.

**E2. Full-canvas mask.** Same as E1 but conditioning carries a mask covering
everything. Pass: still matches. Isolates mask handling from hook handling.

**E3. One region.** LoRA A on one box only; the rest of the canvas uses a
default conditioning (no regional LoRA). Pass: LoRA A's effect is visible
inside the box and absent outside. Record how far it bleeds past the edge,
with and without mask feathering, and whether the boxes' layout still holds.

**E4. Two regions.** LoRA A in box 1, LoRA B in box 2, default elsewhere
(`PairConditioningSetDefaultCombine` or an inverted mask so every pixel is
covered; an uncovered area gives beige output per the Comfy blog). Pass: each
region shows only its own LoRA.

**E5. Composition with DMD2.** Check that the 8-step LoRA stays global (model
patch) while style/character LoRAs are hooks, versus DMD2 as one more hook per
region. Compare quality and time. Open question: how masked conds combine
with CFG 1, where there is no negative conditioning.

**E6. Cost.** Time and VRAM for 1, 2, 3 regional LoRAs. Hooks re-patch weights
per conditioning, so expect roughly N model passes per step plus patch
overhead. On 12 GB this may OOM or crawl, which is a laptop result, not a
product result; note it and decide if a 24 GB rental is worth testing.

**E7. Text in a region.** One text box inside a LoRA region, to see whether a
style LoRA breaks text rendering.

## Pass / fail

- **Go:** E1-E4 pass, bleed is tolerable with feathering, and 3 regions cost
  under about 3x single-LoRA time on a 24 GB-class card (extrapolated; confirm
  later).
- **No-go for hooks:** E1 fails (MOSS or int8 doesn't hook cleanly) or E3
  can't contain the LoRA. Then try the fallbacks below.
- Report with images, timings and exact graphs, and update `docs/findings.md`
  with verified facts (hook support, bleed, cost).

## Fallbacks if hooks fail

1. **Staged compositing.** Generate the layout once, then re-run each box as a
   masked img2img/inpaint pass with its LoRA (what the user already does in
   Krita). Slower and seams are possible, but uses only proven nodes.
2. **Custom node with a model-function wrapper** that blends per-region noise
   predictions (Mixture-of-Diffusers style) with LoRA toggled per region. More
   work; pin by commit if it ends up in the product.
3. **Attention-level regional LoRA** (patch LoRA on K/V/projections only inside
   a mask). Research-grade; the Comfy blog thread says stock ComfyUI doesn't do
   this.

## If the spike says go: rough product work

Not part of the spike; listed so the cost is visible. All of it depends on the
v1 MOSS panel being finished first.

- Box editor in the SlopTweak UI (ComfyUI's frontend is never shown): draw,
  reorder (draw order matters, background first), caption, text/character flag,
  per-box LoRA picker and strength, feather.
- Rust graph builder next to `moss.rs`: layout JSON, per-box masks, hook chain,
  default conditioning. Unit tests for coordinate conversion and mask coverage.
- LoRA delivery: catalog or user-supplied LoRAs onto the instance, with
  SHA-256 pinning for catalog ones. CivitAI keys stay in Credential Manager.
- Sidecar passes nothing new if ComfyUI stays behind it; check that the
  sidecar's allowed ComfyUI node/endpoint list covers the hook nodes.
- Live acceptance on a rented 24 GB card: needs your explicit approval with GPU,
  $/hr, duration and cap, on-demand only (never interruptible).

## Open questions for the user

1. Which two LoRAs should be the test pair? Suggest one visibly stylistic and
   one character/subject LoRA, ideally from `km-mix-*` / `km-nl-*` plus one
   other. They must be visually distinct so bleed is obvious.
2. Is it fine to run a second ComfyUI server on port 8198 on the laptop?
3. Any preferred test layout (for example a two-character scene with a
   background)? Otherwise I'll use a generic three-box scene.

## Sources

- https://github.com/OpenMOSS/MOSS-Image
- https://docs.comfy.org/tutorials/image/moss-image/moss-image
- https://blog.comfy.org/p/masking-and-scheduling-lora-and-model-weights
- https://huggingface.co/lodestones/Kromoss
- https://github.com/jo-nike/moss-kromoss-eval
- Local: `C:\ai\ComfyUI\comfy_extras\nodes_moss_image.py`,
  `nodes_hooks.py`, `nodes_bounding_boxes.py`

## Results (2026-10-28)

Hook LoRAs: not viable (int8 crashes; bf16 leaks and breaks layout; very slow). Staged per-box
inpaint with the LoRA: works on int8, contained, no visible seams, about 16 s per box. Details in
`docs/findings.md`, "Regional-LoRA spike" sections. Recommended mechanism for SlopTweak: staged passes.
