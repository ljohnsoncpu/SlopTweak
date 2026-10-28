# Spike: LoRA training on Kromoss (MOSS-Image)

Status: research only (2026-10-27). Nothing rented, no code changed. Request:
train character LoRAs on `lodestones/Kromoss` from the Train tab, to see how
it handles complex characters Illustrious struggles with.

## Verdict

Feasible. Phase T's trainer (kohya `sd-scripts`, SDXL only) cannot do it, but
KohakuMossTrainer can, and Kromoss is a drop-in base for it. A second trainer
backend is needed, not a settings change. One licence/naming question (below)
needs the user's call before any code.

## Facts verified (free, no rental)

**Kromoss** (`lodestones/Kromoss`, HF rev
`a4fe97449297af2e3be7841e1c18e431f3b06031`, ungated, Apache-2.0):
- Full finetune of `OpenMOSS-Team/MOSS-Image-Base` at 1024 px; same
  architecture, Qwen3.5-9B text encoder and FLUX.2 small VAE as MOSS-Image-SFT.
  Checkpoint is update 11000 (EMA), 704k images. The repo is re-published as
  training continues, so pin the revision.
- `diffusion_models/kromoss_bf16.safetensors` 10,164,655,808 B,
  sha256 `54c863b9392f7b6a02f557d32a416b22765b4ffbee322ade9945e379f09adaba`.
- `diffusion_models/kromoss_int8_convrot.safetensors` 5,413,698,958 B,
  sha256 `cacff0dd25cd2cf531e1f5ddae7872c3d57a54dbf66f1a1279f2f118514e7905`.
- Text encoder and VAE are the same files the `moss-image-dmd` catalog entry
  already uses (Comfy-Org/MOSS-Image rev `f28da8f9…`). Kromoss also hosts
  `text_encoders/qwen3.5_9b_bf16.safetensors` (19.3 GB), which we don't need.
- Header check: `kromoss_bf16.safetensors` has the same 379 tensor keys, shapes
  and dtypes as Comfy-Org's `moss_image_base_bf16.safetensors` (fused
  `proj.qkv`). So any loader that reads the Comfy-Org file reads Kromoss.
- Ships 8-step LoRAs (`8-steps-dmd2-lora`, rank 128, kohya keys, apply unmerged
  at strength 1). Distilled from Base, "transfer to Kromoss".
- CivitAI page (civitai.red) returned 403 to fetch; HF is the source of truth
  here. Not checked: whether the CivitAI files are the same bytes.

**KohakuMossTrainer** (`KohakuBlueleaf/KohakuMossTrainer`, `kohakumoss` 0.1.0):
- Head at time of check: `67f3bb54d8ee8f18384b11113f96b5b71d38c12f`
  (2026-10-27). Very active (133+ commits, no tagged releases), so pin a SHA.
- Trains LoRA/LoCon, LoHa, LoKr, DoRA, OFT via LyCORIS, plus full finetune.
  Launch: `kogine run scripts/train/moss.py --config <py>`; any knob via
  `--set NAME=value`. Configs are Python modules.
- **Custom base is supported by design.** `WEIGHTS` is a ComfyUI-style
  `models/` folder; `TRANSFORMER` names any file or path, so
  `TRANSFORMER=kromoss_bf16` (or the int8 file) needs no code change. The docs
  describe full finetunes ("your own fine-tune") as first-class bases.
- Dataset: kohya-style folders (`N_name/` with same-stem `.txt`), plus folder,
  jsonl, parquet, webdataset. Our existing dataset tar layout maps directly.
  Text and latent caches are built first (`cache_text.py`, `cache_latents.py`).
- Exports `<run>-comfy.safetensors` (loads in ComfyUI's LoRA loader, which is
  what our MOSS panel already runs) and `-lycoris.safetensors`.
- Memory: README says rank-32 LoRA used ~6.6 GiB allocated, 9-10 GB in
  nvidia-smi on a 4090; recipes exist for 24, 16, 12, 8 and 6 GB. Host RAM is
  the catch: ~19 GiB on small profiles, ~32 GiB at build.
- Deps: `torch>=2.13,<2.15`, `transformers>=5.3`, `diffusers>=0.38`,
  `lycoris-lora>=4.1.3`, optional tilelang/triton kernels (first run tunes
  kernels before step 0). Python 3.10+.
- Licence: "KohakuMoss License 1.0", Apache-2.0 plus a naming requirement
  (derivative works, packages, products or services built on it must include
  "Kohaku" in their name). Trained models, adapters and images are explicitly
  not affected. GitHub reports NOASSERTION, so read `LICENSE` in full.

## Not verified

- That a LoRA trains end to end on the Kromoss file. Expected from the layout
  match and the docs, but only a run proves it.
- Whether training on bf16 vs int8 Kromoss matters for LoRA quality. The
  trainer's default LoRA recipe uses int8 as stored (5.4 GB); a bf16 base is
  likely the better target for a LoRA applied to the bf16 file at inference.
- Whether the DMD2 turbo LoRA stacks cleanly with a Kromoss-trained character
  LoRA (probable, untested), and whether the character LoRA should be tested
  at 50 steps CFG 3 or 8 steps CFG 1.
- Real timing and VRAM on a Vast card; the trainer's published numbers are on
  its own rigs.

## Design

Keep Phase T's lifecycle (quote, approve, run, sync, hash-verify, destroy) and
swap the instance-side trainer by `trainer_kind`.

1. **Settings** (`train/settings.rs`): a `TrainerKind { Kohya, KohakuMoss }`
   with a base preset. For MOSS render a Python config instead of kohya TOML:
   `WEIGHTS`, `TRANSFORMER`, `DATASETS`, `ALGO=lora`, `DIM`/alpha, LR, `STEPS`,
   `SAVE_EVERY`, resolution 1024. Defaults come from the trainer's
   `lora_24g_bf16` recipe; they are untuned for characters, so the first real
   run sets them.
2. **Runner** (`instance/train_runner.py`): a second bootstrap path. Pinned
   trainer commit and tree digest (same `tree_digest` check as kohya), a venv
   from a new lockfile with torch 2.13/2.14 (needs a pin check against the
   image's CUDA), `kohakumoss-doctor` as a stage-0 check, then cache_text,
   cache_latents, train, export. Parse progress from `metrics.jsonl`/`log.txt`
   rather than kohya's tqdm line.
3. **Evaluation**: kohya's `sdxl_gen_img.py` does not apply. Use the existing
   MOSS ComfyUI graph (`moss.rs` / `moss_workflow.py`) with the exported comfy
   LoRA, base-vs-LoRA control as now. Needs ComfyUI up on the training
   instance or a trainer-side sampler (`scripts/sample/sample.py`); the latter
   avoids a second stack on the train image.
4. **Assets**: catalog entry `kromoss` (`backend: comfyui`, `base: moss-image`)
   with the two files above, Apache-2.0, and a separate trainer pin. Also lets
   the MOSS panel generate with Kromoss, which is useful for judging results.
5. **Hardware**: size per [[size-hardware-to-model]]: a 24 GB card with >= 48 GB
   host RAM and a disk for ~30 GB of weights plus caches; confirm offers expose
   host RAM. Expect roughly the Thalia cost order, to be measured, not promised.
6. **Dataset audit** (`dataset.rs`): bucket prediction is SDXL-specific; MOSS
   uses token-budget batching, so audit would only check image count, size and
   caption presence. Captions: natural language likely suits MOSS (Qwen text
   encoder) better than Illustrious tag lists; the trainer supports both.

## Open decisions (ask the user; PLAN §9 / CLAUDE.md)

- **Naming:** if SlopTweak is a "product or service built on" KohakuMoss, the
  licence asks for "Kohaku" in the name. Running it as an instance-side tool
  may not count, but this is a legal-reading call, not mine. Alternatives:
  Diffusers/DiffSynth-Studio (also lists MOSS LoRA training; not evaluated
  here), or write our own thin training loop (large).
- **Scope:** one LoRA preset for Kromoss only, or a general MOSS preset that
  also takes Base/SFT.
- **First real run:** needs explicit approval with GPU, $/hr, duration, cap.
  Suggested acceptance: one small character set, LoRA on Kromoss bf16, 24 GB
  card, eval against the base control.

## Proposed phases

- **K0** (free): read the trainer `LICENSE`; compute the tree digest of the
  pinned commit; confirm torch/CUDA pin against the Vast image; run
  `kohakumoss-config new train` locally on the 12 GB laptop with a tiny
  dataset (the laptop already has `C:\ai` Kromoss files) as a no-cost
  smoke test of "Kromoss as base".
- **K1** (mock): settings, config rendering, runner stages, sim, UI picker,
  tests; no rentals.
- **K2** (paid, approved per run): one real run, record in `findings.md`.

## Sources

- https://huggingface.co/lodestones/Kromoss
- https://github.com/KohakuBlueleaf/KohakuMossTrainer (docs: weights.md,
  first-lora.md, datasets.md, full-finetune.md at the pinned commit)
- https://github.com/OpenMOSS/MOSS-Image
- https://comfyui-wiki.com/en/news/2026-10-19-kromoss

## K0 results (2026-10-27, free, nothing installed or rented)

**Licence (read in full, section 10).** The naming rule covers a derivative
work, product, package or Service that "incorporates, is built upon, or is
substantially derived from the Work". "Service" means offering the Work's
functionality to third parties over a network. Explicitly exempt: internal
tools or scripts not distributed to third parties, and models, adapters,
images or other outputs of running the Work. Reading: SlopTweak does not
bundle or modify the trainer; it fetches the pinned source at run time onto
the user's own rented instance and keeps the LoRA. That looks outside the
rule today. Caveats: (a) if SlopTweak is ever distributed to third parties
*and* counts as "built upon" the trainer, "Kohaku" in the name would be
required; (b) if the repo vendors trainer files, that is redistribution. We
fetch, never vendor. This is my reading, not legal advice; the user decides
(PLAN §9 naming).

**Tree digest** of commit `67f3bb54d8ee8f18384b11113f96b5b71d38c12f`, from
GitHub's codeload tarball (5,997,367 B) with `tree_digest` from
`instance/train_runner.py`:
`d51a2ec20987f8e8b7b9088ee5f7943e120593250dc2d8b86e22a11163338f12`.

**torch/CUDA pin: this is a real conflict.** The trainer needs
`torch>=2.13,<2.15` plus `transformers>=5.3`, `diffusers>=0.38`; its install
helper picks cu126/cu128/cu130 by driver. The existing train backend is
pinned to torch 2.6.0+cu124 (Python 3.12, hash-locked) on a
`pytorch/pytorch:2.6.0-cuda12.4` image and limits offers to compute
capability 800-999. So MOSS training cannot reuse `train_requirements.lock`:
it needs its own hash-locked venv (torch 2.13/2.14 from a cu128 or cu130
index) and its own offer filter (host driver CUDA >= 12.8; Blackwell becomes
allowed with cu128+). Expect a bigger download (torch cu128 wheels are a few
GB) and a longer setup than the 17.5 min kohya run. Not checked: which
Vast driver versions the cheap 24 GB offers actually report; do that from
the offer data before choosing cu128 vs cu130.

**Laptop facts.** RTX 4080 Laptop 12 GB, driver CUDA 13.3, 31.7 GB RAM, 84 GB
free on C:. `C:\ai\ComfyUI\models` already holds the trainer's whole default
int8 set: `kromoss_int8_convrot` (5.04 GB), `qwen3.5_9b_int8_convrot`
(9.08 GB), `flux2-vae_small` (0.23 GB), so no model download is needed.
ComfyUI's venv has torch 2.11.0+cu128, too old for the trainer, so it must
not be reused or modified. No `cl.exe` on PATH: the kernel extras need Visual
Studio Build Tools on Windows; check with `kohakumoss-doctor` whether they
are required or optional before installing them.

### Local smoke test plan (needs the user's go-ahead for each install)

Goal: show that `TRANSFORMER=kromoss_int8_convrot` loads, caches, trains and
exports a LoRA that ComfyUI loads. Quality is not the point.

1. Clone the pinned commit to `C:\ai\KohakuMossTrainer` (about 6 MB). No
   effect on the existing ComfyUI install.
2. New venv inside the clone: `uv venv -p 3.12`, torch 2.13/2.14 from the
   cu130 (or cu128) index (a few GB download), then `pip install -e ".[cuda]"`.
   Only the clone's `.venv` is touched. Then `kohakumoss-doctor`.
3. Tiny dataset: 8-12 images with `.txt` captions in kohya folder layout. Which
   images (user's choice; nothing from `.dev/` is read without asking).
4. `kohakumoss-config new train --vram 12 --weights C:\ai\ComfyUI\models
   --data <dataset> --name km-smoke`, set `TRANSFORMER=kromoss_int8_convrot`,
   `STEPS` ~100, then the three commands (inspect, cache_text, cache_latents,
   train). The ComfyUI server on :8199 must be stopped first: 12 GB is shared.
5. Load `export/km-smoke-comfy.safetensors` in the local ComfyUI with Kromoss
   and compare with and without the LoRA.
6. Pass criteria: no key-mismatch errors loading Kromoss, loss decreases, LoRA
   visibly changes the output, peak VRAM and s/step recorded. Record the
   result in `findings.md`.

Risks: laptop quiet mode halves speed (keep it off); the first run tunes
kernels; Windows may need Build Tools and the CUDA toolkit; step time on a
12 GB streaming profile will be much slower than a 24 GB cloud card, so this
tells us "does it work", not cost.

## Local smoke test result (2026-10-27)

Cloud attempts first (all destroyed, total about $0.15): the first run failed
at the torch install (cu128 index only has torch <= 2.11; use cu126 or cu130),
and Vast's create call returned `success: false` with a valid contract on three
launches, so a driver must record `new_contract` before checking `success`. The
last instance sat in "loading" for 15 min and was destroyed. So the cloud path
is **still unverified**: setup after the torch install never ran.

Then the user asked for a local run, with everything on D:.

- **Setup:** pinned commit `67f3bb54…` at `D:\ai\KohakuMossTrainer`, uv venv
  (Python 3.12.11) with torch 2.14.1+cu130, `.[cuda]` extras, uv caches and
  Python on D:. `kohakumoss-doctor` ok on the RTX 4080 Laptop (sm89, 12 GB).
- **Windows toolchain caveat:** the machine has VS 2019 Build Tools (MSVC
  14.29) and Windows SDK 10.0.19041. Triton's `driver.c` needs `stdalign.h`
  (absent) and compiles `= {}` initializers MSVC 14.29 rejects as C. Fixed
  inside the venv only (stub header; two `= {}` -> `= {0}`). A newer VS Build
  Tools would remove the need. SlopTweak's cloud path is Linux, so unaffected.
- **Kromoss as base: works.** `TRANSFORMER=kromoss_int8_convrot` with the
  int8 Qwen3.5 encoder and `flux2-vae_small`, all read from the ComfyUI models
  folder. No key mismatch, no errors. Config used the `lora_12g_stream` recipe
  plus: `DATASETS=[{"source":"folder","txt_as":"nl"}]`, `CAPTION={"template":
  "nl"}`, 200 steps, save every 100.
- **Config gotcha:** in the data-prep config `TEXT_ENCODER`/`IMAGE_ENCODER`
  are encoder *factory paths* (`kohakumoss.encoders.text_encoder`,
  `...vae_encoder`), while in the train config `TEXT_ENCODER` names the weight
  file. A data config that `use_config`s the train config must reset them.
  `kohakumoss-config new train` also refuses a ComfyUI folder lacking the
  *base* file names; build from `new --from <recipe>` and override instead.
- **Dataset:** 21 images (1024x1536, 1312x1199, 1374x1145), natural-language
  captions. Planner buckets to 960x1056, 1248x832, 928x1120.
- **Run:** text cache 22 embeddings in about 2 min; latents 21 in 4 s; 200
  steps in about 26 min (step 1 took 17.5 s of compile/tuning; steady state
  about 4.6 s/step, 865 tokens/s, 31 TFLOPS). Peak VRAM only 2.3 GiB
  allocated / 3.0 reserved (weights streamed), though host-to-device traffic
  is about 10.6 GB per step. No skipped or non-finite steps.
- **Loss:** 0.341 at step 1, 0.265 at step 200; logged every 10 steps and noisy
  (flow-matching with random sigma), so it says "trains and is finite", not
  "learned the character". Not a quality result.
- **Outputs:** `runs/moss/km-smoke/export/km-smoke-comfy.safetensors`
  (198 MB, rank 32) and `-lycoris.safetensors` (149 MB).

**Not yet done:** loading the LoRA in ComfyUI on Kromoss and comparing with and
without it; that is the real quality check. Cloud timing and cost for a 24 GB
card remain unmeasured.

### ComfyUI A/B (2026-10-27, local)

`km-smoke-comfy.safetensors` (strength 1.0) stacked on Kromoss int8 + the
`8-steps-dmd2-lora`, euler/simple, 8 steps, CFG 1, 832x1216, same seed with and
without the character LoRA, three prompts (close-up, rainy street, full body).
About 16-22 s per image on the 4080 laptop. Pictures and the side-by-side are
in `D:\ai\km-compare\` (`montage.png`).

- The LoRA stacks on the DMD2 LoRA with no errors and changes every image.
- With it, all three images share one look: smooth brown skin, large upright
  ears with pink interiors, four tendrils, gray cybernetic limbs on the right
  side described. Without it, the same prompts gave a gray-furred close-up and
  maroon skin in the other two, i.e. the base follows the words but drifts.
- Caveats: 200 steps, 21 images, one seed per prompt, judged by eye; stray
  signatures appear in some outputs of both columns (from the training data).
  The character is closer, not proven consistent. A real test needs more seeds
  and a held-out reference comparison.

## Next: bigger Vast run, tags + natural language, and SlopTweak integration

Decided 2026-10-27 after the local smoke test: the LoRA picked up the concept
fast; the user wants (1) a bigger cloud run, (2) e621 tags mixed into the
captions, (3) a workflow that can move into SlopTweak's Train tab.

### Captions: tags + natural language

The v3 Roka set has three sidecars per image: `.nl.txt` (natural language,
trigger first), `.e621.txt` (e621 tags, trigger `roka_soldier` first) and `.txt`
(a booru/e621 hybrid for SDXL). SlopTweak's dataset audit already ignores
`*.e621.txt` and `*.nl.txt` as alternates, so the folder format needs no change.

The trainer's `mix` template does the rest (verified locally with `inspect_data`):
a `train.jsonl` with `{"image", "caption": <nl>, "tags": [<e621 tags>]}` per row and

```python
CAPTION = {"template": "mix", "p_nl": 0.5, "nl": {"template": "nl"},
           "tags": {"template": "tags", "keep": [], "keep_first": 1, "shuffle": True,
                    "tag_dropout": 0.1, "underscores": False}}
CAPTION_VARIANTS = 8
```

- `keep_first: 1` pins the trigger token; the other tags shuffle and drop 10 %
  so the LoRA does not learn tag order. `underscores: False` keeps
  `roka_soldier` spelled the same in both forms; this is a knob to A/B later.
- `CAPTION_VARIANTS` is how many captions per image the text cache holds, so 8
  gives the mix real variety at a small cache cost (about 2 s per caption).
- Nothing here has been trained yet. Whether tags help or hurt is the
  experiment.

### The experiment

One setup, two LoRAs from the same 21 images and seeds, 800 steps, a checkpoint
every 200 (`/w/data/nl` plus `/w/data/mix/train.jsonl`):

| run | captions |
| --- | --- |
| `nl` | natural language only (what the 200-step local LoRA used) |
| `mix` | 50 % natural language, 50 % e621 tags, tag dropout |

Compare checkpoints 200/400/600/800 of both on the same prompts and 3 seeds
each, locally in ComfyUI (Kromoss int8 + the 8-step DMD2 LoRA, as before), with
prompts in both styles (a natural-language one and a tag one) to see whether
the tag-trained LoRA is better at tags and no worse at sentences.

### Vast run plan (not yet run, needs approval per rental)

- Script: `remote.sh` v2 (kept in the session scratchpad, to be moved under
  `instance/` once proven). One setup, then the two runs back to back; it
  uses the int8 Kromoss file (5.4 GB, sha256 `cacff0dd...`), the int8 Qwen
  encoder and the full FLUX.2 VAE (same latents as the small one), so about
  16 GB of downloads instead of 30. Torch from the **cu126** index (the
  cu128 index tops out at 2.11).
- Offer filter: one GPU >= 23 GB, compute capability >= 8.0, driver CUDA >=
  12.6, host RAM >= 48 GB (the trainer used up to 32 GB), disk >= 100 GB, down
  >= 500 Mbps, reliability >= 0.98. Today (read-only) that gave an RTX 3090 Ti
  in Vietnam at $0.243/hr base (126 GB RAM), an A10 in the Netherlands at
  $0.243, and 3090s from $0.339. All-in is about $0.045/hr higher (disk and
  bandwidth). The offer list changes within hours; ask again before renting.
- Time and cost: unknown until measured. Local 12 GB streaming ran 4.6
  s/step; a resident 24 GB profile should be several times faster. Estimate
  60-120 min for setup plus both runs, so $0.30-0.60 at about $0.29/hr all-in;
  a 120-minute hard cap bounds it near $0.60.
- Driver (scratchpad `driver.py`) fixes learned the hard way: track
  `new_contract` before reading `success` (Vast returns `success: false` with a
  real contract); make the cap timer a daemon thread; redact
  `instance_api_key` in logs; kill nothing without the instance id; fetch only
  `checkpoints/*/export/*-comfy.safetensors` (198 MB each), `metrics.jsonl`
  and `log.txt`, not the 1.5 GB resume states.
- What is still unproven on Linux/Vast: everything after the torch install
  (the trainer's `.[cuda]` extras, kernel build, model downloads, caches,
  training). Expect one more failed attempt to cost cents, not dollars.

### Integration into SlopTweak (after the workflow is proven)

Reuses Phase T's lifecycle (quote, approve, run, sync, hash-verify, destroy;
`manager.rs`, `run.rs`) and swaps what is trainer-specific:

1. `TrainerKind { Kohya, KohakuMoss }` in `settings.rs`; base presets
   `sdxl-illustrious` and `kromoss`. Fields that do not apply (resolution
   buckets, CLIP tokens) are hidden for MOSS.
2. `job.rs`: a MOSS job bundle (pinned trainer commit and tree digest, the
   three model files and their SHA-256, caption mode `nl` | `mix`, steps, save
   interval, rank). The runner renders the trainer config on the instance.
3. `instance/train_runner.py`: a MOSS bootstrap with its own hash-locked venv
   (generate the lock from a working Linux install, as `train_requirements.lock`
   was), stages `bootstrap -> prepare -> cache -> train -> export`, progress
   from `metrics.jsonl`.
4. `run::offer_query`: a MOSS variant (cuda >= 12.6, RAM >= 48 GB, Blackwell
   allowed once the cu126+ stack is verified there).
5. `dataset.rs`: for MOSS, also read `name.e621.txt` and `name.nl.txt` and
   build the jsonl; audit warns when only one style exists (then `mix` is off).
   Image-size rules relax to a pixel-area budget.
6. Evaluation: the existing MOSS ComfyUI graph (`moss.rs`) with the exported
   LoRA and a base control, instead of kohya's `sdxl_gen_img.py`; checkpoints
   downloaded and hash-verified before destroy, as now.
7. UI: Train tab model picker (Illustrious | Kromoss), caption mode, steps and
   checkpoint count; the Quote screen shows the actual offer and cap. Cloud
   defaults come from the measured run, not from guesses.
8. Catalog: a `kromoss` model entry so the MOSS panel can generate with it.

Open decisions for the user: the "Kohaku" naming reading (see K0 results), and
whether the Kromoss preset should default to `mix` or `nl` once the experiment
is in.

## Vast run v2: `nl` only, and why it was slow (2026-10-27, no new spend)

Instance 58007312, RTX 3090 24 GB (Saskatchewan, Vast offer 37247586, PCIe 23.2 GB/s),
$0.288/hr all-in, 105.9 min, about $0.51, destroyed. The time cap stopped `nl` at step 710
of 800 (checkpoints 200/400/600 fetched and fine); `mix` never started. Evidence: the run's
`metrics.jsonl`, `log.txt`, `config.json`, the driver and remote logs, and the laptop
`km-smoke` run for comparison.

### Where the 105.9 minutes went

| phase | minutes | note |
| --- | --- | --- |
| create to SSH ready | 8.2 | image pull and boot |
| trainer, venv, torch, model downloads | 6.4 | |
| text and latent caches | 8.9 | text 7.6 (about 3 of it one-off Qwen kernel work), latents 1.0 |
| model build | 0.8 | |
| kernel prewarm and adapter prewarm | 11.7 | 382 s + 256 s of timing before step 1; stored per card, so a second run on the same instance pays 0 |
| training, 710 steps | 68.8 | includes one 88 s sample at step 500 and 2 s exports |
| fetch and destroy | about 1.9 | |

So 35 % of the rental was setup. Training itself: 5.72 s/step median (range 5.2 to 6.1 after
step 1), 1463 tokens/s, 52 TFLOP/s, reported MFU 0.73 against the 3090's 71 TFLOP/s bf16
peak. Peak VRAM only 8.0 GiB allocated of 24.

### The "5.5 s/step" is mostly a bigger step, not a slow card

The cloud config's `MAX_TOKENS` is 12288 (the `vram_24g` layer); the laptop smoke run used 6144
(`vram_12g`). A cloud step is about 2.0 samples (8.4k real tokens); a laptop step is 1.0
sample (4.0k tokens). Per token the 3090 is 1.67x the laptop (1463 vs 874 tok/s), not
"several times faster", and 800 cloud steps are about 76 epochs of the 21 images, about
twice the data of 800 laptop steps. Compare tokens/s, not s/step.

### Hypotheses, checked

- **(a) Streaming every step: not the cost.** The recipe was `lora_24g_bf16`, not `12g_stream`,
  but its `balanced` profile is `ckpt-ring16`: block checkpointing plus a weight ring (16 of
  42 blocks resident), 5.9 GB host-to-device per step. In the sampler log the copy engine was busy 0.141 s
  of each 1.65 s step with "compute waited 0.000 s" (3.17 GB in 0.141 s, about 22 GB/s). Copies are hidden behind
  compute. The real price of `balanced` is **block checkpointing**: it recomputes the forward in
  backward, about 8NT instead of 6NT FLOPs (roughly +33 %), while 16 GiB of VRAM sit unused.
  The profile catalog lists `nockpt-ring16` at about 30 GiB, which does not fit 24 GB at
  12288 tokens; 6144-token micro-batches x `GRAD_ACC=2` (same 12288 tokens per optimizer
  step) should. Not yet measured.
- **(b) Missing kernels: no.** `kohakumoss-doctor` ok; triton 3.8.0 and tilelang 0.1.15
  installed; the registry log shows every training op on `triton` (the only `compile`
  ones are two text-encoder ops, unused at train time because the text cache is offline).
- **(c) PCIe: no.** See (a).
- **(d) Batch, resolution, dataloader: no.** About 1 MP images, fill about 68 % of 12288
  tokens, data time 2.5 s on step 1 only.
- **(e) Sampling and exports: small.** One 50-step sample is 88 s (about 2 % of the training time, equal to 15 steps);
  each export about 2 s.
- **(f) dtype: small.** The base is the int8 ConvRot file used weight-only (dequantised
  inside a bf16 GEMM). The trainer's own GEMM bench on a 3090 has weight-only 3 to 10 %
  slower than dense bf16 (qkv fwd 2.74 vs 2.64 ms) and `INT8_COMPUTE` 15 to 25 % faster
  than dense (qkv 2.14 ms, up_gate 3.61 vs 4.73), so a bf16 base would gain at most a few
  percent and cost a 10 GB download; `INT8_COMPUTE` changes the numerics (quantised
  activations), so it is a deliberate choice, not a free win.

### What would help, largest first (estimates until the v3 benchmark measures them)

1. A faster card. The trainer's docs give 175 TFLOP/s measured MMA peak on a 4090 vs 71 on a
   3090 (2.5x); Vast's `dlperf` is 97 vs 44 (2.2x).
2. No block checkpointing (`nockpt-ring16`, 6144 x `GRAD_ACC 2`): up to 1.33x, same numerics.
3. Keep the kernel tuning tables: 11.7 min per rental. They live in
   `~/.cache/kohakumoss_kernels` and `~/.cache/lycoris_kernels`, are keyed by card, CUDA
   and torch version, and v3 fetches them so a later rental on the same card can upload them.
4. `INT8_COMPUTE`: about 1.2x, numerics change.
5. Sampling and exports: leave at the end only.

### v3 plan (benchmark, then `mix`; not yet run)

`km-run/v3` in the session scratchpad: one rental, a 20-step benchmark of three variants on
the mix data (v1 control = the v2 config, v2 = `INT8_COMPUTE`, v3 = `nockpt-ring16` with
6144 x `GRAD_ACC 2`), then the full `mix` LoRA with the fastest eligible variant. v3 is
eligible only if its peak reserved VRAM is at most 20.5 GiB; v2 must beat the best
numerics-preserving variant by 1.2x. The full run's step count is cut to what fits the cap
(multiples of 200, at most 800). Sample only at the last step.

## Vast run v3 on an RTX 4090: benchmark done, `mix` lost to host stops (2026-10-27)

Offer 51842864 (RTX 4090, Quebec, machine 151648, 503 GB RAM, cgroup limit 171 GiB, PCIe
24 GB/s), $0.466/hr all-in. Three launches, all destroyed:

| instance | outcome | cost |
| --- | --- | --- |
| 58018445 | `bench-v1` config rejected (LR warmup 100 > 20 steps); fixed with a 2-step warmup for short runs | ~$0.05 |
| 58019202 | host stopped the container 7 min in (during the text cache); no cause in the logs | ~$0.06 |
| 58021317 | boot took 16 min; benchmark finished; `mix` stopped by the host at step 280 of 800 (52.3 min) | ~$0.41 |

Both stops show `actual_status: exited`, `intended_status: stopped` with RAM, GPU and
disk flat in the 20 s monitor, so a host-side stop, not an OOM of ours. The `mix` step-200
checkpoint existed only on the instance and was lost with it.

### Benchmark (20 steps each, same mix data, tuned kernels)

| variant | s/step (steps 2-10) | tokens/s | vs 3090 (5.72 s) | $ per 1000 steps |
| --- | --- | --- | --- | --- |
| v1 control (`balanced`, int8 weight-only) | 2.82 (3.12 median of rows 10-20) | 2835 | 2.0x | 0.37 |
| v2 `INT8_COMPUTE` | 1.91 | 3812 | 3.0x | 0.25 |
| v3 `nockpt-ring16`, 6144 x `GRAD_ACC 2` | OOM at 23.3 GiB | | | |

- `INT8_COMPUTE` is 1.34x the control on a 4090, past the 1.2x bar, so the script chose it. It
  changes the numerics, so the `mix` run is not a pure A/B against `nl` (trained
  weight-only); say so when comparing.
- No checkpointing does not fit at 6144 tokens per micro-batch even on 24 GB; it would need
  about 3072 x `GRAD_ACC 4`, untested.
- Setup on this host: caches 7.6 min (105 text embeddings; 8 variants collapse to 5 distinct
  per image), kernel tuning 4.4 + 2.7 min for the control, then tuning table hits (variant
  switches cost seconds). A second variant needs 0 to 9 s of tuning.
- Only the first 280 `mix` steps were logged; no `mix` LoRA exists yet.

## Vast run v4: `mix` trained and compared (2026-10-27) ✅

Spain RTX 4090 (offer 51882495, machine 151330), instance 58029400, on-demand, $0.486/hr,
52.1 min, about $0.42, destroyed. 800 steps of `mix` with `INT8_COMPUTE`, exit 0, no skipped or
non-finite steps. Checkpoints 200/400/600/800 are in `D:\ai\km-compare\mix\` (step 200 came
from the continuous sync, which worked: the driver copied each export within about 30 s).

### What went wrong before this, and what fixed it

Five 4090 rentals failed before this one (about $0.78, plus $0.15 earlier in the spike):

| cause | fix |
| --- | --- |
| benchmark config rejected: LR warmup 100 > 20 steps | 2-step warmup for runs under 200 steps |
| three Vast stops (Quebec x2, Oregon) with flat RAM/GPU, `intended_status: stopped` | the driver sent `"price"` in the create call, which makes an **interruptible bid**; removed, and the driver refuses `is_bid` |
| Japan host: CUDA Error 804 (forward-compat libs) with host CUDA 12.6 vs the 12.8 image; driver then hung 16 min on the dead SSH session | offers need host CUDA >= 12.8; CUDA probe right after SSH; 5-min output watchdog; timeouts on post-run ssh/scp |

The bid explanation fits all three stops but was not directly confirmed; on-demand
instances have run without a stop since (`is_bid False`, `min_bid` 0.40 shown by Vast).

### Timeline (52.1 min)

| phase | min |
| --- | --- |
| create to running | 6.9 (boot varies 0.5 to 16 min by host) |
| install, downloads | ~3.8 + 1.4 |
| text and latent caches | 5.0 (105 text embeddings, 21 latents) |
| kernel tuning (first run on the card) | 6.1 (227 s + 140 s) |
| training 800 steps | 26.8 (1.985 s/step median, 4225 tok/s, 150 TFLOP/s, 8.2 GiB peak) |
| fetch and destroy | ~1.2 |

Against the 3090 run: 2.9x faster per step (5.72 to 1.985), same $ per 1000 steps down from
$0.46 to $0.27 at $0.486/hr, and the whole run 52 min vs 106. The kernel tuning tables are
saved at `D:\ai\km-compare\kcache-rtx4090-cu126-torch2.14.1.tgz`; extracting them to
`~/.cache/kohakumoss_kernels` and `~/.cache/lycoris_kernels` on a later 4090 rental should
skip the 6.1 min (not yet tried).

### Loss

Mean loss per window is the same for both runs (mix vs nl): 0.319/0.322 (1-100),
0.328/0.326, 0.338/0.341 (201-400), 0.334/0.333 (401-600), 0.350 (601-800) vs 0.391
(601-710). Flow-matching loss is noisy; it says both train, not which is better.

### Image comparison (ComfyUI, Kromoss int8 + 8-step DMD2, same seeds as `gen_nl.py`)

`D:\ai\km-compare\montage_mix.png`: rows closeup, street, fullbody (sentence prompts), tags,
tags2 (e621 tag prompts; `tags2` is new), columns base, nl 200/400/600, mix 200/400/600/800.
Judged by eye, one seed per cell, so treat as a first look:

- Both LoRAs carry the character (orange-brown skin, large pink-eared head, four
  tendrils, purple eyes, gray cybernetic limbs) onto both sentence and tag prompts; the
  base model gives a different character every time. The `nl`-only LoRA already follows
  tag prompts.
- Closeup: from 400 on, `mix` puts the blue-gray armour and dark top on the shoulders; `nl`
  leaves them bare. Street, tags and tags2: `mix` 400+ draws the four tendrils more reliably,
  and the skin colour is steadier (at step 200 both runs can still drift to gray or blue in
  `tags2`).
- No clear win for either on likeness; `mix` looks more consistent on tendrils and outfit
  across tag and sentence prompts. 800 steps is not visibly better than 600.
- Caveats: `mix` used `INT8_COMPUTE` and `nl` did not; there is one seed per prompt; the
  `nl` run only reached step 710 of its 800-step schedule (its checkpoints are 200/400/600).
  Re-run with more seeds before drawing a conclusion.

ComfyUI (pid 45232, port 8199) was started for this and stopped afterwards. The mix LoRAs
were copied into `C:\ai\ComfyUI\models\loras\` next to the nl ones.

## Local prompt-response check, `nl` 400 vs `mix` 400 (2026-10-27)

40 images, ComfyUI (Kromoss int8 + 8-step DMD2, euler/simple, CFG 1, 832x1216): five scenes
not in the training data (jungle with a rifle, rooftop at night, desert walking away,
spaceship corridor waving, snowy forest in a red hooded cloak), each as a sentence prompt
and a tag prompt, both LoRAs at strength 1.0, two seeds (101, 202). Every prompt starts with
the trigger `roka_soldier` and states the outfit and anatomy (purple sports bra and shorts,
blue-gray shoulder armour, cybernetic right arm and left leg, four tendrils, claws, large
ears, purple eyes); the cloak scene drops the armour. Prompts: `D:\ai\km-compare\prompts_400.json`;
sheets: `D:\ai\km-compare\r400_<scene>.png`; script `gen_400.py`. By eye, 2 seeds, so a first look:

- Both LoRAs hold the character and the outfit in all 40 images: brown skin, tall pink-eared
  head, four tendrils, purple top and shorts, gray metal right arm and left leg (on the
  viewer's left arm and right leg in front views, matching the captions).
- Scene, pose and props follow the prompt: rifle, sitting on a ledge, waving, a sad lowered gaze,
  and a new red hooded cloak that is worn over the unchanged purple outfit. Shoulder armour
  appears when asked and is absent in the cloak scene. Expressions are subtle.
- Tag prompts drift more in the cybernetic colour: pale blue metal on limbs or torso in 5 of
  10 `nl` tag images vs 2 of 10 for `mix`; sentence prompts stayed gray in all 20.
- The `rear view` request was honoured in 4 of 4 `mix` images and 0 of 4 `nl` images (`nl`
  turned to a front or three-quarter view). Rear views put the gray arm and leg on the same
  screen sides as the front views, so the sides follow the screen, not the anatomy.
- Limits: two seeds, judged by eye, `mix` used `INT8_COMPUTE`. Not tested: LoRA strength below
  1.0, other characters mixed in, text prompts without the trigger.
