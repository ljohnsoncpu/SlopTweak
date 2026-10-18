# Plan: MOSS-Image 8-step DMD as a ComfyUI-backed panel

Status: plan; step 0 done 2026-10-17 (see the 'Step 0 results' section at the end). Nothing built, no instance rented. Per CLAUDE.md,
anything that spends money needs your explicit yes in chat, per action.

Goal: `lodestones/MOSS-Image-Base-8-steps-dmd` as a catalog model with
`backend: comfyui`, driven from a SlopTweak panel (like Identity Edit). Invoke
6.14.1 can't load MOSS (see `docs/findings.md`, "MOSS-Image 8-step DMD").

## What the research says (and what is unverified)

- The model is a LoRA (rank 128, kohya keys) plus `time_embedding.safetensors`
  on top of `OpenMOSS-Team/MOSS-Image-Base`. Settings: 4-8 steps, CFG 1, shift 5.
- **ComfyUI core now has native MOSS-Image** (PR #17095 merged 2026-10-16;
  INT8 speed-up PR #17099 on 2026-10-17). It is *not* in v0.41.0 (Oct 15),
  so it is on master only until the next tag. Source: a third-party article,
  not yet checked against the PRs. **Step 0 verifies this.**
- Native files live in `Comfy-Org/MOSS-Image`: diffusion model ~10.2 GB bf16 or
  ~5.4 GB INT8, Qwen3.5-9B text encoder ~9.8 GB INT8 (~7.3 GB W4A8), FLUX.2 VAE
  0.34 GB. Total about 16-21 GB, far smaller than Kroma.
- The alternative is the official `OpenMOSS/MOSS-Image` plugin (`comfyui/moss-image/`,
  its HF Loader wants a diffusers-style folder) or the community T8 pack. Both
  need a custom node pinned by commit. Native avoids that.
- **Unknown:** whether `time_embedding.safetensors` can be applied through
  ComfyUI's stock LoRA loader. The card says LoRA alone is the recommended
  route ("nothing else from this repo is needed"), so assume yes, and prove it in
  Step 2. Also unknown: whether the DMD LoRA's keys match ComfyUI's MOSS key
  layout (the card's keys are kohya-style on the diffusers transformer).

## Decisions I need from you

1. **ComfyUI source.** The pinned `vastai/comfy` image (see `config.rs`) likely
   predates PR #17095. Options: (a) wait for the next ComfyUI tag and a
   matching `vastai/comfy` image, the cleanest and pinned; (b) `provision.sh`
   checks out ComfyUI at a pinned master commit SHA over the image's copy. (b)
   works now but changes ComfyUI for Identity Edit too unless gated on the
   model. Recommend (a) if the tag lands within days, else (b) gated per model.
2. **Precision.** INT8 ConvRot is half the size, but a LoRA on INT8 weights is
   unproven. Recommend bf16 first (10.2 GB, fits 24 GB), INT8 as a later
   optimisation.
3. **Text encoder.** Qwen3.5-9B INT8 (9.8 GB) vs W4A8 (7.3 GB): recommend INT8.
4. **Prompt style.** The model takes natural language, tags and JSON layouts.
   Recommend v1 = plain text prompt only (no layout/bounding boxes).

## Steps

0. **Verify (free, no rental).** Read PR #17095/#17099 and ComfyUI's `nodes`
   for MOSS; list `Comfy-Org/MOSS-Image` files with sizes and LFS SHA-256;
   download only the LoRA headers by ranged GET to compare keys. Record in
   `docs/findings.md`. Stop and report.
1. **Catalog entry** (`catalog/catalog.json`, `catalog.rs`): `moss-image-dmd`,
   `backend: comfyui`, `base: moss-image`, files with `dest`
   `diffusion_models` / `text_encoders` / `vae` / `loras` (main, text_encoder,
   vae, lora, plus `time_embedding` if Step 0/2 says it's needed), all pinned
   by revision with SHA-256 and sizes. Needs a new `kind`/`dest` check only if
   `time_embedding` isn't a LoRA. `min_vram_gb` 24 (bf16) and
   `min_compute_cap` 800, tier $$. License: Apache-2.0, a short `license_note`.
   `description` states the WIP status of the distillation and the 4-8 step
   limit. Tests in `catalog.rs` for the new fields.
2. **Graph builder** (new `moss.rs`, modelled on `identity.rs::build_graph`):
   UNETLoader, CLIPLoader, VAELoader, LoraLoader (strength 1), ModelSamplingSD3
   (shift 5), CLIPTextEncode, EmptyLatent (multiples of 16), KSampler (euler,
   steps 8, cfg 1), VAEDecode, SaveImage. Node names come from Step 0. Unit
   tests assert the graph shape, size clamping and prompt limits. Reuse
   `comfy.rs` (`parse_history`, `/view` fetch) and the poll/timeout/cancel loop
   from `identity.rs`; factor the shared loop out rather than copying it.
3. **Provisioning** (`instance/provision.sh`, `BACKEND=comfyui`): generalise
   `finish_comfy` so the Identity Edit custom node is only installed for
   Identity Edit models, and (if decision 1b) pin ComfyUI to a master SHA for
   MOSS. Health check polls `/object_info/<MOSS node>`. ComfyUI-Manager stays
   moved aside; ComfyUI stays on 127.0.0.1. Any change under `instance/`
   changes the bundle hash: rebuild with `build_assets.py`, update
   `ASSETS_SHA256`, and publishing a new pre-release needs your OK.
4. **Panel** (`app/src/main.ts`, `index.html`): a text-to-image panel: prompt,
   aspect, steps (default 8, range 4-8), seed, results grid with save and "use
   as reference" omitted. Selected when the model's `base` is `moss-image`.
   Mock-UI check in `app/dev` (like `identity-ui-check.mjs`) on `MockProvider`.
5. **Defaults and limits.** Ready timeout already scales with download size
   (`max(setting, 8 + 0.6 min/GB)`, about 19 min for 18 GB). Not a problem.
6. **Docs.** `docs/user-guide.md` entry, `docs/findings.md` update, PLAN.md
   backends row note (the ComfyUI backend now serves two panels).
7. **Live acceptance (needs your approval to spend).** One instance, RTX 3090
   or 4090 class, 24 GB; est. 15-20 min ready + 5 min of tests, roughly $0.10-0.25
   at $0.20-0.35/hr, capped at 40 min. Script follows `vast-acceptance.mjs`:
   instance id printed first, destroyed on every exit path. Checks: all files
   register, one image at 8 steps / CFG 1 that is not noise, steps 4 vs 8,
   warm-image time, cost bar, Stop. I will ask for GPU, $/hr, duration and cap
   again right before.

## Risks

- ComfyUI master is unreleased code; pin by SHA and re-test.
- The DMD checkpoint is marked work in progress; quality may vary and may change
  upstream. Pin the HF revision.
- LoRA key mismatch (Step 0/2). Fallback: apply the LoRA with the official
  plugin's loader, which means the custom-node route (pinned by commit).
- fp32 requirement applies to the data-space sampler only, which this plan
  doesn't use.

## Out of scope for v1

Layout/bounding-box prompts, INT8, W4A8, the data-space sampler, img2img,
LoRA stacking in the panel, Invoke support (revisit if Invoke adds MOSS).

## Sources

- https://huggingface.co/lodestones/MOSS-Image-Base-8-steps-dmd
- https://huggingface.co/OpenMOSS-Team/MOSS-Image-Base
- https://github.com/OpenMOSS/MOSS-Image/blob/main/comfyui/README.md
- https://artrealmai.com/article/moss-image-native-comfyui-int8-layout-node

## Step 0 results (2026-10-17)

Full detail in `docs/findings.md`, "MOSS-Image step 0 verification".

- Native support confirmed (PR 17095 and 17099 merged), but **after** v0.41.0,
  the newest release and the newest `vastai/comfy` image. Decision 1 is now:
  pin ComfyUI to master commit `a639f187a9f6abfecbda9673034f2045148b6d85` in
  `provision.sh` (gated to this model), or wait for the next tag.
- File sizes and hashes for the base, encoder, VAE, LoRA are known (ungated).
- LoRA keys map onto ComfyUI's fused qkv model.
- **Plan change:** `time_embedding.safetensors` cannot go through a LoRA loader.
  Add a step that builds a `.diff`/`.diff_b` delta file for the time embedder
  and the two modulation biases (at provision time from the two source files,
  or a one-off hosted file, which would need your OK to publish).
- **New risk:** bf16 merge may erase the distillation delta. Live acceptance
  must A/B a merged and an unmerged (on-the-fly) patch, and compare 8-step
  output with the base model's 50-step output.
