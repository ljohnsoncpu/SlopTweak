# Krea 2 edit spike — session prompt

Paste everything below the line into a new session in this repo.

---

You are running a **spike** for SlopTweak: can we do reference-based image
editing and multi-character composition on the Krea 2 family (Kroma / Wulver
fine-tunes) using ComfyUI, and what would it take to add ComfyUI as a second
backend next to InvokeAI?

Read first: `CLAUDE.md`, `PLAN.md` (§1 non-goals, §2 architecture, §9), and
`docs/findings.md` (Krea 2 / Kroma / Wulver sections). Follow the CLAUDE.md
rules on money, secrets, and git. Do not edit PLAN.md; put concerns in
`docs/findings.md`.

## Why

- Invoke 6.14 lists reference images as unsupported for Krea-2, so Krea-based
  editing means ComfyUI.
- Qwen Image Edit is judged too censored and weak on furry content; FLUX.2
  Klein is not the target either. The target is Krea-family models.
- We want one edit LoRA with identity preservation plus multi-character
  composition, run on the **Ostris node pack**. The existing Identity Edit LoRA
  is only a quality benchmark (its nodes are incompatible with Ostris-style
  LoRAs; do not try to stack or mix them).

## Facts to verify, not assume (all from web research, untested)

- `ostris/ComfyUI-Krea2-Ostris-Edit` (MIT): "Text Encode Krea 2 Ostris Edit"
  takes up to 3 reference images through Krea's Qwen3-VL encoder, and with a
  VAE connected also encodes them as reference latents. Needs a text encoder
  checkpoint that includes the Qwen3-VL **vision** weights. Check whether
  `Comfy-Org/Krea-2` `qwen3vl_4b_*` files have them.
- LoRAs must be trained with ai-toolkit (`arch: krea2`,
  `model_kwargs.edit: true`). Same weights are said to work on Turbo and Raw.
  It is **unknown** whether they transfer to Kroma v0.3 / Wulver v0.5 Turbo.
- `lbouaraba/comfyui-krea2edit` + `conradlocke/krea2-identity-edit` v1.2
  (Krea 2 Community License; SFW-trained): benchmark only. Pin its commit.
- Whether any public Ostris-format edit LoRA exists is unknown. Finding one,
  or deciding we must train one, is part of Phase A.

## Phase A — research, no spend

1. Pin versions: ComfyUI release with native Krea 2, both node packs (commit
   SHAs), ai-toolkit commit. Record them in `docs/findings.md`.
2. Find candidate edit LoRAs in ai-toolkit/Ostris format (Hugging Face,
   CivitAI). Record name, rank, license, training data notes. If none exist,
   say so and move the decision to the user.
3. Read the Ostris node source and the ai-toolkit edit config. Write down, in
   findings, how reference latents and Qwen3-VL image tokens are injected, so
   we know what a training dataset must look like (pair format, multiple
   references, resolution).
4. Check the Krea 2 Community License and Kroma/Wulver terms for the intended
   use (including renting a GPU, redistribution of merged weights).
5. Look at the `manage-creative-vast` skill and its CLI as prior art for
   provisioning ComfyUI on Vast. Verify before reusing anything; it must still
   satisfy our rules (destroy on every exit path, instance id printed first,
   binds 127.0.0.1, keys in env only).
6. Draft the Phase B run sheet: GPU, $/hr, expected duration, hard cap,
   download sizes, exact steps, test cases, pass/fail criteria.

**Stop after Phase A and present the run sheet. Do not rent anything yet.**

## Phase B — inference spike (needs explicit per-action approval)

Ask for approval in chat with: GPU model, $/hr, expected duration, cap. Likely
a 24 GB card with fp8 weights, or 48 GB if bf16 Kroma is needed. Any script
that creates an instance prints the id first and destroys it on every exit
path (trap/finally); verify the instance is gone at the end.

Test matrix (use SFW test images; the user judges content quality themselves
and will supply their own character sheets later):

| # | Case | Models |
| --- | --- | --- |
| 1 | Ostris nodes load and run | Krea 2 Turbo, stock |
| 2 | Single-image edit with a candidate edit LoRA | Krea 2 Turbo |
| 3 | Same LoRA and prompt | Kroma v0.3 turbo (does it transfer?) |
| 4 | Same LoRA and prompt | Wulver v0.5 turbo (if time) |
| 5 | Two characters from two reference sheets into one scene | best of 2-4 |
| 6 | Benchmark: Identity Edit LoRA on its own node pack | Krea 2 Turbo |

Record for each: load time, peak VRAM, seconds per image, whether identity
holds, whether the edit instruction is followed, and failure modes. Save
outputs locally under the scratchpad, not in the repo. Redact keys and tokens
in anything pasted into docs.

If no usable Ostris-format edit LoRA exists, do not train one in this phase.
Report that result and what a training run would need.

## Phase C — only if the user approves separately

A minimal ai-toolkit edit-LoRA training run on a tiny paired dataset (~1,750
steps is the reported scale for one concept) to prove the pipeline end to end
on Kroma. Separate cost approval, same destroy-on-exit rules. Out of scope for
the first session unless the user says so.

## Deliverables

1. `docs/findings.md` section "Krea 2 edit spike": pinned versions, what was
   verified (✅), what differs from assumptions (⚠️), what is untested (🧪),
   spend and credit before/after.
2. A short recommendation covering:
   - whether the Ostris path works on Kroma/Wulver;
   - what a ComfyUI backend needs in the launcher (image, upstream port,
     provisioner, output adapter, catalog `backend` field), kept to a sketch;
   - the training dataset and effort estimate for the custom edit LoRA;
   - open licensing questions.
3. No code changes to `app/` or `instance/`, no commits, unless the user asks.

Ask, don't assume: anything that spends money, and the content policy
question (PLAN §9.3). Keep characters adult in any test material.
