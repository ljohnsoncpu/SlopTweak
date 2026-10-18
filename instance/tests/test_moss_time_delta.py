from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING

import pytest

if TYPE_CHECKING:
    import torch
    from safetensors import torch as safetensors_torch
else:  # the dev group has no torch; these tests run where it is installed
    torch = pytest.importorskip("torch")
    safetensors_torch = pytest.importorskip("safetensors.torch")

import moss_time_delta as m


def _tensors(seed: int, dtype: torch.dtype) -> dict[str, torch.Tensor]:
    g = torch.Generator().manual_seed(seed)
    shapes = {
        "time_embedding.0.weight": (6, 4),
        "time_embedding.0.bias": (6,),
        "time_embedding.2.weight": (6, 6),
        "time_embedding.2.bias": (6,),
        "shared_block_modulation.bias": (36,),
        "video_head_modulation.bias": (12,),
    }
    return {k: torch.randn(s, generator=g).to(dtype) for k, s in shapes.items()}


def test_delta_has_set_weights_and_bias_diffs(tmp_path: Path) -> None:
    base = _tensors(1, torch.bfloat16)
    # The base also holds tensors the patch must ignore.
    base["layers.0.proj.o.weight"] = torch.zeros(2, 2, dtype=torch.bfloat16)
    base["shared_block_modulation.weight"] = torch.zeros(36, 6, dtype=torch.bfloat16)
    new = _tensors(2, torch.float32)
    safetensors_torch.save_file(base, str(tmp_path / "base.safetensors"))
    safetensors_torch.save_file(new, str(tmp_path / "time.safetensors"))
    out = tmp_path / "delta.safetensors"

    patch = m.build_delta(tmp_path / "base.safetensors", tmp_path / "time.safetensors", out)

    assert set(patch) == {
        "diffusion_model.time_embedding.0.set_weight",
        "diffusion_model.time_embedding.2.set_weight",
        "diffusion_model.time_embedding.0.diff_b",
        "diffusion_model.time_embedding.2.diff_b",
        "diffusion_model.shared_block_modulation.diff_b",
        "diffusion_model.video_head_modulation.diff_b",
    }
    assert set(safetensors_torch.load_file(str(out))) == set(patch)
    assert all(t.dtype == torch.float32 for t in patch.values())
    assert torch.equal(
        patch["diffusion_model.time_embedding.0.set_weight"], new["time_embedding.0.weight"]
    )
    # Adding the diff to the base bias gives back the replacement bias.
    for module in ("time_embedding.0", "shared_block_modulation", "video_head_modulation"):
        got = base[f"{module}.bias"].float() + patch[f"diffusion_model.{module}.diff_b"]
        assert torch.allclose(got, new[f"{module}.bias"])


def test_unexpected_tensors_are_refused(tmp_path: Path) -> None:
    new = _tensors(2, torch.float32)
    new["extra.weight"] = torch.zeros(1)
    safetensors_torch.save_file(_tensors(1, torch.bfloat16), str(tmp_path / "base.safetensors"))
    safetensors_torch.save_file(new, str(tmp_path / "time.safetensors"))
    with pytest.raises(ValueError, match="unexpected tensors"):
        m.build_delta(
            tmp_path / "base.safetensors",
            tmp_path / "time.safetensors",
            tmp_path / "out.safetensors",
        )


def test_shape_mismatch_is_refused(tmp_path: Path) -> None:
    base = _tensors(1, torch.bfloat16)
    base["video_head_modulation.bias"] = torch.zeros(7, dtype=torch.bfloat16)
    safetensors_torch.save_file(base, str(tmp_path / "base.safetensors"))
    safetensors_torch.save_file(_tensors(2, torch.float32), str(tmp_path / "time.safetensors"))
    with pytest.raises(ValueError, match="shape"):
        m.build_delta(
            tmp_path / "base.safetensors",
            tmp_path / "time.safetensors",
            tmp_path / "out.safetensors",
        )
