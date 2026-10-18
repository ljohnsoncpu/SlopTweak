"""Write ComfyUI workflows for the MOSS-Image model into ComfyUI's saved-workflows folder.

"Open ComfyUI (advanced)" shows ComfyUI's own editor, which starts empty. Identity Edit
ships a workflow from its node pack; for MOSS-Image this writes two, so the Workflows
sidebar has something to open: **Fast** (the 8-step distillation: base model, the LoRA,
the time-embedding patch, CFG 1) and **Full quality** (the base model alone, CFG 3).
They mirror the graph the SlopTweak panel runs (app/src-tauri/src/moss.rs).

Standard library only. Usage (env MODEL_LINES is provision.sh's tab-separated model list):
    moss_workflow.py WORKFLOWS_DIR TIME_DELTA_NAME TIME_EMBEDDING_NAME
"""

from __future__ import annotations

import json
import os
import sys
import uuid
from pathlib import Path
from typing import Any

FAST_TITLE = "SlopTweak MOSS-Image Fast.json"
FULL_TITLE = "SlopTweak MOSS-Image Full quality.json"

FAST_NOTE = (
    "MOSS-Image, Fast: lodestones' 8-step distillation (work in progress).\n"
    "Base model + distillation LoRA + time-embedding patch. Sample with euler / simple, CFG 1.\n"
    "Trained for 4 to 8 steps. Sizes: multiples of 16."
)
FULL_NOTE = (
    "MOSS-Image, Full quality: the base model alone.\n"
    "Sample with euler / simple, CFG 3, 30 to 50 steps (the card uses 50). Layer 22 is skipped\n"
    "in the negative branch, as in the official workflow. Sizes: multiples of 16."
)

Node = dict[str, Any]


class _Graph:
    """Builds a ComfyUI UI-format workflow: nodes, and the links between them."""

    def __init__(self) -> None:
        self.nodes: list[Node] = []
        self.links: list[list[Any]] = []

    def add(
        self,
        node_type: str,
        pos: tuple[int, int],
        size: tuple[int, int],
        inputs: list[tuple[str, str, tuple[int, int] | None]],
        outputs: list[tuple[str, str]],
        widgets: list[Any],
        title: str | None = None,
    ) -> int:
        """Add a node. Each input is (name, type, (origin node, origin slot) or None)."""
        node_id = len(self.nodes) + 1
        node: Node = {
            "id": node_id,
            "type": node_type,
            "pos": list(pos),
            "size": list(size),
            "flags": {},
            "order": len(self.nodes),
            "mode": 0,
            "inputs": [],
            "outputs": [
                {"name": n, "type": t, "links": [], "slot_index": i}
                for i, (n, t) in enumerate(outputs)
            ],
            "properties": {"Node name for S&R": node_type},
            "widgets_values": widgets,
        }
        if title:
            node["title"] = title
        for slot, (name, type_, origin) in enumerate(inputs):
            entry: dict[str, Any] = {"name": name, "type": type_, "link": None}
            if origin is not None:
                link_id = len(self.links) + 1
                src = self.nodes[origin[0] - 1]
                self.links.append([link_id, origin[0], origin[1], node_id, slot, type_])
                src["outputs"][origin[1]]["links"].append(link_id)
                entry["link"] = link_id
            node["inputs"].append(entry)
        self.nodes.append(node)
        return node_id

    def dump(self, name: str) -> dict[str, Any]:
        return {
            "id": str(uuid.uuid5(uuid.NAMESPACE_URL, "sloptweak/" + name)),
            "revision": 0,
            "last_node_id": len(self.nodes),
            "last_link_id": len(self.links),
            "nodes": self.nodes,
            "links": self.links,
            "groups": [],
            "config": {},
            "extra": {"ds": {"scale": 0.8, "offset": [20, 20]}},
            "version": 0.4,
        }


def build(unet: str, clip: str, vae: str, lora: str, delta: str, fast: bool) -> dict[str, Any]:
    """The workflow for one mode. File names are the ones placed in ComfyUI's model folders."""
    g = _Graph()
    note = g.add(
        "Note", (20, -150), (520, 120), [], [], [FAST_NOTE if fast else FULL_NOTE], title="Read me"
    )
    assert note == 1
    loader = g.add("UNETLoader", (20, 20), (330, 82), [], [("MODEL", "MODEL")], [unet, "default"])
    te = g.add(
        "CLIPLoader", (20, 140), (330, 106), [], [("CLIP", "CLIP")], [clip, "moss_image", "default"]
    )
    vae_node = g.add("VAELoader", (20, 290), (330, 58), [], [("VAE", "VAE")], [vae])
    model: tuple[int, int] = (loader, 0)
    if fast:
        lora_node = g.add(
            "LoraLoaderModelOnly",
            (380, 20),
            (330, 82),
            [("model", "MODEL", model)],
            [("MODEL", "MODEL")],
            [lora, 1.0],
            title="Distillation LoRA",
        )
        patch = g.add(
            "LoraLoaderModelOnly",
            (380, 140),
            (330, 82),
            [("model", "MODEL", (lora_node, 0))],
            [("MODEL", "MODEL")],
            [delta, 1.0],
            title="Time-embedding patch",
        )
        model = (patch, 0)
    else:
        skip = g.add(
            "SkipLayerGuidanceDiTSimple",
            (380, 20),
            (330, 130),
            [("model", "MODEL", model)],
            [("MODEL", "MODEL")],
            ["", "22", 0.0, 1.0],
            title="Skip layer 22 in the negative",
        )
        model = (skip, 0)
    positive = g.add(
        "CLIPTextEncode",
        (380, 270),
        (400, 160),
        [("clip", "CLIP", (te, 0))],
        [("CONDITIONING", "CONDITIONING")],
        ["A red fox sitting in a snowy forest at dusk, soft light, detailed fur."],
        title="Prompt",
    )
    negative = g.add(
        "CLIPTextEncode",
        (380, 460),
        (400, 100),
        [("clip", "CLIP", (te, 0))],
        [("CONDITIONING", "CONDITIONING")],
        [""],
        title="Negative prompt (unused at CFG 1)",
    )
    latent = g.add(
        "EmptyFlux2LatentImage",
        (20, 400),
        (330, 106),
        [],
        [("LATENT", "LATENT")],
        [1024, 1024, 1],
    )
    steps, cfg = (8, 1.0) if fast else (40, 3.0)
    sampler = g.add(
        "KSampler",
        (820, 20),
        (330, 262),
        [
            ("model", "MODEL", model),
            ("positive", "CONDITIONING", (positive, 0)),
            ("negative", "CONDITIONING", (negative, 0)),
            ("latent_image", "LATENT", (latent, 0)),
        ],
        [("LATENT", "LATENT")],
        [1234, "randomize", steps, cfg, "euler", "simple", 1.0],
    )
    decode = g.add(
        "VAEDecode",
        (820, 330),
        (230, 46),
        [("samples", "LATENT", (sampler, 0)), ("vae", "VAE", (vae_node, 0))],
        [("IMAGE", "IMAGE")],
        [],
    )
    g.add(
        "SaveImage",
        (1080, 330),
        (330, 270),
        [("images", "IMAGE", (decode, 0))],
        [],
        ["SlopTweak"],
    )
    return g.dump(FAST_TITLE if fast else FULL_TITLE)


def names_from_model_lines(lines: str, time_embedding: str) -> dict[str, str]:
    """Pick each model's file name by its ComfyUI folder (provision.sh's MODEL_LINES:
    url, sha256, size, filename, needs_token, dest; tab-separated)."""
    found: dict[str, str] = {}
    for line in lines.splitlines():
        parts = line.split("\t")
        if len(parts) < 6:
            continue
        name, dest = parts[3], parts[5]
        if name == time_embedding:
            continue  # only the patch's source, not a LoRA
        key = {
            "diffusion_models": "unet",
            "text_encoders": "clip",
            "vae": "vae",
            "loras": "lora",
        }.get(dest)
        if key:
            found[key] = name
    missing = {"unet", "clip", "vae", "lora"} - found.keys()
    if missing:
        raise ValueError(f"missing model files: {sorted(missing)}")
    return found


def write(out_dir: Path, names: dict[str, str], delta: str) -> list[Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    written = []
    for fast, title in ((True, FAST_TITLE), (False, FULL_TITLE)):
        wf = build(names["unet"], names["clip"], names["vae"], names["lora"], delta, fast)
        path = out_dir / title
        path.write_text(json.dumps(wf, indent=1), encoding="utf-8")
        written.append(path)
    return written


def main(argv: list[str]) -> int:
    if len(argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    names = names_from_model_lines(os.environ.get("MODEL_LINES", ""), argv[3])
    for path in write(Path(argv[1]), names, argv[2]):
        print(f"wrote {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
