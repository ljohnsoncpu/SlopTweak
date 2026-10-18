"""Turn the MOSS-Image 8-step distillation's time-embedding file into a ComfyUI patch file.

lodestones' ``time_embedding.safetensors`` holds full replacement tensors for the
step embedder and two modulation biases. ComfyUI's stock LoRA loader can't take that,
but it does understand ``<module>.set_weight`` (replace a weight) and
``<module>.diff_b`` (add to a bias), so this writes those, to be loaded as a second
``LoraLoaderModelOnly`` after the distillation LoRA.

Runs on the instance with ComfyUI's python (torch and safetensors are already there);
it reads six small tensors from the base model and does not install anything.
Usage: moss_time_delta.py BASE.safetensors TIME_EMBEDDING.safetensors OUT.safetensors
"""

from __future__ import annotations

import sys
from pathlib import Path

import torch
from safetensors import safe_open
from safetensors.torch import save_file

PREFIX = "diffusion_model."
# Replaced outright (the module's weight changes).
SET_WEIGHTS = ("time_embedding.0", "time_embedding.2")
# Shifted by (new - base) (only the module's bias changes).
DIFF_BIASES = (
    "time_embedding.0",
    "time_embedding.2",
    "shared_block_modulation",
    "video_head_modulation",
)
EXPECTED = {f"{m}.weight" for m in SET_WEIGHTS} | {f"{m}.bias" for m in DIFF_BIASES}


def build_delta(base: Path, time_embedding: Path, out: Path) -> dict[str, torch.Tensor]:
    """Write the patch file to ``out`` and return its tensors."""
    with safe_open(str(time_embedding), framework="pt") as f:
        new = {k: f.get_tensor(k).to(torch.float32) for k in f.keys()}
    if set(new) != EXPECTED:
        raise ValueError(
            f"unexpected tensors in {time_embedding.name}: {sorted(set(new) ^ EXPECTED)}"
        )
    patch: dict[str, torch.Tensor] = {}
    with safe_open(str(base), framework="pt") as f:
        for module in SET_WEIGHTS:
            patch[f"{PREFIX}{module}.set_weight"] = new[f"{module}.weight"].contiguous()
        for module in DIFF_BIASES:
            old = f.get_tensor(f"{module}.bias").to(torch.float32)
            current = new[f"{module}.bias"]
            if old.shape != current.shape:
                raise ValueError(
                    f"{module}.bias: shape {tuple(current.shape)} != {tuple(old.shape)}"
                )
            patch[f"{PREFIX}{module}.diff_b"] = (current - old).contiguous()
    save_file(patch, str(out))
    return patch


def main(argv: list[str]) -> int:
    if len(argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    patch = build_delta(Path(argv[1]), Path(argv[2]), Path(argv[3]))
    print(f"wrote {len(patch)} tensors to {argv[3]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
