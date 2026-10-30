"""SlopTweak LoRA training runner (instance side, standard library only).

Started by provision.sh for ``BACKEND=train``. It waits for the launcher to
upload ``job.json`` and ``dataset.tar`` through the sidecar and to POST
``/__train/start``, then runs the stages in order and reports through
``state.json`` and ``manifest.json`` (both read by the sidecar):

    bootstrap -> prepare -> train -> evaluate -> done

* bootstrap: pinned uv, Python 3.12.12, hash-locked requirements, and the
  kohya sd-scripts commit (source tree digest checked). Never touches the
  image's own Python.
* prepare: check the model and the extracted dataset against the hashes in
  job.json, write the configs.
* train / evaluate: ``sdxl_train_network.py`` then ``sdxl_gen_img.py`` for the
  base model and every checkpoint at every strength.

A job whose ``job.json`` says ``"trainer": "kohakumoss"`` runs ``MossRunner``
instead: KohakuMossTrainer on Kromoss (docs/spikes/kromoss-lora-training.md),
bootstrap -> prepare -> train (text and latent caches, then training, with
progress from ``metrics.jsonl``) -> done. It has no evaluate stage yet.

The manifest lists result files (checkpoints, evaluation images, loss log)
with SHA-256 as they appear, so the launcher can fetch and verify them even if
the run dies part-way. Only manifest paths are ever served.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tarfile
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import kernel_cache

PYTHON_VERSION = "3.12.12"
TORCH_INDEX = "https://download.pytorch.org/whl/cu124"
PYPI_INDEX = "https://pypi.org/simple"
TRAINER_REPO = "kohya-ss/sd-scripts"
MOSS_TRAINER_REPO = "KohakuBlueleaf/KohakuMossTrainer"
# The cu128 index only has torch <= 2.11 and the trainer needs >= 2.13. Offers
# need host CUDA >= 12.8 (a 12.6 host broke CUDA with Error 804).
MOSS_TORCH_INDEX = "https://download.pytorch.org/whl/cu126"
# Every package of the trainer's environment (torch included, from the cu126
# index) with hashes: uv pip compile trainer pyproject --extra cuda + torch==2.14.1.
MOSS_LOCK = "moss_requirements.lock"
# Tuned-kernel tables shipped in the asset bundle, and the ones a run hands back.
MOSS_KERNEL_CACHE = "moss_kernel_cache.tgz"
MOSS_KERNEL_EXPORT = "kernel-cache.tgz"
MOSS_RUN_CONFIG = "sloptweak-train.py"
MOSS_DATA_CONFIG = "sloptweak-train_data.py"
# Hex of a SHA-256 / a git commit.
HEX64 = re.compile(r"^[0-9a-f]{64}$")
HEX40 = re.compile(r"^[0-9a-f]{40}$")
SAFE_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
DATA_EXTS = frozenset({".png", ".jpg", ".txt"})
MAX_DATASET_FILES = 1200
STAGES = ("waiting", "bootstrap", "prepare", "train", "evaluate", "done", "failed", "cancelled")
FINAL_STAGES = frozenset({"done", "failed", "cancelled"})
# kohya prints e.g. ``steps:  45%|####| 415/920 [01:02<01:15,  6.7it/s, avr_loss=0.0773]``
PROGRESS_RE = re.compile(r"(\d+)/(\d+) \[[^\]]*?(?:avr_loss=([0-9.]+))?\]")

Executor = Callable[[Sequence[str], Path, Path, Mapping[str, str], Callable[[], None]], int]


class RunError(Exception):
    """A stage failed; the message is shown to the user."""


class Cancelled(Exception):
    """The launcher asked to stop."""


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(8 * 1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def tree_digest(archive: Path) -> str:
    """Digest of a source archive's file contents and relative paths, ignoring
    tar metadata (GitHub's archive bytes are not stable, its contents are)."""
    items: list[tuple[str, str]] = []
    with tarfile.open(archive) as tar:
        for member in tar.getmembers():
            if not member.isfile():
                continue
            rel = member.name.split("/", 1)[1] if "/" in member.name else member.name
            f = tar.extractfile(member)
            assert f is not None
            items.append((rel, hashlib.sha256(f.read()).hexdigest()))
    h = hashlib.sha256()
    for rel, digest in sorted(items):
        h.update(f"{digest}  {rel}\n".encode())
    return h.hexdigest()


def atomic_write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    tmp.replace(path)


@dataclass(frozen=True)
class Paths:
    work: Path

    @property
    def incoming(self) -> Path:
        return self.work / "incoming"

    @property
    def data(self) -> Path:
        return self.work / "data"

    @property
    def runtime(self) -> Path:
        return self.work / "runtime"

    @property
    def trainer(self) -> Path:
        return self.work / "trainer"

    @property
    def tokenizers(self) -> Path:
        return self.work / "tokenizers"

    @property
    def venv(self) -> Path:
        return self.work / "venv"

    @property
    def toolchain(self) -> Path:
        return self.work / "toolchain"

    @property
    def checkpoints(self) -> Path:
        return self.runtime / "checkpoints"

    @property
    def evaluation(self) -> Path:
        return self.runtime / "evaluation"

    @property
    def state(self) -> Path:
        return self.work / "state.json"

    @property
    def manifest(self) -> Path:
        return self.work / "manifest.json"

    @property
    def start_marker(self) -> Path:
        return self.incoming / "start"

    @property
    def cancel_marker(self) -> Path:
        return self.incoming / "cancel"

    @property
    def python(self) -> Path:
        return self.venv / "bin" / "python"

    @property
    def uv(self) -> Path:
        return self.toolchain / "uv" / "bin" / "uv"


@dataclass(frozen=True)
class Job:
    output_name: str
    model_path: Path
    model_sha256: str
    trainer_commit: str
    trainer_tree_sha256: str
    files: dict[str, str]
    total_steps: int
    checkpoint_steps: tuple[int, ...]
    eval_checkpoint_steps: tuple[int, ...]
    eval_strengths: tuple[float, ...]
    eval_images_per_run: int
    train_toml: str
    dataset_toml: str
    eval_prompts: str
    # False: train and save checkpoints only, make no test images.
    eval_enabled: bool = True

    @classmethod
    def parse(cls, text: str, models_dir: Path) -> Job:
        try:
            raw = json.loads(text)
            assert isinstance(raw, dict)
            name = str(raw["output_name"])
            model_file = str(raw["model_file"])
            files = {str(f["file"]): str(f["sha256"]) for f in raw["files"]}
            job = cls(
                output_name=name,
                model_path=models_dir / model_file,
                model_sha256=str(raw["model_sha256"]).lower(),
                trainer_commit=str(raw["trainer_commit"]).lower(),
                trainer_tree_sha256=str(raw["trainer_tree_sha256"]).lower(),
                files=files,
                total_steps=int(raw["total_steps"]),
                checkpoint_steps=tuple(int(s) for s in raw["checkpoint_steps"]),
                # Older jobs tested every checkpoint.
                eval_checkpoint_steps=tuple(
                    int(s)
                    for s in (
                        []
                        if raw.get("eval_enabled", True) is False
                        else raw.get("eval_checkpoints") or raw["checkpoint_steps"]
                    )
                ),
                eval_strengths=tuple(float(s) for s in raw["eval_strengths"]),
                eval_images_per_run=int(raw["eval_images_per_run"]),
                train_toml=str(raw["train_toml"]),
                dataset_toml=str(raw["dataset_toml"]),
                eval_prompts=str(raw["eval_prompts"]),
                eval_enabled=raw.get("eval_enabled", True) is not False,
            )
        except (KeyError, TypeError, ValueError, AssertionError) as e:
            raise RunError(f"job.json is not valid: {e!r}") from e
        job.validate(model_file)
        return job

    def validate(self, model_file: str) -> None:
        if not SAFE_NAME.match(self.output_name):
            raise RunError("job.json: bad output name")
        if not SAFE_NAME.match(model_file):
            raise RunError("job.json: bad model file name")
        if not HEX64.match(self.model_sha256) or not HEX64.match(self.trainer_tree_sha256):
            raise RunError("job.json: bad SHA-256")
        if not HEX40.match(self.trainer_commit):
            raise RunError("job.json: bad trainer commit")
        if not self.files or len(self.files) > MAX_DATASET_FILES:
            raise RunError("job.json: bad dataset file list")
        for name, digest in self.files.items():
            if (
                not SAFE_NAME.match(name)
                or Path(name).suffix.lower() not in DATA_EXTS
                or not HEX64.match(digest)
            ):
                raise RunError(f"job.json: bad dataset file entry {name!r}")
        if self.total_steps < 1 or not self.checkpoint_steps:
            raise RunError("job.json: bad step counts")
        if self.checkpoint_steps[-1] != self.total_steps:
            raise RunError("job.json: the last checkpoint must be the final step")
        if not set(self.eval_checkpoint_steps) <= set(self.checkpoint_steps):
            raise RunError("job.json: evaluation checkpoints must be saved checkpoints")
        if self.eval_enabled and (not self.eval_strengths or self.eval_images_per_run < 1):
            raise RunError("job.json: bad evaluation settings")

    def expected_eval_images(self) -> int:
        if not self.eval_enabled:
            return 0
        runs = 1 + len(self.eval_checkpoint_steps) * len(self.eval_strengths)
        return runs * self.eval_images_per_run

    def checkpoint_file(self, step: int) -> str:
        return f"{self.output_name}-step{step:08d}.safetensors"

    def eval_dirs(self) -> list[str]:
        dirs = ["base"]
        for step in self.eval_checkpoint_steps:
            dirs.extend(f"{step}-{s}" for s in self.eval_strengths)
        return dirs


def parse_progress(log_tail: str) -> tuple[int, int, float | None] | None:
    """Latest (step, total, loss) from trainer output, if any."""
    best: tuple[int, int, float | None] | None = None
    for m in PROGRESS_RE.finditer(log_tail):
        loss = float(m.group(3)) if m.group(3) else None
        best = (int(m.group(1)), int(m.group(2)), loss)
    return best


def tail(path: Path, nbytes: int = 8192) -> str:
    try:
        with path.open("rb") as f:
            f.seek(0, os.SEEK_END)
            f.seek(max(0, f.tell() - nbytes))
            return f.read().decode("utf-8", errors="replace").replace("\r", "\n")
    except OSError:
        return ""


def real_executor(
    cmd: Sequence[str],
    cwd: Path,
    log: Path,
    env: Mapping[str, str],
    on_tick: Callable[[], None],
) -> int:
    """Run ``cmd`` in its own process group, logging to ``log``; call ``on_tick``
    about every two seconds. ``on_tick`` may raise to stop the run."""
    log.parent.mkdir(parents=True, exist_ok=True)
    with log.open("ab") as out:
        proc = subprocess.Popen(  # noqa: S603
            list(cmd),
            cwd=cwd,
            stdout=out,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            env=dict(env),
            start_new_session=True,
        )
        try:
            while True:
                try:
                    return proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    on_tick()
        except BaseException:
            os.killpg(proc.pid, signal.SIGINT)
            try:
                proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
            raise


class Runner:
    def __init__(
        self,
        paths: Paths,
        asset_dir: Path,
        models_dir: Path,
        executor: Executor = real_executor,
        fetch_trainer: Callable[[Path, str], Path] | None = None,
        fetch_url: Callable[[str, Path], None] | None = None,
        clock: Callable[[], float] = time.time,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        self.p = paths
        self.asset_dir = asset_dir
        self.models_dir = models_dir
        self.run = executor
        self.fetch_trainer = fetch_trainer or download_trainer
        self.fetch_url = fetch_url or download_url
        self.clock = clock
        self.sleep = sleep
        self.job: Job | None = None
        self.state: dict[str, Any] = {"stage": "waiting"}
        self._hashes: dict[tuple[str, int, int], str] = {}
        self._files: dict[str, dict[str, Any]] = {}
        # Result files besides the checkpoints and logs (names under runtime/).
        self.extra_results: tuple[str, ...] = ()

    # ----- reporting ---------------------------------------------------------

    def set_state(self, stage: str, **extra: Any) -> None:
        assert stage in STAGES
        self.state = {"stage": stage, "updated": int(self.clock()), **extra}
        atomic_write(self.p.state, json.dumps(self.state))

    def check_cancel(self) -> None:
        if self.p.cancel_marker.exists():
            raise Cancelled

    def refresh_manifest(self) -> None:
        """Hash result files that are new or changed; rewrite the manifest."""
        candidates: list[Path] = []
        candidates += sorted(self.p.checkpoints.glob("*.safetensors"))
        candidates += sorted(self.p.evaluation.glob("*/*.png"))
        candidates += sorted((self.p.runtime / "logs").glob("*.log"))
        for name in ("loss.csv", "metrics.jsonl", "train.toml", "dataset.toml", "eval-prompts.txt"):
            candidates.append(self.p.runtime / name)
        candidates += [self.p.runtime / name for name in self.extra_results]
        changed = False
        for path in candidates:
            try:
                st = path.stat()
            except OSError:
                continue
            # Skip files that may still be mid-write.
            if self.clock() - st.st_mtime < 3:
                continue
            key = (str(path), st.st_size, int(st.st_mtime))
            rel = path.relative_to(self.p.runtime).as_posix()
            if self._files.get(rel, {}).get("key") == key:
                continue
            digest = self._hashes.get(key) or sha256_file(path)
            self._hashes[key] = digest
            self._files[rel] = {"path": rel, "sha256": digest, "bytes": st.st_size, "key": key}
            changed = True
        if changed:
            listing = [{k: v for k, v in f.items() if k != "key"} for f in self._files.values()]
            atomic_write(self.p.manifest, json.dumps({"files": listing}))

    # ----- stages ------------------------------------------------------------

    def wait_for_start(self) -> None:
        self.set_state("waiting")
        while not self.p.start_marker.exists():
            self.sleep(2)

    def command(
        self,
        cmd: Sequence[str],
        log: Path,
        tick: Callable[[], None],
        cwd: Path | None = None,
        extra_env: Mapping[str, str] | None = None,
    ) -> None:
        env = {
            **os.environ,
            "HF_HUB_OFFLINE": "1",
            "TRANSFORMERS_OFFLINE": "1",
            **(extra_env or {}),
        }
        code = self.run(cmd, cwd or self.p.work, log, env, tick)
        if code != 0:
            script = cmd[1] if len(cmd) > 1 and cmd[1].endswith(".py") else cmd[0]
            # The cause is usually in the log's last lines; show them, not just a name.
            last = " / ".join(ln.strip() for ln in tail(log, 1500).splitlines() if ln.strip())
            cause = f": {last[-400:]}" if last else ""
            raise RunError(f"{Path(script).name} failed (exit {code}); see {log.name}{cause}")

    def bootstrap(self) -> None:
        self.set_state("bootstrap", detail="installing the training environment")
        logs = self.p.runtime / "logs"
        steps: list[tuple[str, list[str]]] = [
            (
                "tooling",
                [
                    sys.executable,
                    "-m",
                    "pip",
                    "install",
                    "--target",
                    str(self.p.toolchain / "uv"),
                    "--require-hashes",
                    "-r",
                    str(self.asset_dir / "train_bootstrap.lock"),
                ],
            ),
            ("python", [str(self.p.uv), "python", "install", PYTHON_VERSION]),
            ("venv", [str(self.p.uv), "venv", "--python", PYTHON_VERSION, str(self.p.venv)]),
            (
                "packages",
                [
                    str(self.p.uv),
                    "pip",
                    "sync",
                    "--python",
                    str(self.p.python),
                    "--require-hashes",
                    str(self.asset_dir / "train_requirements.lock"),
                    "--index",
                    TORCH_INDEX,
                    "--index",
                    PYPI_INDEX,
                    "--index-strategy",
                    "unsafe-best-match",
                ],
            ),
            ("check", [str(self.p.uv), "pip", "check", "--python", str(self.p.python)]),
        ]
        for name, cmd in steps:
            self.check_cancel()
            self.set_state("bootstrap", detail=f"installing: {name}")
            self.command(
                cmd,
                logs / f"bootstrap-{name}.log",
                self.check_cancel,
                extra_env={"UV_PYTHON_INSTALL_DIR": str(self.p.toolchain / "python")},
            )

        self.check_cancel()
        assert self.job is not None
        self.set_state("bootstrap", detail="fetching the tokenizers")
        fetch_tokenizers(
            self.asset_dir / "train_tokenizers.json", self.p.tokenizers, self.fetch_url
        )
        self.check_cancel()
        self.set_state("bootstrap", detail="fetching the trainer")
        archive = self.fetch_trainer(self.p.work / "trainer.tar.gz", self.job.trainer_commit)
        if tree_digest(archive) != self.job.trainer_tree_sha256:
            raise RunError("The trainer source does not match its pinned digest.")
        if self.p.trainer.exists():
            shutil.rmtree(self.p.trainer)
        extract = self.p.work / "trainer-extract"
        if extract.exists():
            shutil.rmtree(extract)
        with tarfile.open(archive) as tar:
            tar.extractall(extract, filter="data")
        (root,) = [d for d in extract.iterdir() if d.is_dir()]
        root.rename(self.p.trainer)

    def prepare(self) -> None:
        assert self.job is not None
        job = self.job
        self.set_state("prepare", detail="checking the model")
        if not job.model_path.is_file() or sha256_file(job.model_path) != job.model_sha256:
            raise RunError("The base model is missing or its checksum is wrong.")
        self.set_state("prepare", detail="unpacking the dataset")
        if self.p.data.exists():
            shutil.rmtree(self.p.data)
        self.p.data.mkdir(parents=True)
        extract_dataset(self.p.incoming / "dataset.tar", self.p.data, job.files)
        self.p.runtime.mkdir(parents=True, exist_ok=True)
        atomic_write(self.p.runtime / "train.toml", job.train_toml)
        atomic_write(self.p.runtime / "dataset.toml", job.dataset_toml)
        atomic_write(self.p.runtime / "eval-prompts.txt", job.eval_prompts)

    def tick_train(self) -> None:
        self.check_cancel()
        assert self.job is not None
        progress = parse_progress(tail(self.p.runtime / "logs" / "train.log"))
        if progress:
            step, total, loss = progress
            self.set_state("train", step=step, total=total, loss=loss)
        self.refresh_manifest()

    def train(self) -> None:
        assert self.job is not None
        self.set_state("train", step=0, total=self.job.total_steps, loss=None)
        cmd = [
            str(self.p.python),
            str(self.p.trainer / "sdxl_train_network.py"),
            "--config_file",
            str(self.p.runtime / "train.toml"),
        ]
        self.command(
            cmd, self.p.runtime / "logs" / "train.log", self.tick_train, cwd=self.p.trainer
        )
        final = self.p.checkpoints / self.job.checkpoint_file(self.job.total_steps)
        if not final.is_file():
            raise RunError("Training ended without writing the final checkpoint.")
        self.write_loss_csv()
        self.refresh_manifest()

    def write_loss_csv(self) -> None:
        """One row per step (the progress bar repeats lines), last value wins."""
        text = (self.p.runtime / "logs" / "train.log").read_text("utf-8", "replace")
        by_step: dict[int, str] = {}
        for m in PROGRESS_RE.finditer(text.replace(chr(13), chr(10))):
            if m.group(3):
                by_step[int(m.group(1))] = m.group(3)
        rows = ["step,loss", *(f"{step},{loss}" for step, loss in sorted(by_step.items()))]
        atomic_write(self.p.runtime / "loss.csv", chr(10).join(rows) + chr(10))

    def evaluation_commands(self) -> list[tuple[str, list[str]]]:
        assert self.job is not None
        job = self.job
        if not job.eval_enabled:
            return []
        base = [
            str(self.p.python),
            str(self.p.trainer / "sdxl_gen_img.py"),
            "--ckpt",
            str(job.model_path),
            "--from_file",
            str(self.p.runtime / "eval-prompts.txt"),
            "--tokenizer_cache_dir",
            str(self.p.tokenizers),
            "--max_embeddings_multiples",
            "2",
            "--sampler",
            "euler_a",
            "--bf16",
            "--no_half_vae",
            "--sdpa",
            "--sequential_file_name",
        ]
        out: list[tuple[str, list[str]]] = [
            ("base", [*base, "--outdir", str(self.p.evaluation / "base")])
        ]
        for step in job.eval_checkpoint_steps:
            weights = self.p.checkpoints / job.checkpoint_file(step)
            for strength in job.eval_strengths:
                name = f"{step}-{strength}"
                out.append(
                    (
                        name,
                        [
                            *base,
                            "--network_module",
                            "networks.lora",
                            "--network_weights",
                            str(weights),
                            "--network_mul",
                            str(strength),
                            "--outdir",
                            str(self.p.evaluation / name),
                        ],
                    )
                )
        return out

    def evaluate(self) -> None:
        assert self.job is not None
        job = self.job
        total = job.expected_eval_images()

        def done_count() -> int:
            return sum(1 for _ in self.p.evaluation.glob("*/*.png"))

        def tick() -> None:
            self.check_cancel()
            self.set_state("evaluate", done=done_count(), total=total)
            self.refresh_manifest()

        self.set_state("evaluate", done=0, total=total)
        for i, (name, cmd) in enumerate(self.evaluation_commands()):
            out_dir = self.p.evaluation / name
            existing = len(list(out_dir.glob("*.png"))) if out_dir.exists() else 0
            if existing >= job.eval_images_per_run:
                continue  # finished before a restart
            if existing:
                shutil.rmtree(out_dir)  # partial: redo this one set
            self.command(
                cmd, self.p.runtime / "logs" / f"evaluate-{i}.log", tick, cwd=self.p.trainer
            )
        self.set_state("evaluate", done=done_count(), total=total)

    # ----- driver ------------------------------------------------------------

    def execute(self) -> None:
        try:
            self.job = Job.parse(
                (self.p.incoming / "job.json").read_text(encoding="utf-8"), self.models_dir
            )
            self.bootstrap()
            self.check_cancel()
            self.prepare()
            self.check_cancel()
            self.train()
            self.check_cancel()
            self.evaluate()
            self.refresh_manifest()
            self.sleep(3.5)  # let the last files age past the mid-write guard
            self.refresh_manifest()
            self.set_state("done")
        except Cancelled:
            self.sleep(3.5)  # the last log must age past the mid-write guard to be saved
            self.refresh_manifest()
            self.set_state("cancelled")
        except RunError as e:
            self.sleep(3.5)
            self.refresh_manifest()
            self.set_state("failed", error=str(e))
        except Exception as e:
            self.sleep(3.5)
            self.refresh_manifest()
            self.set_state("failed", error=f"Unexpected error: {type(e).__name__}: {e}")


# ----- Kromoss (KohakuMossTrainer) ---------------------------------------------


@dataclass(frozen=True)
class MossModel:
    """A weight file: downloaded flat into the models dir by provision.sh, then
    linked at ``dest`` (e.g. ``diffusion_models/x.safetensors``) in the
    trainer's ComfyUI-style folder."""

    file: str
    dest: str
    sha256: str


@dataclass(frozen=True)
class MossJob:
    output_name: str
    trainer_commit: str
    trainer_tree_sha256: str
    models: tuple[MossModel, ...]
    files: dict[str, str]
    total_steps: int
    checkpoint_steps: tuple[int, ...]
    run_config: str
    data_config: str
    train_jsonl: str

    @classmethod
    def parse(cls, text: str) -> MossJob:
        try:
            raw = json.loads(text)
            assert isinstance(raw, dict) and raw["trainer"] == "kohakumoss"
            job = cls(
                output_name=str(raw["output_name"]),
                trainer_commit=str(raw["trainer_commit"]).lower(),
                trainer_tree_sha256=str(raw["trainer_tree_sha256"]).lower(),
                models=tuple(
                    MossModel(str(m["file"]), str(m["dest"]), str(m["sha256"]).lower())
                    for m in raw["models"]
                ),
                files={str(f["file"]): str(f["sha256"]) for f in raw["files"]},
                total_steps=int(raw["total_steps"]),
                checkpoint_steps=tuple(int(s) for s in raw["checkpoint_steps"]),
                run_config=str(raw["run_config"]),
                data_config=str(raw["data_config"]),
                train_jsonl=str(raw.get("train_jsonl", "")),
            )
        except (KeyError, TypeError, ValueError, AssertionError) as e:
            raise RunError(f"job.json is not valid: {e!r}") from e
        job.validate()
        return job

    def validate(self) -> None:
        if not SAFE_NAME.match(self.output_name):
            raise RunError("job.json: bad output name")
        if not HEX64.match(self.trainer_tree_sha256) or not HEX40.match(self.trainer_commit):
            raise RunError("job.json: bad trainer pin")
        if not self.models:
            raise RunError("job.json: no model files")
        for m in self.models:
            dest = Path(m.dest)
            if (
                not SAFE_NAME.match(m.file)
                or dest.is_absolute()
                or len(dest.parts) != 2
                or not all(SAFE_NAME.match(part) for part in dest.parts)
                or not HEX64.match(m.sha256)
            ):
                raise RunError(f"job.json: bad model entry {m.file!r}")
        if not self.files or len(self.files) > MAX_DATASET_FILES:
            raise RunError("job.json: bad dataset file list")
        for name, digest in self.files.items():
            if (
                not SAFE_NAME.match(name)
                or Path(name).suffix.lower() not in DATA_EXTS
                or not HEX64.match(digest)
            ):
                raise RunError(f"job.json: bad dataset file entry {name!r}")
        if self.total_steps < 1 or not self.checkpoint_steps:
            raise RunError("job.json: bad step counts")
        if self.checkpoint_steps[-1] != self.total_steps:
            raise RunError("job.json: the last checkpoint must be the final step")
        if not self.run_config.strip() or not self.data_config.strip():
            raise RunError("job.json: missing trainer config")

    def checkpoint_file(self, step: int) -> str:
        return f"{self.output_name}-step{step:08d}.safetensors"


def parse_moss_progress(metrics_tail: str) -> tuple[int, float | None] | None:
    """Latest (step, loss) from the tail of the trainer's ``metrics.jsonl``. The
    first line of a tail read may be cut off, so unparseable lines are skipped."""
    for line in reversed(metrics_tail.splitlines()):
        try:
            row = json.loads(line)
            step = int(row["step"])
        except (ValueError, KeyError, TypeError):
            continue
        loss = row.get("loss")
        return step, float(loss) if isinstance(loss, int | float) else None
    return None


class MossRunner(Runner):
    """Runs a ``MossJob``. Shares the state, manifest and command plumbing with
    ``Runner``; the kohya stages are not used."""

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self.moss: MossJob | None = None
        self.extra_results = (MOSS_KERNEL_EXPORT,)
        # Where the trainer keeps its tuning tables (``Path.home() / .cache``).
        self.cache_home = Path.home()
        self._kernel_signature: tuple[tuple[str, int], ...] = ()

    @property
    def weights(self) -> Path:
        return self.p.work / "models"

    @property
    def run_dir(self) -> Path:
        assert self.moss is not None
        return self.p.trainer / "runs" / "moss" / self.moss.output_name

    def command(
        self,
        cmd: Sequence[str],
        log: Path,
        tick: Callable[[], None],
        cwd: Path | None = None,
        extra_env: Mapping[str, str] | None = None,
    ) -> None:
        # Unlike kohya, the Qwen tokenizer config may be looked up on first
        # use (the proven cloud run was online), and the venv's console
        # scripts must be on PATH.
        env = {
            "HF_HUB_OFFLINE": "0",
            "TRANSFORMERS_OFFLINE": "0",
            "PATH": f"{self.p.venv / 'bin'}{os.pathsep}{os.environ.get('PATH', '')}",
            **(extra_env or {}),
        }
        super().command(cmd, log, tick, cwd=cwd, extra_env=env)

    def bootstrap(self) -> None:
        assert self.moss is not None
        self.set_state("bootstrap", detail="installing the training environment")
        logs = self.p.runtime / "logs"
        venv_python = str(self.p.python)
        uv = str(self.p.uv)
        steps: list[tuple[str, list[str]]] = [
            (
                "tooling",
                [
                    sys.executable,
                    "-m",
                    "pip",
                    "install",
                    "--target",
                    str(self.p.toolchain / "uv"),
                    "--require-hashes",
                    "-r",
                    str(self.asset_dir / "train_bootstrap.lock"),
                ],
            ),
            ("python", [uv, "python", "install", PYTHON_VERSION]),
            ("venv", [uv, "venv", "--python", PYTHON_VERSION, str(self.p.venv)]),
            (
                "torch",
                [
                    uv,
                    "pip",
                    "install",
                    "--python",
                    venv_python,
                    "--require-hashes",
                    "--no-deps",
                    "-r",
                    str(self.asset_dir / MOSS_LOCK),
                    "--extra-index-url",
                    MOSS_TORCH_INDEX,
                    "--index-strategy",
                    "unsafe-best-match",
                ],
            ),
        ]
        for name, cmd in steps:
            self.check_cancel()
            self.set_state("bootstrap", detail=f"installing: {name}")
            self.command(
                cmd,
                logs / f"bootstrap-{name}.log",
                self.check_cancel,
                extra_env={"UV_PYTHON_INSTALL_DIR": str(self.p.toolchain / "python")},
            )

        self.check_cancel()
        self.set_state("bootstrap", detail="fetching the trainer")
        archive = self.fetch_trainer(self.p.work / "trainer.tar.gz", self.moss.trainer_commit)
        if tree_digest(archive) != self.moss.trainer_tree_sha256:
            raise RunError("The trainer source does not match its pinned digest.")
        if self.p.trainer.exists():
            shutil.rmtree(self.p.trainer)
        extract = self.p.work / "trainer-extract"
        if extract.exists():
            shutil.rmtree(extract)
        with tarfile.open(archive) as tar:
            tar.extractall(extract, filter="data")
        (root,) = [d for d in extract.iterdir() if d.is_dir()]
        root.rename(self.p.trainer)

        # The trainer itself, without dependencies (the hash-locked set above
        # already holds them), then a CUDA probe so a host whose driver is too
        # old fails here and not after the caches.
        self.check_cancel()
        self.set_state("bootstrap", detail="installing: trainer")
        self.command(
            [
                *(uv, "pip", "install", "--python", venv_python, "--no-deps"),
                *("-e", str(self.p.trainer)),
            ],
            logs / "bootstrap-trainer.log",
            self.check_cancel,
            cwd=self.p.trainer,
        )
        self.set_state("bootstrap", detail="checking the GPU")
        probe = (
            "import torch; assert torch.cuda.is_available(); "
            "print(torch.__version__, torch.version.cuda, torch.cuda.get_device_name(0))"
        )
        self.command([venv_python, "-c", probe], logs / "bootstrap-cuda.log", self.check_cancel)
        self.restore_kernels()
        # Informational: the doctor may warn about optional extras.
        self.run(
            [str(self.p.venv / "bin" / "kohakumoss-doctor")],
            self.p.trainer,
            logs / "bootstrap-doctor.log",
            os.environ,
            self.check_cancel,
        )

    def restore_kernels(self) -> None:
        """Unpack the bundled tuning tables so the first steps don't pay for the
        timing runs. Tables for another card are never read, so this is harmless
        there; any problem is logged and the trainer just tunes as before."""
        archive = self.asset_dir / MOSS_KERNEL_CACHE
        log = self.p.runtime / "logs" / "kernel-cache.log"
        log.parent.mkdir(parents=True, exist_ok=True)
        if not archive.is_file():
            log.write_text("no bundled tuning tables\n", encoding="utf-8")
            return
        try:
            n = kernel_cache.restore(archive, self.cache_home)
            log.write_text(f"restored {n} tuning table files\n", encoding="utf-8")
        except (kernel_cache.CacheError, OSError) as e:
            log.write_text(f"could not restore tuning tables: {e}\n", encoding="utf-8")

    def export_kernels(self) -> None:
        """Pack the tuning tables as a result file, once they have changed, so a
        run on a card the bundle doesn't cover can add it (kernel_cache.py merge)."""
        try:
            sig = kernel_cache.signature(self.cache_home)
            if not sig or sig == self._kernel_signature:
                return
            kernel_cache.pack(self.cache_home, self.p.runtime / MOSS_KERNEL_EXPORT)
            self._kernel_signature = sig
        except OSError:
            pass

    def prepare(self) -> None:
        assert self.moss is not None
        job = self.moss
        self.set_state("prepare", detail="checking the models")
        for m in job.models:
            src = self.models_dir / m.file
            if not src.is_file() or sha256_file(src) != m.sha256:
                raise RunError(f"{m.file} is missing or its checksum is wrong.")
            link = self.weights / m.dest
            link.parent.mkdir(parents=True, exist_ok=True)
            link.unlink(missing_ok=True)
            try:
                link.symlink_to(src)
            except OSError:  # no symlink right (Windows dev boxes): same-disk hardlink
                os.link(src, link)
        self.set_state("prepare", detail="unpacking the dataset")
        if self.p.data.exists():
            shutil.rmtree(self.p.data)
        nl = self.p.data / "nl"
        nl.mkdir(parents=True)
        extract_dataset(self.p.incoming / "dataset.tar", nl, job.files)
        if job.train_jsonl:
            atomic_write(self.p.data / "mix" / "train.jsonl", job.train_jsonl)
        cfg_dir = self.p.trainer / "configs" / "user"
        atomic_write(cfg_dir / MOSS_RUN_CONFIG, job.run_config)
        atomic_write(cfg_dir / MOSS_DATA_CONFIG, job.data_config)
        atomic_write(self.p.runtime / "train.py", job.run_config)
        self.command(
            [
                str(self.p.venv / "bin" / "kohakumoss-config"),
                "check",
                f"configs/user/{MOSS_RUN_CONFIG}",
            ],
            self.p.runtime / "logs" / "config-check.log",
            self.check_cancel,
            cwd=self.p.trainer,
        )

    def sync_results(self) -> None:
        """Expose the trainer's per-step ComfyUI exports under the names the
        launcher fetches, and its metrics log; then refresh the manifest."""
        assert self.moss is not None
        job = self.moss
        for step in job.checkpoint_steps:
            src = (
                self.run_dir
                / "checkpoints"
                / f"step_{step:08d}"
                / "export"
                / f"{job.output_name}-comfy.safetensors"
            )
            dst = self.p.checkpoints / job.checkpoint_file(step)
            if src.is_file() and not dst.exists() and self.clock() - src.stat().st_mtime >= 3:
                dst.parent.mkdir(parents=True, exist_ok=True)
                tmp = dst.with_name(dst.name + ".part")
                shutil.copyfile(src, tmp)
                tmp.replace(dst)
        metrics = self.run_dir / "metrics.jsonl"
        copy = self.p.runtime / "metrics.jsonl"
        if metrics.is_file():
            # Only when the trainer wrote more, and keeping its time: a copy that is
            # always fresh would never age past the manifest's mid-write guard.
            old, new = (copy.stat() if copy.exists() else None), metrics.stat()
            if old is None or (old.st_size, int(old.st_mtime)) != (new.st_size, int(new.st_mtime)):
                tmp = copy.with_name(copy.name + ".part")
                shutil.copy2(metrics, tmp)
                tmp.replace(copy)
        self.export_kernels()
        self.refresh_manifest()

    def tick_train(self) -> None:
        self.check_cancel()
        assert self.moss is not None
        progress = parse_moss_progress(tail(self.run_dir / "metrics.jsonl", 65536))
        if progress:
            step, loss = progress
            self.set_state("train", step=step, total=self.moss.total_steps, loss=loss)
        self.sync_results()

    def train(self) -> None:
        assert self.moss is not None
        job = self.moss
        logs = self.p.runtime / "logs"
        kogine = str(self.p.venv / "bin" / "kogine")
        # The sidecar's idle watchdog counts "train" as active; caching can take
        # several minutes.
        self.set_state("train", step=0, total=job.total_steps, loss=None, detail="caching")
        for script, name in (("cache_text", "cache-text"), ("cache_latents", "cache-latents")):
            self.check_cancel()
            self.command(
                [
                    kogine,
                    "run",
                    f"scripts/data/{script}.py",
                    "--config",
                    f"configs/user/{MOSS_DATA_CONFIG}",
                ],
                logs / f"{name}.log",
                self.check_cancel,
                cwd=self.p.trainer,
            )
        self.set_state("train", step=0, total=job.total_steps, loss=None)
        self.command(
            [kogine, "run", "scripts/train/moss.py", "--config", f"configs/user/{MOSS_RUN_CONFIG}"],
            logs / "train.log",
            self.tick_train,
            cwd=self.p.trainer,
        )
        final = self.p.checkpoints / job.checkpoint_file(job.total_steps)
        self.sync_results()
        if not final.is_file():
            # The last export may be younger than the mid-write guard.
            self.sleep(3.5)
            self.sync_results()
        if not final.is_file():
            raise RunError("Training ended without writing the final checkpoint.")

    def execute(self) -> None:
        try:
            self.moss = MossJob.parse((self.p.incoming / "job.json").read_text(encoding="utf-8"))
            self.bootstrap()
            self.check_cancel()
            self.prepare()
            self.check_cancel()
            self.train()
            self.sleep(3.5)  # let the last files age past the mid-write guard
            self.sync_results()
            self.set_state("done")
        except Cancelled:
            self.sync_results_quietly()
            self.set_state("cancelled")
        except RunError as e:
            self.sync_results_quietly()
            self.set_state("failed", error=str(e))
        except Exception as e:
            self.sync_results_quietly()
            self.set_state("failed", error=f"Unexpected error: {type(e).__name__}: {e}")

    def sync_results_quietly(self) -> None:
        if self.moss is not None:
            # The failing step's log is seconds old; let it pass the mid-write guard.
            self.sleep(3.5)
            try:
                self.sync_results()
            except OSError:
                pass


def extract_dataset(archive: Path, dest: Path, expected: Mapping[str, str]) -> None:
    """Unpack a flat tar of images and captions, accepting only the files named
    in the job, each with its expected hash."""
    seen: set[str] = set()
    with tarfile.open(archive) as tar:
        for member in tar.getmembers():
            name = member.name
            if not member.isfile() or name not in expected:
                raise RunError(f"The dataset archive has an unexpected entry: {name!r}")
            f = tar.extractfile(member)
            assert f is not None
            data = f.read()
            if hashlib.sha256(data).hexdigest() != expected[name]:
                raise RunError(f"{name} changed in transit.")
            (dest / name).write_bytes(data)
            seen.add(name)
    missing = sorted(set(expected) - seen)
    if missing:
        raise RunError(f"The dataset archive is missing {missing[0]!r}.")


def download_url(url: str, dest: Path) -> None:
    import urllib.request

    if not url.startswith(("https://huggingface.co/", "https://codeload.github.com/")):
        raise RunError("Refusing to download from an unexpected host.")
    dest.parent.mkdir(parents=True, exist_ok=True)
    with urllib.request.urlopen(url, timeout=60) as resp, dest.open("wb") as out:  # noqa: S310 (host allow-listed above)
        shutil.copyfileobj(resp, out)


def download_trainer(dest: Path, commit: str, repo: str = TRAINER_REPO) -> Path:
    download_url(f"https://codeload.github.com/{repo}/tar.gz/{commit}", dest)
    return dest


def download_moss_trainer(dest: Path, commit: str) -> Path:
    return download_trainer(dest, commit, MOSS_TRAINER_REPO)


def fetch_tokenizers(lock: Path, dest: Path, fetch: Callable[[str, Path], None]) -> None:
    """Download the CLIP tokenizers the trainer would otherwise fetch unpinned, at
    locked revisions, into ``dest/<repo with / as _>/`` (kohya's cache layout)."""
    entries = json.loads(lock.read_text(encoding="utf-8"))
    for entry in entries:
        repo, revision = str(entry["repository"]), str(entry["revision"])
        if not re.fullmatch(r"[A-Za-z0-9][\w.-]*/[A-Za-z0-9][\w.-]*", repo) or not HEX40.match(
            revision
        ):
            raise RunError("The tokenizer lock is not valid.")
        folder = dest / repo.replace("/", "_")
        for name, digest in entry["files"].items():
            target = folder / str(name)
            if target.is_file() and sha256_file(target) == digest:
                continue
            part = target.with_name(target.name + ".part")
            fetch(f"https://huggingface.co/{repo}/resolve/{revision}/{name}", part)
            if sha256_file(part) != digest:
                part.unlink(missing_ok=True)
                raise RunError(f"Tokenizer file {name} does not match its checksum.")
            part.replace(target)


def main() -> None:
    work = Path(os.environ.get("TRAIN_WORK", "/opt/sloptweak/train"))
    asset_dir = Path(__file__).resolve().parent
    models_dir = Path(os.environ.get("TRAIN_MODELS_DIR", "/opt/sloptweak/models"))
    paths = Paths(work)
    runner = Runner(paths, asset_dir, models_dir)
    runner.wait_for_start()
    try:  # job.json is uploaded before the start marker
        raw = json.loads((paths.incoming / "job.json").read_text(encoding="utf-8"))
        kind = raw.get("trainer") if isinstance(raw, dict) else None
    except (OSError, ValueError):
        kind = None
    if kind == "kohakumoss":
        runner = MossRunner(paths, asset_dir, models_dir, fetch_trainer=download_moss_trainer)
    runner.execute()


if __name__ == "__main__":
    main()
