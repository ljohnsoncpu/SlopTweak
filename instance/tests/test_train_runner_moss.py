"""The Kromoss (KohakuMossTrainer) path of train_runner.py, with subprocesses faked."""

from __future__ import annotations

import json
import os
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import Any

import pytest
from test_train_runner import CLOCK, make_tar, sha

import train_runner as tr

STEPS = (2, 4)
COMMIT = "b" * 40


class MossRig:
    def __init__(self, tmp: Path) -> None:
        self.work = tmp / "train"
        self.models = tmp / "models"
        self.assets = tmp / "assets"
        for d in (self.work / "incoming", self.models, self.assets):
            d.mkdir(parents=True)
        self.paths = tr.Paths(self.work)
        self.commands: list[list[str]] = []
        self.envs: list[Mapping[str, str]] = []
        self.fail_on: str | None = None
        self.cancel_on: str | None = None
        self.weights = {
            "kromoss.safetensors": (b"dit", "diffusion_models/kromoss_int8_convrot.safetensors"),
            "qwen.safetensors": (b"te", "text_encoders/qwen3.5_9b_int8_convrot.safetensors"),
        }
        for name, (data, _dest) in self.weights.items():
            (self.models / name).write_bytes(data)
        self.dataset = {
            "a.png": b"png-a",
            "a.nl.txt": b"roka, a woman.",
            "a.e621.txt": b"roka, solo",
        }
        make_tar(self.paths.incoming / "dataset.tar", self.dataset)
        self.trainer_src = tmp / "trainer-src.tar.gz"
        make_tar(self.trainer_src, {"README.md": b"# trainer"}, top="KohakuMossTrainer-abc")
        self.job: dict[str, Any] = {
            "trainer": "kohakumoss",
            "output_name": "lora",
            "trainer_commit": COMMIT,
            "trainer_tree_sha256": tr.tree_digest(self.trainer_src),
            "models": [
                {"file": n, "dest": dest, "sha256": sha(data)}
                for n, (data, dest) in self.weights.items()
            ],
            "files": [{"file": n, "sha256": sha(d)} for n, d in self.dataset.items()],
            "total_steps": STEPS[-1],
            "checkpoint_steps": list(STEPS),
            "run_config": "STEPS = 4\n",
            "data_config": "use_config('x')\n",
            "train_jsonl": '{"image": "a.png"}\n',
        }
        self.write_job()
        self.runner = tr.MossRunner(
            self.paths,
            self.assets,
            self.models,
            executor=self.fake,
            fetch_trainer=self.fetch,
            clock=lambda: CLOCK,
            sleep=lambda _s: None,
        )

    def write_job(self) -> None:
        (self.paths.incoming / "job.json").write_text(json.dumps(self.job))

    def fetch(self, dest: Path, commit: str) -> Path:
        assert commit == COMMIT
        dest.write_bytes(self.trainer_src.read_bytes())
        return dest

    @property
    def run_dir(self) -> Path:
        return self.paths.trainer / "runs" / "moss" / "lora"

    def export(self, step: int) -> None:
        path = (
            self.run_dir / "checkpoints" / f"step_{step:08d}" / "export" / "lora-comfy.safetensors"
        )
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"w%d" % step)
        os.utime(path, (0, 0))

    def fake(
        self,
        cmd: Sequence[str],
        cwd: Path,
        log: Path,
        env: Mapping[str, str],
        on_tick: Callable[[], None],
    ) -> int:
        self.commands.append(list(cmd))
        self.envs.append(env)
        joined = " ".join(cmd)
        log.parent.mkdir(parents=True, exist_ok=True)
        log.write_text("log\n")
        if self.fail_on and self.fail_on in joined:
            return 3
        if "scripts/train/moss.py" in joined:
            assert cwd == self.paths.trainer
            self.run_dir.mkdir(parents=True, exist_ok=True)
            (self.run_dir / "metrics.jsonl").write_text(
                '{"step": 1, "loss": 0.34, "step1_wall": 17.5}\n{"step": 2, "loss": 0.30}\n'
            )
            self.export(STEPS[0])
            on_tick()
            if self.cancel_on == "mid-train":
                return 0
            self.export(STEPS[1])
        return 0


@pytest.fixture
def rig(tmp_path: Path) -> MossRig:
    return MossRig(tmp_path)


def state(rig: MossRig) -> dict[str, Any]:
    data: dict[str, Any] = json.loads(rig.paths.state.read_text())
    return data


def manifest_paths(rig: MossRig) -> set[str]:
    listing = json.loads(rig.paths.manifest.read_text())["files"]
    return {f["path"] for f in listing}


def test_full_run(rig: MossRig) -> None:
    rig.runner.execute()
    assert state(rig)["stage"] == "done", state(rig)

    names = [Path(c[1] if c[0].endswith("python") else c[0]).name for c in rig.commands]
    joined = [" ".join(c) for c in rig.commands]
    # Order: tooling, python, venv, torch, trainer deps, CUDA probe, doctor, config
    # check, text cache, latent cache, train.
    idx = {
        key: next(i for i, j in enumerate(joined) if key in j)
        for key in (
            "torch==2.14.1",
            "[cuda]",
            "torch.cuda.is_available",
            "kohakumoss-config",
            "cache_text.py",
            "cache_latents.py",
            "scripts/train/moss.py",
        )
    }
    assert list(idx.values()) == sorted(idx.values()), (idx, names)
    torch_cmd = next(j for j in joined if "torch==2.14.1" in j)
    assert "--index-url https://download.pytorch.org/whl/cu126" in torch_cmd

    # Files laid out for the trainer.
    trainer = rig.paths.trainer
    assert (trainer / "configs/user/sloptweak-train.py").read_text() == "STEPS = 4\n"
    assert (trainer / "configs/user/sloptweak-train_data.py").exists()
    weights = rig.work / "models"
    dit = weights / "diffusion_models/kromoss_int8_convrot.safetensors"
    assert dit.read_bytes() == b"dit"
    assert (rig.paths.data / "nl/a.e621.txt").read_bytes() == b"roka, solo"
    assert (rig.paths.data / "mix/train.jsonl").read_text() == '{"image": "a.png"}\n'

    # Results are exposed with the launcher's checkpoint names.
    assert manifest_paths(rig) >= {
        "checkpoints/lora-step00000002.safetensors",
        "checkpoints/lora-step00000004.safetensors",
        "metrics.jsonl",
    }
    # Hugging Face lookups are allowed and the venv is on PATH.
    train_env = rig.envs[joined.index(next(j for j in joined if "scripts/train/moss.py" in j))]
    assert train_env["HF_HUB_OFFLINE"] == "0"
    assert train_env["PATH"].startswith(str(rig.paths.venv / "bin"))


def test_progress_comes_from_metrics(rig: MossRig) -> None:
    seen: list[dict[str, Any]] = []
    original = rig.runner.set_state

    def spy(stage: str, **extra: Any) -> None:
        original(stage, **extra)
        seen.append({"stage": stage, **extra})

    rig.runner.set_state = spy  # type: ignore[method-assign]
    rig.runner.execute()
    assert {"stage": "train", "step": 2, "total": 4, "loss": 0.30} in seen
    assert any(s.get("detail") == "caching" for s in seen)


def test_wrong_trainer_digest_fails(rig: MossRig) -> None:
    rig.job["trainer_tree_sha256"] = "0" * 64
    rig.write_job()
    rig.runner.execute()
    assert state(rig)["stage"] == "failed"
    assert "pinned digest" in state(rig)["error"]
    assert not any("scripts/train/moss.py" in " ".join(c) for c in rig.commands)


def test_bad_model_checksum_fails_before_training(rig: MossRig) -> None:
    (rig.models / "kromoss.safetensors").write_bytes(b"tampered")
    rig.runner.execute()
    assert state(rig)["stage"] == "failed"
    assert "kromoss.safetensors" in state(rig)["error"]
    assert not any("cache_text.py" in " ".join(c) for c in rig.commands)


def test_cuda_probe_fails_before_the_caches(rig: MossRig) -> None:
    rig.fail_on = "torch.cuda.is_available"
    rig.runner.execute()
    assert state(rig)["stage"] == "failed"
    assert "bootstrap-cuda.log" in state(rig)["error"]
    assert not any("cache_text.py" in " ".join(c) for c in rig.commands)


def test_doctor_failure_is_not_fatal(rig: MossRig) -> None:
    rig.fail_on = "kohakumoss-doctor"
    rig.runner.execute()
    assert state(rig)["stage"] == "done"


def test_training_failure_keeps_finished_checkpoints(rig: MossRig) -> None:
    rig.cancel_on = "mid-train"  # the fake stops after the first export...
    rig.runner.execute()
    # ...so the final checkpoint is missing: failed, but step 2 is still served.
    assert state(rig)["stage"] == "failed"
    assert "final checkpoint" in state(rig)["error"]
    assert "checkpoints/lora-step00000002.safetensors" in manifest_paths(rig)


def test_cancel_marker_stops_the_run(rig: MossRig) -> None:
    (rig.paths.incoming / "cancel").write_text("")
    rig.runner.execute()
    assert state(rig)["stage"] == "cancelled"
    assert not any("scripts/train/moss.py" in " ".join(c) for c in rig.commands)


def test_unexpected_dataset_entry_is_refused(rig: MossRig) -> None:
    make_tar(rig.paths.incoming / "dataset.tar", {**rig.dataset, "evil.png": b"x"})
    rig.runner.execute()
    assert state(rig)["stage"] == "failed"
    assert "unexpected entry" in state(rig)["error"]


def test_progress_parser_skips_cut_lines() -> None:
    tail = 'ep": 1, "loss": 9}\n{"step": 10, "loss": 0.25}\n{"step": 20, "loss": 0.2'
    assert tr.parse_moss_progress(tail) == (10, 0.25)
    assert tr.parse_moss_progress("") is None
    assert tr.parse_moss_progress('{"step": 3}\n') == (3, None)


@pytest.mark.parametrize(
    "change",
    [
        {"trainer": "kohya"},
        {"output_name": "../x"},
        {"trainer_commit": "abc"},
        {"models": []},
        {"models": [{"file": "m", "dest": "../etc/passwd", "sha256": "a" * 64}]},
        {"models": [{"file": "m", "dest": "/abs/x", "sha256": "a" * 64}]},
        {"models": [{"file": "m", "dest": "a/b/c", "sha256": "a" * 64}]},
        {"files": [{"file": "../a.png", "sha256": "a" * 64}]},
        {"files": [{"file": "a.sh", "sha256": "a" * 64}]},
        {"checkpoint_steps": [2, 3]},
        {"run_config": "  "},
    ],
)
def test_job_validation(rig: MossRig, change: dict[str, Any]) -> None:
    rig.job.update(change)
    with pytest.raises(tr.RunError):
        tr.MossJob.parse(json.dumps(rig.job))


def test_good_job_parses(rig: MossRig) -> None:
    job = tr.MossJob.parse(json.dumps(rig.job))
    assert job.checkpoint_file(2) == "lora-step00000002.safetensors"
    assert len(job.models) == 2
