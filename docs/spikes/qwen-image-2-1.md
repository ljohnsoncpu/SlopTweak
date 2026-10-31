# Qwen-Image 2.1 spike — Phase A (research, no spend)

Status: Phase A done 2026-10-30; Phase B run 2026-10-31 (results at the end).

"Qwen Image Edit 2.1" is not a separate model. The release is **Qwen-Image
2.1** (2026-09-20): one 7B model for text-to-image and editing, with native
ComfyUI support. Edit is a node setting, not a different checkpoint.

## Verified (✅) — read from the HF API and GitHub, not from memory

- **ComfyUI support is in our pinned image.** Core PR Comfy-Org/ComfyUI#16400
  ("Qwen-image 2.1 support", merged 2026-09-19, merge commit
  `6bfaacc67c2103481e5f0c84d75257cd0581d86a`) is an ancestor of v0.37.0, v0.38.0
  and v0.41.0 (GitHub compare: all "ahead"). The pinned
  `vastai/comfy:v0.38.0-cuda-12.9-py312` therefore needs **no ComfyUI overlay**
  (unlike MOSS-Image): no `setup_moss_comfy` copy and no custom node pack.
- **int8_convrot loads in v0.38.0.** `comfy/ops.py` and `comfy/quant_ops.py` at
  v0.38.0 contain the convrot path. (Whether it runs fast on a 24 GB Ampere card
  is 🧪.)
- **Repo `Comfy-Org/Qwen-Image-2.1`**, revision
  `aacf31a6d48e707f352ecba714cee0680da3479b`, not gated. HF license tag
  `other` / `qwen-research` (see licensing below). Files (bytes, sha256 from the
  HF LFS metadata):

  | Path | Bytes | sha256 |
  | --- | --- | --- |
  | `diffusion_models/qwen_image_2.1_int8_convrot.safetensors` | 7,256,783,064 | `cb74113cb03faecd79611b01fd7fd642f0aa60d6f0b95086abee214d75eaa57d` |
  | `diffusion_models/qwen_image_2.1_bf16.safetensors` | 14,230,280,616 | `89f4158d066cc33906a199fca85634f766892dd78f49b6698dabf187ac86c4bc` |
  | `diffusion_models/qwen_image_2.1_turbo_int8_convrot.safetensors` | 7,256,787,829 | `8144a0182964aa8403ad6dbc011db837304167d235161b9aabaa8532e967314c` |
  | `text_encoders/qwen3vl_8b_int8_convrot.safetensors` | 9,350,798,360 | `8bfd0f6e12abf2d2d697ecc888e5e90b0d6741d6708f05799f53afa560452e8f` |
  | `text_encoders/qwen3vl_8b_bf16.safetensors` | 17,534,334,616 | `68bdc82bc1b66851162ae656225e7e2068166b603db19bd5d5a3b90eb12669a9` |
  | `vae/qwen_image_2.1_vae_bf16.safetensors` | 675,509,688 | `bb21f7473051e1ac368515dd3f2e15cd44d7a11748ee8823e1ddca3e4876b7c9` |
  | `loras/qwen_image_2.1_turbo_lora_avg_rank_178_bf16.safetensors` | 913,314,512 | `208dd43250e1e01467ba190572ae2e7107a7870ec1ff026bc65f791fa1e80e95` |

  Smallest useful set (int8 model + int8 encoder + VAE) is **17.3 GB**, versus
  29 GB for Wulver Identity Edit. Turbo checkpoint (8 steps) is the same size.
- **Edit node:** core `TextEncodeQwenImage21` ("Text Encode Qwen Image 2.1",
  `comfy_extras/nodes_qwen.py`). Inputs: `clip`, `prompt`, `negative_prompt`,
  optional `vae`, `resolution` (default 1024, 0 = keep each reference's size,
  multiples of 32), and `image_1`..`image_16`. Outputs positive, negative and an
  **empty latent sized to the first reference** (use it as the sampler latent;
  any other size shifts the edit). With `vae` connected, references also go in
  as reference latents. Prompts address references as `<image1>`, `<image2>`.
  RGBA references are composited on white for the vision tower.
- **Sampler (docs):** base 25 steps, CFG 1, euler, simple; Turbo 8 steps
  (ManualSigmas in the template; or Turbo LoRA on base, 8 steps, simple). Docs
  say higher CFG degrades quality and quote ~40-50 steps for the reference
  pipeline.
- **Optional node `QwenImage21Cache`** (experimental KV cache: device
  auto/gpu/cpu/off, dtype default/int8/int4). `off` rules the cache out if
  outputs look wrong.
- Official templates: `image_qwen_image_2_1_image_edit`,
  `image_qwen_image_2_1_turbo_image_edit`.

## Differs from assumptions (⚠️)

- **No LoRA, no custom nodes, no ComfyUI pin needed.** My earlier estimate
  assumed a newer ComfyUI. Not so; the work is the graph and the catalog entry.
- **The graph is much simpler than Krea's.** `UNETLoader` + `CLIPLoader` +
  `VAELoader` + `TextEncodeQwenImage21` + `KSampler` + `VAEDecode`. No
  `Krea2EditModelPatch`, no `ref_boost`, no grounding step.
- **Content risk.** `docs/spikes/krea2-edit-spike.md` records that Qwen Image
  Edit was judged too censored and weak on furry content, which is why the
  project went to Krea. 2.1 is a new model and nobody has tested it for this
  user's content. This decides whether the feature is worth shipping, so Phase
  B tests it first (SFW only, user judges quality).

## Untested (🧪)

1. `CLIPLoader` `type` for the single-file Qwen3-VL-8B encoder. v0.38.0 lists
   `qwen_image` among the types and `comfy/sd.py` imports
   `text_encoders.qwen_image21`; the exact value and any auto-detect must come
   from the official template workflow JSON (read it on the instance).
2. Peak VRAM and s/image for int8 on a 24 GB card; whether 24 GB is enough with
   two references at `resolution` 1024. The docs give no VRAM numbers.
3. Whether the sidecar's `/prompt` path and `/queue` idle check need changes
   (expected: none, same ComfyUI).
4. Cold-start time with a 17 GB download.
5. Whether `weight_dtype: default` is right for the convrot checkpoint.
6. Multi-reference behaviour (2 and 3 references) and the `<imageN>` syntax.

## Licensing (open, needs the user)

- HF tags the repo `qwen-research`. The license text was **not** read in this
  phase. A research license may bar commercial use or hosting, which matters
  for a rented-GPU product. Read the LICENSE file in the Qwen/Qwen-Image-2.1
  repo (and the Comfy-Org copy's README) before any catalog entry ships.
  PLAN §9 (content policy, naming) applies; ask, don't assume.

## Launcher changes (sketch, nothing written)

1. `catalog/catalog.json`: new `backend: comfyui` entry (e.g.
   `qwen-image-edit`) with the four files above (model, encoder, VAE, plus the
   Turbo LoRA only if Turbo is wanted), `dest` = `diffusion_models`,
   `text_encoders`, `vae`; `min_vram_gb` 24 pending the spike; `min_ram_gb`
   from the spike. Extend the bundled-entry tests in `catalog.rs`.
2. `identity.rs`: `build_graph_with` is Krea-specific (`ModelFiles`, class
   names, `ref_boost`, the prompt prefix). Add a model-family switch and a
   second graph builder; reuse the mask/crop/composite helpers
   (`add_edit_nodes`, `add_composite_nodes`) unchanged.
3. UI (`main.ts`): hide the ref-boost / pull controls for this family; allow up
   to 3 references to start.
4. `provision.sh`: nothing expected beyond the existing comfyui path.
5. Findings update and Rust tests for the new graph.

## Phase B run sheet (needs approval; do not start)

- **Instance:** one RTX 3090 or 4090 (24 GB), `vastai/comfy:v0.38.0-cuda-12.9-py312`
  pinned by digest, on-demand only (never interruptible), Turing+ floor.
- **Cost guess:** about $0.20-0.30/hr (the 3090 in the Krea spike was
  $0.199/hr); download 17.3 GB, so budget ~15 min startup plus ~45 min of
  testing, **cap about $0.50 total**. Exact quote to be confirmed from live
  offers at approval time.
- **Safety:** script prints the instance id first, destroys on every exit path
  (trap/finally), I verify it is gone at the end. Keys only from env.
- **Steps:** provision with the int8 set; load the official
  `image_qwen_image_2_1_image_edit` template and read its real node graph
  (resolves untested 1, 5); run the matrix below; record load time, peak VRAM,
  s/image.
- **Matrix (SFW, adult characters):**

  | # | Case |
  | --- | --- |
  | 1 | Template runs as shipped (int8, 25 steps) |
  | 2 | Single-reference edit with our simplified graph |
  | 3 | Two references into one scene (`<image1>`, `<image2>`) |
  | 4 | Turbo checkpoint, 8 steps: speed and quality delta |
  | 5 | Masked edit through our existing mask/crop helpers |
  | 6 | Character-sheet style input of the kind the user supplies |

- **Pass:** loads on 24 GB; identity holds on case 2/3 to the user's eye; 25-step
  time is acceptable (target under ~30 s/image); no changes needed in the
  sidecar.
- **Fail/stop:** OOM on 24 GB (retry once on 32 GB only with new approval) or
  the user judges the content quality unusable.

## Sources

- https://docs.comfy.org/tutorials/image/qwen/qwen-image-2-1.md
- https://comfyui-wiki.com/news/2026-09-21-qwen-image-2-1
- https://huggingface.co/Comfy-Org/Qwen-Image-2.1 (API, revision above)
- https://github.com/Comfy-Org/ComfyUI/pull/16400
- `comfy_extras/nodes_qwen.py` at ComfyUI v0.38.0

## Phase B results (2026-10-31, approved, instance 58544111)

Run by `app/dev/qwen-spike.mjs` (draft catalog entry added at runtime; repo
catalog unchanged; provisioning via the existing ComfyUI path). One RTX 3090
(24 GB, Czechia, 251 GB RAM) at $0.2778/hr on `vastai/comfy:v0.38.0`. Instance
destroyed by Stop (verified gone; none left). **Spend: about $0.07** (credit
$11.2645 to $11.1950; late charges may post).

Two attempts failed before spending anything: the app refuses Start when the
pinned instance bundle isn't published for this branch (dev pre-release
`instance-v0.1.15` was published to fix that, same bytes as the pin).

- ✅ **Ready in 5 min** for 18.2 GB (download 5 min, host-dependent).
- ✅ **Runs on 24 GB** with the int8 set; after each run ~8-10 GB VRAM was
  still free. No ComfyUI overlay, no custom nodes needed.
- ✅ **Settings that worked** (no retries): `CLIPLoader` type `qwen_image`,
  `UNETLoader` weight_dtype `default`, `TextEncodeQwenImage21` with API-format
  reference keys `images.image_1`, `images.image_2`; `resolution` 1024; KSampler
  25 steps, CFG 1, euler, simple, latent from the node's third output.
  `<image1>`/`<image2>` prompt syntax works.
- ✅ **Speed (25 steps, ~832x1248):** 62 s one ref, 62 s and 55 s two refs, 53 s
  instruction edit, 62 s at `resolution` 0. **Turbo LoRA, 8 steps: 30 s.** The
  first run (model load) was no slower than the rest.
- ✅ **Identity holds** (user to confirm by eye): fox character kept face, hair,
  ears, earrings, armor details and tail through a new scene; two-reference
  cafe scene kept both characters recognisable (tiger's stripes, wraps, red
  cape and blue cloth; fox's armor). The "armor into red hoodie" edit changed
  only the torso and kept pose, face and legs. Background went to plain white
  like the reference. Turbo LoRA output was close to the 25-step one.
- ✅ **Furry content is not a blocker on this test.** SFW anthro characters
  rendered faithfully. The earlier worry (Qwen Image Edit judged weak and
  censored on furry content) was about the older model; 2.1 looks fine here.
  NSFW behaviour was not tested.
- ⚠️ Small flaw: garbled text on the cafe chalkboard (expected).
- ⚠️ The official template was not readable through
  `/api/workflow_templates` (only the `comfyui-krea2edit` module was listed), so
  the settings above come from successful runs, not the template.
- 🧪 Not tested: mask/crop helpers, 3+ references, resolution above 1024,
  `QwenImage21Cache`, bf16 weights, 4090/5090, NSFW.

Outputs (scratch, not in repo): scratchpad `qwen-out3/` (A-F PNGs, log,
summary.json, object_info.json).

### Recommendation

Go ahead with the catalog entry and a second graph builder. It is cheaper and
faster than Wulver Identity Edit (18 GB vs 29 GB, 24 GB card vs 32 GB, about
$0.28/hr vs $0.50-0.65/hr, ~1 min per image). Still blocking: **read the
`qwen-research` license** before shipping, and decide naming and content policy
(PLAN section 9).

## Panel live test (2026-10-31, approved, instance 58548233)

Run by `app/dev/qwen-panel-live.mjs` against the real Identity Edit panel code
(catalog entry `qwen-image-edit`, one RTX 3090 at $0.2682/hr, ready in 3 min).
Instance destroyed; **spend about $0.07**. Seven generations, all completed.

| Run | What | Time | Result (by eye) |
| --- | --- | --- | --- |
| R1 | 3 references via the Make image button, slots 0/3/4 (1 and 2 empty), landscape | 102 s | ✅ all three characters recognisable; panel tags `<image1>`..`<image3>` were right |
| R2 | New, reference cropped to the face ("Focus on a part") | 51 s | ✅ face and colours kept |
| R3 | Edit, picture only, full strength | 58 s | ✅ (same behaviour as the spike's outfit edit) |
| R4 | Edit, picture plus a second reference | 65 s | ✅ fox kept, tiger added from its reference |
| R5 | Edit at 70% strength | 55 s | ⚠️ barely changed: only the collar turned red. Strength below 100 stays too close to the picture for a big change |
| R6 | Edit with a painted rectangle, no zoom | 56 s | ✅ hoodie in the painted area; ⚠️ a little red bleed on the shoulder/arm edges (rectangle mask, soft seam) |
| R7 | Same, zoomed in | 57 s | ✅ ran; looks the same as R6 (zoom may not engage for this mask size) |

- ✅ The 64-channel latent from `VAEEncode` works for strength < 100 and
  painted areas; the node's size alignment held (no shift).
- ✅ Gaps in the slot numbers are fine: the backend and the panel both number
  only the filled pictures, in the same order.
- ⚠️ Harness note: the panel keeps 6 results, so a result-count check says R7
  "failed" although it succeeded (the file is in the output folder).
- 🧪 Still not tried: 4+ references on a real GPU, Turbo LoRA, NSFW.
