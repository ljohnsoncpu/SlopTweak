from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import pytest

import moss_workflow as w

UNET = "moss_image_base_bf16.safetensors"
CLIP = "qwen3.5_9b_int8_convrot.safetensors"
VAE = "flux2-vae.safetensors"
LORA = "moss-image-8-step-dmd-lora.safetensors"
TIME = "moss-image-8-step-dmd-time-embedding.safetensors"
DELTA = "moss-image-8-step-dmd-time-delta.safetensors"

LINES = "\n".join(
    "\t".join([f"https://x/{n}", "0" * 64, "1", n, "0", dest])
    for n, dest in [
        (UNET, "diffusion_models"),
        (CLIP, "text_encoders"),
        (VAE, "vae"),
        (LORA, "loras"),
        (TIME, "loras"),
    ]
)


def _wf(fast: bool) -> dict[str, Any]:
    return w.build(UNET, CLIP, VAE, LORA, DELTA, fast)


def _by_type(wf: dict[str, Any], t: str) -> list[dict[str, Any]]:
    return [n for n in wf["nodes"] if n["type"] == t]


@pytest.mark.parametrize("fast", [True, False])
def test_links_are_consistent(fast: bool) -> None:
    wf = _wf(fast)
    nodes = {n["id"]: n for n in wf["nodes"]}
    assert len(nodes) == len(wf["nodes"])
    assert wf["last_node_id"] == max(nodes) and wf["last_link_id"] == len(wf["links"])
    for link_id, src, src_slot, dst, dst_slot, type_ in wf["links"]:
        assert nodes[src]["outputs"][src_slot]["type"] == type_
        assert nodes[dst]["inputs"][dst_slot]["type"] == type_
        assert nodes[dst]["inputs"][dst_slot]["link"] == link_id
        assert link_id in nodes[src]["outputs"][src_slot]["links"]
    # Every connected input and output slot is in the link list, nothing dangles.
    ids = {link[0] for link in wf["links"]}
    for n in nodes.values():
        for i in n["inputs"]:
            assert i["link"] is None or i["link"] in ids
        for o in n["outputs"]:
            assert set(o["links"]) <= ids
    # A node that needs an input has it connected: only the loaders and latent have none.
    for n in nodes.values():
        if n["type"] not in {
            "Note",
            "UNETLoader",
            "CLIPLoader",
            "VAELoader",
            "EmptyFlux2LatentImage",
        }:
            assert all(i["link"] is not None for i in n["inputs"]), n["type"]


def test_fast_chain_runs_through_the_lora_and_the_patch() -> None:
    wf = _wf(True)
    (sampler,) = _by_type(wf, "KSampler")
    loras = _by_type(wf, "LoraLoaderModelOnly")
    assert [n["widgets_values"][0] for n in loras] == [LORA, DELTA]
    nodes = {n["id"]: n for n in wf["nodes"]}
    links = {link[0]: link for link in wf["links"]}
    model_in = next(i for i in sampler["inputs"] if i["name"] == "model")
    assert nodes[links[model_in["link"]][1]]["widgets_values"][0] == DELTA
    # KSampler widgets: seed, control_after_generate, steps, cfg, sampler, scheduler, denoise.
    assert sampler["widgets_values"] == [1234, "randomize", 8, 1.0, "euler", "simple", 1.0]


def test_full_quality_has_no_distillation_and_skips_layer_22() -> None:
    wf = _wf(False)
    assert not _by_type(wf, "LoraLoaderModelOnly")
    (slg,) = _by_type(wf, "SkipLayerGuidanceDiTSimple")
    assert slg["widgets_values"] == ["", "22", 0.0, 1.0]
    (sampler,) = _by_type(wf, "KSampler")
    assert sampler["widgets_values"][2:4] == [40, 3.0]
    nodes = {n["id"]: n for n in wf["nodes"]}
    links = {link[0]: link for link in wf["links"]}
    model_in = next(i for i in sampler["inputs"] if i["name"] == "model")
    assert nodes[links[model_in["link"]][1]]["type"] == "SkipLayerGuidanceDiTSimple"
    # The skip node takes the base model, and the Fast graph has none.
    slg_in = next(i for i in slg["inputs"] if i["name"] == "model")
    assert nodes[links[slg_in["link"]][1]]["type"] == "UNETLoader"
    assert not _by_type(_wf(True), "SkipLayerGuidanceDiTSimple")


def test_loaders_use_the_placed_files_and_moss_clip_type() -> None:
    wf = _wf(True)
    assert _by_type(wf, "UNETLoader")[0]["widgets_values"] == [UNET, "default"]
    assert _by_type(wf, "CLIPLoader")[0]["widgets_values"] == [CLIP, "moss_image", "default"]
    assert _by_type(wf, "VAELoader")[0]["widgets_values"] == [VAE]
    assert _by_type(wf, "EmptyFlux2LatentImage")[0]["widgets_values"] == [1024, 1024, 1]


def test_workflow_is_deterministic_and_json() -> None:
    assert json.dumps(_wf(True)) == json.dumps(_wf(True))
    assert _wf(True)["id"] != _wf(False)["id"]


def test_names_come_from_the_model_lines() -> None:
    assert w.names_from_model_lines(LINES, TIME) == {
        "unet": UNET,
        "clip": CLIP,
        "vae": VAE,
        "lora": LORA,
    }
    with pytest.raises(ValueError, match="missing"):
        w.names_from_model_lines("", TIME)


def test_fast_chain_without_a_delta_is_just_the_lora() -> None:
    wf = w.build(UNET, CLIP, VAE, LORA, None, True)
    assert [n["widgets_values"][0] for n in _by_type(wf, "LoraLoaderModelOnly")] == [LORA]
    (sampler,) = _by_type(wf, "KSampler")
    nodes = {n["id"]: n for n in wf["nodes"]}
    links = {link[0]: link for link in wf["links"]}
    model_in = next(i for i in sampler["inputs"] if i["name"] == "model")
    assert nodes[links[model_in["link"]][1]]["widgets_values"][0] == LORA
    # Full quality has no LoRA either way.
    assert not _by_type(w.build(UNET, CLIP, VAE, LORA, None, False), "LoraLoaderModelOnly")


def test_kromoss_lines_write_workflows_with_no_patch(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    lines = "\n".join(
        "\t".join([f"https://x/{n}", "0" * 64, "1", n, "0", dest])
        for n, dest in [
            ("kromoss_bf16.safetensors", "diffusion_models"),
            (CLIP, "text_encoders"),
            ("flux2-vae_small.safetensors", "vae"),
            ("kromoss-8-steps-dmd2-lora.safetensors", "loras"),
        ]
    )
    monkeypatch.setenv("MODEL_LINES", lines)
    assert w.main(["x", str(tmp_path / "wf"), DELTA, TIME]) == 0
    fast = json.loads((tmp_path / "wf" / w.FAST_TITLE).read_text(encoding="utf-8"))
    names = [n["widgets_values"][0] for n in _by_type(fast, "LoraLoaderModelOnly")]
    assert names == ["kromoss-8-steps-dmd2-lora.safetensors"]
    assert DELTA not in json.dumps(fast)


def test_write_makes_both_files(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("MODEL_LINES", LINES)
    assert w.main(["x", str(tmp_path / "wf"), DELTA, TIME]) == 0
    files = sorted(p.name for p in (tmp_path / "wf").iterdir())
    assert files == sorted([w.FAST_TITLE, w.FULL_TITLE])
    for f in (tmp_path / "wf").iterdir():
        assert json.loads(f.read_text(encoding="utf-8"))["nodes"]
